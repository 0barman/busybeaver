use crate::error::{BeaverError, BeaverResult};
use crate::execution::{
    self, CancelReason, ExecutionRegistry, ExecutionStart, ExecutorLifetime, PreparedExecution,
    TaskControlHandle, TaskHandle, TaskSpec,
};
use crate::ids::{ExecutionId, LaneId};
use crate::retry::{self, RetrySpec};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Ownership policy for a lane generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LaneLifetime {
    Executor,
    Explicit,
}

/// Immutable lane configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneConfig {
    name: String,
    capacity: usize,
    concurrency: usize,
    lifetime: LaneLifetime,
}

impl LaneConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            capacity: 256,
            concurrency: 1,
            lifetime: LaneLifetime::Executor,
        }
    }

    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    pub fn lifetime(mut self, lifetime: LaneLifetime) -> Self {
        self.lifetime = lifetime;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn queue_capacity(&self) -> usize {
        self.capacity
    }

    pub fn max_concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn lane_lifetime(&self) -> LaneLifetime {
        self.lifetime
    }

    pub(crate) fn validate(&self) -> BeaverResult<()> {
        if self.capacity == 0 {
            return Err(BeaverError::InvalidLaneCapacity);
        }
        if self.concurrency == 0 {
            return Err(BeaverError::InvalidLaneConcurrency);
        }
        Ok(())
    }
}

/// Admission error for the public Lane API.
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnError {
    QueueFull,
    LaneClosing,
    ExecutorShuttingDown,
    AdmissionTimedOut,
    AdmissionDeadlineExceeded,
    Internal(BeaverError),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("lane queue is full"),
            Self::LaneClosing => formatter.write_str("lane is closing"),
            Self::ExecutorShuttingDown => formatter.write_str("executor is shutting down"),
            Self::AdmissionTimedOut => formatter.write_str("lane admission timed out"),
            Self::AdmissionDeadlineExceeded => {
                formatter.write_str("retry deadline elapsed before admission")
            }
            Self::Internal(error) => write!(formatter, "lane admission failed: {error}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BeaverError> for SpawnError {
    fn from(error: BeaverError) -> Self {
        match error {
            BeaverError::ExecutorShuttingDown => Self::ExecutorShuttingDown,
            other => Self::Internal(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneStats {
    pub queued_live: usize,
    pub running: usize,
    pub available_queue_capacity: usize,
    pub waiting_producers: usize,
}

struct QueueEntry {
    start: ExecutionStart,
}

struct LaneState {
    closing: bool,
    queue: VecDeque<ExecutionId>,
    entries: HashMap<ExecutionId, QueueEntry>,
    active: HashMap<ExecutionId, TaskControlHandle>,
    running: usize,
    waiting_producers: usize,
}

struct LaneShared {
    id: LaneId,
    config: LaneConfig,
    runtime: Handle,
    state: Mutex<LaneState>,
    work_changed: Notify,
    capacity_changed: Notify,
}

struct WaitingProducer<'a> {
    shared: &'a LaneShared,
}

impl<'a> WaitingProducer<'a> {
    fn new(shared: &'a LaneShared) -> Self {
        shared.lock_state().waiting_producers += 1;
        Self { shared }
    }
}

impl Drop for WaitingProducer<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.lock_state();
        state.waiting_producers = state.waiting_producers.saturating_sub(1);
    }
}

impl LaneShared {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, LaneState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remove_queued(&self, execution_id: ExecutionId) {
        let removed = {
            let mut state = self.lock_state();
            let removed = state.entries.remove(&execution_id);
            if removed.is_some() {
                state.queue.retain(|id| *id != execution_id);
                state.active.remove(&execution_id);
            }
            removed
        };
        if removed.is_some() {
            // Dropping the queued start invokes its runner guard outside the
            // lane lock, committing the already-selected cancellation exit.
            // A user future destructor must not unwind through lane cleanup.
            let _ = catch_unwind(AssertUnwindSafe(|| drop(removed)));
            self.capacity_changed.notify_one();
            self.work_changed.notify_one();
        }
    }

    fn take_startable(&self) -> (Vec<ExecutionStart>, bool) {
        let mut state = self.lock_state();
        let mut starts = Vec::new();
        while state.running < self.config.concurrency {
            let Some(execution_id) = state.queue.pop_front() else {
                break;
            };
            let Some(entry) = state.entries.remove(&execution_id) else {
                continue;
            };
            state.running += 1;
            starts.push(entry.start);
        }
        let should_exit = state.closing && state.active.is_empty();
        drop(state);
        if !starts.is_empty() {
            self.capacity_changed.notify_waiters();
        }
        (starts, should_exit)
    }

    fn execution_finished(&self, execution_id: ExecutionId) {
        let mut state = self.lock_state();
        if state.active.remove(&execution_id).is_some() {
            state.running = state.running.saturating_sub(1);
        }
        drop(state);
        self.work_changed.notify_one();
        self.capacity_changed.notify_waiters();
    }

    fn request_close(&self) -> Vec<TaskControlHandle> {
        let controls = {
            let mut state = self.lock_state();
            state.closing = true;
            state.active.values().cloned().collect()
        };
        self.work_changed.notify_waiters();
        self.capacity_changed.notify_waiters();
        controls
    }

    fn stats(&self) -> LaneStats {
        let state = self.lock_state();
        LaneStats {
            queued_live: state.entries.len(),
            running: state.running,
            available_queue_capacity: self.config.capacity.saturating_sub(state.entries.len()),
            waiting_producers: state.waiting_producers,
        }
    }

    fn dispatcher_stopped(&self) {
        let (queued, running_controls) = {
            let mut state = self.lock_state();
            state.closing = true;
            state.queue.clear();
            let queued = std::mem::take(&mut state.entries);
            let running_controls = state
                .active
                .iter()
                .filter(|(execution_id, _)| !queued.contains_key(execution_id))
                .map(|(_, control)| control.clone())
                .collect::<Vec<_>>();
            state.active.clear();
            state.running = 0;
            (queued, running_controls)
        };

        // Queued starts own their RunnerGuard. Dropping them outside the lane
        // lock commits ExecutorStopped even when the dispatcher future was
        // never polled by a runtime that disappeared during admission.
        for entry in queued.into_values() {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(entry)));
        }
        for control in running_controls {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                control.cancel(CancelReason::ExecutorShutdown)
            }));
        }
        self.capacity_changed.notify_waiters();
        self.work_changed.notify_waiters();
    }
}

