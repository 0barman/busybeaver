use crate::error::BeaverResult;
use crate::ids::{ExecutionId, LaneId, ScopeId, TaskSpecId};
use crate::retry::{RetryFailure, RetryPolicyStage};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

type BoxOperationFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;
type OperationFactory<T, E> =
    dyn Fn(WorkContext) -> BoxOperationFuture<T, E> + Send + Sync + 'static;

/// Why cooperative cancellation was requested.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CancelReason {
    UserRequested,
    ExecutorShutdown,
    LaneClosing,
    ScopeCancelled,
    Replaced,
    Other(String),
}

/// Error returned by a cancellation-aware operation such as
/// [`WorkContext::sleep`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("execution was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The first lifecycle stop cause accepted by an execution.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StopCauseSummary {
    Cancel(CancelReason),
    Deadline,
}

/// Public execution state. Business terminal state is separate from internal
/// callback/resource cleanup in later 0.3 milestones.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskState {
    Queued,
    Running { attempt: u32 },
    Sleeping { attempt: u32, wake_at: Instant },
    Stopping { cause: StopCauseSummary },
    Completed,
    Failed,
    Cancelled,
    Aborted,
    Panicked,
    ExecutorStopped,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Aborted
                | Self::Panicked
                | Self::ExecutorStopped
        )
    }
}

/// Source of a caught unwind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PanicSource {
    Factory,
    WorkFuture,
    Child,
}

/// Why an execution could not complete after its runtime disappeared.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutorStopReason {
    RuntimeUnavailable,
    RunnerCancelled,
    InternalInvariantViolation,
}

/// Typed business failure. Retry-specific variants are added by the typed
/// retry milestone without changing the outer [`TaskExit`] model.
#[derive(Debug)]
#[non_exhaustive]
pub enum TaskFailure<E> {
    Operation {
        error: E,
    },
    Retry(RetryFailure<E>),
    DeadlineExceeded {
        last_error: Option<E>,
    },
    PolicyPanicked {
        stage: RetryPolicyStage,
        last_error: Option<E>,
    },
    ChildFailed,
}

/// The unique terminal outcome of an execution.
#[derive(Debug)]
#[non_exhaustive]
pub enum TaskExit<T, E> {
    Completed(T),
    Failed(TaskFailure<E>),
    Cancelled {
        reason: CancelReason,
    },
    Aborted {
        preceding_stop: Option<StopCauseSummary>,
    },
    Panicked {
        source: PanicSource,
        message: String,
    },
    ExecutorStopped {
        reason: ExecutorStopReason,
    },
}

impl<T, E> TaskExit<T, E> {
    fn summary(&self) -> TaskExitSummary {
        match self {
            Self::Completed(_) => TaskExitSummary::Completed,
            Self::Failed(_) => TaskExitSummary::Failed,
            Self::Cancelled { reason } => TaskExitSummary::Cancelled {
                reason: reason.clone(),
            },
            Self::Aborted { preceding_stop } => TaskExitSummary::Aborted {
                preceding_stop: preceding_stop.clone(),
            },
            Self::Panicked { source, .. } => TaskExitSummary::Panicked { source: *source },
            Self::ExecutorStopped { reason } => TaskExitSummary::ExecutorStopped {
                reason: reason.clone(),
            },
        }
    }
}

/// Copyable/redacted terminal information used by repeated waiters, history
/// and shutdown reports. It never contains `T`, `E` or panic text.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskExitSummary {
    Completed,
    Failed,
    Cancelled {
        reason: CancelReason,
    },
    Aborted {
        preceding_stop: Option<StopCauseSummary>,
    },
    Panicked {
        source: PanicSource,
    },
    ExecutorStopped {
        reason: ExecutorStopReason,
    },
}

impl TaskExitSummary {
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// A redacted point-in-time view of an execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshot {
    pub execution_id: ExecutionId,
    pub task_spec_id: TaskSpecId,
    pub state: TaskState,
    pub stop_cause: Option<StopCauseSummary>,
    pub final_exit: Option<TaskExitSummary>,
}

#[derive(Debug)]
struct CoreState {
    public_state: TaskState,
    stop_cause: Option<StopCauseSummary>,
    terminal: Option<TaskExitSummary>,
}

