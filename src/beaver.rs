use crate::dam::Dam;
use crate::error::{BeaverError, BeaverResult};
use crate::execution::{
    self, BatchCancelReport, CancelReason, ExecutionRegistry, ExecutorLifetime, TaskControlHandle,
    TaskExitSummary, TaskHandle, TaskSelector, TaskSpec,
};
use crate::ids::ExecutionId;
use crate::lane::{Lane, LaneConfig, LaneCore};
use crate::shutdown::{
    CleanupOutcome, CleanupProgress, ShutdownError, ShutdownHandle, ShutdownMode, ShutdownOptions,
    ShutdownOutcome, ShutdownProcess, ShutdownReport, ShutdownReportSnapshot,
    ShutdownTimeoutAction, TaskShutdownProgressRecord, TaskShutdownRecord, WorkerFailure,
};
use crate::task::Task;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Handle;

/// BusyBeaver: Because sometimes your tasks need to run like a Busy Beaver — tirelessly attempting until they produce the maximum possible success (or hit their busy beaver bound).
///
/// BusyBeaver is a task-scheduling library that supports execution strategies based on counts, cycles, and custom time intervals. It streamlines periodic tasks in your codebase—such as heartbeats, metric reporting, scheduled polling, and automated cleanup—making them simpler and more reliable. At its core, BusyBeaver is an asynchronous task executor with configurable retry strategies, purpose-built for Rust async runtimes like Tokio. Whether a task needs to stop after exactly `N` executions, repeat every `X` milliseconds, or run at specific intervals within a defined time window, BusyBeaver handles the complexity elegantly. Equipped with built-in mechanisms like exponential backoff, retry limits, task listeners, and progress callbacks, it eliminates the need to manually write tedious tokio::time + loop + retry boilerplate in your asynchronous code.
///
/// # Lifecycle
///
/// Prefer calling [`Beaver::destroy`] before letting a `Beaver` go out of scope —
/// it is the deterministic way to stop all tasks and release resources. As a
/// safety net, dropping the final executor owner also signals its tasks to stop
/// (so a [`PeriodicBuilder`](crate::PeriodicBuilder) task that has not returned
/// [`WorkResult::Done`](crate::WorkResult::Done) does not keep running on the
/// underlying runtime), but this best-effort cleanup is not awaited. Public
/// [`Lane`] handles share ownership of typed executor resources, so dropping the
/// `Beaver` facade alone does not invalidate a lane that remains in use.
///
/// # Creation Methods
///
/// 1. **Within a tokio runtime**:
/// ```ignore
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let beaver = Beaver::new("default", 256);
///     beaver.enqueue(task).await?;
///     Ok(())
/// }
/// ```
///
/// 2. **With an external runtime handle** (can be called outside tokio runtime):
/// ```ignore
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let rt = tokio::runtime::Runtime::new()?;
///     let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
///     rt.block_on(beaver.enqueue(task))?;
///     Ok(())
/// }
/// ```
///
/// # Async API
///
/// [`enqueue`](Self::enqueue), [`enqueue_on_new_thread`](Self::enqueue_on_new_thread),
/// [`cancel_all`](Self::cancel_all), [`cancel_non_long_resident`](Self::cancel_non_long_resident),
/// [`release_thread_resource_by_name`](Self::release_thread_resource_by_name), and
/// [`destroy`](Self::destroy) are **async** and must be awaited on a Tokio runtime.
pub struct Beaver {
    default: Arc<Mutex<Option<Arc<Dam>>>>,
    named: Arc<Mutex<HashMap<String, NamedEntry>>>,
    handle: Option<Handle>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    executions: Arc<ExecutionRegistry>,
    lifetime: Arc<ExecutorLifetime>,
    lanes: Arc<Mutex<HashMap<String, Arc<LaneCore>>>>,
    admission_open: Arc<Mutex<bool>>,
    shutdown_process: Arc<Mutex<Option<ShutdownHandle>>>,
}

struct NamedEntry {
    dam: Arc<Dam>,
    long_resident: bool,
}

enum Lifecycle {
    Running,
    ShuttingDown,
    Stopped,
}