struct DispatcherGuard {
    shared: Arc<LaneShared>,
}

impl Drop for DispatcherGuard {
    fn drop(&mut self) {
        self.shared.dispatcher_stopped();
    }
}

async fn run_dispatcher(shared: Arc<LaneShared>) {
    loop {
        let notified = shared.work_changed.notified();
        let (starts, should_exit) = shared.take_startable();
        if should_exit {
            return;
        }
        if starts.is_empty() {
            notified.await;
            continue;
        }

        for start in starts {
            let execution_id = start.execution_id();
            let control = start.control();
            start.start();
            let monitor_shared = Arc::clone(&shared);
            shared.runtime.spawn(async move {
                control.wait().await;
                monitor_shared.execution_finished(execution_id);
            });
        }
    }
}

pub(crate) struct LaneCore {
    shared: Arc<LaneShared>,
    registry: Arc<ExecutionRegistry>,
    admission_open: Arc<Mutex<bool>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
}

impl LaneCore {
    pub(crate) fn new(
        config: LaneConfig,
        runtime: Handle,
        registry: Arc<ExecutionRegistry>,
        admission_open: Arc<Mutex<bool>>,
    ) -> Arc<Self> {
        let shared = Arc::new(LaneShared {
            id: LaneId::new(),
            config,
            runtime: runtime.clone(),
            state: Mutex::new(LaneState {
                closing: false,
                queue: VecDeque::new(),
                entries: HashMap::new(),
                active: HashMap::new(),
                running: 0,
                waiting_producers: 0,
            }),
            work_changed: Notify::new(),
            capacity_changed: Notify::new(),
        });
        let dispatcher_shared = Arc::clone(&shared);
        let dispatcher_guard = DispatcherGuard {
            shared: Arc::clone(&shared),
        };
        let dispatcher = runtime.spawn(async move {
            let _guard = dispatcher_guard;
            run_dispatcher(dispatcher_shared).await;
        });
        Arc::new(Self {
            shared,
            registry,
            admission_open,
            dispatcher: Mutex::new(Some(dispatcher)),
        })
    }