struct ChildSet {
    admission_open: bool,
    current_attempt: u32,
    attempt_admission_open: bool,
    controls: Vec<TrackedChild>,
}

struct TrackedChild {
    attempt: u32,
    control: TaskControlHandle,
}

pub(crate) struct ExecutionCore {
    execution_id: ExecutionId,
    task_spec_id: TaskSpecId,
    state: Mutex<CoreState>,
    cancellation: watch::Sender<bool>,
    terminal: watch::Sender<Option<TaskExitSummary>>,
    runtime: Handle,
    registry: Option<Weak<ExecutionRegistry>>,
    children: Mutex<ChildSet>,
    tag: Option<Arc<str>>,
    lane_id: Mutex<Option<LaneId>>,
    scope_id: Mutex<Option<ScopeId>>,
    queued_cancel_hook: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

impl ExecutionCore {
    fn new(
        execution_id: ExecutionId,
        task_spec_id: TaskSpecId,
        runtime: Handle,
        registry: Option<Weak<ExecutionRegistry>>,
        tag: Option<Arc<str>>,
    ) -> Self {
        let (cancellation, _) = watch::channel(false);
        let (terminal, _) = watch::channel(None);
        Self {
            execution_id,
            task_spec_id,
            state: Mutex::new(CoreState {
                public_state: TaskState::Queued,
                stop_cause: None,
                terminal: None,
            }),
            cancellation,
            terminal,
            runtime,
            registry,
            children: Mutex::new(ChildSet {
                admission_open: true,
                current_attempt: 1,
                attempt_admission_open: true,
                controls: Vec::new(),
            }),
            tag,
            lane_id: Mutex::new(None),
            scope_id: Mutex::new(None),
            queued_cancel_hook: Mutex::new(None),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CoreState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn state(&self) -> TaskState {
        self.lock_state().public_state.clone()
    }

    fn snapshot(&self) -> TaskSnapshot {
        let state = self.lock_state();
        TaskSnapshot {
            execution_id: self.execution_id,
            task_spec_id: self.task_spec_id,
            state: state.public_state.clone(),
            stop_cause: state.stop_cause.clone(),
            final_exit: state.terminal.clone(),
        }
    }

    fn set_active_state(&self, next: TaskState) {
        let mut state = self.lock_state();
        if state.terminal.is_none() && state.stop_cause.is_none() {
            state.public_state = next;
        }
    }

    fn claim_start(&self) -> bool {
        let mut state = self.lock_state();
        if state.terminal.is_some() || state.stop_cause.is_some() {
            return false;
        }
        state.public_state = TaskState::Running { attempt: 1 };
        true
    }

    fn install_queued_cancel_hook(&self, hook: Box<dyn FnOnce() + Send + 'static>) {
        *self
            .queued_cancel_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    fn clear_queued_cancel_hook(&self) {
        self.queued_cancel_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn cancel(&self, reason: CancelReason) -> CancelRequestOutcome {
        self.request_stop(StopCauseSummary::Cancel(reason))
    }

    fn deadline(&self) -> CancelRequestOutcome {
        self.request_stop(StopCauseSummary::Deadline)
    }

    fn request_stop(&self, cause: StopCauseSummary) -> CancelRequestOutcome {
        let (children, queued_cancel_hook) = {
            let mut state = self.lock_state();
            if state.terminal.is_some() {
                return CancelRequestOutcome::AlreadyTerminal;
            }
            if state.stop_cause.is_some() {
                return CancelRequestOutcome::AlreadyStopping;
            }
            state.stop_cause = Some(cause.clone());
            state.public_state = TaskState::Stopping { cause };
            drop(state);

            let children = self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let queued_cancel_hook = self
                .queued_cancel_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            (
                children
                    .controls
                    .iter()
                    .map(|child| child.control.clone())
                    .collect::<Vec<_>>(),
                queued_cancel_hook,
            )
        };

        self.cancellation.send_replace(true);
        if let Some(hook) = queued_cancel_hook {
            hook();
        }
        for child in children {
            child.cancel(CancelReason::ExecutorShutdown);
        }
        CancelRequestOutcome::Requested
    }

    fn is_cancelled(&self) -> bool {
        *self.cancellation.borrow()
    }

    async fn cancelled(&self) {
        let mut cancellation = self.cancellation.subscribe();
        if *cancellation.borrow() {
            return;
        }
        while cancellation.changed().await.is_ok() {
            if *cancellation.borrow_and_update() {
                return;
            }
        }
    }

    async fn wait(&self) -> TaskExitSummary {
        let mut terminal = self.terminal.subscribe();
        loop {
            if let Some(summary) = terminal.borrow().clone() {
                return summary;
            }
            if terminal.changed().await.is_err() {
                return TaskExitSummary::ExecutorStopped {
                    reason: ExecutorStopReason::RuntimeUnavailable,
                };
            }
        }
    }

    fn commit<T, E>(&self, result: &ResultCell<T, E>, normal_exit: TaskExit<T, E>) -> bool {
        let mut unused_exit = Some(normal_exit);
        let summary = {
            let mut state = self.lock_state();
            if state.terminal.is_some() {
                return false;
            }

            let selected = match state.stop_cause.clone() {
                Some(StopCauseSummary::Cancel(reason)) => TaskExit::Cancelled { reason },
                Some(StopCauseSummary::Deadline)
                    if matches!(
                        unused_exit.as_ref(),
                        Some(TaskExit::Failed(TaskFailure::DeadlineExceeded { .. }))
                    ) =>
                {
                    match unused_exit.take() {
                        Some(exit) => exit,
                        None => {
                            eprintln!(
                                "busybeaver: deadline terminal selection lost its prepared exit"
                            );
                            TaskExit::ExecutorStopped {
                                reason: ExecutorStopReason::InternalInvariantViolation,
                            }
                        }
                    }
                }
                Some(StopCauseSummary::Deadline) => {
                    TaskExit::Failed(TaskFailure::DeadlineExceeded { last_error: None })
                }
                None => match unused_exit.take() {
                    Some(exit) => exit,
                    None => {
                        eprintln!("busybeaver: terminal selection lost its prepared exit");
                        TaskExit::ExecutorStopped {
                            reason: ExecutorStopReason::InternalInvariantViolation,
                        }
                    }
                },
            };
            let summary = selected.summary();
            result.store(selected);
            state.public_state = summary.state();
            state.terminal = Some(summary.clone());
            summary
        };

        // A business value that lost to an earlier stop cause is dropped after
        // releasing the execution lock.
        drop(unused_exit);
        self.terminal.send_replace(Some(summary.clone()));
        if let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) {
            registry.finish(self.execution_id, summary);
        }
        true
    }

    async fn close_children_and_wait(&self) {
        let controls = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            children.admission_open = false;
            children.attempt_admission_open = false;
            std::mem::take(&mut children.controls)
                .into_iter()
                .map(|child| child.control)
                .collect::<Vec<_>>()
        };
        for child in &controls {
            child.cancel(CancelReason::ExecutorShutdown);
        }
        for child in controls {
            child.wait().await;
        }
    }

    fn begin_attempt(&self, attempt: u32) {
        let mut children = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        children.current_attempt = attempt;
        children.attempt_admission_open = children.admission_open;
    }

    async fn close_attempt_children_and_wait(&self, attempt: u32) {
        let controls = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if children.current_attempt == attempt {
                children.attempt_admission_open = false;
            }
            let mut controls = Vec::new();
            let mut retained = Vec::with_capacity(children.controls.len());
            for child in std::mem::take(&mut children.controls) {
                if child.attempt == attempt {
                    controls.push(child.control);
                } else {
                    retained.push(child);
                }
            }
            children.controls = retained;
            controls
        };
        for child in &controls {
            child.cancel(CancelReason::ExecutorShutdown);
        }
        for child in controls {
            child.wait().await;
        }
    }
}

impl TaskExitSummary {
    fn state(&self) -> TaskState {
        match self {
            Self::Completed => TaskState::Completed,
            Self::Failed => TaskState::Failed,
            Self::Cancelled { .. } => TaskState::Cancelled,
            Self::Aborted { .. } => TaskState::Aborted,
            Self::Panicked { .. } => TaskState::Panicked,
            Self::ExecutorStopped { .. } => TaskState::ExecutorStopped,
        }
    }
}

struct ResultCell<T, E> {
    value: Mutex<Option<TaskExit<T, E>>>,
}

impl<T, E> ResultCell<T, E> {
    fn new() -> Self {
        Self {
            value: Mutex::new(None),
        }
    }

