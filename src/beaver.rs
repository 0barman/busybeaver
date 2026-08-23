use crate::dam::Dam;
use crate::error::{BeaverError, BeaverResult};
use crate::execution::{
    self, BatchCancelReport, CancelReason, ExecutionRegistry, ExecutorLifetime, TaskControlHandle,
    TaskExitSummary, TaskHandle, TaskSelector, TaskSpec,
};
use crate::ids::ExecutionId;
use crate::lane::{Lane, LaneConfig, LaneCore};
use crate::observation::{
    EventStream, EventSubscribeError, ExecutorSnapshot, LaneSnapshot, ResourceLimits,
};
use crate::recurring::{self, RecurringSpec};
use crate::scope::Scope;
use crate::scope::ScopeInner;
use crate::service::{self, ServiceHandle, ServiceSpec};
use crate::shutdown::{
    CallbackFailure, CleanupOutcome, CleanupProgress, ShutdownError, ShutdownHandle, ShutdownMode,
    ShutdownOptions, ShutdownOutcome, ShutdownProcess, ShutdownReport, ShutdownReportSnapshot,
    ShutdownTimeoutAction, TaskShutdownProgressRecord, TaskShutdownRecord, WorkerFailure,
};
use crate::slot::{SlotCore, SlotKey, TaskSlot};
use crate::task::Task;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::sync::{Arc, Weak};
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
/// ```no_run
/// use busybeaver::{Beaver, TaskExit, TaskSpec};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let beaver = Beaver::try_new("default", 256)?;
/// let mut handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(42) }))?;
/// if !matches!(handle.join().await?, TaskExit::Completed(42)) {
///     return Err(std::io::Error::other("unexpected task exit").into());
/// }
/// # Ok(()) }
/// ```
///
/// 2. **With an external runtime handle** (can be called outside tokio runtime):
/// ```no_run
/// use busybeaver::{Beaver, TaskSpec};
/// let runtime = tokio::runtime::Builder::new_multi_thread().enable_time().build()?;
/// let beaver = Beaver::try_new_with_handle("default", 256, runtime.handle().clone())?;
/// let handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) }))?;
/// runtime.block_on(handle.wait());
/// # Ok::<(), Box<dyn std::error::Error>>(())
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
    scopes: Arc<Mutex<HashMap<String, Weak<ScopeInner>>>>,
    slots: Arc<Mutex<HashMap<SlotKey, Weak<SlotCore>>>>,
    limits: Arc<ResourceLimits>,
    #[cfg(test)]
    shutdown_process_creations: Arc<std::sync::atomic::AtomicUsize>,
}

/// Non-panicking executor construction.
pub struct BeaverBuilder {
    name: String,
    buffer: usize,
    handle: Option<Handle>,
    limits: ResourceLimits,
}

impl BeaverBuilder {
    pub fn runtime_handle(mut self, handle: Handle) -> Self {
        self.handle = Some(handle);
        self
    }

    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn build(self) -> BeaverResult<Beaver> {
        let handle = match self.handle {
            Some(handle) => handle,
            None => Handle::try_current().map_err(|_| BeaverError::RuntimeUnavailable)?,
        };
        Beaver::build_with_handle(self.name, self.buffer, handle, self.limits)
    }
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
    pub fn builder(name: impl Into<String>, buffer: usize) -> BeaverBuilder {
        BeaverBuilder {
            name: name.into(),
            buffer,
            handle: None,
            limits: ResourceLimits::default(),
        }
    }

    pub fn try_new(name: impl Into<String>, buffer: usize) -> BeaverResult<Self> {
        Self::builder(name, buffer).build()
    }

    pub fn try_new_with_handle(
        name: impl Into<String>,
        buffer: usize,
        handle: Handle,
    ) -> BeaverResult<Self> {
        Self::builder(name, buffer).runtime_handle(handle).build()
    }

