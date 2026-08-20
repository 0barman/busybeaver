use crate::error::{BeaverError, BeaverResult};
use crate::fixed_count_task::FixedCountTask;
use crate::periodic_task::PeriodicTask;
use crate::platform;
use crate::range_interval_task::RangeIntervalTask;
use crate::task::Task;
use crate::time_interval_task::TimeIntervalTask;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// A task handed to the worker, tagged with the sequence number it was assigned
/// at enqueue time. The sequence lets the worker decide, when it finally pulls
/// the task off the queue, whether the task was cancelled while it waited.
struct Splash {
    execution: Arc<LegacyExecution>,
    seq: u64,
}

/// Per-enqueue runtime state for a legacy task. The public `Arc<Task>` remains
/// the immutable compatibility definition; cancellation belongs to an
/// execution so submitting the same task twice cannot couple the two runs.
struct LegacyExecution {
    task: Arc<Task>,
    callback_failures: Arc<Mutex<Vec<String>>>,
    cancelled: AtomicBool,
    cancellation: watch::Sender<bool>,
    interrupt_notified: AtomicBool,
}

impl LegacyExecution {
    fn new(task: Arc<Task>, callback_failures: Arc<Mutex<Vec<String>>>) -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            task,
            callback_failures,
            cancelled: AtomicBool::new(false),
            cancellation,
            interrupt_notified: AtomicBool::new(false),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            // Kept only for the deprecated `Task::interrupted()` compatibility
            // accessor. Execution code never reads this shared task flag.
            self.task.set_interrupted(true);
            self.cancellation.send_replace(true);
        }
    }

    async fn cancelled(&self) {
        let mut receiver = self.cancellation.subscribe();
        if *receiver.borrow() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }

    async fn sleep(&self, duration: Duration) -> bool {
        if self.is_cancelled() {
            return false;
        }
        if duration.is_zero() {
            tokio::task::yield_now().await;
            return !self.is_cancelled();
        }
        tokio::select! {
            biased;
            _ = self.cancelled() => false,
            _ = sleep(duration) => !self.is_cancelled(),
        }
    }

    fn notify_interrupt(&self) {
        self.cancel();
        if !self.interrupt_notified.swap(true, Ordering::AcqRel) {
            notify_interrupt(self);
        }
    }

    fn record_callback_failure(
        &self,
        source: &'static str,
        payload: Box<dyn std::any::Any + Send>,
    ) {
        let message = format!("{source}: {}", panic_message_to_string(payload));
        self.callback_failures
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .push(message);
    }
}

pub(crate) struct Dam {
    tx: Mutex<Option<mpsc::Sender<Splash>>>,
    current: Arc<Mutex<Option<Arc<LegacyExecution>>>>,
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
    callback_failures: Arc<Mutex<Vec<String>>>,
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
        let callback_failures = Arc::new(Mutex::new(Vec::new()));
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
            callback_failures,
        }
    }

    /// Creates a dam with a specified tokio runtime handle. Can be called outside tokio runtime.
    pub(crate) fn with_handle(_name: impl Into<String>, capacity: usize, handle: Handle) -> Self {
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);
        let cancel_watermark = Arc::new(AtomicU64::new(0));
        let watermark_worker = Arc::clone(&cancel_watermark);

        let (tx, mut rx) = mpsc::channel::<Splash>(capacity);
        let callback_failures = Arc::new(Mutex::new(Vec::new()));
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
            callback_failures,
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
        let execution = Arc::new(LegacyExecution::new(
            task,
            Arc::clone(&self.callback_failures),
        ));
        let guard = self.tx.lock()?;
        match guard.as_ref() {
            Some(tx) => tx
                .try_send(Splash { execution, seq })
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
            s.cancel();
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
            s.cancel();
        }
        // Dropping the sender lets the worker drain the remaining buffered tasks
        // (all now cancelled by the watermark) and then exit when the channel
        // closes.
        let _ = self.tx.lock()?.take();
        Ok(())
    }

    /// Takes the background worker's join handle so the caller can await its
    /// termination. Returns `None` if it was already taken. A poisoned lock is
    /// recovered with an internal diagnostic so shutdown can still join it.
    pub(crate) fn take_worker(&self) -> Option<JoinHandle<()>> {
        self.worker
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .take()
    }

    pub(crate) fn callback_failures(&self) -> Vec<String> {
        self.callback_failures
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .clone()
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
        let guard = self
            .current
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if let Some(s) = guard.as_ref() {
            s.cancel();
        }
        // `self.tx` (the sender) is dropped with the remaining fields right after
        // this, closing the channel so the worker exits once the task has stopped.
    }
}