    fn store(&self, exit: TaskExit<T, E>) {
        let mut value = self
            .value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(value.is_none());
        *value = Some(exit);
    }

    fn take(&self) -> Option<TaskExit<T, E>> {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Result of submitting a cooperative cancellation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CancelRequestOutcome {
    Requested,
    AlreadyStopping,
    AlreadyTerminal,
}

/// Error returned when a typed result cannot be taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinResultError {
    AlreadyTaken,
}

impl fmt::Display for JoinResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("typed task result was already taken")
    }
}

impl std::error::Error for JoinResultError {}

/// Unique typed owner of an execution result.
#[must_use = "dropping a TaskHandle detaches the execution and discards its typed result"]
pub struct TaskHandle<T, E> {
    core: Arc<ExecutionCore>,
    result: Arc<ResultCell<T, E>>,
}

impl<T: Send, E: Send> TaskHandle<T, E> {
    pub fn execution_id(&self) -> ExecutionId {
        self.core.execution_id
    }

    pub fn task_spec_id(&self) -> TaskSpecId {
        self.core.task_spec_id
    }

    pub fn state(&self) -> TaskState {
        self.core.state()
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        self.core.snapshot()
    }

    pub fn control(&self) -> TaskControlHandle {
        TaskControlHandle {
            core: Arc::clone(&self.core),
        }
    }

