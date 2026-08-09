use crate::error::{BeaverError, BeaverResult, RuntimeError, ValidationError};
use crate::fixed_count_task::FixedCountTask;
use crate::listener::isolate_callback;
use crate::periodic_task::PeriodicTask;
use crate::platform;
use crate::range_interval_task::RangeIntervalTask;
use crate::scheduler::{Job, LaneConfig, Scheduler, TaskTerminal};
use crate::task::Task;
use crate::time_interval_task::TimeIntervalTask;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

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
    fn validate_capacity(capacity: usize) -> Result<(), ValidationError> {
        let maximum = tokio::sync::Semaphore::MAX_PERMITS;
        if capacity == 0 || capacity > maximum {
            return Err(ValidationError::InvalidCapacity { capacity, maximum });
        }
        Ok(())
    }

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
    pub(crate) fn with_capacity(name: impl Into<String>, buffer: usize) -> Self {
        match Self::try_new(name, buffer) {
            Ok(dam) => dam,
            Err(error) => legacy_construction_failure(error),
        }
    }

    pub(crate) fn try_new(name: impl Into<String>, buffer: usize) -> Result<Self, ValidationError> {
        Self::validate_capacity(buffer)?;
        let handle = Handle::try_current().map_err(|_| ValidationError::RuntimeUnavailable)?;
        Self::try_with_handle(name, buffer, handle)
    }

    /// Creates a dam with a specified tokio runtime handle. Can be called outside tokio runtime.
    pub(crate) fn with_handle(name: impl Into<String>, capacity: usize, handle: Handle) -> Self {
        match Self::try_with_handle(name, capacity, handle) {
            Ok(dam) => dam,
            Err(error) => legacy_construction_failure(error),
        }
    }

    pub(crate) fn try_with_handle(
        _name: impl Into<String>,
        capacity: usize,
        handle: Handle,
    ) -> Result<Self, ValidationError> {
        Self::validate_capacity(capacity)?;
        let maximum = tokio::sync::Semaphore::MAX_PERMITS;
        let lane = LaneConfig::new(capacity, 1)
            .map_err(|_| ValidationError::InvalidCapacity { capacity, maximum })?;
        let current = Arc::new(Mutex::new(None));
        let current_worker = Arc::clone(&current);
        let cancel_watermark = Arc::new(AtomicU64::new(0));
        let watermark_worker = Arc::clone(&cancel_watermark);
        let scheduler = Scheduler::builder()
            .runtime_handle(handle.clone())
            .default_lane(lane)
            .build()
            .map_err(|_| ValidationError::ExecutorConfiguration)?;

        let (tx, mut rx) = mpsc::channel::<Splash>(capacity);
        let join = platform::spawn_on(&handle, async move {
            while let Some(msg) = rx.recv().await {
                run_loop_msg(&current_worker, &watermark_worker, &scheduler, msg).await;
            }
        });

        Ok(Self {
            tx: Mutex::new(Some(tx)),
            current,
            release_flag: AtomicBool::new(false),
            enqueue_seq: AtomicU64::new(0),
            cancel_watermark,
            worker: Mutex::new(Some(join)),
        })
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
        let seq = self
            .enqueue_seq
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| BeaverError::DamReleased)?;
        let guard = self.tx.lock()?;
        match guard.as_ref() {
            Some(tx) => match tx.try_send(Splash { task, seq }) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(BeaverError::QueueFull),
                Err(TrySendError::Closed(_)) => Err(BeaverError::DamReleased),
            },
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
    pub(crate) fn release(&self) -> BeaverResult<()> {
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

fn legacy_construction_failure(error: ValidationError) -> ! {
    crate::diagnostic::error(
        error.code(),
        "legacy-dam",
        format_args!("legacy Beaver construction failed: {error}"),
    );
    std::panic::panic_any(format!("legacy Beaver construction failed: {error}"));
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

/// Executes a time-interval task: waits according to intervals (milliseconds), then executes work,
/// until it returns Done or reaches the last attempt.
#[inline]
async fn run_time_interval(task: &TimeIntervalTask) {
    let intervals = &task.intervals[..];
    let work = &task.work;
    let listener = task.listener.as_ref();

    for (i, &millis) in intervals.iter().enumerate() {
        if task.control.is_cancelled() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
        }

        if millis > 0 && task.control.wait(Duration::from_millis(millis)).await {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_complete());
            }
            return;
        }

        if i == intervals.len() - 1 {
            if let Some(l) = listener {
                isolate_callback(|| {
                    l.on_error(crate::error::RuntimeError::RetriesExhausted);
                });
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
        if task.control.is_cancelled() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
        }

        if attempt > 0 {
            let millis = intervals[attempt - 1];
            if millis > 0 && task.control.wait(Duration::from_millis(millis)).await {
                if let Some(l) = listener {
                    isolate_callback(|| l.on_interrupt());
                }
                return;
            }
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_complete());
            }
            return;
        }

        if attempt == total - 1 {
            if let Some(l) = listener {
                isolate_callback(|| {
                    l.on_error(crate::error::RuntimeError::RetriesExhausted);
                });
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
        if task.control.is_cancelled() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
        }

        if let Some(p) = progress {
            isolate_callback(|| p.on_progress(current, total, tag));
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_complete());
            }
            return;
        }

        if current == total {
            if let Some(l) = listener {
                isolate_callback(|| {
                    l.on_error(crate::error::RuntimeError::RetriesExhausted);
                });
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

    if task.initial_delay && !interval.is_zero() && task.control.wait(interval).await {
        if let Some(l) = listener {
            isolate_callback(|| l.on_interrupt());
        }
        return;
    }

    loop {
        if task.control.is_cancelled() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
        }

        let result = work.execute().await;
        if !result.need_retry() {
            if let Some(l) = listener {
                isolate_callback(|| l.on_complete());
            }
            return;
        }

        if !interval.is_zero() && task.control.wait(interval).await {
            if let Some(l) = listener {
                isolate_callback(|| l.on_interrupt());
            }
            return;
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

fn notify_error(task: &Task, error: crate::error::RuntimeError) {
    match task {
        Task::TimeInterval(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_error(error));
            }
        }
        Task::RangeInterval(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_error(error));
            }
        }
        Task::FixedCount(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_error(error));
            }
        }
        Task::Periodic(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_error(error));
            }
        }
    }
}

