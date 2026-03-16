use crate::error::{BeaverError, BeaverResult, RuntimeError};
use crate::fixed_count_task::FixedCountTask;
use crate::periodic_task::PeriodicTask;
use crate::platform;
use crate::task::Task;
use crate::time_interval_task::TimeIntervalTask;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::sleep;

enum Splash {
    Run(Arc<Task>),
    CancelAll,
}

#[derive(Clone)]
pub(crate) struct DamBuilder {
    name: String,
    capacity: usize,
}

impl DamBuilder {
    pub(crate) fn new(name: impl Into<String>, buffer: usize) -> Self {
        DamBuilder {
            name: name.into(),
            capacity: buffer,
        }
    }

    /// Sets the queue capacity; `enqueue` returns `Err` when full.
    pub(crate) fn capacity(mut self, n: usize) -> Self {
        self.capacity = n;
        self
    }

    pub(crate) fn build(self) -> Dam {
        Dam::with_capacity(self.name, self.capacity)
    }
}

pub(crate) struct Dam {
    name: String,
    tx: Mutex<Option<mpsc::Sender<Splash>>>,
    current: Arc<Mutex<Option<Arc<Task>>>>,
    release_flag: AtomicBool,
}

impl Dam {
    /// Creates a dam with default capacity. Must be called within a tokio runtime.
    #[inline]
    pub(crate) fn new(name: impl Into<String>, buffer: usize) -> Self {
        Self::with_capacity(name, buffer)
    }

    /// Creates a dam with specified queue capacity. Must be called within a tokio runtime.
    pub(crate) fn with_capacity(name: impl Into<String>, buffer: usize) -> Self {
        let name = name.into();
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);

        let (tx, mut rx) = mpsc::channel::<Splash>(buffer);
        platform::spawn(async move {
            while let Some(msg) = rx.recv().await {
                run_loop_msg(&current_worker, msg, &mut rx).await;
            }
        });

        Self {
            name,
            tx: Mutex::new(Some(tx)),
            current,
            release_flag: AtomicBool::new(false),
        }
    }

    /// Creates a dam with a specified tokio runtime handle. Can be called outside tokio runtime.
    pub(crate) fn with_handle(name: impl Into<String>, capacity: usize, handle: Handle) -> Self {
        let name = name.into();
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);

        let (tx, mut rx) = mpsc::channel::<Splash>(capacity);
        platform::spawn_on(&handle, async move {
            while let Some(msg) = rx.recv().await {
                run_loop_msg(&current_worker, msg, &mut rx).await;
            }
        });

        Self {
            name,
            tx: Mutex::new(Some(tx)),
            current,
            release_flag: AtomicBool::new(false),
        }
    }

    pub(crate) fn builder(name: impl Into<String>, buffer: usize) -> DamBuilder {
        DamBuilder::new(name, buffer)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Adds a task to the queue.
    ///
    /// # Errors
    ///
    /// * [`BeaverError::DamReleased`] - The dam has been released.
    /// * [`BeaverError::QueueFull`] - The queue is full.
    /// * [`BeaverError::LockPoisoned`] - An internal lock was poisoned.
    pub(crate) fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        if self.release_flag.load(Ordering::Acquire) {
            return Err(BeaverError::DamReleased);
        }

        let guard = self.tx.lock()?;
        match guard.as_ref() {
            Some(tx) => tx
                .try_send(Splash::Run(task))
                .map_err(|_| BeaverError::QueueFull),
            None => Err(BeaverError::DamReleased),
        }
    }

    /// Cancels the current task and triggers listener callbacks for all queued tasks.
    ///
    /// # Errors
    ///
    /// * [`BeaverError::LockPoisoned`] - An internal lock was poisoned.
    pub(crate) fn cancel_all(&self) -> BeaverResult<()> {
        if let Some(tx) = self.tx.lock()?.as_ref() {
            let _ = tx.try_send(Splash::CancelAll);
        }
        if let Some(s) = self.current.lock()?.as_ref() {
            s.set_interrupted(true);
        }
        Ok(())
    }

    /// Releases the dam: stops accepting new tasks, interrupts current task,
    /// sends CancelAll, and closes the channel. The worker will exit.
    ///
    /// # Errors
    ///
    /// * [`BeaverError::LockPoisoned`] - An internal lock was poisoned.
    pub(crate) fn release(&self) -> BeaverResult<()> {
        self.release_flag.store(true, Ordering::Release);
        if let Some(s) = self.current.lock()?.as_ref() {
            s.interrupt();
        }
        if let Some(tx) = self.tx.lock()?.take() {
            let _ = tx.try_send(Splash::CancelAll);
        }
        Ok(())
    }
}

