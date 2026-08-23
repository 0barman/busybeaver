use crate::error::BeaverResult;
use crate::ids::{ExecutionId, LaneId, ScopeId, TaskSpecId};
use crate::observation::{
    EventStream, EventSubscribeError, ResourceLimits, TaskEvent, TerminalRecord,
};
use crate::recurring::RecurringFailure;
use crate::retry::{RetryFailure, RetryPolicyStage};
use crate::service::ServiceFailure;
use crate::shutdown::{CleanupOutcome, CleanupPhase, CleanupProgress};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

tokio::task_local! {
    static CURRENT_EXECUTION_PATH: Arc<[ExecutionId]>;
}

pub(crate) fn would_join(execution_id: ExecutionId) -> bool {
    CURRENT_EXECUTION_PATH
        .try_with(|path| path.contains(&execution_id))
        .is_ok_and(|would_join| would_join)
}

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
    Schedule,
    RestartPolicy,
    ServiceBody,
    ShutdownHook,
}

/// Lifecycle class used by drain shutdown to distinguish finite work from
/// executions that require an explicit stop request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionKind {
    Finite,
    Recurring,
    Service,
}

/// Whether the SDK may drop a tracked async body after cooperative shutdown
/// has been requested. This never applies to OS threads, blocking syscalls or
/// untracked children.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AbortPolicy {
    #[default]
    CooperativeOnly,
    Allowed,
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
    Recurring(RecurringFailure<E>),
    Service(ServiceFailure<E>),
    DeadlineExceeded {
        last_error: Option<E>,
    },
    PolicyPanicked {
        stage: RetryPolicyStage,
        last_error: Option<E>,
    },
    ChildFailed,
    TimerUnavailable,
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
    pub kind: ExecutionKind,
    pub lane_id: Option<LaneId>,
    pub scope_id: Option<ScopeId>,
    pub scope_generation: Option<u64>,
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
    scope_generation: Mutex<Option<u64>>,
    kind: Mutex<ExecutionKind>,
    resume_epoch: watch::Sender<u64>,
    abort_policy: Mutex<AbortPolicy>,
    abort_handle: Mutex<Option<AbortHandle>>,
    forced_cancellation_requested: AtomicBool,
    registered: AtomicBool,
    execution_path: Arc<[ExecutionId]>,
    cleanup: Mutex<CleanupProgress>,
    queued_cancel_hook: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
    #[cfg(all(test, not(feature = "tracing")))]
    snapshot_calls: AtomicUsize,
}