    fn build_with_handle(
        name: String,
        buffer: usize,
        handle: Handle,
        limits: ResourceLimits,
    ) -> BeaverResult<Self> {
        limits
            .validate()
            .map_err(|error| BeaverError::InvalidResourceLimit { field: error.field })?;
        if buffer == 0 || buffer > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(BeaverError::InvalidLaneCapacity);
        }
        let executions = ExecutionRegistry::new_with_limits(limits.clone());
        let lifetime = ExecutorLifetime::new(Arc::clone(&executions));
        let default = Arc::new(Mutex::new(Some(Arc::new(Dam::with_handle(
            name,
            buffer,
            handle.clone(),
        )))));
        Ok(Self {
            default,
            named: Arc::new(Mutex::new(HashMap::new())),
            handle: Some(handle),
            lifecycle: Arc::new(Mutex::new(Lifecycle::Running)),
            executions,
            lifetime,
            lanes: Arc::new(Mutex::new(HashMap::new())),
            admission_open: Arc::new(Mutex::new(true)),
            shutdown_process: Arc::new(Mutex::new(None)),
            scopes: Arc::new(Mutex::new(HashMap::new())),
            slots: Arc::new(Mutex::new(HashMap::new())),
            limits: Arc::new(limits),
            #[cfg(test)]
            shutdown_process_creations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

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
    /// Returns [`BeaverError::RuntimeUnavailable`] outside a Tokio runtime and
    /// [`BeaverError::InvalidLaneCapacity`] for an unsupported capacity.
    pub fn new(name: impl Into<String>, buffer: usize) -> BeaverResult<Self> {
        Self::try_new(name, buffer)
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
    /// Returns [`BeaverError::InvalidLaneCapacity`] for an unsupported capacity.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use busybeaver::{Beaver, TaskSpec};
    /// let runtime = tokio::runtime::Builder::new_multi_thread().enable_time().build()?;
    /// let beaver = Beaver::try_new_with_handle("default", 256, runtime.handle().clone())?;
    /// let handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) }))?;
    /// runtime.block_on(handle.wait());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new_with_handle(
        name: impl Into<String>,
        buffer: usize,
        handle: Handle,
    ) -> BeaverResult<Self> {
        Self::try_new_with_handle(name, buffer, handle)
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
        if lanes.len() >= self.limits.max_lanes {
            return Err(BeaverError::ResourceLimitExceeded { resource: "lanes" });
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

    /// Creates or returns a named generation scope bound to a lane.
    pub fn create_scope(&self, name: impl Into<String>, lane: Lane) -> BeaverResult<Scope> {
        let name = name.into();
        let lifecycle = self.lifecycle.lock()?;
        if !matches!(*lifecycle, Lifecycle::Running) {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let mut scopes = self.scopes.lock()?;
        scopes.retain(|_, scope| scope.strong_count() > 0);
        if let Some(existing) = scopes.get(&name).and_then(Weak::upgrade) {
            let existing = Scope { inner: existing };
            if existing.lane().id() == lane.id() {
                return Ok(existing);
            }
            return Err(BeaverError::ScopeConfigConflict { name });
        }
        if scopes.len() >= self.limits.max_scopes {
            return Err(BeaverError::ResourceLimitExceeded { resource: "scopes" });
        }
        let scope = Scope::new(name.clone(), lane);
        scopes.insert(name, Arc::downgrade(&scope.inner));
        Ok(scope)
    }

    /// Creates or returns a newest-wins task slot bound to a lane.
    pub fn create_task_slot(&self, key: SlotKey, lane: Lane) -> BeaverResult<TaskSlot> {
        let lifecycle = self.lifecycle.lock()?;
        if !matches!(*lifecycle, Lifecycle::Running) {
            return Err(BeaverError::ExecutorShuttingDown);
        }
        let mut slots = self.slots.lock()?;
        slots.retain(|_, slot| slot.strong_count() > 0);
        if let Some(existing) = slots.get(&key).and_then(Weak::upgrade) {
            let existing = TaskSlot { core: existing };
            if existing.lane().id() == lane.id() {
                return Ok(existing);
            }
            return Err(BeaverError::SlotConfigConflict {
                key: key.as_str().to_string(),
            });
        }
        if slots.len() >= self.limits.max_slots {
            return Err(BeaverError::ResourceLimitExceeded {
                resource: "task slots",
            });
        }
        let slot = TaskSlot::new(key.clone(), lane);
        slots.insert(key, Arc::downgrade(&slot.core));
        Ok(slot)
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

    /// Starts an unbounded recurring execution outside a bounded FIFO lane.
    pub fn spawn_recurring<T, E>(&self, spec: RecurringSpec<T, E>) -> BeaverResult<TaskHandle<T, E>>
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
        let prepared = recurring::prepare_recurring(&self.executions, runtime, spec)?;
        let execution::PreparedExecution { handle, start } = prepared;
        start.register()?;
        start.start();
        Ok(handle)
    }

    /// Starts a supervised long-running service outside ordinary lane FIFO
    /// capacity so it cannot permanently occupy a serial lane slot.
    pub fn start_service<T, E>(&self, spec: ServiceSpec<T, E>) -> BeaverResult<ServiceHandle<T, E>>
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
        let (prepared, state) = service::prepare_service(&self.executions, runtime, spec)?;
        let execution::PreparedExecution { handle, start } = prepared;
        start.register()?;
        start.start();
        Ok(ServiceHandle::new(handle, state))
    }

    /// Publishes an explicit platform/application resume notification to all
    /// currently active typed executions.
    pub fn notify_resumed(&self) -> usize {
        self.executions.notify_resumed()
    }

    pub fn subscribe_events(&self) -> Result<EventStream, EventSubscribeError> {
        self.executions.subscribe()
    }

    pub fn resource_limits(&self) -> &ResourceLimits {
        &self.limits
    }

    pub fn snapshot(&self) -> ExecutorSnapshot {
        let lane_cores = self
            .lanes
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut lanes = lane_cores
            .into_iter()
            .map(|core| {
                let lane = Lane {
                    core,
                    _lifetime: Arc::clone(&self.lifetime),
                };
                LaneSnapshot {
                    lane_id: lane.id(),
                    name: lane.name().to_string(),
                    stats: lane.stats(),
                }
            })
            .collect::<Vec<_>>();
        lanes.sort_unstable_by_key(|lane| lane.lane_id);
        ExecutorSnapshot {
            active: self.executions.snapshots(),
            terminal_history: self.executions.history(),
            lanes,
            scope_count: self
                .scopes
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard)
                .values()
                .filter(|scope| scope.strong_count() > 0)
                .count(),
            slot_count: self
                .slots
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard)
                .values()
                .filter(|slot| slot.strong_count() > 0)
                .count(),
            event_subscribers: self.executions.subscriber_count(),
        }
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
        ShutdownProcess::validate_options(&options)?;
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

        let process = ShutdownProcess::new(options.clone())?;
        #[cfg(test)]
        self.shutdown_process_creations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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

    #[cfg(test)]
    fn shutdown_process_creations_for_test(&self) -> usize {
        self.shutdown_process_creations
            .load(std::sync::atomic::Ordering::Relaxed)
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
        *self
            .lifecycle
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Lifecycle::Stopped;
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
            *self
                .lifecycle
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard) = Lifecycle::Stopped;
        }
    }
}

async fn run_shutdown_supervisor(resources: ShutdownResources, mut guard: ShutdownSupervisorGuard) {
    let mut dams = Vec::new();
    if let Some(default) = resources
        .default
        .lock()
        .map_or_else(crate::internal::recover_poison, |guard| guard)
        .take()
    {
        dams.push(default);
    }
    {
        let mut named = resources
            .named
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        dams.extend(named.drain().map(|(_, entry)| entry.dam));
    }
    for dam in &dams {
        let _ = dam.release().await;
    }

    let lanes: Vec<_> = resources
        .lanes
        .lock()
        .map_or_else(crate::internal::recover_poison, |guard| guard)
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
            for (control, initial) in &resources.initial {
                if !matches!(initial.kind, crate::ExecutionKind::Finite) {
                    control.cancel(CancelReason::ExecutorShutdown);
                }
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
        let mut summaries = Vec::with_capacity(waiting_controls.len());
        for control in waiting_controls {
            summaries.push(control.wait().await);
        }
        let mut worker_failures = Vec::new();
        for worker in workers {
            if let Err(error) = worker.await {
                worker_failures.push(WorkerFailure {
                    message: error.to_string(),
                });
            }
        }
        (summaries, worker_failures)
    });