    pub async fn wait(&self) -> TaskExitSummary {
        self.core.wait().await
    }

    pub async fn join(&mut self) -> Result<TaskExit<T, E>, JoinResultError> {
        if self.core.state().is_terminal() {
            return self.result.take().ok_or(JoinResultError::AlreadyTaken);
        }
        self.core.wait().await;
        self.result.take().ok_or(JoinResultError::AlreadyTaken)
    }

    pub fn try_join(&mut self) -> Result<Option<TaskExit<T, E>>, JoinResultError> {
        if !self.core.state().is_terminal() {
            return Ok(None);
        }
        self.result
            .take()
            .map(Some)
            .ok_or(JoinResultError::AlreadyTaken)
    }

    pub fn cancel_and_wait(&self, reason: CancelReason) -> CancelWait<'_> {
        self.core.cancel(reason);
        CancelWait {
            inner: Box::pin(self.wait()),
        }
    }

    pub fn detach(self) -> TaskControlHandle {
        self.control()
    }

    pub fn cancel_on_drop(self, reason: CancelReason) -> CancelOnDrop<T, E> {
        CancelOnDrop {
            handle: self,
            reason,
        }
    }
}

/// Wrapper that requests cancellation if it is dropped while still owning the
/// typed handle.
pub struct CancelOnDrop<T, E> {
    handle: TaskHandle<T, E>,
    reason: CancelReason,
}

impl<T, E> Deref for CancelOnDrop<T, E> {
    type Target = TaskHandle<T, E>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl<T, E> DerefMut for CancelOnDrop<T, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl<T, E> Drop for CancelOnDrop<T, E> {
    fn drop(&mut self) {
        self.handle.core.cancel(self.reason.clone());
    }
}

/// Cloneable, type-erased execution control.
#[derive(Clone)]
pub struct TaskControlHandle {
    core: Arc<ExecutionCore>,
}

impl fmt::Debug for TaskControlHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskControlHandle")
            .field("execution_id", &self.execution_id())
            .field("state", &self.state())
            .finish()
    }
}

impl TaskControlHandle {
    pub fn execution_id(&self) -> ExecutionId {
        self.core.execution_id
    }

    pub fn state(&self) -> TaskState {
        self.core.state()
    }

    pub fn snapshot(&self) -> TaskSnapshot {
        self.core.snapshot()
    }

    pub fn is_cancelled(&self) -> bool {
        self.core.is_cancelled()
    }

    pub async fn wait(&self) -> TaskExitSummary {
        self.core.wait().await
    }

    pub fn cancel(&self, reason: CancelReason) -> CancelRequestOutcome {
        self.core.cancel(reason)
    }

    pub(crate) fn mark_deadline(&self) -> CancelRequestOutcome {
        self.core.deadline()
    }

    pub fn cancel_and_wait(&self, reason: CancelReason) -> CancelWait<'_> {
        self.cancel(reason);
        CancelWait {
            inner: Box::pin(self.wait()),
        }
    }
}

