use crate::error::{BeaverError, BeaverResult};
use crate::fixed_count_task::FixedCountTask;
use crate::periodic_task::PeriodicTask;
use crate::platform;
use crate::range_interval_task::RangeIntervalTask;
use crate::task::Task;
use crate::time_interval_task::TimeIntervalTask;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// A task handed to the worker, tagged with the sequence number it was assigned
/// at enqueue time. The sequence lets the worker decide, when it finally pulls
/// the task off the queue, whether the task was cancelled while it waited.
struct Splash {
    task: Arc<Task>,
    seq: u64,
}

pub(crate) struct Dam {
    tx: Mutex<Option<mpsc::Sender<Splash>>>,
    current: Arc<Mutex<Option<Arc<Task>>>>,
    release_flag: AtomicBool,
    /// Monotonic counter; each enqueued task is stamped with the value it reads
    /// here. Never decreases.
    enqueue_seq: AtomicU64,
    /// Cancellation watermark shared with the worker. Any task whose stamped
    /// `seq` is **strictly less** than this value was already enqueued when a
    /// cancel was requested, so the worker drops it instead of running it.
    /// Tasks enqueued afterwards get a higher `seq` and run normally.
    cancel_watermark: Arc<AtomicU64>,
    /// Join handle of the background worker task, taken by `destroy` so it can
    /// await the worker's termination (graceful shutdown). `None` once taken.
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Dam {
    /// Creates a dam with the given queue capacity. Must be called within a tokio runtime.
    ///
    /// `name` is currently unused (reserved for future task naming via
    /// `tokio::task::Builder::name`); it is kept on the signature so that the
    /// API can be extended without a breaking change.
    #[inline]
    pub(crate) fn new(name: impl Into<String>, buffer: usize) -> Self {
        Self::with_capacity(name, buffer)
    }

    /// Creates a dam with the specified queue capacity. Must be called within a tokio runtime.
    pub(crate) fn with_capacity(_name: impl Into<String>, buffer: usize) -> Self {
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);
        let cancel_watermark = Arc::new(AtomicU64::new(0));
        let watermark_worker = Arc::clone(&cancel_watermark);

        let (tx, mut rx) = mpsc::channel::<Splash>(buffer);
        let join = platform::spawn(async move {
            while let Some(msg) = rx.recv().await {
                run_loop_msg(&current_worker, &watermark_worker, msg).await;
            }
        });

        Self {
            tx: Mutex::new(Some(tx)),
            current,
            release_flag: AtomicBool::new(false),
            enqueue_seq: AtomicU64::new(0),
            cancel_watermark,
            worker: Mutex::new(Some(join)),
        }
    }

    /// Creates a dam with a specified tokio runtime handle. Can be called outside tokio runtime.
    pub(crate) fn with_handle(_name: impl Into<String>, capacity: usize, handle: Handle) -> Self {
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);
        let cancel_watermark = Arc::new(AtomicU64::new(0));
        let watermark_worker = Arc::clone(&cancel_watermark);

        let (tx, mut rx) = mpsc::channel::<Splash>(capacity);
        let join = platform::spawn_on(&handle, async move {
            while let Some(msg) = rx.recv().await {
                run_loop_msg(&current_worker, &watermark_worker, msg).await;
            }
        });

