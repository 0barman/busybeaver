use crate::dam::Dam;
use crate::error::{BeaverError, BeaverResult, ValidationError};
use crate::task::Task;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Upper bound on how long [`Beaver::destroy`] waits for background workers to
/// terminate before giving up. Workers normally exit promptly once `release`
/// signals them; this cap only guards against a pathologically stuck task (e.g.
/// a `work.execute()` that never returns and never yields) so `destroy` cannot
/// hang forever.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// BusyBeaver: Because sometimes your tasks need to run like a Busy Beaver — tirelessly attempting until they produce the maximum possible success (or hit their busy beaver bound).
///
/// BusyBeaver is a task-scheduling library that supports execution strategies based on counts, cycles, and custom time intervals. It streamlines periodic tasks in your codebase—such as heartbeats, metric reporting, scheduled polling, and automated cleanup—making them simpler and more reliable. At its core, BusyBeaver is an asynchronous task executor with configurable retry strategies, purpose-built for Rust async runtimes like Tokio. Whether a task needs to stop after exactly `N` executions, repeat every `X` milliseconds, or run at specific intervals within a defined time window, BusyBeaver handles the complexity elegantly. Equipped with built-in mechanisms like exponential backoff, retry limits, task listeners, and progress callbacks, it eliminates the need to manually write tedious tokio::time + loop + retry boilerplate in your asynchronous code.
///
/// # Lifecycle
///
/// Prefer calling [`Beaver::destroy`] before letting a `Beaver` go out of scope —
/// it is the deterministic way to stop all tasks and release resources. As a
/// safety net, dropping a `Beaver` without calling `destroy` now also signals its
/// tasks to stop (so a [`PeriodicBuilder`](crate::PeriodicBuilder) task that has
/// not returned [`WorkResult::Done`](crate::WorkResult::Done) does not keep
/// running on the underlying runtime), but this best-effort cleanup is not
/// awaited.
///
/// # Creation Methods
///
/// 1. **Within a tokio runtime**:
/// ```no_run
/// use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
///
/// #[tokio::main]
/// async fn main() {
///     let beaver = Beaver::new("default", 256);
///     let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
///         .build()
///         .unwrap();
///     beaver.enqueue(task).await.unwrap();
///     beaver.destroy().await.unwrap();
/// }
/// ```
///
/// 2. **With an external runtime handle** (can be called outside tokio runtime):
/// ```no_run
/// use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
///
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
/// rt.block_on(async {
///     let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
///         .build()
///         .unwrap();
///     beaver.enqueue(task).await.unwrap();
///     beaver.destroy().await.unwrap();
/// });
/// ```
///
/// # Async API
///
/// [`enqueue`](Self::enqueue), [`enqueue_on_new_thread`](Self::enqueue_on_new_thread),
/// [`cancel_all`](Self::cancel_all), [`cancel_non_long_resident`](Self::cancel_non_long_resident),
/// [`release_thread_resource_by_name`](Self::release_thread_resource_by_name), and
/// [`destroy`](Self::destroy) are **async** and must be awaited on a Tokio runtime.
pub struct Beaver {
    default: Mutex<Option<Arc<Dam>>>,
    named: Mutex<HashMap<String, NamedEntry>>,
    handle: Option<Handle>,
    shutdown: Mutex<Option<LegacyShutdown>>,
}

struct NamedEntry {
    dam: Arc<Dam>,
    long_resident: bool,
}

/// Progress observed while a legacy [`Beaver`] shuts down its worker lanes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    total_workers: usize,
    stopped_workers: usize,
    timed_out: bool,
}

impl ShutdownReport {
    fn progress(total_workers: usize, stopped_workers: usize) -> Self {
        Self {
            total_workers,
            stopped_workers,
            timed_out: false,
        }
    }

    /// Returns the number of worker lanes captured when shutdown started.
    pub fn total_workers(self) -> usize {
        self.total_workers
    }

    /// Returns how many captured worker lanes have confirmed termination.
    pub fn stopped_workers(self) -> usize {
        self.stopped_workers
    }