/// Future returned after cancellation has already been synchronously
/// submitted. Dropping this future never withdraws the request.
pub struct CancelWait<'a> {
    inner: Pin<Box<dyn Future<Output = TaskExitSummary> + Send + 'a>>,
}

impl Future for CancelWait<'_> {
    type Output = TaskExitSummary;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(context)
    }
}

/// Immutable, reusable typed operation definition.
pub struct TaskSpec<T, E> {
    id: TaskSpecId,
    factory: Arc<OperationFactory<T, E>>,
    tag: Option<Arc<str>>,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for TaskSpec<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            factory: Arc::clone(&self.factory),
            tag: self.tag.clone(),
            marker: PhantomData,
        }
    }
}

impl<T, E> TaskSpec<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    pub fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(WorkContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        Self {
            id: TaskSpecId::new(),
            factory: Arc::new(move |context| Box::pin(factory(context))),
            tag: None,
            marker: PhantomData,
        }
    }

    pub fn id(&self) -> TaskSpecId {
        self.id
    }

    /// Adds a bounded selector/observability tag to this immutable definition.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Arc::from(tag.into()));
        self
    }
}

/// Context supplied to a typed reusable operation.
#[derive(Clone)]
pub struct WorkContext {
    core: Arc<ExecutionCore>,
    attempt: u32,
}

impl WorkContext {
    pub fn execution_id(&self) -> ExecutionId {
        self.core.execution_id
    }

    pub fn task_spec_id(&self) -> TaskSpecId {
        self.core.task_spec_id
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn is_cancelled(&self) -> bool {
        self.core.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.core.cancelled().await;
    }

    pub async fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            return Err(Cancelled);
        }
        let wake_at = Instant::now().checked_add(duration).ok_or(Cancelled)?;
        self.core.set_active_state(TaskState::Sleeping {
            attempt: self.attempt,
            wake_at,
        });
        let completed = if duration.is_zero() {
            tokio::task::yield_now().await;
            !self.is_cancelled()
        } else {
            tokio::select! {
                biased;
                _ = self.cancelled() => false,
                _ = tokio::time::sleep(duration) => !self.is_cancelled(),
            }
        };
        if completed {
            self.core.set_active_state(TaskState::Running {
                attempt: self.attempt,
            });
            Ok(())
        } else {
            Err(Cancelled)
        }
    }

    pub fn control(&self) -> TaskControlHandle {
        TaskControlHandle {
            core: Arc::clone(&self.core),
        }
    }

    pub(crate) fn for_attempt(&self, attempt: u32) -> Self {
        Self {
            core: Arc::clone(&self.core),
            attempt,
        }
    }

    pub(crate) fn begin_attempt(&self) {
        self.core.begin_attempt(self.attempt);
        self.core.set_active_state(TaskState::Running {
            attempt: self.attempt,
        });
    }

    pub(crate) fn mark_deadline(&self) -> CancelRequestOutcome {
        self.core.deadline()
    }

    pub(crate) async fn close_attempt_children_and_wait(&self) {
        self.core
            .close_attempt_children_and_wait(self.attempt)
            .await;
    }

    pub fn spawn_child<F, T, E>(&self, future: F) -> Result<ChildHandle<T, E>, SpawnChildError>
    where
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        let mut children = self
            .core
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !children.admission_open
            || !children.attempt_admission_open
            || children.current_attempt != self.attempt
        {
            return Err(SpawnChildError::ParentClosing);
        }
        let handle = spawn_future_internal(
            None,
            self.core.runtime.clone(),
            TaskSpecId::new(),
            None,
            future,
            PanicSource::Child,
            true,
        );
        children.controls.push(TrackedChild {
            attempt: self.attempt,
            control: handle.control(),
        });
        Ok(ChildHandle { inner: handle })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnChildError {
    ParentClosing,
}

impl fmt::Display for SpawnChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("parent execution is closing child admission")
    }
}

impl std::error::Error for SpawnChildError {}

pub struct ChildHandle<T, E> {
    inner: TaskHandle<T, E>,
}