    let mut exceeded = std::collections::HashSet::new();
    let mut forced = std::collections::HashSet::new();
    let (final_summaries, worker_failures) = if matches!(
        resources.process.options().timeout_action(),
        ShutdownTimeoutAction::Wait
    ) {
        completion.await
    } else {
        let grace_timer = catch_unwind(AssertUnwindSafe(|| {
            Box::pin(tokio::time::sleep_until(resources.process.grace_deadline()))
        }));
        match grace_timer {
            Err(_) => {
                resources.process.publish_timer_unavailable();
                completion.await
            }
            Ok(mut grace_timer) => tokio::select! {
                biased;
                failures = &mut completion => failures,
                _ = &mut grace_timer => {
                let pending: Vec<_> = resources.initial.iter()
                    .filter(|(control, _)| !control.state().is_terminal())
                    .map(|(control, _)| control.clone())
                    .collect();
                exceeded.extend(pending.iter().map(TaskControlHandle::execution_id));
                if matches!(
                    resources.process.options().timeout_action(),
                    ShutdownTimeoutAction::AbortAllowed
                ) {
                    for control in &pending {
                        if matches!(
                            control.request_forced_cancellation(),
                            Ok(crate::ForcedCancellationOutcome::Requested)
                                | Ok(crate::ForcedCancellationOutcome::AlreadyRequested)
                        ) {
                            forced.insert(control.execution_id());
                        }
                    }
                }
                let tasks = resources.initial.iter().map(|(control, initial)| {
                    let current = control.snapshot();
                    TaskShutdownProgressRecord {
                        execution_id: control.execution_id(),
                        snapshot_at_start: initial.clone(),
                        grace_deadline_exceeded: exceeded.contains(&control.execution_id()),
                        forced_cancellation_requested: forced.contains(&control.execution_id()),
                        final_exit: current.final_exit,
                        cleanup: match control.cleanup_progress() {
                            CleanupProgress::NotRequired if control.state().is_terminal() => {
                                CleanupProgress::Finished(CleanupOutcome::NotRequired)
                            }
                            CleanupProgress::NotRequired => CleanupProgress::Pending {
                                phase: crate::shutdown::CleanupPhase::WorkerJoin,
                            },
                            progress => progress,
                        },
                    }
                }).collect();
                let callback_failures = collect_callback_failures(&dams);
                let snapshot = Arc::new(ShutdownReportSnapshot {
                    tasks,
                    callback_failures,
                    worker_failures: Vec::new(),
                    elapsed: resources.process.accepted_at().elapsed(),
                });
                resources.process.publish_timeout(snapshot, pending);
                completion.await
                }
            },
        }
    };