impl Beaver {
    /// Creates a new Beaver instance.
    ///
    /// **Note**: Must be called within a tokio runtime context.
    ///
    /// * `name` - The name of the default lane. Calling `enqueue(&self, task: Arc<Task>)`
    ///   sends tasks to this lane for execution.
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
        let handle = Handle::current();
        let executions = ExecutionRegistry::new();
        let lifetime = ExecutorLifetime::new(Arc::clone(&executions));
        let default = Arc::new(Mutex::new(Some(Arc::new(Dam::with_handle(
            name,
            buffer,
            handle.clone(),
        )))));
        Self {
            default,
            named: Arc::new(Mutex::new(HashMap::new())),
            handle: Some(handle),
            lifecycle: Arc::new(Mutex::new(Lifecycle::Running)),
            executions,
            lifetime,
            lanes: Arc::new(Mutex::new(HashMap::new())),
            admission_open: Arc::new(Mutex::new(true)),
            shutdown_process: Arc::new(Mutex::new(None)),
        }
    }

    /// Creates a Beaver instance with a specified tokio runtime handle.
    ///
    /// Can be called outside a tokio runtime context.
    /// * `name` - The name of the default lane. Calling `enqueue(&self, task: Arc<Task>)`
    ///   sends tasks to this lane for execution.
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
    /// ```ignore
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let rt = tokio::runtime::Runtime::new()?;
    ///     let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
    ///     rt.block_on(beaver.enqueue(task))?;
    ///     Ok(())
    /// }
    /// ```
    pub fn new_with_handle(name: impl Into<String>, buffer: usize, handle: Handle) -> Self {
        let executions = ExecutionRegistry::new();
        let lifetime = ExecutorLifetime::new(Arc::clone(&executions));
        let default = Arc::new(Mutex::new(Some(Arc::new(Dam::with_handle(
            name,
            buffer,
            handle.clone(),
        )))));

        Self {
            default,
            named: Arc::new(Mutex::new(HashMap::new())),
            handle: Some(handle),
            lifecycle: Arc::new(Mutex::new(Lifecycle::Running)),
            executions,
            lifetime,
            lanes: Arc::new(Mutex::new(HashMap::new())),
            admission_open: Arc::new(Mutex::new(true)),
            shutdown_process: Arc::new(Mutex::new(None)),
        }
    }

    /// Creates or returns an immutable public lane generation.
    pub fn create_lane(&self, config: LaneConfig) -> BeaverResult<Lane> {
        config.validate()?;
        let lifecycle = self.lifecycle.lock()?;
        if !matches!(*lifecycle, Lifecycle::Running) {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let admission = self.admission_open.lock()?;
        if !*admission {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let mut lanes = self.lanes.lock()?;
        if let Some(existing) = lanes.get(config.name()) {
            if existing.config() == &config {
                return Ok(Lane {
                    core: Arc::clone(existing),
                    _lifetime: Arc::clone(&self.lifetime),
                });
            }
            return Err(BeaverError::LaneConfigConflict {
                name: config.name().to_string(),
            });
        }
        let runtime = self
            .handle
            .clone()
            .ok_or_else(|| BeaverError::WorkerFailed("runtime handle missing".to_string()))?;
        let core = LaneCore::new(
            config.clone(),
            runtime,
            Arc::clone(&self.executions),
            Arc::clone(&self.admission_open),
        );
        lanes.insert(config.name().to_string(), Arc::clone(&core));
        drop(lanes);
        drop(admission);
        drop(lifecycle);
        Ok(Lane {
            core,
            _lifetime: Arc::clone(&self.lifetime),
        })
    }

    /// Spawns a reusable typed operation on the captured runtime.
    pub fn spawn<T, E>(&self, spec: TaskSpec<T, E>) -> BeaverResult<TaskHandle<T, E>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let lifecycle = self.lifecycle.lock()?;
        if !matches!(*lifecycle, Lifecycle::Running) {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let runtime = self
            .handle
            .clone()
            .ok_or_else(|| BeaverError::WorkerFailed("runtime handle missing".to_string()))?;
        execution::spawn_spec(&self.executions, runtime, spec)
    }

    /// Spawns a one-shot typed future on the captured runtime.
    pub fn spawn_future<T, E, F>(&self, future: F) -> BeaverResult<TaskHandle<T, E>>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let lifecycle = self.lifecycle.lock()?;
        if !matches!(*lifecycle, Lifecycle::Running) {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let runtime = self
            .handle
            .clone()
            .ok_or_else(|| BeaverError::WorkerFailed("runtime handle missing".to_string()))?;
        execution::spawn_future(&self.executions, runtime, future)
    }

    /// Returns a type-erased control only while the execution remains active.
    pub fn execution_control(&self, execution_id: ExecutionId) -> Option<TaskControlHandle> {
        self.executions.control(execution_id)
    }

    /// Returns a bounded redacted terminal summary, if it has not been evicted.
    pub fn execution_summary(&self, execution_id: ExecutionId) -> Option<TaskExitSummary> {
        self.executions.summary(execution_id)
    }

    /// Cancels the executions matching a registry snapshot. Concurrently
    /// admitted executions are deliberately not part of the report.
    pub fn cancel_snapshot(
        &self,
        selector: TaskSelector<'_>,
        reason: CancelReason,
    ) -> BatchCancelReport {
        self.executions.cancel_snapshot(selector, reason)
    }

    /// Enqueues a task to be executed on the default execution thread.
    #[inline]
    pub async fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        let default = {
            let lifecycle = self.lifecycle.lock()?;
            if !matches!(*lifecycle, Lifecycle::Running) {
                return Err(BeaverError::ExecutorShuttingDown);
            }
            self.default.lock()?.as_ref().cloned()
        };
        match default {
            Some(d) => d.enqueue(task).await,
            None => Err(BeaverError::NoDam),
        }
    }

    /// Enqueues a task to a named execution thread; creates it if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `task` - The task to enqueue for execution.
    /// * `name` - The name of the execution thread where the task will run.
    ///   If a thread with this name doesn't exist, a new one will be created.
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
            let lifecycle = self.lifecycle.lock()?;
            if !matches!(*lifecycle, Lifecycle::Running) {
                return Err(BeaverError::ExecutorShuttingDown);
            }
            let mut named = self.named.lock()?;
            let entry = named.entry(name.clone()).or_insert_with(|| {
                let dam = match handle {
                    Some(h) => Dam::with_handle(&name, buffer, h),
                    None => Dam::new(&name, buffer),
                };
                NamedEntry {
                    dam: Arc::new(dam),
                    long_resident: false,
                }
            });
            entry.long_resident = long_resident;
            let dam = Arc::clone(&entry.dam);
            drop(lifecycle);
            dam
        };
        dam.enqueue(task).await
    }

    /// Cancels all pending and running tasks on all execution threads.
    ///
    /// This includes tasks enqueued via [`enqueue`](Self::enqueue) on the default thread
    /// and all named threads, plus the typed executions admitted through
    /// [`spawn`](Self::spawn), [`spawn_future`](Self::spawn_future), and public
    /// [`Lane`] handles. Executions admitted after this method snapshots the
    /// typed registry remain eligible to run.
    pub async fn cancel_all(&self) -> BeaverResult<()> {
        let typed_controls = self.executions.active_controls();
        for control in typed_controls {
            control.cancel(CancelReason::UserRequested);
        }

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

    /// Releases a named execution thread and all its resources by name.
    ///
    /// The thread must have been created via [`enqueue_on_new_thread`](Self::enqueue_on_new_thread).
    pub async fn release_thread_resource_by_name(
        &self,
        name: impl Into<String>,
    ) -> BeaverResult<()> {
        let removed = { self.named.lock()?.remove(&name.into()) };
        if let Some(e) = removed {
            let _ = e.dam.release().await;
        }
        Ok(())
    }

    /// Destroys the Beaver instance and all its resources.
    ///
    /// This includes the default execution thread and all named threads, and cancels
    /// **all** tasks (both normal and long-resident). Call this before letting a
    /// Beaver go out of scope so that no background threads or resources keep running
    /// after the Beaver is dropped.
    ///
    /// **Graceful shutdown**: after signalling every lane to stop, `destroy`
    /// awaits the termination of each background worker, so when it returns the
    /// workers have actually exited (not merely been signalled). The wait is
    /// bounded by an internal timeout (`SHUTDOWN_TIMEOUT`); if a task is stuck
    /// in a `work.execute()` that never returns, `destroy` gives up waiting after
    /// the timeout rather than hanging forever. Concurrent and later callers
    /// observe the same shared shutdown result.
    pub async fn destroy(&self) -> BeaverResult<()> {
        let shutdown = match self.shutdown(ShutdownOptions::default()) {
            Ok(shutdown) => shutdown,
            Err(ShutdownError::ConfigConflict { existing, .. }) => existing,
            Err(ShutdownError::InvalidGracePeriod) => {
                return Err(BeaverError::WorkerFailed(
                    "shutdown grace period overflowed".to_string(),
                ));
            }
            Err(ShutdownError::LockPoisoned) => return Err(BeaverError::LockPoisoned),
        };
        match shutdown.wait_grace_outcome().await {
            Ok(ShutdownOutcome::Stopped(report)) => {
                if let Some(failure) = report.worker_failures.first() {
                    Err(BeaverError::WorkerFailed(failure.message.clone()))
                } else {
                    Ok(())
                }
            }
            Ok(ShutdownOutcome::TimedOut { .. }) => Err(BeaverError::ShutdownTimedOut),
            Err(error) => Err(BeaverError::WorkerFailed(error.to_string())),
        }
    }

    /// Synchronously accepts an irreversible checked shutdown and starts its
    /// independently-owned supervisor before returning.
    pub fn shutdown(&self, options: ShutdownOptions) -> Result<ShutdownHandle, ShutdownError> {
        let process = ShutdownProcess::new(options.clone())?;
        let mut shutdown_slot = self
            .shutdown_process
            .lock()
            .map_err(|_| ShutdownError::LockPoisoned)?;
        if let Some(existing) = shutdown_slot.as_ref() {
            let lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| ShutdownError::LockPoisoned)?;
            if matches!(*lifecycle, Lifecycle::Stopped) || existing.effective_options() == &options
            {
                return Ok(existing.clone());
            }
            return Err(ShutdownError::ConfigConflict {
                existing: existing.clone(),
                effective_options: existing.effective_options().clone(),
            });
        }

        let initial_controls = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| ShutdownError::LockPoisoned)?;
            match &*lifecycle {
                Lifecycle::Running => {
                    *self
                        .admission_open
                        .lock()
                        .map_err(|_| ShutdownError::LockPoisoned)? = false;
                    *lifecycle = Lifecycle::ShuttingDown;
                }
                Lifecycle::ShuttingDown => {
                    return Err(ShutdownError::LockPoisoned);
                }
                Lifecycle::Stopped => {
                    return Err(ShutdownError::LockPoisoned);
                }
            }
            self.executions.active_controls()
        };

        let shutdown = process.handle();
        *shutdown_slot = Some(shutdown.clone());
        drop(shutdown_slot);

        let resources = ShutdownResources {
            default: Arc::clone(&self.default),
            named: Arc::clone(&self.named),
            lanes: Arc::clone(&self.lanes),
            process: Arc::clone(&process),
            initial: initial_controls
                .into_iter()
                .map(|control| {
                    let snapshot = control.snapshot();
                    (control, snapshot)
                })
                .collect(),
        };
        let guard = ShutdownSupervisorGuard::new(Arc::clone(&process), Arc::clone(&self.lifecycle));
        let runtime = self.handle.clone().ok_or(ShutdownError::LockPoisoned)?;
        let supervisor = async move {
            run_shutdown_supervisor(resources, guard).await;
        };
        let _ = catch_unwind(AssertUnwindSafe(|| runtime.spawn(supervisor)));
        Ok(shutdown)
    }
}