impl<T, E> Deref for ChildHandle<T, E> {
    type Target = TaskHandle<T, E>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T, E> DerefMut for ChildHandle<T, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

const HISTORY_CAPACITY: usize = 1024;

pub(crate) struct ExecutionRegistry {
    active: Mutex<HashMap<ExecutionId, Arc<ExecutionCore>>>,
    history: Mutex<VecDeque<(ExecutionId, TaskExitSummary)>>,
}

pub(crate) struct ExecutorLifetime {
    registry: Arc<ExecutionRegistry>,
}

impl ExecutorLifetime {
    pub(crate) fn new(registry: Arc<ExecutionRegistry>) -> Arc<Self> {
        Arc::new(Self { registry })
    }
}

impl Drop for ExecutorLifetime {
    fn drop(&mut self) {
        // TaskHandle and TaskControlHandle deliberately do not own the
        // executor. Once the last Beaver/Lane owner disappears, submit a
        // best-effort cooperative stop to the registry snapshot.
        let controls = self.registry.active_controls();
        for control in controls {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                control.cancel(CancelReason::ExecutorShutdown)
            }));
        }
    }
}

impl ExecutionRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(HashMap::new()),
            history: Mutex::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
        })
    }

    fn register(&self, core: Arc<ExecutionCore>) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(core.execution_id, core);
    }

    fn finish(&self, execution_id: ExecutionId, summary: TaskExitSummary) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&execution_id);
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if history.len() == HISTORY_CAPACITY {
            history.pop_front();
        }
        history.push_back((execution_id, summary));
    }

    pub(crate) fn control(&self, execution_id: ExecutionId) -> Option<TaskControlHandle> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&execution_id)
            .map(|core| TaskControlHandle {
                core: Arc::clone(core),
            })
    }

    pub(crate) fn summary(&self, execution_id: ExecutionId) -> Option<TaskExitSummary> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find_map(|(id, summary)| (*id == execution_id).then(|| summary.clone()))
    }

    pub(crate) fn active_controls(&self) -> Vec<TaskControlHandle> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|core| TaskControlHandle {
                core: Arc::clone(core),
            })
            .collect()
    }

    fn select_controls(&self, selector: TaskSelector<'_>) -> Vec<TaskControlHandle> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut controls: Vec<_> = active
            .values()
            .filter(|core| match selector {
                TaskSelector::Execution(id) => core.execution_id == id,
                TaskSelector::Spec(id) => core.task_spec_id == id,
                TaskSelector::Tag(tag) => core.tag.as_deref() == Some(tag),
                TaskSelector::Lane(id) => {
                    *core
                        .lane_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        == Some(id)
                }
                TaskSelector::Scope(id) => {
                    *core
                        .scope_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        == Some(id)
                }
            })
            .map(|core| TaskControlHandle {
                core: Arc::clone(core),
            })
            .collect();
        controls.sort_unstable_by_key(TaskControlHandle::execution_id);
        controls
    }

    pub(crate) fn cancel_snapshot(
        &self,
        selector: TaskSelector<'_>,
        reason: CancelReason,
    ) -> BatchCancelReport {
        let controls = self.select_controls(selector);
        let records = controls
            .into_iter()
            .map(|control| BatchCancelRecord {
                execution_id: control.execution_id(),
                outcome: control.cancel(reason.clone()),
            })
            .collect();
        BatchCancelReport { records }
    }
}