    let mut tasks = Vec::with_capacity(resources.initial.len());
    let mut summaries = final_summaries.into_iter();
    for (control, initial) in &resources.initial {
        let final_exit = match summaries.next() {
            Some(summary) => summary,
            None => {
                crate::internal::log_internal_error(
                    "BB-SHUTDOWN-SUMMARY-MISSING",
                    "shutdown completion returned fewer summaries than its initial snapshot",
                );
                control.wait().await
            }
        };
        tasks.push(TaskShutdownRecord {
            execution_id: control.execution_id(),
            snapshot_at_start: initial.clone(),
            grace_deadline_exceeded: exceeded.contains(&control.execution_id()),
            forced_cancellation_requested: forced.contains(&control.execution_id()),
            final_exit,
            cleanup: final_cleanup_outcome(control.cleanup_progress()),
        });
    }
    if summaries.next().is_some() {
        crate::internal::log_internal_error(
            "BB-SHUTDOWN-SUMMARY-EXTRA",
            "shutdown completion returned more summaries than its initial snapshot",
        );
    }
    tasks.sort_unstable_by_key(|record| record.execution_id);
    let callback_failures = collect_callback_failures(&dams);
    let report = Arc::new(ShutdownReport {
        tasks,
        callback_failures,
        worker_failures,
        elapsed: resources.process.accepted_at().elapsed(),
    });
    resources.process.publish_final(Arc::clone(&report));
    guard.finish();
}

fn collect_callback_failures(dams: &[Arc<Dam>]) -> Vec<CallbackFailure> {
    dams.iter()
        .flat_map(|dam| dam.callback_failures())
        .map(|message| CallbackFailure { message })
        .collect()
}

fn final_cleanup_outcome(progress: CleanupProgress) -> CleanupOutcome {
    match progress {
        CleanupProgress::NotRequired => CleanupOutcome::NotRequired,
        CleanupProgress::Finished(outcome) => outcome,
        CleanupProgress::Pending { phase } => {
            CleanupOutcome::Failed(format!("cleanup remained pending in phase {phase:?}"))
        }
    }
}

#[cfg(test)]
#[path = "beaver_internal_tests.rs"]
mod tests;