const INTERRUPT_ORDERING: Ordering = Ordering::Relaxed;

/// Executes a time-interval task: waits according to intervals (milliseconds), then executes work,
/// until it returns Done or reaches the last attempt.
#[inline]
async fn run_time_interval(task: &TimeIntervalTask) {
    let intervals = &task.intervals[..];
    let work = &task.work;
    let listener = task.listener.as_ref();

    for (i, &millis) in intervals.iter().enumerate() {
        if task.interrupted.load(INTERRUPT_ORDERING) {
            if let Some(l) = listener {
                l.on_interrupt();
            }
            return;
        }

        if millis > 0 {
            sleep(Duration::from_millis(millis)).await;
        }

        let result = work.execute().await;
        if !result.need_retry() {
            return;
        }

        if i == intervals.len() - 1 {
            if let Some(l) = listener {
                l.on_complete();
            }
        }
    }
}

/// Executes a fixed-count task: runs at most `count` times, calling progress before each attempt.
#[inline]
async fn run_fixed_count(task: &FixedCountTask) {
    let total = task.count;
    let work = &task.work;
    let progress = task.progress.as_ref();
    let listener = task.listener.as_ref();
    let tag = task.tag.as_deref().map_or("", |v| v);

    for current in 1..=total {
        if task.interrupted.load(INTERRUPT_ORDERING) {
            if let Some(l) = listener {
                l.on_interrupt();
            }
            return;
        }

        if let Some(p) = progress {
            p.on_progress(current, total, tag);
        }

        let result = work.execute().await;
        if !result.need_retry() {
            return;
        }

        if current == total {
            if let Some(l) = listener {
                l.on_complete();
            }
        }
    }
}

/// Executes a periodic task: loops indefinitely at fixed intervals until interrupted or work returns Done.
#[inline]
async fn run_periodic(task: &PeriodicTask) {
    let interval = Duration::from_millis(task.interval_ms);
    let work = &task.work;
    let listener = task.listener.as_ref();

    if task.initial_delay && !interval.is_zero() {
        sleep(interval).await;
    }

    loop {
        if task.interrupted.load(INTERRUPT_ORDERING) {
            if let Some(l) = listener {
                l.on_interrupt();
            }
            return;
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                l.on_complete();
            }
            return;
        }

        if !interval.is_zero() {
            sleep(interval).await;
        }
    }
}

/// Dispatches execution based on task type.
#[inline]
async fn run_task(task: &Task) {
    match task {
        Task::TimeInterval(s) => run_time_interval(s).await,
        Task::FixedCount(s) => run_fixed_count(s).await,
        Task::Periodic(s) => run_periodic(s).await,
    }
}

fn notify_error(task: &Task, error: RuntimeError) {
    match task {
        Task::TimeInterval(t) => {
            if let Some(l) = &t.listener {
                l.on_error(error);
            }
        }
        Task::FixedCount(t) => {
            if let Some(l) = &t.listener {
                l.on_error(error);
            }
        }
        Task::Periodic(t) => {
            if let Some(l) = &t.listener {
                l.on_error(error);
            }
        }
    }
}

async fn run_loop_msg(
    current_worker: &Mutex<Option<Arc<Task>>>,
    msg: Splash,
    rx: &mut mpsc::Receiver<Splash>,
) {
    match msg {
        Splash::Run(s) => {
            match current_worker.lock() {
                Ok(mut guard) => *guard = Some(Arc::clone(&s)),
                Err(_) => {
                    notify_error(&s, RuntimeError::LockPoisoned);
                    return;
                }
            }

            run_task(s.as_ref()).await;

            match current_worker.lock() {
                Ok(mut guard) => *guard = None,
                Err(_) => {
                    notify_error(&s, RuntimeError::LockPoisoned);
                }
            }
        }
        Splash::CancelAll => {
            if let Ok(guard) = current_worker.lock() {
                if let Some(s) = guard.as_ref() {
                    s.set_interrupted(true);
                }
            }
            while let Ok(Splash::Run(s)) = rx.try_recv() {
                s.interrupt();
            }
        }
    }
}