struct ShutdownResources {
    default: Arc<Mutex<Option<Arc<Dam>>>>,
    named: Arc<Mutex<HashMap<String, NamedEntry>>>,
    lanes: Arc<Mutex<HashMap<String, Arc<LaneCore>>>>,
    process: Arc<ShutdownProcess>,
    initial: Vec<(TaskControlHandle, crate::execution::TaskSnapshot)>,
}

struct ShutdownSupervisorGuard {
    process: Arc<ShutdownProcess>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    armed: bool,
}

impl ShutdownSupervisorGuard {
    fn new(process: Arc<ShutdownProcess>, lifecycle: Arc<Mutex<Lifecycle>>) -> Self {
        Self {
            process,
            lifecycle,
            armed: true,
        }
    }

    fn finish(&mut self) {
        if let Ok(mut lifecycle) = self.lifecycle.lock() {
            *lifecycle = Lifecycle::Stopped;
        }
        self.armed = false;
    }
}

impl Drop for ShutdownSupervisorGuard {
    fn drop(&mut self) {
        if self.armed {
            let error = BeaverError::WorkerFailed(
                "shutdown supervisor was cancelled by its runtime".to_string(),
            );
            self.process.publish_failure(error.clone());
            if let Ok(mut lifecycle) = self.lifecycle.lock() {
                *lifecycle = Lifecycle::Stopped;
            }
        }
    }
}