    pub(crate) fn config(&self) -> &LaneConfig {
        &self.shared.config
    }

    pub(crate) fn request_close_and_cancel(&self) {
        let controls = self.shared.request_close();
        for control in controls {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                control.cancel(CancelReason::ExecutorShutdown)
            }));
        }
    }

    pub(crate) fn request_close(&self) {
        self.shared.request_close();
    }

    pub(crate) fn take_dispatcher(&self) -> Option<JoinHandle<()>> {
        self.dispatcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn insert_prepared<T, E>(
        &self,
        prepared: PreparedExecution<T, E>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let PreparedExecution { handle, start } = prepared;
        let execution_id = start.execution_id();
        start.set_lane_id(self.shared.id);
        let weak_shared: Weak<LaneShared> = Arc::downgrade(&self.shared);
        start.install_cancel_hook(Box::new(move || {
            if let Some(shared) = weak_shared.upgrade() {
                shared.remove_queued(execution_id);
            }
        }));

        let mut state = self.shared.lock_state();
        if state.closing {
            drop(state);
            drop(start);
            return Err(SpawnError::LaneClosing);
        }
        if state.entries.len() >= self.shared.config.capacity {
            drop(state);
            drop(start);
            return Err(SpawnError::QueueFull);
        }
        state.active.insert(execution_id, handle.control());
        state.entries.insert(execution_id, QueueEntry { start });
        state.queue.push_back(execution_id);
        drop(state);
        self.shared.work_changed.notify_one();
        Ok(handle)
    }
}

impl Drop for LaneCore {
    fn drop(&mut self) {
        self.request_close_and_cancel();
        if let Some(dispatcher) = self
            .dispatcher
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            dispatcher.abort();
        }
    }
}

/// A cloneable handle to one immutable lane generation.
#[derive(Clone)]
pub struct Lane {
    pub(crate) core: Arc<LaneCore>,
    pub(crate) _lifetime: Arc<ExecutorLifetime>,
}

impl Lane {
    pub fn id(&self) -> LaneId {
        self.core.shared.id
    }

    pub fn name(&self) -> &str {
        self.core.shared.config.name()
    }

    pub fn config(&self) -> &LaneConfig {
        &self.core.shared.config
    }

    pub fn stats(&self) -> LaneStats {
        self.core.shared.stats()
    }