/// Selector used to form a cancellation snapshot under the active registry
/// lock. Cancellation itself happens after the lock is released.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum TaskSelector<'a> {
    Execution(ExecutionId),
    Spec(TaskSpecId),
    Tag(&'a str),
    Lane(LaneId),
    Scope(ScopeId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchCancelRecord {
    pub execution_id: ExecutionId,
    pub outcome: CancelRequestOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchCancelReport {
    pub records: Vec<BatchCancelRecord>,
}

struct RunnerGuard<T, E> {
    core: Arc<ExecutionCore>,
    result: Arc<ResultCell<T, E>>,
    armed: bool,
}

impl<T, E> RunnerGuard<T, E> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<T, E> Drop for RunnerGuard<T, E> {
    fn drop(&mut self) {
        if self.armed {
            self.core.commit(
                &self.result,
                TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RunnerCancelled,
                },
            );
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    if let Ok(message) = payload.downcast::<String>() {
        return *message;
    }
    "panic (unknown payload)".to_string()
}

fn prepare_execution<T, E>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
) -> (TaskHandle<T, E>, RunnerGuard<T, E>) {
    let core = Arc::new(ExecutionCore::new(
        ExecutionId::new(),
        task_spec_id,
        runtime,
        registry.map(Arc::downgrade),
        tag,
    ));
    let result = Arc::new(ResultCell::new());
    if let Some(registry) = registry {
        registry.register(Arc::clone(&core));
    }
    let handle = TaskHandle {
        core: Arc::clone(&core),
        result: Arc::clone(&result),
    };
    let guard = RunnerGuard {
        core,
        result,
        armed: true,
    };
    (handle, guard)
}

pub(crate) struct ExecutionStart {
    core: Arc<ExecutionCore>,
    start: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl ExecutionStart {
    pub(crate) fn execution_id(&self) -> ExecutionId {
        self.core.execution_id
    }

    pub(crate) fn control(&self) -> TaskControlHandle {
        TaskControlHandle {
            core: Arc::clone(&self.core),
        }
    }

    pub(crate) fn set_lane_id(&self, lane_id: LaneId) {
        *self
            .core
            .lane_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lane_id);
    }

    pub(crate) fn install_cancel_hook(&self, hook: Box<dyn FnOnce() + Send + 'static>) {
        self.core.install_queued_cancel_hook(hook);
    }

    pub(crate) fn start(mut self) {
        self.core.clear_queued_cancel_hook();
        if let Some(start) = self.start.take() {
            start();
        }
    }
}

pub(crate) struct PreparedExecution<T, E> {
    pub(crate) handle: TaskHandle<T, E>,
    pub(crate) start: ExecutionStart,
}

pub(crate) fn prepare_spec<T, E>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    spec: TaskSpec<T, E>,
) -> BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let (handle, guard) =
        prepare_execution(Some(registry), runtime.clone(), spec.id, spec.tag.clone());
    let core = Arc::clone(&handle.core);
    let result = Arc::clone(&handle.result);
    let factory = Arc::clone(&spec.factory);
    let start_core = Arc::clone(&core);
    let runner = async move {
        let mut guard = guard;
        if !core.claim_start() {
            core.commit(
                &result,
                TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RunnerCancelled,
                },
            );
            guard.disarm();
            return;
        }
        let context = WorkContext {
            core: Arc::clone(&core),
            attempt: 1,
        };
        let operation = catch_unwind(AssertUnwindSafe(|| factory(context)));
        let exit = match operation {
            Ok(operation) => match core.runtime.spawn(operation).await {
                Ok(Ok(value)) => TaskExit::Completed(value),
                Ok(Err(error)) => TaskExit::Failed(TaskFailure::Operation { error }),
                Err(error) if error.is_panic() => TaskExit::Panicked {
                    source: PanicSource::WorkFuture,
                    message: panic_message(error.into_panic()),
                },
                Err(_) => TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RuntimeUnavailable,
                },
            },
            Err(payload) => TaskExit::Panicked {
                source: PanicSource::Factory,
                message: panic_message(payload),
            },
        };
        core.close_children_and_wait().await;
        core.commit(&result, exit);
        guard.disarm();
    };
    let start_runtime = runtime.clone();
    let start = Box::new(move || {
        // If the runtime rejects the task, dropping the unpolled runner invokes
        // its guard and commits ExecutorStopped.
        let _ = catch_unwind(AssertUnwindSafe(|| start_runtime.spawn(runner)));
    });
    Ok(PreparedExecution {
        handle,
        start: ExecutionStart {
            core: start_core,
            start: Some(start),
        },
    })
}