impl ExecutionCore {
    fn new(
        execution_id: ExecutionId,
        task_spec_id: TaskSpecId,
        runtime: Handle,
        registry: Option<Weak<ExecutionRegistry>>,
        tag: Option<Arc<str>>,
        ancestors: Arc<[ExecutionId]>,
    ) -> Self {
        let (cancellation, _) = watch::channel(false);
        let (terminal, _) = watch::channel(None);
        let (resume_epoch, _) = watch::channel(0);
        let mut execution_path = ancestors.to_vec();
        execution_path.push(execution_id);
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
            scope_generation: Mutex::new(None),
            kind: Mutex::new(ExecutionKind::Finite),
            resume_epoch,
            abort_policy: Mutex::new(AbortPolicy::CooperativeOnly),
            abort_handle: Mutex::new(None),
            forced_cancellation_requested: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            execution_path: execution_path.into(),
            cleanup: Mutex::new(CleanupProgress::NotRequired),
            queued_cancel_hook: Mutex::new(None),
            #[cfg(all(test, not(feature = "tracing")))]
            snapshot_calls: AtomicUsize::new(0),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CoreState> {
        self.state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
    }

    fn state(&self) -> TaskState {
        self.lock_state().public_state.clone()
    }

    fn stop_cause(&self) -> Option<StopCauseSummary> {
        self.lock_state().stop_cause.clone()
    }

    #[cfg(all(test, not(feature = "tracing")))]
    fn snapshot_calls_for_test(&self) -> usize {
        self.snapshot_calls.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> TaskSnapshot {
        #[cfg(all(test, not(feature = "tracing")))]
        self.snapshot_calls.fetch_add(1, Ordering::Relaxed);
        let state = self.lock_state();
        TaskSnapshot {
            execution_id: self.execution_id,
            task_spec_id: self.task_spec_id,
            state: state.public_state.clone(),
            stop_cause: state.stop_cause.clone(),
            final_exit: state.terminal.clone(),
            kind: *self
                .kind
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard),
            lane_id: *self
                .lane_id
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard),
            scope_id: *self
                .scope_id
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard),
            scope_generation: *self
                .scope_generation
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard),
        }
    }

    fn kind(&self) -> ExecutionKind {
        *self
            .kind
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
    }

    fn set_kind(&self, kind: ExecutionKind) {
        *self
            .kind
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = kind;
    }

    fn set_abort_policy(&self, policy: AbortPolicy) {
        *self
            .abort_policy
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = policy;
    }

    fn cleanup_progress(&self) -> CleanupProgress {
        self.cleanup
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .clone()
    }

    fn set_cleanup_pending(&self, phase: CleanupPhase) {
        let mut cleanup = self
            .cleanup
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if !matches!(
            *cleanup,
            CleanupProgress::Finished(CleanupOutcome::Failed(_))
                | CleanupProgress::Finished(CleanupOutcome::ForcedCancelled { .. })
        ) {
            *cleanup = CleanupProgress::Pending { phase };
        }
    }

    fn finish_cleanup(&self, outcome: CleanupOutcome) {
        let mut cleanup = self
            .cleanup
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        let preserve_failure = matches!(
            *cleanup,
            CleanupProgress::Finished(CleanupOutcome::Failed(_))
                | CleanupProgress::Finished(CleanupOutcome::ForcedCancelled { .. })
        ) && matches!(
            outcome,
            CleanupOutcome::Completed | CleanupOutcome::NotRequired
        );
        if !preserve_failure {
            *cleanup = CleanupProgress::Finished(outcome);
        }
    }

    fn register(self: &Arc<Self>) -> BeaverResult<()> {
        let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) else {
            return Ok(());
        };
        if self.registered.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(error) = registry.register(Arc::clone(self)) {
            self.registered.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    fn install_abort_handle(&self, handle: AbortHandle) {
        *self
            .abort_handle
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Some(handle);
    }

    fn clear_abort_handle(&self) {
        self.abort_handle
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .take();
    }

    fn request_forced_cancellation(
        &self,
    ) -> Result<ForcedCancellationOutcome, ForcedCancellationError> {
        if self.state().is_terminal() {
            return Ok(ForcedCancellationOutcome::AlreadyTerminal);
        }
        if !matches!(
            *self
                .abort_policy
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard),
            AbortPolicy::Allowed
        ) {
            return Err(ForcedCancellationError::NotAllowed);
        }
        let Some(handle) = self
            .abort_handle
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .clone()
        else {
            return Err(ForcedCancellationError::NoTrackedFuture);
        };
        if self
            .forced_cancellation_requested
            .swap(true, Ordering::AcqRel)
        {
            return Ok(ForcedCancellationOutcome::AlreadyRequested);
        }
        handle.abort();
        Ok(ForcedCancellationOutcome::Requested)
    }

    fn forced_exit<T, E>(&self) -> TaskExit<T, E> {
        TaskExit::Aborted {
            preceding_stop: self.lock_state().stop_cause.clone(),
        }
    }

    fn notify_resumed(&self) {
        let current = *self.resume_epoch.borrow();
        let next = current.checked_add(1).map_or_else(
            || {
                crate::internal::log_internal_error(
                    "BB-RESUME-EPOCH-OVERFLOW",
                    "resume notification generation wrapped after reaching u64::MAX",
                );
                0
            },
            |value| value,
        );
        self.resume_epoch.send_replace(next);
    }

    fn set_active_state(&self, next: TaskState) {
        let mut state = self.lock_state();
        if state.terminal.is_none() && state.stop_cause.is_none() {
            state.public_state = next;
            drop(state);
            self.emit_state();
        }
    }

    fn emit_state(&self) {
        if !self.registered.load(Ordering::Acquire) {
            return;
        }
        if let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) {
            if registry.should_capture_event_snapshot() {
                registry.emit(TaskEvent::StateChanged(self.snapshot()));
            }
        }
    }

    fn claim_start(&self) -> bool {
        let mut state = self.lock_state();
        if state.terminal.is_some() || state.stop_cause.is_some() {
            return false;
        }
        state.public_state = TaskState::Running { attempt: 1 };
        drop(state);
        self.emit_state();
        true
    }

    fn install_queued_cancel_hook(&self, hook: Box<dyn FnOnce() + Send + 'static>) {
        *self
            .queued_cancel_hook
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Some(hook);
    }

    fn clear_queued_cancel_hook(&self) {
        self.queued_cancel_hook
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
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
                .map_or_else(crate::internal::recover_poison, |guard| guard);
            let queued_cancel_hook = self
                .queued_cancel_hook
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard)
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
        self.emit_state();
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

            let forced_exit = matches!(unused_exit.as_ref(), Some(TaskExit::Aborted { .. }));
            let selected = match state.stop_cause.clone() {
                _ if forced_exit => match unused_exit.take() {
                    Some(exit) => exit,
                    None => {
                        crate::internal::log_internal_error(
                            "BB-TERMINAL-FORCED-EXIT-MISSING",
                            "forced terminal selection lost its prepared exit",
                        );
                        TaskExit::ExecutorStopped {
                            reason: ExecutorStopReason::InternalInvariantViolation,
                        }
                    }
                },
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
                            crate::internal::log_internal_error(
                                "BB-TERMINAL-DEADLINE-EXIT-MISSING",
                                "deadline terminal selection lost its prepared exit",
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
                        crate::internal::log_internal_error(
                            "BB-TERMINAL-EXIT-MISSING",
                            "terminal selection lost its prepared exit",
                        );
                        TaskExit::ExecutorStopped {
                            reason: ExecutorStopReason::InternalInvariantViolation,
                        }
                    }
                },
            };
            let summary = result.store(selected);
            state.public_state = summary.state();
            state.terminal = Some(summary.clone());
            summary
        };

        // A business value that lost to an earlier stop cause is dropped after
        // releasing the execution lock.
        drop(unused_exit);
        self.terminal.send_replace(Some(summary.clone()));
        if self.registered.load(Ordering::Acquire) {
            if let Some(registry) = self.registry.as_ref().and_then(Weak::upgrade) {
                registry.finish(self.execution_id, summary);
            }
        }
        true
    }

    async fn close_children_and_wait(&self) {
        let controls = {
            let mut children = self
                .children
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard);
            children.admission_open = false;
            children.attempt_admission_open = false;
            std::mem::take(&mut children.controls)
                .into_iter()
                .map(|child| child.control)
                .collect::<Vec<_>>()
        };
        let had_controls = !controls.is_empty();
        if had_controls {
            self.set_cleanup_pending(CleanupPhase::TrackedChildren);
        }
        for child in &controls {
            child.cancel(CancelReason::ExecutorShutdown);
        }
        for child in controls {
            child.wait().await;
        }
        if had_controls {
            self.finish_cleanup(CleanupOutcome::Completed);
        }
    }

    fn begin_attempt(&self, attempt: u32) {
        let mut children = self
            .children
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        children.current_attempt = attempt;
        children.attempt_admission_open = children.admission_open;
    }

    async fn close_attempt_children_and_wait(&self, attempt: u32) {
        let controls = {
            let mut children = self
                .children
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard);
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
        let had_controls = !controls.is_empty();
        if had_controls {
            self.set_cleanup_pending(CleanupPhase::TrackedChildren);
        }
        for child in &controls {
            child.cancel(CancelReason::ExecutorShutdown);
        }
        for child in controls {
            child.wait().await;
        }
        if had_controls {
            self.finish_cleanup(CleanupOutcome::Completed);
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

    fn store(&self, exit: TaskExit<T, E>) -> TaskExitSummary {
        let mut value = self
            .value
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if let Some(existing) = value.as_ref() {
            crate::internal::log_internal_error(
                "BB-RESULT-DUPLICATE-STORE",
                "attempted to overwrite an execution's terminal result",
            );
            return existing.summary();
        }
        let summary = exit.summary();
        *value = Some(exit);
        summary
    }

    fn take(&self) -> Option<TaskExit<T, E>> {
        self.value
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ForcedCancellationOutcome {
    Requested,
    AlreadyRequested,
    AlreadyTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ForcedCancellationError {
    NotAllowed,
    NoTrackedFuture,
}

impl fmt::Display for ForcedCancellationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed => {
                formatter.write_str("execution did not opt into forced cancellation")
            }
            Self::NoTrackedFuture => {
                formatter.write_str("execution has no active tracked async body")
            }
        }
    }
}

impl std::error::Error for ForcedCancellationError {}

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

/// Error returned by a checked wait that would create a direct structured
/// concurrency cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionWaitError {
    WouldJoin {
        current: ExecutionId,
        target: ExecutionId,
    },
}