async fn run_loop_msg(
    current_worker: &Mutex<Option<Arc<Task>>>,
    cancel_watermark: &AtomicU64,
    scheduler: &Scheduler,
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
    if !set_current_task(
        current_worker,
        Some(Arc::clone(&task)),
        task.as_ref(),
        "publish",
    ) {
        return;
    }

    // Close the check/publish race: cancellation can advance the watermark
    // after the first check but before this task becomes visible as current.
    if seq < cancel_watermark.load(Ordering::Acquire) {
        task.set_interrupted(true);
        notify_interrupt(&task);
        let _ = set_current_task(current_worker, None, task.as_ref(), "cancel-cleanup");
        return;
    }

    // Adapt the complete legacy task runner to the typed Scheduler core. The
    // legacy queue/cancellation/listener semantics stay in this module, while
    // task ownership, panic isolation and terminal convergence use one core.
    // For a periodic task we self-heal: a panic is reported via `on_error` and the
    // loop then restarts (throttled by the interval) instead of killing the task
    // forever. Bounded tasks keep the prior behavior (panic -> on_error -> stop).
    // A normal finish, an interrupted periodic, or executor stop breaks.
    loop {
        let task_for_join = Arc::clone(&task);
        let submitted = scheduler
            .submit(Job::once(move |_| async move {
                run_task(task_for_join.as_ref()).await;
                Ok::<(), ()>(())
            }))
            .await;
        let terminal = match submitted {
            Ok(handle) => handle.join().await,
            Err(_) => break,
        };
        match terminal {
            TaskTerminal::Completed(()) => break,
            TaskTerminal::Panicked(info) => {
                let msg = info
                    .message()
                    .unwrap_or("panic (unknown payload)")
                    .to_owned();
                notify_error(&task, crate::error::RuntimeError::TaskExecutionFailed(msg));
                // Self-heal: only restart a periodic task that was not interrupted.
                let restart_interval = match task.as_ref() {
                    Task::Periodic(p) if !task.interrupted() => Some(p.interval),
                    _ => None,
                };
                match restart_interval {
                    Some(interval) => {
                        if !interval.is_zero() && task.wait_or_cancel(interval).await {
                            notify_interrupt(&task);
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ => break,
        }
    }

    let _ = set_current_task(current_worker, None, task.as_ref(), "complete-cleanup");
}

fn set_current_task(
    current_worker: &Mutex<Option<Arc<Task>>>,
    current: Option<Arc<Task>>,
    task: &Task,
    phase: &'static str,
) -> bool {
    match current_worker.lock() {
        Ok(mut guard) => {
            *guard = current;
            true
        }
        Err(_) => {
            let error = RuntimeError::LockPoisoned;
            crate::diagnostic::error(
                error.code(),
                "legacy-dam-worker",
                format_args!("current task lock poisoned phase={phase}"),
            );
            notify_error(task, error);
            false
        }
    }
}

fn notify_interrupt(task: &Task) {
    match task {
        Task::TimeInterval(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_interrupt());
            }
        }
        Task::RangeInterval(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_interrupt());
            }
        }
        Task::FixedCount(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_interrupt());
            }
        }
        Task::Periodic(t) => {
            if let Some(l) = &t.listener {
                isolate_callback(|| l.on_interrupt());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_loop_msg, Dam, Splash};
    use crate::{
        listener, listener_with_error, work, BeaverResult, PeriodicBuilder, RangeIntervalBuilder,
        RuntimeError, Scheduler, TimeIntervalBuilder, WorkResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn legacy_constructor_logs_missing_runtime_before_preserving_panic_contract() {
        crate::test_log::init();
        let result = std::panic::catch_unwind(|| Dam::new("missing-runtime", 1));
        assert!(result.is_err());
        assert!(crate::test_log::contains("BB-VAL-009"));
    }

    #[test]
    fn legacy_constructor_logs_invalid_capacity_before_preserving_panic_contract() {
        crate::test_log::init();
        let result = std::panic::catch_unwind(|| Dam::new("invalid-capacity", 0));
        assert!(result.is_err());
        assert!(crate::test_log::contains("BB-VAL-008"));
    }

    #[tokio::test]
    async fn poisoned_worker_lock_reports_typed_error_and_does_not_unwind() -> BeaverResult<()> {
        crate::test_log::init();
        let current = std::sync::Mutex::new(None);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let guard = match current.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let _guard = guard;
            std::panic::panic_any("poison current worker lock");
        }));
        assert!(poisoned.is_err());

        let errors = Arc::new(AtomicUsize::new(0));
        let errors_for_listener = Arc::clone(&errors);
        let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .listener(listener_with_error(
                || {},
                || {},
                move |error| {
                    if matches!(error, RuntimeError::LockPoisoned) {
                        errors_for_listener.fetch_add(1, Ordering::SeqCst);
                    }
                },
            ))
            .build()?;
        let scheduler = Scheduler::builder().build().unwrap();

        run_loop_msg(
            &current,
            &std::sync::atomic::AtomicU64::new(0),
            &scheduler,
            Splash { task, seq: 0 },
        )
        .await;

        assert_eq!(errors.load(Ordering::SeqCst), 1);
        assert!(crate::test_log::contains("BB-RUN-001"));
        assert!(scheduler.shutdown().await.is_complete());
        Ok(())
    }

    async fn wait_until_current(dam: &Dam) {
        for _ in 0..32 {
            if dam.current.lock().expect("current lock").is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("worker did not publish its current task");
    }

    async fn assert_cancel_wakes_wait(dam: &Dam, interrupted: &AtomicUsize) -> BeaverResult<()> {
        wait_until_current(dam).await;
        dam.cancel_all().await?;
        for _ in 0..32 {
            if interrupted.load(Ordering::SeqCst) == 1 {
                dam.release()?;
                dam.take_worker().expect("worker handle").await.unwrap();
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
        panic!("cancellation did not wake the scheduler-managed wait");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_wakes_time_interval_first_delay() -> BeaverResult<()> {
        let dam = Dam::new("cancel-time", 1);
        let interrupted = Arc::new(AtomicUsize::new(0));
        let interrupted_for_listener = Arc::clone(&interrupted);
        let task = TimeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }))
            .intervals_millis([60_000])
            .listener(listener(
                || {},
                move || {
                    interrupted_for_listener.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .build()?;

        dam.enqueue(task).await?;
        assert_cancel_wakes_wait(&dam, &interrupted).await
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_wakes_range_retry_backoff() -> BeaverResult<()> {
        let dam = Dam::new("cancel-range", 1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_work = Arc::clone(&attempts);
        let interrupted = Arc::new(AtomicUsize::new(0));
        let interrupted_for_listener = Arc::clone(&interrupted);
        let task = RangeIntervalBuilder::new(
            work(move || {
                let attempts = Arc::clone(&attempts_for_work);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    WorkResult::NeedRetry
                }
            }),
            2,
        )
        .add_range(0, 0, Duration::from_secs(60))
        .listener(listener(
            || {},
            move || {
                interrupted_for_listener.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()?;

        dam.enqueue(task).await?;
        while attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert_cancel_wakes_wait(&dam, &interrupted).await
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_wakes_periodic_interval() -> BeaverResult<()> {
        let dam = Dam::new("cancel-periodic", 1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_work = Arc::clone(&attempts);
        let interrupted = Arc::new(AtomicUsize::new(0));
        let interrupted_for_listener = Arc::clone(&interrupted);
        let task = PeriodicBuilder::new(work(move || {
            let attempts = Arc::clone(&attempts_for_work);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .interval(Duration::from_secs(60))
        .listener(listener(
            || {},
            move || {
                interrupted_for_listener.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()?;

        dam.enqueue(task).await?;
        while attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert_cancel_wakes_wait(&dam, &interrupted).await
    }

    #[tokio::test]
    async fn closed_receiver_is_not_reported_as_queue_full() {
        let dam = Dam::new("closed-receiver", 1);
        let worker = dam.take_worker().expect("worker handle");
        worker.abort();
        let _ = worker.await;
        let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .build()
            .expect("task");

        let error = dam.enqueue(task).await.expect_err("receiver is closed");
        assert!(matches!(error, crate::BeaverError::DamReleased));
    }

    #[tokio::test]
    async fn exhausted_legacy_sequence_rejects_without_wrapping() {
        let dam = Dam::new("sequence-exhausted", 1);
        dam.enqueue_seq.store(u64::MAX, Ordering::Release);
        let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .build()
            .expect("task");

        let error = dam.enqueue(task).await.expect_err("sequence is exhausted");
        assert!(matches!(error, crate::BeaverError::DamReleased));
        dam.release().expect("release");
        dam.take_worker().expect("worker handle").await.unwrap();
    }
}