pub(crate) fn prepare_exit_future<T, E, F, Fut>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
    factory: F,
) -> PreparedExecution<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce(WorkContext) -> Fut + Send + 'static,
    Fut: Future<Output = TaskExit<T, E>> + Send + 'static,
{
    let (handle, guard) = prepare_execution(Some(registry), runtime.clone(), task_spec_id, tag);
    let core = Arc::clone(&handle.core);
    let result = Arc::clone(&handle.result);
    let start_core = Arc::clone(&core);
    let runner = async move {
        let mut guard = guard;
        if !core.claim_start() {
            core.commit(
                &result,
                TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RunnerCancelled,
                },
            );
            guard.disarm();
            return;
        }
        let context = WorkContext {
            core: Arc::clone(&core),
            attempt: 1,
        };
        let future = catch_unwind(AssertUnwindSafe(|| factory(context)));
        let exit = match future {
            Ok(future) => match core.runtime.spawn(future).await {
                Ok(exit) => exit,
                Err(error) if error.is_panic() => TaskExit::Panicked {
                    source: PanicSource::WorkFuture,
                    message: panic_message(error.into_panic()),
                },
                Err(_) => TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RuntimeUnavailable,
                },
            },
            Err(payload) => TaskExit::Panicked {
                source: PanicSource::Factory,
                message: panic_message(payload),
            },
        };
        core.close_children_and_wait().await;
        core.commit(&result, exit);
        guard.disarm();
    };
    let start_runtime = runtime.clone();
    let start = Box::new(move || {
        let _ = catch_unwind(AssertUnwindSafe(|| start_runtime.spawn(runner)));
    });
    PreparedExecution {
        handle,
        start: ExecutionStart {
            core: start_core,
            start: Some(start),
        },
    }
}

pub(crate) fn spawn_spec<T, E>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    spec: TaskSpec<T, E>,
) -> BeaverResult<TaskHandle<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let prepared = prepare_spec(registry, runtime, spec)?;
    let PreparedExecution { handle, start } = prepared;
    start.start();
    Ok(handle)
}

pub(crate) fn spawn_future<T, E, F>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    future: F,
) -> BeaverResult<TaskHandle<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    Ok(spawn_future_internal(
        Some(registry),
        runtime,
        TaskSpecId::new(),
        None,
        future,
        PanicSource::WorkFuture,
        false,
    ))
}

fn spawn_future_internal<T, E, F>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
    future: F,
    panic_source: PanicSource,
    cancel_drops_future: bool,
) -> TaskHandle<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let prepared = prepare_future_internal(
        registry,
        runtime,
        task_spec_id,
        tag,
        future,
        panic_source,
        cancel_drops_future,
    );
    let PreparedExecution { handle, start } = prepared;
    start.start();
    handle
}

pub(crate) fn prepare_future<T, E, F>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    future: F,
) -> PreparedExecution<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    prepare_future_internal(
        Some(registry),
        runtime,
        TaskSpecId::new(),
        None,
        future,
        PanicSource::WorkFuture,
        false,
    )
}

fn prepare_future_internal<T, E, F>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
    future: F,
    panic_source: PanicSource,
    cancel_drops_future: bool,
) -> PreparedExecution<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let (handle, guard) = prepare_execution(registry, runtime.clone(), task_spec_id, tag);
    let core = Arc::clone(&handle.core);
    let result = Arc::clone(&handle.result);
    let start_core = Arc::clone(&core);
    let runner = async move {
        let mut guard = guard;
        if !core.claim_start() {
            core.commit(
                &result,
                TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RunnerCancelled,
                },
            );
            guard.disarm();
            return;
        }
        let mut body = core.runtime.spawn(future);
        let joined = if cancel_drops_future {
            tokio::select! {
                biased;
                _ = core.cancelled() => {
                    body.abort();
                    let _ = (&mut body).await;
                    None
                },
                result = &mut body => Some(result),
            }
        } else {
            Some(body.await)
        };
        let exit = match joined {
            None => TaskExit::ExecutorStopped {
                reason: ExecutorStopReason::RunnerCancelled,
            },
            Some(Ok(Ok(value))) => TaskExit::Completed(value),
            Some(Ok(Err(error))) => TaskExit::Failed(TaskFailure::Operation { error }),
            Some(Err(error)) if error.is_panic() => TaskExit::Panicked {
                source: panic_source,
                message: panic_message(error.into_panic()),
            },
            Some(Err(_)) => TaskExit::ExecutorStopped {
                reason: ExecutorStopReason::RuntimeUnavailable,
            },
        };
        core.close_children_and_wait().await;
        core.commit(&result, exit);
        guard.disarm();
    };
    let start_runtime = runtime.clone();
    let start = Box::new(move || {
        let _ = catch_unwind(AssertUnwindSafe(|| start_runtime.spawn(runner)));
    });
    PreparedExecution {
        handle,
        start: ExecutionStart {
            core: start_core,
            start: Some(start),
        },
    }
}