fn isolate_callback(execution: &LegacyExecution, source: &'static str, callback: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(callback)) {
        execution.record_callback_failure(source, payload);
    }
}

fn notify_complete(execution: &LegacyExecution) {
    let listener = match execution.task.as_ref() {
        Task::TimeInterval(task) => task.listener.as_ref(),
        Task::RangeInterval(task) => task.listener.as_ref(),
        Task::FixedCount(task) => task.listener.as_ref(),
        Task::Periodic(task) => task.listener.as_ref(),
    };
    if let Some(listener) = listener {
        isolate_callback(execution, "on_complete", || listener.on_complete());
    }
}

fn notify_interrupt(execution: &LegacyExecution) {
    let listener = match execution.task.as_ref() {
        Task::TimeInterval(task) => task.listener.as_ref(),
        Task::RangeInterval(task) => task.listener.as_ref(),
        Task::FixedCount(task) => task.listener.as_ref(),
        Task::Periodic(task) => task.listener.as_ref(),
    };
    if let Some(listener) = listener {
        isolate_callback(execution, "on_interrupt", || listener.on_interrupt());
    }
}

/// Executes a time-interval task: waits according to intervals (milliseconds), then executes work,
/// until it returns Done or reaches the last attempt.
#[inline]
async fn run_time_interval(execution: &LegacyExecution, task: &TimeIntervalTask) {
    let intervals = &task.intervals[..];
    let work = &task.work;

    for (i, &millis) in intervals.iter().enumerate() {
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }

        if !execution.sleep(Duration::from_millis(millis)).await {
            execution.notify_interrupt();
            return;
        }

        let result = work.execute().await;
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }
        if !result.need_retry() {
            notify_complete(execution);
            return;
        }

        if i.checked_add(1) == Some(intervals.len()) {
            notify_error(execution, crate::error::RuntimeError::RetriesExhausted);
        }
    }
}

/// Executes a range-interval task: at most `total_retries` attempts, with range-based sleep
/// before each attempt (except the first). If the interval for an attempt is 0, no sleep.
#[inline]
async fn run_range_interval(execution: &LegacyExecution, task: &RangeIntervalTask) {
    let total = task.total_retries as usize;
    let intervals = &task.intervals[..];
    let work = &task.work;

    for attempt in 0..total {
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }

        if let Some(interval_index) = attempt.checked_sub(1) {
            let Some(&millis) = intervals.get(interval_index) else {
                crate::internal::log_internal_error(
                    "BB-RANGE-INTERVAL-MISSING",
                    "validated range interval task is missing an attempt delay",
                );
                notify_error(
                    execution,
                    crate::error::RuntimeError::InternalInvariantViolation {
                        code: "BB-RANGE-INTERVAL-MISSING",
                    },
                );
                return;
            };
            if !execution.sleep(Duration::from_millis(millis)).await {
                execution.notify_interrupt();
                return;
            }
        }

        let result = work.execute().await;
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }
        if !result.need_retry() {
            notify_complete(execution);
            return;
        }

        if attempt.checked_add(1) == Some(total) {
            notify_error(execution, crate::error::RuntimeError::RetriesExhausted);
        }
    }
}

/// Executes a fixed-count task: runs at most `count` times, calling progress before each attempt.
#[inline]
async fn run_fixed_count(execution: &LegacyExecution, task: &FixedCountTask) {
    let total = task.count;
    let work = &task.work;
    let progress = task.progress.as_ref();
    let tag = task.tag.as_deref().map_or("", |v| v);

    for current in 1..=total {
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }

        if let Some(p) = progress {
            isolate_callback(execution, "progress", || p.on_progress(current, total, tag));
        }
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }

        let result = work.execute().await;
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }
        if !result.need_retry() {
            notify_complete(execution);
            return;
        }

        if current == total {
            notify_error(execution, crate::error::RuntimeError::RetriesExhausted);
        }
    }
}