    /// Returns whether the caller's observation deadline elapsed.
    pub fn timed_out(self) -> bool {
        self.timed_out
    }

    /// Returns whether every captured worker lane has stopped.
    pub fn is_complete(self) -> bool {
        self.stopped_workers == self.total_workers
    }
}

struct LegacyShutdown {
    progress: watch::Receiver<ShutdownReport>,
    started_at: Instant,
    _coordinator: Option<JoinHandle<()>>,
}

impl Beaver {
    fn validate_capacity(capacity: usize) -> Result<(), ValidationError> {
        let maximum = tokio::sync::Semaphore::MAX_PERMITS;
        if capacity == 0 || capacity > maximum {
            return Err(ValidationError::InvalidCapacity { capacity, maximum });
        }
        Ok(())
    }

    /// Strict constructor that reports invalid capacity and missing runtime
    /// without panicking. All lanes created by this instance use the captured
    /// runtime handle.
    pub fn try_new(name: impl Into<String>, buffer: usize) -> Result<Self, ValidationError> {
        Self::validate_capacity(buffer)?;
        let handle = Handle::try_current().map_err(|_| ValidationError::RuntimeUnavailable)?;
        let default = Dam::try_with_handle(name, buffer, handle.clone())?;
        Ok(Self {
            default: Mutex::new(Some(Arc::new(default))),
            named: Mutex::new(HashMap::new()),
            handle: Some(handle),
            shutdown: Mutex::new(None),
        })
    }

    /// Strict constructor using an explicit runtime handle.
    pub fn try_new_with_handle(
        name: impl Into<String>,
        buffer: usize,
        handle: Handle,
    ) -> Result<Self, ValidationError> {
        Self::validate_capacity(buffer)?;
        let default = Dam::try_with_handle(name, buffer, handle.clone())?;
        Ok(Self {
            default: Mutex::new(Some(Arc::new(default))),
            named: Mutex::new(HashMap::new()),
            handle: Some(handle),
            shutdown: Mutex::new(None),
        })
    }