        Self {
            tx: Mutex::new(Some(tx)),
            current,
            release_flag: AtomicBool::new(false),
            enqueue_seq: AtomicU64::new(0),
            cancel_watermark,
            worker: Mutex::new(Some(join)),
        }
    }

    /// Adds a task to the queue.
    ///
    /// # Errors
    ///
    /// * [`BeaverError::DamReleased`] - The dam has been released.
    /// * [`BeaverError::QueueFull`] - The queue is full.
    pub(crate) async fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        if self.release_flag.load(Ordering::Acquire) {
            return Err(BeaverError::DamReleased);
        }

        // Stamp the task with the next sequence number *before* sending it so
        // that a concurrent cancel can decide whether this task predates it.
        let seq = self.enqueue_seq.fetch_add(1, Ordering::AcqRel);
        let guard = self.tx.lock()?;
        match guard.as_ref() {
            Some(tx) => tx
                .try_send(Splash { task, seq })
                .map_err(|_| BeaverError::QueueFull),
            None => Err(BeaverError::DamReleased),
        }
    }

    /// Cancels the currently running task and every task already queued.
    ///
    /// Cancellation is driven by the [`cancel_watermark`](Self::cancel_watermark):
    /// every task enqueued up to this point has a `seq` below the snapshot we
    /// take here, so the worker will drop (and interrupt) each of them as it
    /// dequeues them — even tasks sitting behind a long-running blocker. Tasks
    /// enqueued **after** this call get a higher `seq` and run normally.
    pub(crate) async fn cancel_all(&self) -> BeaverResult<()> {
        let watermark = self.enqueue_seq.load(Ordering::Acquire);
        self.cancel_watermark.fetch_max(watermark, Ordering::AcqRel);
        if let Some(s) = self.current.lock()?.as_ref() {
            s.set_interrupted(true);
        }
        Ok(())
    }

    /// Releases the dam: stops accepting new tasks, cancels the current task and
    /// the entire backlog, and closes the channel so the worker exits.
    pub(crate) async fn release(&self) -> BeaverResult<()> {
        self.release_flag.store(true, Ordering::Release);
        // Cancel everything still queued (and anything that races in): no task
        // can have a `seq` of `u64::MAX`, so the worker drops all of them.
        self.cancel_watermark.store(u64::MAX, Ordering::Release);
        // Use `set_interrupted` (flag only, no synchronous callback) rather than
        // `interrupt()` here: the currently-running task detects the flag at its
        // next loop check and fires `on_interrupt` itself. Calling `interrupt()`
        // would fire `on_interrupt` synchronously *and* let the running loop fire
        // it again — a double callback. This keeps `release`/`destroy` consistent
        // with `cancel_all`, which also fires `on_interrupt` exactly once.
        if let Some(s) = self.current.lock()?.as_ref() {
            s.set_interrupted(true);
        }
        // Dropping the sender lets the worker drain the remaining buffered tasks
        // (all now cancelled by the watermark) and then exit when the channel
        // closes.
        let _ = self.tx.lock()?.take();
        Ok(())
    }

    /// Takes the background worker's join handle so the caller can await its
    /// termination. Returns `None` if it was already taken or the lock is
    /// poisoned. Used by [`Beaver::destroy`](crate::Beaver::destroy) for a
    /// bounded graceful shutdown.
    pub(crate) fn take_worker(&self) -> Option<JoinHandle<()>> {
        self.worker.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for Dam {
    /// Safety net for callers who forget to `destroy()` / `release()`.
    ///
    /// Without this, dropping a `Beaver` that still has a running
    /// [`PeriodicTask`](crate::periodic_task::PeriodicTask) would leak it: the
    /// worker is parked on `join.await` of that forever-looping task, so closing
    /// the channel alone never stops it. Here we synchronously raise the
    /// cancellation watermark and signal the currently-running task to stop, so
    /// the periodic loop exits at its next check and the worker can then drain
    /// and exit when the channel closes (the `tx` field is dropped right after).
    ///
    /// Uses `set_interrupted` (flag only, no callback) on purpose: a `Drop`
    /// implementation must never panic, and `on_interrupt` runs user code that
    /// might. The running task fires `on_interrupt` itself when it observes the
    /// flag, off the dropping thread. An explicit `destroy()` is still preferred
    /// for deterministic, awaited shutdown (see [`Beaver::destroy`]).
    fn drop(&mut self) {
        self.release_flag.store(true, Ordering::Release);
        self.cancel_watermark.store(u64::MAX, Ordering::Release);
        if let Ok(guard) = self.current.lock() {
            if let Some(s) = guard.as_ref() {
                s.set_interrupted(true);
            }
        }
        // `self.tx` (the sender) is dropped with the remaining fields right after
        // this, closing the channel so the worker exits once the task has stopped.
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
            if let Some(l) = listener {
                l.on_complete();
            }
            return;
        }

        if i == intervals.len() - 1 {
            if let Some(l) = listener {
                l.on_error(crate::error::RuntimeError::RetriesExhausted);
            }
        }
    }
}