/// Executes a periodic task: loops indefinitely at fixed intervals until interrupted or work returns Done.
#[inline]
async fn run_periodic(execution: &LegacyExecution, task: &PeriodicTask) {
    let interval = task.interval;
    let work = &task.work;

    if task.initial_delay && !execution.sleep(interval).await {
        execution.notify_interrupt();
        return;
    }

    loop {
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }

        let result = work.execute().await;
        if execution.is_cancelled() {
            execution.notify_interrupt();
            return;
        }
        if !result.need_retry() {
            notify_complete(execution);
            return;
        }

        if !execution.sleep(interval).await {
            execution.notify_interrupt();
            return;
        }
    }
}

/// Dispatches execution based on task type.
#[inline]
async fn run_task(execution: &LegacyExecution) {
    match execution.task.as_ref() {
        Task::TimeInterval(task) => run_time_interval(execution, task).await,
        Task::RangeInterval(task) => run_range_interval(execution, task).await,
        Task::FixedCount(task) => run_fixed_count(execution, task).await,
        Task::Periodic(task) => run_periodic(execution, task).await,
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

fn notify_error(execution: &LegacyExecution, error: crate::error::RuntimeError) {
    let listener = match execution.task.as_ref() {
        Task::TimeInterval(task) => task.listener.as_ref(),
        Task::RangeInterval(task) => task.listener.as_ref(),
        Task::FixedCount(task) => task.listener.as_ref(),
        Task::Periodic(task) => task.listener.as_ref(),
    };
    if let Some(listener) = listener {
        isolate_callback(execution, "on_error", || listener.on_error(error));
    }
}

async fn run_loop_msg(
    current_worker: &Mutex<Option<Arc<LegacyExecution>>>,
    cancel_watermark: &AtomicU64,
    splash: Splash,
) {
    let Splash { execution, seq } = splash;

    // A cancel raised the watermark above this task's sequence while it was
    // waiting in the queue: drop it. It never runs; it just receives a single
    // `on_interrupt`. This is what lets `cancel_all` / `destroy` reach tasks
    // that are queued *behind* a long-running blocker.
    if seq < cancel_watermark.load(Ordering::Acquire) {
        execution.notify_interrupt();
        return;
    }

    // Publish the task as "current" so that a cancel arriving *while it runs*
    // can interrupt it via the shared `current` slot.
    {
        let mut guard = lock_current_worker(current_worker);
        *guard = Some(Arc::clone(&execution));
    }

    // Close the watermark/current hand-off window: cancellation may have
    // linearized after the first watermark read but before publication.
    if seq < cancel_watermark.load(Ordering::Acquire) {
        execution.notify_interrupt();
        let mut guard = lock_current_worker(current_worker);
        *guard = None;
        return;
    }

    // Run the task as an isolated tokio task so a panic in `work` is caught here.
    // For a periodic task we self-heal: a panic is reported via `on_error` and the
    // loop then restarts (throttled by the interval) instead of killing the task
    // forever. Bounded tasks keep the prior behavior (panic -> on_error -> stop).
    // A normal finish, an interrupted periodic, or a non-panic join error breaks.
    loop {
        let execution_for_join = Arc::clone(&execution);
        let join = platform::spawn(async move { run_task(&execution_for_join).await });
        match join.await {
            Ok(()) => break,
            Err(join_err) => {
                if !join_err.is_panic() {
                    break;
                }
                let Some(payload) = crate::internal::take_join_panic(
                    join_err,
                    "BB-LEGACY-JOIN-PANIC-MISCLASSIFIED",
                ) else {
                    break;
                };
                let msg = panic_message_to_string(payload);
                notify_error(
                    &execution,
                    crate::error::RuntimeError::TaskExecutionFailed(msg),
                );
                // Self-heal: only restart a periodic task that was not interrupted.
                let restart_interval = match execution.task.as_ref() {
                    Task::Periodic(p) if !execution.is_cancelled() => Some(p.interval),
                    _ => None,
                };
                match restart_interval {
                    Some(interval) => {
                        if !execution.sleep(interval).await {
                            execution.notify_interrupt();
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    {
        let mut guard = lock_current_worker(current_worker);
        *guard = None;
    }
}

fn lock_current_worker(
    current_worker: &Mutex<Option<Arc<LegacyExecution>>>,
) -> MutexGuard<'_, Option<Arc<LegacyExecution>>> {
    match current_worker.lock() {
        Ok(guard) => guard,
        Err(poisoned) => crate::internal::recover_poison(poisoned),
    }
}