    /// Creates a new Beaver instance.
    ///
    /// **Note**: Must be called within a tokio runtime context.
    ///
    /// * `name` - The name of the default execution lane, backed by a Tokio channel. Calling `enqueue(&self, task: Arc<Task>)` sends tasks to this lane.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///   Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///   The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime context.
    ///
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`tokio::sync::Semaphore::MAX_PERMITS`].
    pub fn new(name: impl Into<String>, buffer: usize) -> Self {
        let default = Mutex::new(Some(Arc::new(Dam::new(name, buffer))));
        Self {
            default,
            named: Mutex::new(HashMap::new()),
            handle: None,
            shutdown: Mutex::new(None),
        }
    }

    /// Creates a Beaver instance with a specified tokio runtime handle.
    ///
    /// Can be called outside a tokio runtime context.
    /// * `name` - The name of the default execution lane, backed by a Tokio channel. Calling `enqueue(&self, task: Arc<Task>)` sends tasks to this lane.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///   Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///   The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`tokio::sync::Semaphore::MAX_PERMITS`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
    ///
    /// let rt = tokio::runtime::Runtime::new().unwrap();
    /// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
    /// rt.block_on(async {
    ///     let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
    ///         .build()
    ///         .unwrap();
    ///     beaver.enqueue(task).await.unwrap();
    ///     beaver.destroy().await.unwrap();
    /// });
    /// ```
    pub fn new_with_handle(name: impl Into<String>, buffer: usize, handle: Handle) -> Self {
        let default = Mutex::new(Some(Arc::new(Dam::with_handle(
            name,
            buffer,
            handle.clone(),
        ))));

        Self {
            default,
            named: Mutex::new(HashMap::new()),
            handle: Some(handle),
            shutdown: Mutex::new(None),
        }
    }

    /// Enqueues a task on the default execution lane.
    #[inline]
    pub async fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        match default {
            Some(d) => d.enqueue(task).await,
            None => Err(BeaverError::NoDam),
        }
    }

    /// Enqueues a task to a named execution lane; creates it if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `task` - The task to enqueue for execution.
    /// * `name` - The name of the execution lane where the task will run.
    ///   If a lane with this name doesn't exist, a new one will be created.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///   Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///   The provided buffer capacity must be at least 1.
    /// * `long_resident` - Whether the task should be "long-resident":
    ///   - `true`: Background task (e.g., heartbeat, periodic sync) that should
    ///     not be cancelled during normal cleanup. The task still follows its
    ///     retry schedule.
    ///   - `false`: Temporary task (e.g., request-related retry) that can be
    ///     cleaned up when the request ends.
    pub async fn enqueue_on_new_thread(
        &self,
        task: Arc<Task>,
        name: impl Into<String>,
        buffer: usize,
        long_resident: bool,
    ) -> BeaverResult<()> {
        let name = name.into();
        let handle = self.handle.clone();
        let dam = {
            let mut named = self.named.lock()?;
            match named.entry(name.clone()) {
                Entry::Occupied(mut occupied) => {
                    occupied.get_mut().long_resident = long_resident;
                    Arc::clone(&occupied.get().dam)
                }
                Entry::Vacant(vacant) => {
                    let dam = match handle {
                        Some(runtime) => Dam::try_with_handle(&name, buffer, runtime),
                        None => Dam::try_new(&name, buffer),
                    }?;
                    let entry = vacant.insert(NamedEntry {
                        dam: Arc::new(dam),
                        long_resident,
                    });
                    Arc::clone(&entry.dam)
                }
            }
        };
        dam.enqueue(task).await
    }

    /// Cancels all pending and running tasks on all execution lanes.
    ///
    /// This includes tasks enqueued via [`enqueue`](Self::enqueue) on the default lane
    /// and all named lanes.
    pub async fn cancel_all(&self) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        if let Some(d) = default {
            let _ = d.cancel_all().await;
        }
        let named_dams: Vec<Arc<Dam>> = {
            let mut named = self.named.lock()?;
            let dams = named.values().map(|e| Arc::clone(&e.dam)).collect();
            named.clear();
            dams
        };
        for dam in named_dams {
            let _ = dam.cancel_all().await;
        }
        Ok(())
    }

    /// Cancels all non-long-resident tasks.
    ///
    /// Long-resident tasks (e.g., heartbeat, periodic sync) are preserved.
    pub async fn cancel_non_long_resident(&self) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        if let Some(d) = default {
            let _ = d.cancel_all().await;
        }
        let to_cancel: Vec<Arc<Dam>> = {
            let mut named = self.named.lock()?;
            let keys: Vec<String> = named
                .iter()
                .filter(|(_, e)| !e.long_resident)
                .map(|(k, _)| k.clone())
                .collect();
            let mut dams = Vec::with_capacity(keys.len());
            for k in keys {
                if let Some(e) = named.remove(&k) {
                    dams.push(e.dam);
                }
            }
            dams
        };
        for dam in to_cancel {
            let _ = dam.cancel_all().await;
        }
        Ok(())
    }

    /// Releases a named execution lane and all its resources by name.
    ///
    /// The lane must have been created via [`enqueue_on_new_thread`](Self::enqueue_on_new_thread).
    pub async fn release_thread_resource_by_name(
        &self,
        name: impl Into<String>,
    ) -> BeaverResult<()> {
        let removed = { self.named.lock()?.remove(&name.into()) };
        if let Some(e) = removed {
            let _ = e.dam.release();
        }
        Ok(())
    }

    /// Destroys the Beaver instance and all its resources.
    ///
    /// This includes the default execution lane and all named lanes, and cancels
    /// **all** tasks (both normal and long-resident). Call this before letting a
    /// Beaver go out of scope so that no background threads or resources keep running
    /// after the Beaver is dropped.
    ///
    /// **Graceful shutdown**: after signalling every lane to stop, `destroy`
    /// awaits the termination of each background worker, so when it returns the
    /// workers have actually exited (not merely been signalled). The wait is
    /// bounded by an internal timeout ([`SHUTDOWN_TIMEOUT`]); if a task is stuck
    /// in a `work.execute()` that never returns, `destroy` gives up waiting after
    /// the timeout rather than hanging forever. `destroy` is idempotent: repeated
    /// and concurrent calls observe the same shared shutdown coordinator.
    fn start_shutdown(&self) -> (watch::Receiver<ShutdownReport>, Instant) {
        let mut shutdown = self
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = shutdown.as_ref() {
            return (existing.progress.clone(), existing.started_at);
        }

        // Collect every dam exactly once. Holding the shutdown lock makes the
        // coordinator publication atomic with respect to concurrent callers.
        let mut dams: Vec<Arc<Dam>> = Vec::new();
        if let Some(d) = self
            .default
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            dams.push(d);
        }
        {
            let mut named = self
                .named
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (_, v) in named.drain() {
                dams.push(v.dam);
            }
        }

        // Signal release before starting the waiter so shutdown does not depend
        // on the lifetime of the public future that initiated it.
        for dam in &dams {
            let _ = dam.release();
        }

        let workers: Vec<_> = dams.iter().filter_map(|d| d.take_worker()).collect();
        let total_workers = workers.len();
        let (progress_tx, progress_rx) = watch::channel(ShutdownReport::progress(total_workers, 0));
        let started_at = Instant::now();
        let runtime = self.handle.clone().or_else(|| Handle::try_current().ok());
        let coordinator = runtime.map(|runtime| {
            runtime.spawn(async move {
                let mut stopped_workers = 0;
                for worker in workers {
                    let _ = worker.await;
                    stopped_workers += 1;
                    progress_tx
                        .send_replace(ShutdownReport::progress(total_workers, stopped_workers));
                }
            })
        });

        *shutdown = Some(LegacyShutdown {
            progress: progress_rx.clone(),
            started_at,
            _coordinator: coordinator,
        });
        (progress_rx, started_at)
    }

    async fn wait_for_shutdown(&self, timeout: Duration) -> ShutdownReport {
        let (mut progress, started_at) = self.start_shutdown();
        let current = *progress.borrow_and_update();
        if current.is_complete() {
            return current;
        }

        let wait = async {
            loop {
                if progress.changed().await.is_err() {
                    let mut report = *progress.borrow();
                    report.timed_out = !report.is_complete();
                    return report;
                }
                let report = *progress.borrow_and_update();
                if report.is_complete() {
                    return report;
                }
            }
        };

        let Some(deadline) = started_at.checked_add(timeout) else {
            return wait.await;
        };
        match tokio::time::timeout_at(deadline, wait).await {
            Ok(report) => report,
            Err(_) => {
                let mut report = *progress.borrow();
                report.timed_out = !report.is_complete();
                report
            }
        }
    }

    /// Starts shutdown once and waits up to the legacy five-second deadline,
    /// returning a typed progress report without abandoning unfinished workers.
    pub async fn destroy_with_report(&self) -> ShutdownReport {
        self.wait_for_shutdown(SHUTDOWN_TIMEOUT).await
    }

    /// Starts shutdown once and observes it until the supplied total deadline.
    /// A timeout does not stop the shared coordinator; a later call can continue
    /// observing the same workers.
    pub async fn destroy_with_timeout(&self, timeout: Duration) -> ShutdownReport {
        self.wait_for_shutdown(timeout).await
    }

    /// Shuts down every existing lane. Returns `Ok` only after every worker has
    /// actually stopped; the legacy error set maps a five-second timeout to
    /// [`BeaverError::DamReleased`]. Use [`destroy_with_report`](Self::destroy_with_report)
    /// when the exact progress counts are required.
    pub async fn destroy(&self) -> BeaverResult<()> {
        if self.destroy_with_report().await.is_complete() {
            Ok(())
        } else {
            // Preserve the exhaustive 0.2.x BeaverError member set. The typed
            // report API above distinguishes timeout precisely.
            Err(BeaverError::DamReleased)
        }
    }
}