    pub fn try_spawn<T, E>(&self, spec: TaskSpec<T, E>) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let admission = self
            .core
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*admission {
            return Err(SpawnError::ExecutorShuttingDown);
        }
        {
            let state = self.core.shared.lock_state();
            if state.closing {
                return Err(SpawnError::LaneClosing);
            }
            if state.entries.len() >= self.core.shared.config.capacity {
                return Err(SpawnError::QueueFull);
            }
        }
        let prepared =
            execution::prepare_spec(&self.core.registry, self.core.shared.runtime.clone(), spec)?;
        let result = self.core.insert_prepared(prepared);
        drop(admission);
        result
    }

    pub fn try_spawn_future<T, E, F>(&self, future: F) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let admission = self
            .core
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*admission {
            return Err(SpawnError::ExecutorShuttingDown);
        }
        {
            let state = self.core.shared.lock_state();
            if state.closing {
                return Err(SpawnError::LaneClosing);
            }
            if state.entries.len() >= self.core.shared.config.capacity {
                return Err(SpawnError::QueueFull);
            }
        }
        let prepared = execution::prepare_future(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            future,
        );
        let result = self.core.insert_prepared(prepared);
        drop(admission);
        result
    }

    fn try_spawn_retry_at<T, E>(
        &self,
        spec: RetrySpec<T, E>,
        accepted_at: Instant,
        deadline: Option<Instant>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(SpawnError::AdmissionDeadlineExceeded);
        }
        let admission = self
            .core
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*admission {
            return Err(SpawnError::ExecutorShuttingDown);
        }
        {
            let state = self.core.shared.lock_state();
            if state.closing {
                return Err(SpawnError::LaneClosing);
            }
            if state.entries.len() >= self.core.shared.config.capacity {
                return Err(SpawnError::QueueFull);
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(SpawnError::AdmissionDeadlineExceeded);
        }
        let prepared = retry::prepare_retry(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            spec,
            accepted_at,
            deadline,
        );
        let result = self.core.insert_prepared(prepared);
        drop(admission);
        result
    }

    pub fn try_spawn_retry<T, E>(
        &self,
        spec: RetrySpec<T, E>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let accepted_at = Instant::now();
        let deadline = spec
            .deadline_at(accepted_at)
            .map_err(|()| SpawnError::AdmissionDeadlineExceeded)?;
        self.try_spawn_retry_at(spec, accepted_at, deadline)
    }

    pub fn spawn_retry<T, E>(
        &self,
        spec: RetrySpec<T, E>,
    ) -> impl Future<Output = Result<TaskHandle<T, E>, SpawnError>> + Send + '_
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let accepted_at = Instant::now();
        let deadline = spec.deadline_at(accepted_at);
        async move {
            let deadline = deadline.map_err(|()| SpawnError::AdmissionDeadlineExceeded)?;
            loop {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(SpawnError::AdmissionDeadlineExceeded);
                }
                let notified = self.core.shared.capacity_changed.notified();
                match self.try_spawn_retry_at(spec.clone(), accepted_at, deadline) {
                    Ok(handle) => return Ok(handle),
                    Err(SpawnError::QueueFull) => {
                        let _waiting = WaitingProducer::new(&self.core.shared);
                        if let Some(deadline) = deadline {
                            tokio::select! {
                                biased;
                                _ = tokio::time::sleep_until(deadline) => {
                                    return Err(SpawnError::AdmissionDeadlineExceeded);
                                }
                                _ = notified => {}
                            }
                        } else {
                            notified.await;
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    pub async fn spawn<T, E>(&self, spec: TaskSpec<T, E>) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        loop {
            let notified = self.core.shared.capacity_changed.notified();
            match self.try_spawn(spec.clone()) {
                Ok(handle) => return Ok(handle),
                Err(SpawnError::QueueFull) => {
                    let _waiting = WaitingProducer::new(&self.core.shared);
                    notified.await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn spawn_timeout<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        timeout: Duration,
    ) -> impl Future<Output = Result<TaskHandle<T, E>, SpawnError>> + Send + '_
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let deadline = Instant::now().checked_add(timeout);
        async move {
            let Some(deadline) = deadline else {
                return Err(SpawnError::AdmissionTimedOut);
            };
            match tokio::time::timeout_at(deadline, self.spawn(spec)).await {
                Ok(result) => result,
                Err(_) => Err(SpawnError::AdmissionTimedOut),
            }
        }
    }

    /// Closes admission without cancelling already admitted executions.
    pub fn close(&self) {
        let _admission = self
            .core
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.core.shared.request_close();
    }

    pub async fn close_and_cancel(&self) {
        let controls = {
            let _admission = self
                .core
                .admission_open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.core.shared.request_close()
        };
        for control in &controls {
            control.cancel(CancelReason::LaneClosing);
        }
        for control in controls {
            control.wait().await;
        }
    }

    pub fn cancel_snapshot(&self, reason: CancelReason) -> Vec<ExecutionId> {
        let controls: Vec<_> = self
            .core
            .shared
            .lock_state()
            .active
            .values()
            .cloned()
            .collect();
        controls
            .into_iter()
            .map(|control| {
                let id = control.execution_id();
                control.cancel(reason.clone());
                id
            })
            .collect()
    }
}