/// Executes a range-interval task: at most `total_retries` attempts, with range-based sleep
/// before each attempt (except the first). If the interval for an attempt is 0, no sleep.
#[inline]
async fn run_range_interval(task: &RangeIntervalTask) {
    let total = task.total_retries as usize;
    let intervals = &task.intervals[..];
    let work = &task.work;
    let listener = task.listener.as_ref();

    for attempt in 0..total {
        if task.interrupted.load(INTERRUPT_ORDERING) {
            if let Some(l) = listener {
                l.on_interrupt();
            }
            return;
        }

        if attempt > 0 {
            let millis = intervals[attempt - 1];
            if millis > 0 {
                sleep(Duration::from_millis(millis)).await;
            }
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                l.on_complete();
            }
            return;
        }

        if attempt == total - 1 {
            if let Some(l) = listener {
                l.on_error(crate::error::RuntimeError::RetriesExhausted);
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
            if let Some(l) = listener {
                l.on_complete();
            }
            return;
        }

        if current == total {
            if let Some(l) = listener {
                l.on_error(crate::error::RuntimeError::RetriesExhausted);
            }
        }
    }
}

/// Executes a periodic task: loops indefinitely at fixed intervals until interrupted or work returns Done.
#[inline]
async fn run_periodic(task: &PeriodicTask) {
    let interval = task.interval;
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
        Task::RangeInterval(s) => run_range_interval(s).await,
        Task::FixedCount(s) => run_fixed_count(s).await,
        Task::Periodic(s) => run_periodic(s).await,
    }
}

/// Converts a panic payload to a string for reporting.
fn panic_message_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Ok(s) = payload.downcast::<String>() {
        return *s;
    }
    "panic (unknown payload)".to_string()
}

fn notify_error(task: &Task, error: crate::error::RuntimeError) {
    match task {
        Task::TimeInterval(t) => {
            if let Some(l) = &t.listener {
                l.on_error(error);
            }
        }
        Task::RangeInterval(t) => {
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
    cancel_watermark: &AtomicU64,
    splash: Splash,
) {
    let Splash { task, seq } = splash;

    // A cancel raised the watermark above this task's sequence while it was
    // waiting in the queue: drop it. It never runs; it just receives a single
    // `on_interrupt`. This is what lets `cancel_all` / `destroy` reach tasks
    // that are queued *behind* a long-running blocker.
    if seq < cancel_watermark.load(Ordering::Acquire) {
        task.interrupt();
        return;
    }

    // Publish the task as "current" so that a cancel arriving *while it runs*
    // can interrupt it via the shared `current` slot.
    {
        let mut guard = current_worker.lock().expect("mutex poisoned");
        *guard = Some(Arc::clone(&task));
    }

    // Run the task as an isolated tokio task so a panic in `work` is caught here.
    // For a periodic task we self-heal: a panic is reported via `on_error` and the
    // loop then restarts (throttled by the interval) instead of killing the task
    // forever. Bounded tasks keep the prior behavior (panic -> on_error -> stop).
    // A normal finish, an interrupted periodic, or a non-panic join error breaks.
    loop {
        let task_for_join = Arc::clone(&task);
        let join = platform::spawn(async move { run_task(task_for_join.as_ref()).await });
        match join.await {
            Ok(()) => break,
            Err(join_err) => {
                if !join_err.is_panic() {
                    break;
                }
                let msg = panic_message_to_string(join_err.into_panic());
                notify_error(&task, crate::error::RuntimeError::TaskExecutionFailed(msg));
                // Self-heal: only restart a periodic task that was not interrupted.
                let restart_interval = match task.as_ref() {
                    Task::Periodic(p) if !task.interrupted() => Some(p.interval),
                    _ => None,
                };
                match restart_interval {
                    Some(interval) => {
                        if !interval.is_zero() {
                            sleep(interval).await;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    {
        let mut guard = current_worker.lock().expect("mutex poisoned");
        *guard = None;
    }
}