async fn run_shutdown_supervisor(resources: ShutdownResources, mut guard: ShutdownSupervisorGuard) {
    let mut dams = Vec::new();
    if let Some(default) = resources
        .default
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        dams.push(default);
    }
    {
        let mut named = resources
            .named
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dams.extend(named.drain().map(|(_, entry)| entry.dam));
    }
    for dam in &dams {
        let _ = dam.release().await;
    }

    let lanes: Vec<_> = resources
        .lanes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .map(Arc::clone)
        .collect();
    match resources.process.options().shutdown_mode() {
        ShutdownMode::CancelAll => {
            for lane in &lanes {
                lane.request_close_and_cancel();
            }
            for (control, _) in &resources.initial {
                control.cancel(CancelReason::ExecutorShutdown);
            }
        }
        // At this milestone every typed model is finite (one-shot or retry).
        // Recurring/service will add an execution-kind classifier and cancel
        // only those unbounded kinds while finite work continues to drain.
        ShutdownMode::DrainFinite => {
            for lane in &lanes {
                lane.request_close();
            }
        }
    }

    let mut workers: Vec<_> = dams.iter().filter_map(|dam| dam.take_worker()).collect();
    workers.extend(lanes.iter().filter_map(|lane| lane.take_dispatcher()));
    let waiting_controls: Vec<_> = resources
        .initial
        .iter()
        .map(|(control, _)| control.clone())
        .collect();
    let mut completion = Box::pin(async move {
        for control in waiting_controls {
            control.wait().await;
        }
        let mut worker_failures = Vec::new();
        for worker in workers {
            if let Err(error) = worker.await {
                worker_failures.push(WorkerFailure {
                    message: error.to_string(),
                });
            }
        }
        worker_failures
    });

    let mut exceeded = std::collections::HashSet::new();
    let worker_failures = if matches!(
        resources.process.options().timeout_action(),
        ShutdownTimeoutAction::Wait
    ) {
        completion.await
    } else {
        tokio::select! {
            biased;
            failures = &mut completion => failures,
            _ = tokio::time::sleep_until(resources.process.grace_deadline()) => {
                let pending: Vec<_> = resources.initial.iter()
                    .filter(|(control, _)| !control.state().is_terminal())
                    .map(|(control, _)| control.clone())
                    .collect();
                exceeded.extend(pending.iter().map(TaskControlHandle::execution_id));
                let tasks = resources.initial.iter().map(|(control, initial)| {
                    let current = control.snapshot();
                    TaskShutdownProgressRecord {
                        execution_id: control.execution_id(),
                        snapshot_at_start: initial.clone(),
                        grace_deadline_exceeded: exceeded.contains(&control.execution_id()),
                        forced_cancellation_requested: false,
                        final_exit: current.final_exit,
                        cleanup: if control.state().is_terminal() {
                            CleanupProgress::Finished(CleanupOutcome::NotRequired)
                        } else {
                            CleanupProgress::Pending {
                                phase: crate::shutdown::CleanupPhase::WorkerJoin,
                            }
                        },
                    }
                }).collect();
                let snapshot = Arc::new(ShutdownReportSnapshot {
                    tasks,
                    callback_failures: Vec::new(),
                    worker_failures: Vec::new(),
                    elapsed: resources.process.accepted_at().elapsed(),
                });
                resources.process.publish_timeout(snapshot, pending);
                completion.await
            }
        }
    };

    let mut tasks = Vec::with_capacity(resources.initial.len());
    for (control, initial) in &resources.initial {
        let final_exit = control.wait().await;
        tasks.push(TaskShutdownRecord {
            execution_id: control.execution_id(),
            snapshot_at_start: initial.clone(),
            grace_deadline_exceeded: exceeded.contains(&control.execution_id()),
            forced_cancellation_requested: false,
            final_exit,
            cleanup: CleanupOutcome::NotRequired,
        });
    }
    tasks.sort_unstable_by_key(|record| record.execution_id);
    let report = Arc::new(ShutdownReport {
        tasks,
        callback_failures: Vec::new(),
        worker_failures,
        elapsed: resources.process.accepted_at().elapsed(),
    });
    resources.process.publish_final(Arc::clone(&report));
    guard.finish();
}