impl fmt::Display for ExecutionWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldJoin { current, target } => write!(
                formatter,
                "execution {current} cannot wait for itself or ancestor execution {target}"
            ),
        }
    }
}

impl std::error::Error for ExecutionWaitError {}

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

    pub async fn wait_checked(&self) -> Result<TaskExitSummary, ExecutionWaitError> {
        self.control().wait_checked().await
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

    pub fn request_forced_cancellation(
        &self,
    ) -> Result<ForcedCancellationOutcome, ForcedCancellationError> {
        self.core.request_forced_cancellation()
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
    pub(crate) fn register(&self) -> BeaverResult<()> {
        self.core.register()
    }

    pub(crate) fn cleanup_progress(&self) -> CleanupProgress {
        self.core.cleanup_progress()
    }

    pub fn execution_id(&self) -> ExecutionId {
        self.core.execution_id
    }

    pub fn state(&self) -> TaskState {
        self.core.state()
    }

    pub fn kind(&self) -> ExecutionKind {
        self.core.kind()
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

    pub async fn wait_checked(&self) -> Result<TaskExitSummary, ExecutionWaitError> {
        if would_join(self.execution_id()) {
            let current = CURRENT_EXECUTION_PATH
                .try_with(|path| path.last().copied())
                .ok()
                .flatten()
                .map_or_else(|| self.execution_id(), |current| current);
            return Err(ExecutionWaitError::WouldJoin {
                current,
                target: self.execution_id(),
            });
        }
        Ok(self.wait().await)
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

    pub fn request_forced_cancellation(
        &self,
    ) -> Result<ForcedCancellationOutcome, ForcedCancellationError> {
        self.core.request_forced_cancellation()
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
    abort_policy: AbortPolicy,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for TaskSpec<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            factory: Arc::clone(&self.factory),
            tag: self.tag.clone(),
            abort_policy: self.abort_policy,
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
            abort_policy: AbortPolicy::CooperativeOnly,
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

    pub fn abort_policy(mut self, policy: AbortPolicy) -> Self {
        self.abort_policy = policy;
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

    pub fn lane_id(&self) -> Option<LaneId> {
        *self
            .core
            .lane_id
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
    }

    pub fn scope_id(&self) -> Option<ScopeId> {
        *self
            .core
            .scope_id
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
    }

    pub fn scope_generation(&self) -> Option<u64> {
        *self
            .core
            .scope_generation
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
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

    pub(crate) fn stop_cause(&self) -> Option<StopCauseSummary> {
        self.core.stop_cause()
    }

    /// Returns the current explicit resume notification generation.
    pub fn resume_epoch(&self) -> u64 {
        *self.core.resume_epoch.borrow()
    }

    /// Waits until [`Beaver::notify_resumed`](crate::Beaver::notify_resumed)
    /// publishes a later resume generation or this execution is cancelled.
    pub async fn resumed_after(&self, observed: u64) -> Result<u64, Cancelled> {
        let mut resume = self.core.resume_epoch.subscribe();
        loop {
            let current = *resume.borrow();
            if current != observed {
                return Ok(current);
            }
            tokio::select! {
                biased;
                _ = self.cancelled() => return Err(Cancelled),
                changed = resume.changed() => {
                    if changed.is_err() {
                        return Err(Cancelled);
                    }
                }
            }
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

    pub(crate) fn set_cleanup_pending(&self, phase: CleanupPhase) {
        self.core.set_cleanup_pending(phase);
    }

    pub(crate) fn finish_cleanup(&self, outcome: CleanupOutcome) {
        self.core.finish_cleanup(outcome);
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
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if !children.admission_open
            || !children.attempt_admission_open
            || (children.current_attempt != self.attempt)
        {
            return Err(SpawnChildError::ParentClosing);
        }
        let maximum = self
            .core
            .registry
            .as_ref()
            .and_then(Weak::upgrade)
            .map_or(1_024, |registry| {
                registry.limits().max_children_per_execution
            });
        if children.controls.len() >= maximum {
            return Err(SpawnChildError::LimitReached { maximum });
        }
        let handle = spawn_future_internal(
            None,
            self.core.runtime.clone(),
            future,
            FutureExecutionConfig {
                task_spec_id: TaskSpecId::new(),
                tag: None,
                panic_source: PanicSource::Child,
                cancel_drops_future: true,
                ancestors: Arc::clone(&self.core.execution_path),
            },
        )
        .map_err(|_| SpawnChildError::ExecutorUnavailable)?;
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
    ExecutorUnavailable,
    LimitReached { maximum: usize },
}

impl fmt::Display for SpawnChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentClosing => {
                formatter.write_str("parent execution is closing child admission")
            }
            Self::ExecutorUnavailable => formatter.write_str("parent runtime is unavailable"),
            Self::LimitReached { maximum } => {
                write!(formatter, "tracked child limit reached ({maximum})")
            }
        }
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

pub(crate) struct ExecutionRegistry {
    active: Mutex<HashMap<ExecutionId, Arc<ExecutionCore>>>,
    history: Mutex<VecDeque<(ExecutionId, TaskExitSummary, Instant)>>,
    limits: ResourceLimits,
    events: broadcast::Sender<TaskEvent>,
    subscribers: Arc<AtomicUsize>,
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
    pub(crate) fn new_with_limits(limits: ResourceLimits) -> Arc<Self> {
        let (events, _) = broadcast::channel(limits.event_capacity);
        Arc::new(Self {
            active: Mutex::new(HashMap::new()),
            history: Mutex::new(VecDeque::with_capacity(limits.terminal_history_capacity)),
            limits,
            events,
            subscribers: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn limits(&self) -> &ResourceLimits {
        &self.limits
    }

    fn register(&self, core: Arc<ExecutionCore>) -> BeaverResult<()> {
        if core
            .tag
            .as_ref()
            .is_some_and(|tag| tag.len() > self.limits.max_tag_bytes)
        {
            return Err(crate::BeaverError::ResourceLimitExceeded {
                resource: "tag bytes",
            });
        }
        let mut active = self
            .active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if active.len() >= self.limits.max_active_executions {
            return Err(crate::BeaverError::ResourceLimitExceeded {
                resource: "active executions",
            });
        }
        active.insert(core.execution_id, Arc::clone(&core));
        drop(active);
        if self.should_capture_event_snapshot() {
            self.emit(TaskEvent::Admitted(core.snapshot()));
        }
        Ok(())
    }

    fn finish(&self, execution_id: ExecutionId, summary: TaskExitSummary) {
        self.active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .remove(&execution_id);
        let mut history = self
            .history
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        self.evict_expired_locked(&mut history);
        if self.limits.terminal_history_capacity == 0 {
            self.emit(TaskEvent::Terminal {
                execution_id,
                summary,
            });
            return;
        }
        if history.len() == self.limits.terminal_history_capacity {
            history.pop_front();
        }
        history.push_back((execution_id, summary.clone(), Instant::now()));
        drop(history);
        self.emit(TaskEvent::Terminal {
            execution_id,
            summary,
        });
    }

    fn evict_expired_locked(
        &self,
        history: &mut VecDeque<(ExecutionId, TaskExitSummary, Instant)>,
    ) {
        let Some(ttl) = self.limits.terminal_history_ttl else {
            return;
        };
        let now = Instant::now();
        while history
            .front()
            .is_some_and(|(_, _, inserted)| now.duration_since(*inserted) >= ttl)
        {
            history.pop_front();
        }
    }

    pub(crate) fn emit(&self, event: TaskEvent) {
        #[cfg(not(feature = "tracing"))]
        if self.subscriber_count() == 0 {
            return;
        }
        #[cfg(feature = "tracing")]
        match &event {
            TaskEvent::Admitted(snapshot) => tracing::trace!(
                execution_id = %snapshot.execution_id,
                task_spec_id = %snapshot.task_spec_id,
                "busybeaver execution admitted"
            ),
            TaskEvent::StateChanged(snapshot) => tracing::trace!(
                execution_id = %snapshot.execution_id,
                state = ?snapshot.state,
                "busybeaver execution state changed"
            ),
            TaskEvent::Terminal {
                execution_id,
                summary,
            } => tracing::trace!(
                execution_id = %execution_id,
                summary = ?summary,
                "busybeaver execution terminal"
            ),
        }
        if self.subscriber_count() > 0 {
            let _ = self.events.send(event);
        }
    }

    fn should_capture_event_snapshot(&self) -> bool {
        if self.subscriber_count() > 0 {
            return true;
        }
        #[cfg(feature = "tracing")]
        {
            true
        }
        #[cfg(not(feature = "tracing"))]
        {
            false
        }
    }

    pub(crate) fn subscribe(&self) -> Result<EventStream, EventSubscribeError> {
        let mut current = self.subscribers.load(Ordering::Acquire);
        loop {
            if current >= self.limits.max_event_subscribers {
                return Err(EventSubscribeError::SubscriberLimitReached);
            }
            let Some(next) = current.checked_add(1) else {
                return Err(EventSubscribeError::SubscriberLimitReached);
            };
            match self.subscribers.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(EventStream {
                        receiver: self.events.subscribe(),
                        subscribers: Arc::clone(&self.subscribers),
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::Acquire)
    }

    pub(crate) fn snapshots(&self) -> Vec<TaskSnapshot> {
        let cores = self
            .active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = cores
            .into_iter()
            .map(|core| core.snapshot())
            .collect::<Vec<_>>();
        snapshots.sort_unstable_by_key(|snapshot| snapshot.execution_id);
        snapshots
    }

    pub(crate) fn history(&self) -> Vec<TerminalRecord> {
        let mut history = self
            .history
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        self.evict_expired_locked(&mut history);
        history
            .iter()
            .map(|(execution_id, summary, _)| TerminalRecord {
                execution_id: *execution_id,
                summary: summary.clone(),
            })
            .collect()
    }

    pub(crate) fn control(&self, execution_id: ExecutionId) -> Option<TaskControlHandle> {
        self.active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .get(&execution_id)
            .map(|core| TaskControlHandle {
                core: Arc::clone(core),
            })
    }

    pub(crate) fn summary(&self, execution_id: ExecutionId) -> Option<TaskExitSummary> {
        let mut history = self
            .history
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        self.evict_expired_locked(&mut history);
        history
            .iter()
            .rev()
            .find_map(|(id, summary, _)| (*id == execution_id).then(|| summary.clone()))
    }

    pub(crate) fn active_controls(&self) -> Vec<TaskControlHandle> {
        self.active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .values()
            .map(|core| TaskControlHandle {
                core: Arc::clone(core),
            })
            .collect()
    }

    pub(crate) fn notify_resumed(&self) -> usize {
        let cores = self
            .active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for core in &cores {
            core.notify_resumed();
        }
        cores.len()
    }

    fn select_controls(&self, selector: TaskSelector<'_>) -> Vec<TaskControlHandle> {
        let active = self
            .active
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
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
                        .map_or_else(crate::internal::recover_poison, |guard| guard)
                        == Some(id)
                }
                TaskSelector::Scope(id) => {
                    *core
                        .scope_id
                        .lock()
                        .map_or_else(crate::internal::recover_poison, |guard| guard)
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

pub(crate) fn is_timer_unavailable_message(message: &str) -> bool {
    message.contains("timers are disabled")
        || message.contains("no reactor running")
        || message.contains("must be called from the context of a Tokio")
}

fn panic_exit<T, E>(source: PanicSource, payload: Box<dyn std::any::Any + Send>) -> TaskExit<T, E> {
    let message = panic_message(payload);
    if is_timer_unavailable_message(&message) {
        TaskExit::Failed(TaskFailure::TimerUnavailable)
    } else {
        TaskExit::Panicked { source, message }
    }
}

fn join_panic_exit<T, E>(
    source: PanicSource,
    error: tokio::task::JoinError,
    code: &'static str,
) -> TaskExit<T, E> {
    match crate::internal::take_join_panic(error, code) {
        Some(payload) => panic_exit(source, payload),
        None => TaskExit::ExecutorStopped {
            reason: ExecutorStopReason::InternalInvariantViolation,
        },
    }
}

fn prepare_execution<T, E>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
    ancestors: Arc<[ExecutionId]>,
) -> BeaverResult<(TaskHandle<T, E>, RunnerGuard<T, E>)> {
    let core = Arc::new(ExecutionCore::new(
        ExecutionId::new(),
        task_spec_id,
        runtime,
        registry.map(Arc::downgrade),
        tag,
        ancestors,
    ));
    let result = Arc::new(ResultCell::new());
    let handle = TaskHandle {
        core: Arc::clone(&core),
        result: Arc::clone(&result),
    };
    let guard = RunnerGuard {
        core,
        result,
        armed: true,
    };
    Ok((handle, guard))
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

    pub(crate) fn register(&self) -> BeaverResult<()> {
        self.core.register()
    }

    pub(crate) fn set_lane_id(&self, lane_id: LaneId) {
        *self
            .core
            .lane_id
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Some(lane_id);
    }

    pub(crate) fn set_scope(&self, scope_id: ScopeId, generation: u64) {
        *self
            .core
            .scope_id
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Some(scope_id);
        *self
            .core
            .scope_generation
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard) = Some(generation);
    }

    pub(crate) fn set_kind(&self, kind: ExecutionKind) {
        self.core.set_kind(kind);
    }

    pub(crate) fn set_abort_policy(&self, policy: AbortPolicy) {
        self.core.set_abort_policy(policy);
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
    let (handle, guard) = prepare_execution(
        Some(registry),
        runtime.clone(),
        spec.id,
        spec.tag.clone(),
        Arc::from([]),
    )?;
    let core = Arc::clone(&handle.core);
    core.set_abort_policy(spec.abort_policy);
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
            Ok(operation) => {
                let execution_path = Arc::clone(&core.execution_path);
                let body = core
                    .runtime
                    .spawn(CURRENT_EXECUTION_PATH.scope(execution_path, operation));
                core.install_abort_handle(body.abort_handle());
                let joined = body.await;
                core.clear_abort_handle();
                match joined {
                    Ok(Ok(value)) => TaskExit::Completed(value),
                    Ok(Err(error)) => TaskExit::Failed(TaskFailure::Operation { error }),
                    Err(error) if error.is_panic() => join_panic_exit(
                        PanicSource::WorkFuture,
                        error,
                        "BB-WORK-JOIN-PANIC-MISCLASSIFIED",
                    ),
                    Err(error)
                        if error.is_cancelled()
                            && core.forced_cancellation_requested.load(Ordering::Acquire) =>
                    {
                        core.forced_exit()
                    }
                    Err(_) => TaskExit::ExecutorStopped {
                        reason: ExecutorStopReason::RuntimeUnavailable,
                    },
                }
            }
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
) -> BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce(WorkContext) -> Fut + Send + 'static,
    Fut: Future<Output = TaskExit<T, E>> + Send + 'static,
{
    let (handle, guard) = prepare_execution(
        Some(registry),
        runtime.clone(),
        task_spec_id,
        tag,
        Arc::from([]),
    )?;
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
            Ok(future) => {
                let execution_path = Arc::clone(&core.execution_path);
                let body = core
                    .runtime
                    .spawn(CURRENT_EXECUTION_PATH.scope(execution_path, future));
                core.install_abort_handle(body.abort_handle());
                let joined = body.await;
                core.clear_abort_handle();
                match joined {
                    Ok(exit) => exit,
                    Err(error) if error.is_panic() => join_panic_exit(
                        PanicSource::WorkFuture,
                        error,
                        "BB-EXIT-JOIN-PANIC-MISCLASSIFIED",
                    ),
                    Err(error)
                        if error.is_cancelled()
                            && core.forced_cancellation_requested.load(Ordering::Acquire) =>
                    {
                        core.forced_exit()
                    }
                    Err(_) => TaskExit::ExecutorStopped {
                        reason: ExecutorStopReason::RuntimeUnavailable,
                    },
                }
            }
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
    Ok(PreparedExecution {
        handle,
        start: ExecutionStart {
            core: start_core,
            start: Some(start),
        },
    })
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
    start.register()?;
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
    spawn_future_internal(
        Some(registry),
        runtime,
        future,
        FutureExecutionConfig::root(PanicSource::WorkFuture),
    )
}

struct FutureExecutionConfig {
    task_spec_id: TaskSpecId,
    tag: Option<Arc<str>>,
    panic_source: PanicSource,
    cancel_drops_future: bool,
    ancestors: Arc<[ExecutionId]>,
}

impl FutureExecutionConfig {
    fn root(panic_source: PanicSource) -> Self {
        Self {
            task_spec_id: TaskSpecId::new(),
            tag: None,
            panic_source,
            cancel_drops_future: false,
            ancestors: Arc::from([]),
        }
    }
}

fn spawn_future_internal<T, E, F>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    future: F,
    config: FutureExecutionConfig,
) -> BeaverResult<TaskHandle<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let prepared = prepare_future_internal(registry, runtime, future, config)?;
    let PreparedExecution { handle, start } = prepared;
    start.register()?;
    start.start();
    Ok(handle)
}

pub(crate) fn prepare_future<T, E, F>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    future: F,
) -> BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    prepare_future_internal(
        Some(registry),
        runtime,
        future,
        FutureExecutionConfig::root(PanicSource::WorkFuture),
    )
}

fn prepare_future_internal<T, E, F>(
    registry: Option<&Arc<ExecutionRegistry>>,
    runtime: Handle,
    future: F,
    config: FutureExecutionConfig,
) -> BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
{
    let (handle, guard) = prepare_execution(
        registry,
        runtime.clone(),
        config.task_spec_id,
        config.tag,
        config.ancestors,
    )?;
    let panic_source = config.panic_source;
    let cancel_drops_future = config.cancel_drops_future;
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
        let execution_path = Arc::clone(&core.execution_path);
        let mut body = core
            .runtime
            .spawn(CURRENT_EXECUTION_PATH.scope(execution_path, future));
        core.install_abort_handle(body.abort_handle());
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
        core.clear_abort_handle();
        let exit = match joined {
            None => TaskExit::ExecutorStopped {
                reason: ExecutorStopReason::RunnerCancelled,
            },
            Some(Ok(Ok(value))) => TaskExit::Completed(value),
            Some(Ok(Err(error))) => TaskExit::Failed(TaskFailure::Operation { error }),
            Some(Err(error)) if error.is_panic() => {
                join_panic_exit(panic_source, error, "BB-FUTURE-JOIN-PANIC-MISCLASSIFIED")
            }
            Some(Err(error))
                if error.is_cancelled()
                    && core.forced_cancellation_requested.load(Ordering::Acquire) =>
            {
                core.forced_exit()
            }
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
    Ok(PreparedExecution {
        handle,
        start: ExecutionStart {
            core: start_core,
            start: Some(start),
        },
    })
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
