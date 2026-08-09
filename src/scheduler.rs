use crate::observe::{
    EventData, EventReceiver, MetricsHook, Observability, SchedulerSnapshot, SchedulerSnapshotData,
    TaskEventKind,
};
use crate::retry::RetryPolicy;
use crate::schedule::{
    FirstRun, RetryExhaustedAction, Schedule, ScheduleCursor, ScheduleError, TaskControl,
};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinHandle};
use uuid::Uuid;

const OPEN: u8 = 0;
const CLOSING: u8 = 1;
const TERMINATED: u8 = 2;

tokio::task_local! {
    static CURRENT_RUN_ID: TaskRunId;
}

fn current_run_id() -> Option<TaskRunId> {
    CURRENT_RUN_ID.try_with(|run_id| *run_id).ok()
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
/// Stable identity shared by every instantiation of one reusable job.
pub struct JobId(Uuid);

impl JobId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the underlying UUID.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
/// Unique identity assigned to one accepted submission.
pub struct TaskRunId(Uuid);

impl TaskRunId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the underlying UUID.
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for TaskRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identifies the first accepted source of cooperative cancellation.
pub enum CancelReason {
    /// Explicit cancellation by a caller.
    User,
    /// Scheduler-wide shutdown.
    Shutdown,
    /// Shutdown of the owning task group.
    GroupShutdown,
    /// Closure of the run's lane.
    LaneClosed,
    /// Replacement by a newer generation of the same key.
    Replaced,
    /// Cancellation requested by a time-bound operation.
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Observable lifecycle state of one task run.
pub enum TaskState {
    /// Accepted and waiting to enter execution.
    Queued,
    /// Waiting for global or lane execution capacity.
    WaitingForPermit,
    /// Polling user work.
    Running,
    /// Waiting for a retry backoff.
    WaitingForRetry,
    /// Waiting for the next scheduled invocation.
    WaitingForSchedule,
    /// Cooperatively paused outside user work.
    Paused,
    /// Cancellation is requested but no terminal has been published.
    CancelRequested,
    /// User work returned a successful value.
    Completed,
    /// User work returned a non-retried error.
    Failed,
    /// Every permitted retry attempt failed.
    RetriesExhausted,
    /// Cooperative cancellation won terminal publication.
    Cancelled,
    /// An attempt or total elapsed deadline expired.
    TimedOut,
    /// Runtime schedule calculation failed.
    ScheduleError,
    /// User work panicked under an unwind-capable build.
    Panicked,
    /// Tokio confirmed forced abortion of the run future.
    Aborted,
    /// The bound runtime stopped before another terminal was published.
    ExecutorStopped,
    /// Scheduler detected an internal executor invariant failure and reported
    /// a stable diagnostic code.
    ExecutorError,
    /// A higher-level handle was created, but Scheduler rejected its job.
    SubmissionFailed,
    /// A pending dispatch entry was evicted before scheduler submission.
    Evicted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Controls how `run_now` requests interact with an active scheduled job.
pub enum TriggerPolicy {
    /// Drops triggers that cannot start immediately.
    Drop,
    #[default]
    /// Retains at most one pending trigger.
    CoalesceOne,
    /// Retains triggers in a bounded counter.
    QueueAll {
        /// Maximum number of pending triggers.
        capacity: NonZeroUsize,
    },
    /// Replaces the single pending trigger with the newest request.
    Replace,
}

impl TriggerPolicy {
    /// Creates a bounded queue-all policy, rejecting zero capacity.
    pub fn queue_all(capacity: usize) -> Result<Self, TriggerPolicyError> {
        NonZeroUsize::new(capacity)
            .map(|capacity| Self::QueueAll { capacity })
            .ok_or(TriggerPolicyError::ZeroCapacity)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Explains invalid manual-trigger configuration.
pub enum TriggerPolicyError {
    /// Queue-all capacity was zero.
    ZeroCapacity,
}

impl fmt::Display for TriggerPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("queue-all trigger capacity must be greater than zero")
    }
}

impl std::error::Error for TriggerPolicyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Reports how a `run_now` request was handled.
pub enum TriggerOutcome {
    /// The request made a future invocation runnable.
    Scheduled,
    /// The request shared an already-pending trigger.
    Coalesced,
    /// The request was added to the bounded trigger queue.
    Queued {
        /// Number of triggers pending after this request.
        pending: usize,
    },
    /// The request replaced the previous pending trigger.
    Replaced,
    /// Policy discarded the request.
    Dropped,
    /// The request was retained but cannot run while paused.
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Explains why a pause, resume, or run-now command was rejected.
pub enum TaskCommandError {
    /// The task already has a terminal state.
    Terminal,
    /// The command requires a scheduled task.
    NotScheduled,
    /// The owning task group remains paused.
    ScopePaused,
}

impl fmt::Display for TaskCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Terminal => f.write_str("task run is already terminal"),
            Self::NotScheduled => f.write_str("run-now requires a scheduled task"),
            Self::ScopePaused => f.write_str("the task group is paused"),
        }
    }
}

impl std::error::Error for TaskCommandError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Lifecycle of one generational task-group scope.
pub enum GroupState {
    /// Accepts new task runs.
    Open,
    /// Rejects new runs while existing members settle.
    Closing,
    /// Every captured member has reached a terminal state.
    Terminated,
}

impl TaskState {
    /// Returns whether no further state transition is permitted.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::RetriesExhausted
                | Self::Cancelled
                | Self::TimedOut
                | Self::ScheduleError
                | Self::Panicked
                | Self::Aborted
                | Self::ExecutorStopped
                | Self::ExecutorError
                | Self::SubmissionFailed
                | Self::Evicted
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Payload-free point-in-time observation of one task run.
pub struct TaskSnapshot {
    snapshot_version: u64,
    observed_at: Duration,
    run_id: TaskRunId,
    job_id: JobId,
    key_generation: Option<u64>,
    lane: Arc<str>,
    lane_generation: u64,
    group_generation: Option<u64>,
    state: TaskState,
    cancel_reason: Option<CancelReason>,
    next_wake_at: Option<Duration>,
}

impl TaskSnapshot {
    /// Returns the monotonic observation sequence assigned to this state.
    pub fn snapshot_version(&self) -> u64 {
        self.snapshot_version
    }

    /// Returns elapsed monotonic time since this scheduler's observation
    /// subsystem was created.
    pub fn observed_at(&self) -> Duration {
        self.observed_at
    }

    /// Returns the unique submitted-run identity.
    pub fn run_id(&self) -> TaskRunId {
        self.run_id
    }

    /// Returns the reusable job identity.
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    /// Returns the keyed-run generation, when present.
    pub fn key_generation(&self) -> Option<u64> {
        self.key_generation
    }

    /// Returns the lane name captured at submission.
    pub fn lane(&self) -> &str {
        &self.lane
    }

    /// Returns the lane generation captured at submission.
    pub fn lane_generation(&self) -> u64 {
        self.lane_generation
    }

    /// Returns the owning task-group generation, when present.
    pub fn group_generation(&self) -> Option<u64> {
        self.group_generation
    }

    /// Returns the observed lifecycle state.
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// Returns the first accepted cancellation reason, if any.
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        self.cancel_reason
    }

    /// Returns the planned wake-up offset from scheduler creation while this
    /// run waits for a retry or schedule delay.
    pub fn next_wake_at(&self) -> Option<Duration> {
        self.next_wake_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Sanitized information captured from an unwinding panic payload.
pub struct PanicInfo {
    message: Option<String>,
}

impl PanicInfo {
    /// Returns a string panic message when the payload used a supported string
    /// representation.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identifies the deadline that terminated a run.
pub enum TimeoutScope {
    /// One individual attempt exceeded its limit.
    Attempt,
    /// A run-level deadline expired.
    Run,
    /// Retry attempts and waits exceeded their shared elapsed limit.
    TotalElapsed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Identifies why pending work was evicted.
pub enum EvictionPolicy {
    /// The oldest pending job was removed.
    Oldest,
    /// The lowest-priority pending job was removed.
    LowestPriority,
    /// A newer pending job with the same key replaced this one.
    Coalesced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Identifies a scheduler invariant failure without exposing task payloads.
pub enum ExecutorErrorCode {
    /// A one-shot job no longer owned the operation required to start it.
    JobAlreadyConsumed,
    /// Scheduled work produced control output without a schedule cursor.
    ScheduleCursorMissing,
    /// A run completion owner no longer held its result sender.
    CompletionAlreadyPublished,
}

impl ExecutorErrorCode {
    /// Returns the stable string code used by logs and metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobAlreadyConsumed => "BB-EXEC-001",
            Self::ScheduleCursorMissing => "BB-EXEC-002",
            Self::CompletionAlreadyPublished => "BB-EXEC-003",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Identifies why Scheduler rejected a job handed off by a higher-level API.
pub enum SubmissionFailure {
    /// Reject backpressure found the destination lane at capacity.
    Full,
    /// Scheduler shutdown has started but has not yet fully terminated.
    ShuttingDown,
    /// The scheduler, task group, or selected lane is closed.
    Closed,
    /// The selected lane name is not registered.
    LaneNotFound,
}

/// The single authoritative terminal result owned by a task handle.
pub enum TaskTerminal<T, E> {
    /// User work completed successfully.
    Completed(T),
    /// User work returned an error that was not retried.
    Failed(E),
    /// Every allowed retry attempt returned an eligible error.
    RetriesExhausted {
        /// Number of attempts performed.
        attempts: u32,
        /// Error returned by the last attempt.
        last_error: E,
    },
    /// Cooperative cancellation won terminal publication.
    Cancelled {
        /// First accepted cancellation reason.
        reason: CancelReason,
    },
    /// A configured deadline expired.
    TimedOut {
        /// Deadline that expired.
        scope: TimeoutScope,
        /// Number of attempts started before the timeout.
        attempts: u32,
        /// Most recent retryable error, when one exists.
        last_error: Option<E>,
    },
    /// Schedule calculation could not produce another valid invocation.
    ScheduleError(ScheduleError),
    /// User work unwound with a panic.
    Panicked(PanicInfo),
    /// Forced abortion was requested and confirmed by Tokio.
    Aborted,
    /// The bound executor stopped before another terminal result was known.
    ExecutorStopped,
    /// Scheduler stopped the run after detecting an internal invariant failure.
    ExecutorError {
        /// Payload-free code that can be correlated with SDK diagnostics.
        code: ExecutorErrorCode,
    },
    /// A higher-level API returned its own handle before Scheduler rejected
    /// the underlying job.
    SubmissionFailed {
        /// Scheduler rejection category. The consumed job is not recoverable
        /// after a high-level handle has been returned.
        reason: SubmissionFailure,
    },
    /// A dispatch layer removed the job before scheduler execution.
    Evicted {
        /// Eviction rule that selected this job.
        policy: EvictionPolicy,
    },
}

impl<T, E> fmt::Debug for TaskTerminal<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed(_) => f.write_str("Completed(..)"),
            Self::Failed(_) => f.write_str("Failed(..)"),
            Self::RetriesExhausted { attempts, .. } => f
                .debug_struct("RetriesExhausted")
                .field("attempts", attempts)
                .field("last_error", &"..")
                .finish(),
            Self::Cancelled { reason } => {
                f.debug_struct("Cancelled").field("reason", reason).finish()
            }
            Self::TimedOut {
                scope, attempts, ..
            } => f
                .debug_struct("TimedOut")
                .field("scope", scope)
                .field("attempts", attempts)
                .field("last_error", &"..")
                .finish(),
            Self::ScheduleError(error) => f.debug_tuple("ScheduleError").field(error).finish(),
            Self::Panicked(info) => f.debug_tuple("Panicked").field(info).finish(),
            Self::Aborted => f.write_str("Aborted"),
            Self::ExecutorStopped => f.write_str("ExecutorStopped"),
            Self::ExecutorError { code } => {
                f.debug_struct("ExecutorError").field("code", code).finish()
            }
            Self::SubmissionFailed { reason } => f
                .debug_struct("SubmissionFailed")
                .field("reason", reason)
                .finish(),
            Self::Evicted { policy } => f.debug_struct("Evicted").field("policy", policy).finish(),
        }
    }
}

impl<T, E> TaskTerminal<T, E> {
    fn state(&self) -> TaskState {
        match self {
            Self::Completed(_) => TaskState::Completed,
            Self::Failed(_) => TaskState::Failed,
            Self::RetriesExhausted { .. } => TaskState::RetriesExhausted,
            Self::Cancelled { .. } => TaskState::Cancelled,
            Self::TimedOut { .. } => TaskState::TimedOut,
            Self::ScheduleError(_) => TaskState::ScheduleError,
            Self::Panicked(_) => TaskState::Panicked,
            Self::Aborted => TaskState::Aborted,
            Self::ExecutorStopped => TaskState::ExecutorStopped,
            Self::ExecutorError { .. } => TaskState::ExecutorError,
            Self::SubmissionFailed { .. } => TaskState::SubmissionFailed,
            Self::Evicted { .. } => TaskState::Evicted,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Controls submission behavior when a lane's pending capacity is exhausted.
pub enum Backpressure {
    /// Returns the original job immediately.
    Reject,
    /// Asynchronously waits for pending capacity or cancellation/closure.
    Wait,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Validated pending capacity, execution concurrency, and backpressure for one
/// lane generation.
pub struct LaneConfig {
    queue_capacity: usize,
    concurrency: usize,
    backpressure: Backpressure,
}

impl LaneConfig {
    /// Creates a lane configuration using wait backpressure.
    pub fn new(queue_capacity: usize, concurrency: usize) -> Result<Self, LaneError> {
        if queue_capacity == 0 || queue_capacity > Semaphore::MAX_PERMITS {
            return Err(LaneError::InvalidCapacity {
                capacity: queue_capacity,
                maximum: Semaphore::MAX_PERMITS,
            });
        }
        if concurrency == 0 || concurrency > Semaphore::MAX_PERMITS {
            return Err(LaneError::InvalidConcurrency {
                concurrency,
                maximum: Semaphore::MAX_PERMITS,
            });
        }
        Ok(Self {
            queue_capacity,
            concurrency,
            backpressure: Backpressure::Wait,
        })
    }

    /// Sets full-lane submission behavior.
    pub fn backpressure(mut self, backpressure: Backpressure) -> Self {
        self.backpressure = backpressure;
        self
    }

    /// Returns the maximum number of accepted runs waiting to start.
    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// Returns the maximum simultaneously executing attempts.
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Returns the lane's full-capacity policy.
    pub fn backpressure_policy(&self) -> Backpressure {
        self.backpressure
    }
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            concurrency: 1,
            backpressure: Backpressure::Wait,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Describes invalid lane configuration or a rejected lane lifecycle
/// operation.
pub enum LaneError {
    /// Pending capacity was outside Tokio's supported permit range.
    InvalidCapacity {
        /// Rejected capacity.
        capacity: usize,
        /// Largest supported capacity.
        maximum: usize,
    },
    /// Execution concurrency was outside Tokio's supported permit range.
    InvalidConcurrency {
        /// Rejected concurrency.
        concurrency: usize,
        /// Largest supported concurrency.
        maximum: usize,
    },
    /// A live lane with the same name has a different configuration.
    ConfigConflict {
        /// Conflicting lane name.
        name: String,
        /// Configuration of the live generation.
        existing: LaneConfig,
        /// Configuration requested by the caller.
        requested: LaneConfig,
    },
    /// No live lane has the requested name.
    NotFound {
        /// Requested lane name.
        name: String,
    },
    /// The requested lane generation no longer accepts work.
    Closed {
        /// Lane name.
        name: String,
        /// Closed lane generation.
        generation: u64,
    },
    /// Deletion requires the current generation to be closed first.
    StillOpen {
        /// Lane name.
        name: String,
        /// Still-open generation.
        generation: u64,
    },
    /// Active task references still retain the closed generation.
    Busy {
        /// Lane name.
        name: String,
        /// Retained generation.
        generation: u64,
    },
    /// The monotonically increasing lane generation was exhausted.
    GenerationExhausted,
    /// Scheduler shutdown has started or completed.
    SchedulerClosed,
}

impl fmt::Display for LaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity { capacity, maximum } => write!(
                f,
                "lane queue capacity ({capacity}) must be between 1 and {maximum}"
            ),
            Self::InvalidConcurrency {
                concurrency,
                maximum,
            } => write!(
                f,
                "lane concurrency ({concurrency}) must be between 1 and {maximum}"
            ),
            Self::ConfigConflict { name, .. } => {
                write!(
                    f,
                    "lane '{name}' already exists with a different configuration"
                )
            }
            Self::NotFound { name } => write!(f, "lane '{name}' does not exist"),
            Self::Closed { name, generation } => {
                write!(f, "lane '{name}' generation {generation} is closed")
            }
            Self::StillOpen { name, generation } => {
                write!(f, "lane '{name}' generation {generation} is still open")
            }
            Self::Busy { name, generation } => write!(
                f,
                "lane '{name}' generation {generation} still has active references"
            ),
            Self::GenerationExhausted => f.write_str("lane generation space is exhausted"),
            Self::SchedulerClosed => write!(f, "scheduler is closing or terminated"),
        }
    }
}

impl std::error::Error for LaneError {}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Describes a rejected task-group lifecycle operation.
pub enum GroupError {
    /// Scheduler shutdown has started or completed.
    SchedulerClosed,
    /// The group's owning scheduler has been dropped.
    SchedulerUnavailable,
    /// A non-terminated generation already owns the requested name.
    NameInUse {
        /// Conflicting group name.
        name: String,
        /// Existing group generation.
        generation: u64,
        /// Existing generation's lifecycle state.
        state: GroupState,
    },
    /// The monotonically increasing group generation was exhausted.
    GenerationExhausted,
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchedulerClosed => f.write_str("scheduler is closing or terminated"),
            Self::SchedulerUnavailable => f.write_str("group scheduler is no longer available"),
            Self::NameInUse {
                name,
                generation,
                state,
            } => write!(
                f,
                "group '{name}' generation {generation} is still {state:?}"
            ),
            Self::GenerationExhausted => f.write_str("group generation space is exhausted"),
        }
    }
}

impl std::error::Error for GroupError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Point-in-time ownership information for a keyed run or replacement
/// reservation.
pub struct KeyStatus {
    generation: u64,
    run_id: Option<TaskRunId>,
    replacing: bool,
}

impl KeyStatus {
    /// Returns the key's monotonically increasing generation.
    pub fn generation(self) -> u64 {
        self.generation
    }

    /// Returns the current run identity, or `None` while a reservation has no
    /// committed replacement run.
    pub fn run_id(self) -> Option<TaskRunId> {
        self.run_id
    }

    /// Returns whether a replacement reservation currently owns the key.
    pub fn is_replacing(self) -> bool {
        self.replacing
    }
}

#[non_exhaustive]
/// A keyed start-if-absent failure that returns ownership of the original job.
pub enum KeyedSubmitError<J> {
    /// A run or replacement reservation already owns the key.
    Occupied {
        /// Rejected job.
        job: J,
        /// Current key owner.
        current: Box<KeyStatus>,
    },
    /// The key generation counter cannot advance.
    GenerationExhausted(J),
    /// Ordinary scheduler submission failed after reserving the key.
    Submit(TrySubmitError<J>),
}

impl<J> KeyedSubmitError<J> {
    /// Returns current key ownership for an occupied-key failure.
    pub fn current(&self) -> Option<KeyStatus> {
        match self {
            Self::Occupied { current, .. } => Some(**current),
            _ => None,
        }
    }

    /// Recovers the rejected job.
    pub fn into_job(self) -> J {
        match self {
            Self::Occupied { job, .. } | Self::GenerationExhausted(job) => job,
            Self::Submit(error) => error.into_job(),
        }
    }
}

impl<J> fmt::Debug for KeyedSubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied { current, .. } => f
                .debug_struct("Occupied")
                .field("job", &"..")
                .field("current", current)
                .finish(),
            Self::GenerationExhausted(_) => f.write_str("GenerationExhausted(..)"),
            Self::Submit(error) => f.debug_tuple("Submit").field(error).finish(),
        }
    }
}

impl<J> fmt::Display for KeyedSubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Occupied { .. } => f.write_str("the key already has a current task run"),
            Self::GenerationExhausted(_) => f.write_str("key generation space is exhausted"),
            Self::Submit(error) => write!(f, "keyed task submission failed: {error}"),
        }
    }
}

impl<J> std::error::Error for KeyedSubmitError<J> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects whether a keyed replacement must await confirmed prior termination.
pub enum ReplaceMode {
    /// Starts only after the previous generation has a terminal state.
    AfterConfirmedStop,
    /// Permits old and new generations to overlap while ownership remains
    /// generation-safe.
    AllowOverlap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects behavior when a previous keyed generation exceeds its replacement
/// grace period.
pub enum ReplaceTimeoutAction {
    /// Returns a timeout error and does not submit the replacement.
    Fail,
    /// Continues waiting without a deadline.
    ContinueWait,
    /// Requests Tokio abortion, then waits for terminal confirmation.
    Abort {
        /// Maximum wait for confirmed termination after requesting abortion.
        confirmation_timeout: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Immutable keyed replacement timing and overlap policy.
pub struct ReplacePolicy {
    mode: ReplaceMode,
    grace_period: Duration,
    timeout_action: ReplaceTimeoutAction,
}

impl ReplacePolicy {
    /// Creates a policy that starts the new generation without waiting for the
    /// previous generation to stop.
    pub fn allow_overlap() -> Self {
        Self {
            mode: ReplaceMode::AllowOverlap,
            grace_period: Duration::ZERO,
            timeout_action: ReplaceTimeoutAction::Fail,
        }
    }

    /// Creates a policy that cooperatively cancels and awaits the previous
    /// generation before submitting the replacement.
    pub fn after_confirmed_stop(
        grace_period: Duration,
        timeout_action: ReplaceTimeoutAction,
    ) -> Self {
        Self {
            mode: ReplaceMode::AfterConfirmedStop,
            grace_period,
            timeout_action,
        }
    }

    /// Returns the overlap mode.
    pub fn mode(self) -> ReplaceMode {
        self.mode
    }

    /// Returns the cooperative-stop grace period.
    pub fn grace_period(self) -> Duration {
        self.grace_period
    }

    /// Returns the action taken after the grace period.
    pub fn timeout_action(self) -> ReplaceTimeoutAction {
        self.timeout_action
    }
}

#[non_exhaustive]
/// A keyed replacement failure that returns ownership of the replacement job.
pub enum ReplaceError<J> {
    /// Another replacement reservation already owns the key.
    Busy {
        /// Rejected replacement job.
        job: J,
        /// Current key owner.
        current: KeyStatus,
    },
    /// The key generation counter cannot advance.
    GenerationExhausted(J),
    /// A task attempted to replace itself with a policy that waits for itself.
    SelfReplacementRequiresOverlap(J),
    /// The previous generation did not confirm termination in time.
    TimedOut {
        /// Rejected replacement job.
        job: J,
        /// Previous generation that remained non-terminal.
        previous: KeyStatus,
    },
    /// Another operation invalidated this replacement reservation.
    Superseded(J),
    /// Ordinary scheduler submission failed after replacement coordination.
    Submit(TrySubmitError<J>),
}

impl<J> ReplaceError<J> {
    /// Recovers the rejected replacement job.
    pub fn into_job(self) -> J {
        match self {
            Self::Busy { job, .. }
            | Self::GenerationExhausted(job)
            | Self::SelfReplacementRequiresOverlap(job)
            | Self::TimedOut { job, .. }
            | Self::Superseded(job) => job,
            Self::Submit(error) => error.into_job(),
        }
    }
}

impl<J> fmt::Debug for ReplaceError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { current, .. } => f
                .debug_struct("Busy")
                .field("job", &"..")
                .field("current", current)
                .finish(),
            Self::GenerationExhausted(_) => f.write_str("GenerationExhausted(..)"),
            Self::SelfReplacementRequiresOverlap(_) => {
                f.write_str("SelfReplacementRequiresOverlap(..)")
            }
            Self::TimedOut { previous, .. } => f
                .debug_struct("TimedOut")
                .field("job", &"..")
                .field("previous", previous)
                .finish(),
            Self::Superseded(_) => f.write_str("Superseded(..)"),
            Self::Submit(error) => f.debug_tuple("Submit").field(error).finish(),
        }
    }
}

impl<J> fmt::Display for ReplaceError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { .. } => f.write_str("the key already has a replacement in progress"),
            Self::GenerationExhausted(_) => f.write_str("key generation space is exhausted"),
            Self::SelfReplacementRequiresOverlap(_) => {
                f.write_str("self replacement requires the allow-overlap policy")
            }
            Self::TimedOut { .. } => {
                f.write_str("the previous task did not stop before the replacement deadline")
            }
            Self::Superseded(_) => f.write_str("the replacement reservation was superseded"),
            Self::Submit(error) => write!(f, "replacement submission failed: {error}"),
        }
    }
}

impl<J> std::error::Error for ReplaceError<J> {}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Explains why a [`Scheduler`] could not be constructed.
pub enum SchedulerBuildError {
    /// No explicit or currently entered Tokio runtime was available.
    RuntimeUnavailable,
    /// Global execution concurrency was outside Tokio's permit range.
    InvalidGlobalConcurrency {
        /// Rejected concurrency.
        concurrency: usize,
        /// Largest supported concurrency.
        maximum: usize,
    },
    /// Event channel capacity was outside Tokio's supported range.
    InvalidEventCapacity {
        /// Rejected capacity.
        capacity: usize,
        /// Largest supported capacity.
        maximum: usize,
    },
}

impl fmt::Display for SchedulerBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeUnavailable => write!(f, "no active Tokio runtime is available"),
            Self::InvalidGlobalConcurrency {
                concurrency,
                maximum,
            } => write!(
                f,
                "global concurrency ({concurrency}) must be between 1 and {maximum}"
            ),
            Self::InvalidEventCapacity { capacity, maximum } => write!(
                f,
                "event capacity ({capacity}) must be between 1 and {maximum}"
            ),
        }
    }
}

impl std::error::Error for SchedulerBuildError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Scheduler- and group-wide shutdown timing frozen at scheduler construction.
pub struct ShutdownPolicy {
    grace_period: Duration,
    abort_after_grace: bool,
    abort_wait: Duration,
    event_drain: Duration,
}

impl ShutdownPolicy {
    /// Requests cooperative cancellation and waits indefinitely after the
    /// grace period for eventual terminal confirmation.
    pub fn graceful(grace_period: Duration) -> Self {
        Self {
            grace_period,
            abort_after_grace: false,
            abort_wait: Duration::ZERO,
            event_drain: Duration::from_secs(1),
        }
    }

    /// Requests cooperative cancellation, then Tokio abortion after the grace
    /// period and waits up to `abort_wait` for confirmation.
    pub fn graceful_then_abort(grace_period: Duration, abort_wait: Duration) -> Self {
        Self {
            grace_period,
            abort_after_grace: true,
            abort_wait,
            event_drain: Duration::from_secs(1),
        }
    }

    /// Sets the independent upper bound for draining the internal metrics
    /// hook after task shutdown reaches a caller-visible outcome.
    pub fn with_event_drain(mut self, event_drain: Duration) -> Self {
        self.event_drain = event_drain;
        self
    }

    /// Returns the cooperative cancellation grace period.
    pub fn grace_period(self) -> Duration {
        self.grace_period
    }

    /// Returns the abort confirmation wait when forced abortion is enabled.
    pub fn abort_wait(self) -> Option<Duration> {
        self.abort_after_grace.then_some(self.abort_wait)
    }

    /// Returns the independent metrics-hook drain deadline.
    pub fn event_drain(self) -> Duration {
        self.event_drain
    }
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self::graceful(Duration::from_secs(5))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Authoritative aggregate of task runs captured by one shared shutdown
/// coordinator.
pub struct SchedulerShutdownReport {
    total: usize,
    completed: usize,
    failed: usize,
    cancelled: usize,
    timed_out: usize,
    panicked: usize,
    aborted: usize,
    executor_stopped: usize,
    evicted: usize,
    abort_requested: usize,
    still_running: Vec<TaskRunId>,
    grace_timed_out: bool,
    settled: bool,
}

impl SchedulerShutdownReport {
    fn collect(
        runs: &[Arc<RunShared>],
        abort_requested: usize,
        grace_timed_out: bool,
        settled: bool,
    ) -> Self {
        let mut report = Self {
            total: runs.len(),
            completed: 0,
            failed: 0,
            cancelled: 0,
            timed_out: 0,
            panicked: 0,
            aborted: 0,
            executor_stopped: 0,
            evicted: 0,
            abort_requested,
            still_running: Vec::new(),
            grace_timed_out,
            settled,
        };
        for run in runs {
            match run.snapshot().state() {
                TaskState::Completed => report.completed += 1,
                TaskState::Failed
                | TaskState::RetriesExhausted
                | TaskState::ScheduleError
                | TaskState::ExecutorError => {
                    report.failed += 1;
                }
                TaskState::Cancelled => report.cancelled += 1,
                TaskState::TimedOut => report.timed_out += 1,
                TaskState::Panicked => report.panicked += 1,
                TaskState::Aborted => report.aborted += 1,
                TaskState::ExecutorStopped => report.executor_stopped += 1,
                TaskState::SubmissionFailed => report.failed += 1,
                TaskState::Evicted => report.evicted += 1,
                _ => report.still_running.push(run.run_id),
            }
        }
        report
            .still_running
            .sort_unstable_by_key(|run_id| run_id.as_uuid());
        report
    }

    /// Returns the number of task runs captured when shutdown started.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Returns runs that completed successfully.
    pub fn completed(&self) -> usize {
        self.completed
    }

    /// Returns runs that failed, exhausted retries, hit a schedule error, or
    /// could not be submitted by a higher-level API.
    pub fn failed(&self) -> usize {
        self.failed
    }

    /// Returns cooperatively cancelled runs.
    pub fn cancelled(&self) -> usize {
        self.cancelled
    }

    /// Returns runs terminated by a configured deadline.
    pub fn timed_out(&self) -> usize {
        self.timed_out
    }

    /// Returns runs whose user work unwound with a panic.
    pub fn panicked(&self) -> usize {
        self.panicked
    }

    /// Returns runs whose forced abortion was confirmed.
    pub fn aborted(&self) -> usize {
        self.aborted
    }

    /// Returns runs terminated because their bound runtime stopped.
    pub fn executor_stopped(&self) -> usize {
        self.executor_stopped
    }

    /// Returns pending runs evicted by a dispatch policy.
    pub fn evicted(&self) -> usize {
        self.evicted
    }

    /// Returns how many captured runs accepted an abort request.
    ///
    /// This is not confirmation; compare [`Self::aborted`] and
    /// [`Self::still_running`] for observed outcomes.
    pub fn abort_requested(&self) -> usize {
        self.abort_requested
    }

    /// Returns captured runs without an authoritative terminal state when this
    /// report was published.
    pub fn still_running(&self) -> &[TaskRunId] {
        &self.still_running
    }

    /// Returns whether the cooperative grace period elapsed.
    pub fn grace_timed_out(&self) -> bool {
        self.grace_timed_out
    }

    /// Returns whether every captured run has an authoritative terminal state.
    pub fn is_complete(&self) -> bool {
        self.still_running.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A caller-local shutdown observation timeout carrying the latest shared
/// report.
pub struct ShutdownWaitError {
    report: SchedulerShutdownReport,
}

impl ShutdownWaitError {
    /// Returns the most recent authoritative shutdown report observed before
    /// this caller's deadline.
    pub fn report(&self) -> &SchedulerShutdownReport {
        &self.report
    }
}

impl fmt::Display for ShutdownWaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shutdown observation deadline elapsed with {} task(s) still running",
            self.report.still_running.len()
        )
    }
}

impl std::error::Error for ShutdownWaitError {}

type JobFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;
type JobOutputFuture<T, E> =
    Pin<Box<dyn Future<Output = Result<JobOutput<T>, E>> + Send + 'static>>;
type OnceJob<T, E> = Box<dyn FnOnce(TaskContext) -> JobFuture<T, E> + Send + 'static>;
type RepeatJob<T, E> = Arc<dyn Fn(TaskContext) -> JobFuture<T, E> + Send + Sync + 'static>;
type ScheduledJobFuture<T, E> =
    Pin<Box<dyn Future<Output = Result<TaskControl<T>, E>> + Send + 'static>>;
type ScheduledJobFactory<T, E> =
    Arc<dyn Fn(TaskContext) -> ScheduledJobFuture<T, E> + Send + Sync + 'static>;

struct ScheduledDefinition<T, E> {
    factory: ScheduledJobFactory<T, E>,
    trigger_policy: TriggerPolicy,
}

enum JobOutput<T> {
    Value(T),
    Control(TaskControl<T>),
}

enum JobKind<T, E> {
    Once(Option<OnceJob<T, E>>),
    Repeat(RepeatJob<T, E>),
    Scheduled(Arc<ScheduledDefinition<T, E>>),
}

impl<T: 'static, E: 'static> JobKind<T, E> {
    fn start(&mut self, context: TaskContext) -> Result<JobOutputFuture<T, E>, ExecutorErrorCode> {
        match self {
            Self::Once(job) => {
                let Some(job) = job.take() else {
                    return Err(ExecutorErrorCode::JobAlreadyConsumed);
                };
                let future = job(context);
                Ok(Box::pin(async move { future.await.map(JobOutput::Value) }))
            }
            Self::Repeat(job) => {
                let future = job(context);
                Ok(Box::pin(async move { future.await.map(JobOutput::Value) }))
            }
            Self::Scheduled(job) => {
                let future = (job.factory)(context);
                Ok(Box::pin(
                    async move { future.await.map(JobOutput::Control) },
                ))
            }
        }
    }

    fn trigger_policy(&self) -> Option<TriggerPolicy> {
        match self {
            Self::Scheduled(definition) => Some(definition.trigger_policy),
            _ => None,
        }
    }
}

/// A single-consumption submission carrying one asynchronous operation.
pub struct Job<T, E> {
    id: JobId,
    key: Option<Arc<str>>,
    key_generation: Option<u64>,
    lane: Arc<str>,
    kind: JobKind<T, E>,
    retry: Option<Arc<RetryPolicy<E>>>,
    schedule: Option<Arc<Schedule>>,
    retry_exhausted_action: RetryExhaustedAction,
}

impl<T, E> Job<T, E> {
    /// Creates a one-shot job whose closure is invoked at most once.
    pub fn once<F, Fut>(job: F) -> Self
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        Self {
            id: JobId::new(),
            key: None,
            key_generation: None,
            lane: Arc::from("default"),
            kind: JobKind::Once(Some(Box::new(move |context| Box::pin(job(context))))),
            retry: None,
            schedule: None,
            retry_exhausted_action: RetryExhaustedAction::Stop,
        }
    }

    /// Returns the job identity.
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Selects the destination lane by name.
    pub fn on_lane(mut self, lane: impl Into<Arc<str>>) -> Self {
        self.lane = lane.into();
        self
    }

    /// Associates a stable key for start-if-absent or replacement APIs.
    pub fn with_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.key = Some(key.into());
        self.key_generation = None;
        self
    }
}

/// A cloneable factory whose instantiations share one [`JobId`] but receive
/// distinct [`TaskRunId`] values and cancellation state.
pub struct ReusableJob<T, E> {
    id: JobId,
    key: Option<Arc<str>>,
    lane: Arc<str>,
    factory: RepeatJob<T, E>,
    retry: Option<Arc<RetryPolicy<E>>>,
}

impl<T, E> Clone for ReusableJob<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            key: self.key.clone(),
            lane: Arc::clone(&self.lane),
            factory: Arc::clone(&self.factory),
            retry: self.retry.clone(),
        }
    }
}

impl<T, E> ReusableJob<T, E> {
    /// Creates a reusable asynchronous job factory.
    pub fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        Self {
            id: JobId::new(),
            key: None,
            lane: Arc::from("default"),
            factory: Arc::new(move |context| Box::pin(factory(context))),
            retry: None,
        }
    }

    /// Returns the identity shared by all instantiations.
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Selects the destination lane for future instantiations.
    pub fn on_lane(mut self, lane: impl Into<Arc<str>>) -> Self {
        self.lane = lane.into();
        self
    }

    /// Associates a stable key with future instantiations.
    pub fn with_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Applies the retry policy to future instantiations.
    pub fn retry(mut self, policy: RetryPolicy<E>) -> Self {
        self.retry = Some(Arc::new(policy));
        self
    }

    /// Produces an independently owned one-shot submission.
    pub fn instantiate(&self) -> Job<T, E> {
        Job {
            id: self.id,
            key: self.key.clone(),
            key_generation: None,
            lane: Arc::clone(&self.lane),
            kind: JobKind::Repeat(Arc::clone(&self.factory)),
            retry: self.retry.clone(),
            schedule: None,
            retry_exhausted_action: RetryExhaustedAction::Stop,
        }
    }
}

/// A cloneable, non-reentrant scheduled job factory.
pub struct ScheduledJob<T, E> {
    id: JobId,
    key: Option<Arc<str>>,
    lane: Arc<str>,
    definition: Arc<ScheduledDefinition<T, E>>,
    schedule: Arc<Schedule>,
    retry: Option<Arc<RetryPolicy<E>>>,
    retry_exhausted_action: RetryExhaustedAction,
}

impl<T, E> Clone for ScheduledJob<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            key: self.key.clone(),
            lane: Arc::clone(&self.lane),
            definition: Arc::clone(&self.definition),
            schedule: Arc::clone(&self.schedule),
            retry: self.retry.clone(),
            retry_exhausted_action: self.retry_exhausted_action,
        }
    }
}

impl<T, E> ScheduledJob<T, E> {
    /// Creates a scheduled factory whose output controls continuation.
    pub fn new<F, Fut>(schedule: Schedule, factory: F) -> Self
    where
        F: Fn(TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TaskControl<T>, E>> + Send + 'static,
    {
        Self {
            id: JobId::new(),
            key: None,
            lane: Arc::from("default"),
            definition: Arc::new(ScheduledDefinition {
                factory: Arc::new(move |context| Box::pin(factory(context))),
                trigger_policy: TriggerPolicy::default(),
            }),
            schedule: Arc::new(schedule),
            retry: None,
            retry_exhausted_action: RetryExhaustedAction::Stop,
        }
    }

    /// Returns the identity shared by all instantiations.
    pub fn id(&self) -> JobId {
        self.id
    }

    /// Selects the destination lane for future instantiations.
    pub fn on_lane(mut self, lane: impl Into<Arc<str>>) -> Self {
        self.lane = lane.into();
        self
    }

    /// Associates a stable key with future instantiations.
    pub fn with_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Applies retries independently to each scheduled invocation.
    pub fn retry(mut self, policy: RetryPolicy<E>) -> Self {
        self.retry = Some(Arc::new(policy));
        self
    }

    /// Selects whether one exhausted invocation stops or advances the schedule.
    pub fn retry_exhausted_action(mut self, action: RetryExhaustedAction) -> Self {
        self.retry_exhausted_action = action;
        self
    }

    /// Selects how manual run-now requests are retained.
    pub fn trigger_policy(mut self, policy: TriggerPolicy) -> Self {
        self.definition = Arc::new(ScheduledDefinition {
            factory: Arc::clone(&self.definition.factory),
            trigger_policy: policy,
        });
        self
    }

    /// Produces an independently owned scheduled submission.
    pub fn instantiate(&self) -> Job<T, E> {
        Job {
            id: self.id,
            key: self.key.clone(),
            key_generation: None,
            lane: Arc::clone(&self.lane),
            kind: JobKind::Scheduled(Arc::clone(&self.definition)),
            retry: self.retry.clone(),
            schedule: Some(Arc::clone(&self.schedule)),
            retry_exhausted_action: self.retry_exhausted_action,
        }
    }
}

struct Cancellation {
    state: Mutex<CancellationState>,
    changed: watch::Sender<Option<CancelReason>>,
}

struct CancellationState {
    reason: Option<CancelReason>,
    outcome_claimed: bool,
}

impl Cancellation {
    fn new() -> Self {
        let (changed, _) = watch::channel(None);
        Self {
            state: Mutex::new(CancellationState {
                reason: None,
                outcome_claimed: false,
            }),
            changed,
        }
    }

    fn reason(&self) -> Option<CancelReason> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reason
    }

    fn cancel(&self, reason: CancelReason) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reason.is_some() || state.outcome_claimed {
            return false;
        }
        state.reason = Some(reason);
        self.changed.send_replace(Some(reason));
        true
    }

    /// Linearizes a completed/failed/panicked outcome against cancellation.
    fn claim_outcome(&self) -> Option<CancelReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reason) = state.reason {
            return Some(reason);
        }
        state.outcome_claimed = true;
        None
    }

    async fn cancelled(&self) -> CancelReason {
        let mut changed = self.changed.subscribe();
        if let Some(reason) = *changed.borrow_and_update() {
            return reason;
        }
        loop {
            if changed.changed().await.is_err() {
                return self.reason().unwrap_or(CancelReason::Shutdown);
            }
            if let Some(reason) = *changed.borrow_and_update() {
                return reason;
            }
        }
    }
}

struct TrackedChildrenState {
    closed: bool,
    handles: HashMap<Uuid, AbortHandle>,
}

struct TrackedChildren {
    registration: Mutex<()>,
    state: Mutex<TrackedChildrenState>,
    runtime: Handle,
}

struct TrackedChildrenGuard(Arc<TrackedChildren>);

struct TrackedChildCompletion {
    children: Arc<TrackedChildren>,
    id: Uuid,
}

impl Drop for TrackedChildrenGuard {
    fn drop(&mut self) {
        self.0.close_and_abort();
    }
}

impl Drop for TrackedChildCompletion {
    fn drop(&mut self) {
        self.children
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handles
            .remove(&self.id);
    }
}

impl TrackedChildren {
    fn new(runtime: Handle) -> Self {
        Self {
            registration: Mutex::new(()),
            state: Mutex::new(TrackedChildrenState {
                closed: false,
                handles: HashMap::new(),
            }),
            runtime,
        }
    }

    fn spawn<F, T>(self: &Arc<Self>, future: F) -> Result<JoinHandle<T>, ContextClosedError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let _registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return Err(ContextClosedError);
            }
            loop {
                let candidate = Uuid::new_v4();
                if !state.handles.contains_key(&candidate) {
                    break candidate;
                }
            }
        };
        let children = Arc::clone(self);
        let completion = TrackedChildCompletion { children, id };
        // A stopped runtime may drop the spawned future synchronously. Keep a
        // live task behind this barrier until its abort handle is registered;
        // a dropped receiver makes `send` fail and removes that registration.
        let (registered, wait_for_registration) = oneshot::channel();
        let handle = self.runtime.spawn(async move {
            let _completion = completion;
            let _ = wait_for_registration.await;
            future.await
        });
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handles
            .insert(id, handle.abort_handle());
        if registered.send(()).is_err() {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .handles
                .remove(&id);
        }
        Ok(handle)
    }

    fn close_and_abort(&self) {
        let registration = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handles = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            std::mem::take(&mut state.handles)
        };
        drop(registration);
        for handle in handles.into_values() {
            handle.abort();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Indicates that a task context no longer accepts tracked child futures.
pub struct ContextClosedError;

impl fmt::Display for ContextClosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task context is already closed")
    }
}

impl std::error::Error for ContextClosedError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Error returned when a cancellation-aware context operation is interrupted.
pub struct Cancelled {
    reason: CancelReason,
}

impl Cancelled {
    /// Returns the first accepted cancellation reason.
    pub fn reason(self) -> CancelReason {
        self.reason
    }
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task cancelled: {:?}", self.reason)
    }
}

impl std::error::Error for Cancelled {}

#[derive(Clone)]
/// Per-run cooperative cancellation, identity, retry, schedule, and child-task
/// context passed to user work.
pub struct TaskContext {
    run_id: TaskRunId,
    job_id: JobId,
    key: Option<Arc<str>>,
    key_generation: Option<u64>,
    run_index: u64,
    attempt: u32,
    cancellation: Arc<Cancellation>,
    children: Arc<TrackedChildren>,
    scheduler: Weak<SchedulerInner>,
    group: Option<Weak<GroupInner>>,
}

impl TaskContext {
    /// Returns this unique submitted-run identity.
    pub fn run_id(&self) -> TaskRunId {
        self.run_id
    }

    /// Returns the reusable job identity.
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    /// Returns the stable keyed-run name, if present.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Returns the keyed-run generation, if present.
    pub fn key_generation(&self) -> Option<u64> {
        self.key_generation
    }

    /// Returns the zero-based invocation index within a schedule. The first
    /// invocation, and every non-scheduled job, uses zero.
    pub fn run_index(&self) -> u64 {
        self.run_index
    }

    /// Returns the one-based retry attempt within the current invocation.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the owning group name without retaining group membership.
    pub fn group_name(&self) -> Option<Arc<str>> {
        self.group
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|group| Arc::clone(&group.name))
    }

    /// Returns the owning group generation without retaining membership.
    pub fn group_generation(&self) -> Option<u64> {
        self.group
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|group| group.generation)
    }

    /// Returns the first accepted cancellation reason without waiting.
    pub fn cancellation_reason(&self) -> Option<CancelReason> {
        self.cancellation.reason()
    }

    /// Waits until cooperative cancellation is requested.
    pub async fn cancelled(&self) -> CancelReason {
        self.cancellation.cancelled().await
    }

    /// Sleeps on Tokio's monotonic clock and wakes promptly on cancellation.
    pub async fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
        if let Some(reason) = self.cancellation.reason() {
            return Err(Cancelled { reason });
        }
        tokio::select! {
            biased;
            reason = self.cancellation.cancelled() => Err(Cancelled { reason }),
            _ = tokio::time::sleep(duration) => {
                match self.cancellation.reason() {
                    Some(reason) => Err(Cancelled { reason }),
                    None => Ok(()),
                }
            }
        }
    }

    /// Spawns a child on the bound runtime and guarantees it is aborted when
    /// this run reaches its completion boundary.
    pub fn spawn_tracked<F, T>(&self, future: F) -> Result<JoinHandle<T>, ContextClosedError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.children.spawn(future)
    }

    /// Starts Scheduler shutdown and returns without waiting for this run.
    /// This is the self-join-safe shutdown entry point for task code.
    pub fn request_scheduler_shutdown(&self) -> bool {
        let Some(inner) = self.scheduler.upgrade() else {
            return false;
        };
        let scheduler = Scheduler { inner };
        scheduler.start_shutdown();
        true
    }

    /// Starts this run's TaskGroup shutdown without waiting for this run.
    pub fn request_group_shutdown(&self) -> bool {
        let Some(group) = self.group.as_ref().and_then(Weak::upgrade) else {
            return false;
        };
        TaskGroup { inner: group }.start_shutdown();
        true
    }

    /// Performs a self-aware keyed replacement in scheduler scope.
    ///
    /// Replacing the current run requires [`ReplaceMode::AllowOverlap`].
    pub async fn replace_in_scheduler<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        job: Job<T, E>,
        policy: ReplacePolicy,
    ) -> Result<TaskHandle<T, E>, ReplaceError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let Some(inner) = self.scheduler.upgrade() else {
            return Err(ReplaceError::Submit(TrySubmitError::Closed(job)));
        };
        let scheduler = Scheduler { inner };
        replace_keyed(
            scheduler.clone(),
            None,
            Arc::clone(&scheduler.inner.keys),
            Some(self.run_id),
            key.into(),
            job,
            policy,
        )
        .await
    }

    /// Performs a self-aware keyed replacement in this run's group scope.
    ///
    /// Replacing the current run requires [`ReplaceMode::AllowOverlap`].
    pub async fn replace_in_group<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        job: Job<T, E>,
        policy: ReplacePolicy,
    ) -> Result<TaskHandle<T, E>, ReplaceError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let Some(group) = self.group.as_ref().and_then(Weak::upgrade) else {
            return Err(ReplaceError::Submit(TrySubmitError::Closed(job)));
        };
        let Some(inner) = group.scheduler.upgrade() else {
            return Err(ReplaceError::Submit(TrySubmitError::Closed(job)));
        };
        replace_keyed(
            Scheduler { inner },
            Some(Arc::clone(&group)),
            Arc::clone(&group.keys),
            Some(self.run_id),
            key.into(),
            job,
            policy,
        )
        .await
    }
}

struct TriggerState {
    pending: usize,
}

struct ManualControl {
    task_paused: AtomicBool,
    scope_paused: AtomicBool,
    pause_changed: Notify,
    trigger_policy: Option<TriggerPolicy>,
    triggers: Mutex<TriggerState>,
    trigger_changed: Notify,
}

impl ManualControl {
    fn new(trigger_policy: Option<TriggerPolicy>) -> Self {
        Self {
            task_paused: AtomicBool::new(false),
            scope_paused: AtomicBool::new(false),
            pause_changed: Notify::new(),
            trigger_policy,
            triggers: Mutex::new(TriggerState { pending: 0 }),
            trigger_changed: Notify::new(),
        }
    }

    fn pause_task(&self) -> bool {
        let changed = !self.task_paused.swap(true, Ordering::AcqRel);
        if changed {
            self.pause_changed.notify_waiters();
        }
        changed
    }

    fn resume_task(&self) -> bool {
        let changed = self.task_paused.swap(false, Ordering::AcqRel);
        if changed {
            self.pause_changed.notify_waiters();
        }
        changed
    }

    fn pause_scope(&self) -> bool {
        let changed = !self.scope_paused.swap(true, Ordering::AcqRel);
        if changed {
            self.pause_changed.notify_waiters();
        }
        changed
    }

    fn resume_scope(&self) -> bool {
        let changed = self.scope_paused.swap(false, Ordering::AcqRel);
        if changed {
            self.pause_changed.notify_waiters();
        }
        changed
    }

    fn is_paused(&self) -> bool {
        self.task_paused.load(Ordering::Acquire) || self.scope_paused.load(Ordering::Acquire)
    }

    fn run_now(&self, state: TaskState) -> Result<TriggerOutcome, TaskCommandError> {
        let Some(policy) = self.trigger_policy else {
            return Err(TaskCommandError::NotScheduled);
        };
        if self.is_paused() {
            return Ok(TriggerOutcome::Paused);
        }
        let mut triggers = self
            .triggers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = match policy {
            TriggerPolicy::Drop => {
                if state == TaskState::WaitingForSchedule && triggers.pending == 0 {
                    triggers.pending = 1;
                    TriggerOutcome::Scheduled
                } else {
                    TriggerOutcome::Dropped
                }
            }
            TriggerPolicy::CoalesceOne => {
                if triggers.pending == 0 {
                    triggers.pending = 1;
                    TriggerOutcome::Scheduled
                } else {
                    TriggerOutcome::Coalesced
                }
            }
            TriggerPolicy::QueueAll { capacity } => {
                if triggers.pending < capacity.get() {
                    triggers.pending += 1;
                    TriggerOutcome::Queued {
                        pending: triggers.pending,
                    }
                } else {
                    TriggerOutcome::Dropped
                }
            }
            TriggerPolicy::Replace => {
                let replaced = triggers.pending != 0;
                triggers.pending = 1;
                if replaced {
                    TriggerOutcome::Replaced
                } else {
                    TriggerOutcome::Scheduled
                }
            }
        };
        drop(triggers);
        if !matches!(outcome, TriggerOutcome::Dropped | TriggerOutcome::Coalesced) {
            self.trigger_changed.notify_waiters();
        }
        Ok(outcome)
    }

    fn take_trigger(&self) -> bool {
        let mut triggers = self
            .triggers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if triggers.pending == 0 {
            false
        } else {
            triggers.pending -= 1;
            true
        }
    }
}

struct RunMetadata {
    run_id: TaskRunId,
    job_id: JobId,
    key_generation: Option<u64>,
    lane: Arc<str>,
    lane_generation: u64,
}

struct RunShared {
    run_id: TaskRunId,
    job_id: JobId,
    lane: Arc<str>,
    lane_generation: u64,
    group_generation: Option<u64>,
    group: Option<Weak<GroupInner>>,
    observability: Arc<Observability>,
    command_gate: Mutex<()>,
    manual: ManualControl,
    key_registration: Mutex<Option<KeyRegistration>>,
    cancellation: Arc<Cancellation>,
    snapshot: watch::Sender<TaskSnapshot>,
    terminal: AtomicBool,
    abort_requested: AtomicBool,
    abort: Mutex<Option<AbortHandle>>,
}

impl RunShared {
    fn new(
        metadata: RunMetadata,
        group: Option<Weak<GroupInner>>,
        observability: Arc<Observability>,
        trigger_policy: Option<TriggerPolicy>,
    ) -> Self {
        let RunMetadata {
            run_id,
            job_id,
            key_generation,
            lane,
            lane_generation,
        } = metadata;
        let group_generation = group
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|group| group.generation);
        let snapshot = TaskSnapshot {
            snapshot_version: 0,
            observed_at: Duration::ZERO,
            run_id,
            job_id,
            key_generation,
            lane: Arc::clone(&lane),
            lane_generation,
            group_generation,
            state: TaskState::Queued,
            cancel_reason: None,
            next_wake_at: None,
        };
        let (snapshot, _) = watch::channel(snapshot);
        Self {
            run_id,
            job_id,
            lane,
            lane_generation,
            group_generation,
            group,
            observability,
            command_gate: Mutex::new(()),
            manual: ManualControl::new(trigger_policy),
            key_registration: Mutex::new(None),
            cancellation: Arc::new(Cancellation::new()),
            snapshot,
            terminal: AtomicBool::new(false),
            abort_requested: AtomicBool::new(false),
            abort: Mutex::new(None),
        }
    }

    fn transition(&self, state: TaskState) {
        self.transition_with_delay(state, None);
    }

    fn transition_with_delay(&self, state: TaskState, delay: Option<Duration>) {
        if self.terminal.load(Ordering::Acquire) {
            return;
        }
        let reason = self.cancellation.reason();
        self.snapshot.send_modify(|snapshot| {
            // The watch value must never regress after the terminal CAS.  The
            // second check is inside the watch lock: either this update wins
            // first and terminal publication overwrites it, or terminal wins
            // first and this update becomes a no-op.
            if !self.terminal.load(Ordering::Acquire)
                && (state == TaskState::CancelRequested
                    || snapshot.state != TaskState::CancelRequested)
            {
                self.observability
                    .publish(TaskEventKind::StateChanged, |sequence, observed_at| {
                        snapshot.snapshot_version = sequence;
                        snapshot.observed_at = observed_at;
                        snapshot.state = state;
                        snapshot.cancel_reason = reason;
                        snapshot.next_wake_at =
                            delay.and_then(|delay| observed_at.checked_add(delay));
                        self.event_data(snapshot)
                    });
            }
        });
    }

    fn publish_submitted(&self) {
        self.snapshot.send_modify(|snapshot| {
            self.observability
                .publish(TaskEventKind::Submitted, |sequence, observed_at| {
                    snapshot.snapshot_version = sequence;
                    snapshot.observed_at = observed_at;
                    self.event_data(snapshot)
                });
        });
    }

    fn event_data(&self, snapshot: &TaskSnapshot) -> EventData {
        EventData {
            run_id: self.run_id,
            job_id: self.job_id,
            lane: Arc::clone(&self.lane),
            lane_generation: self.lane_generation,
            group_generation: self.group_generation,
            state: snapshot.state,
            cancel_reason: snapshot.cancel_reason,
            next_wake_at: snapshot.next_wake_at,
        }
    }

    fn publish_terminal(&self, state: TaskState) -> bool {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .terminal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let reason = self.cancellation.reason();
        self.snapshot.send_modify(|snapshot| {
            self.observability
                .publish(TaskEventKind::Terminal, |sequence, observed_at| {
                    snapshot.snapshot_version = sequence;
                    snapshot.observed_at = observed_at;
                    snapshot.state = state;
                    snapshot.cancel_reason = reason;
                    snapshot.next_wake_at = None;
                    self.event_data(snapshot)
                });
        });
        true
    }

    fn pause(&self) -> Result<bool, TaskCommandError> {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.terminal.load(Ordering::Acquire) {
            return Err(TaskCommandError::Terminal);
        }
        Ok(self.manual.pause_task())
    }

    fn resume(&self) -> Result<bool, TaskCommandError> {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.terminal.load(Ordering::Acquire) {
            return Err(TaskCommandError::Terminal);
        }
        if self
            .group
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|group| group.paused.load(Ordering::Acquire))
        {
            return Err(TaskCommandError::ScopePaused);
        }
        Ok(self.manual.resume_task())
    }

    fn run_now(&self) -> Result<TriggerOutcome, TaskCommandError> {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.terminal.load(Ordering::Acquire) {
            return Err(TaskCommandError::Terminal);
        }
        if self
            .group
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|group| group.paused.load(Ordering::Acquire))
        {
            return Ok(TriggerOutcome::Paused);
        }
        self.manual.run_now(self.snapshot().state())
    }

    fn pause_scope(&self) -> Result<bool, TaskCommandError> {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.terminal.load(Ordering::Acquire) {
            return Err(TaskCommandError::Terminal);
        }
        Ok(self.manual.pause_scope())
    }

    fn resume_scope(&self) -> Result<bool, TaskCommandError> {
        let _commands = self
            .command_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.terminal.load(Ordering::Acquire) {
            return Err(TaskCommandError::Terminal);
        }
        Ok(self.manual.resume_scope())
    }

    fn cancel(&self, reason: CancelReason) -> bool {
        if self.terminal.load(Ordering::Acquire) {
            return false;
        }
        let first = self.cancellation.cancel(reason);
        if first {
            self.transition(TaskState::CancelRequested);
        }
        first
    }

    fn snapshot(&self) -> TaskSnapshot {
        self.snapshot.borrow().clone()
    }

    async fn wait_terminal(&self) -> TaskSnapshot {
        let mut snapshots = self.snapshot.subscribe();
        loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if snapshot.state().is_terminal() {
                return snapshot;
            }
            if snapshots.changed().await.is_err() {
                return self.snapshot();
            }
        }
    }

    fn request_abort(&self) -> bool {
        if self.terminal.load(Ordering::Acquire)
            || self.abort_requested.swap(true, Ordering::AcqRel)
        {
            return false;
        }
        let abort = self
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(abort) = abort {
            abort.abort();
            true
        } else {
            self.abort_requested.store(false, Ordering::Release);
            false
        }
    }
}

#[derive(Clone)]
/// Cloneable, payload-free observation handle for one task run.
pub struct TaskObserver {
    shared: Arc<RunShared>,
    snapshots: watch::Receiver<TaskSnapshot>,
}

impl TaskObserver {
    /// Returns the unique run identity.
    pub fn id(&self) -> TaskRunId {
        self.shared.run_id
    }

    /// Returns the reusable job identity.
    pub fn job_id(&self) -> JobId {
        self.shared.job_id
    }

    /// Returns the latest observed lifecycle state.
    pub fn state(&self) -> TaskState {
        self.shared.snapshot().state()
    }

    /// Returns the latest payload-free task snapshot.
    pub fn snapshot(&self) -> TaskSnapshot {
        self.shared.snapshot()
    }

    /// Waits for the single authoritative terminal state.
    pub async fn wait_terminal(mut self) -> TaskSnapshot {
        loop {
            let snapshot = self.snapshots.borrow_and_update().clone();
            if snapshot.state().is_terminal() {
                return snapshot;
            }
            if self.snapshots.changed().await.is_err() {
                return self.shared.snapshot();
            }
        }
    }
}

#[derive(Clone)]
/// Cloneable command and observation handle without result ownership.
pub struct TaskController {
    observer: TaskObserver,
}

impl TaskController {
    /// Returns the unique run identity.
    pub fn id(&self) -> TaskRunId {
        self.observer.id()
    }

    /// Requests cooperative cancellation, returning true only for the first
    /// accepted request.
    pub fn cancel(&self, reason: CancelReason) -> bool {
        self.observer.shared.cancel(reason)
    }

    /// Creates a payload-free observer for this run.
    pub fn observer(&self) -> TaskObserver {
        self.observer.clone()
    }

    /// Returns the latest observed lifecycle state.
    pub fn state(&self) -> TaskState {
        self.observer.state()
    }

    /// Requests a cooperative pause at the next Scheduler-controlled boundary.
    /// A user future that is already running is never dropped by this command.
    pub fn pause(&self) -> Result<bool, TaskCommandError> {
        self.observer.shared.pause()
    }

    /// Resumes a cooperatively paused task; returns whether state changed.
    pub fn resume(&self) -> Result<bool, TaskCommandError> {
        self.observer.shared.resume()
    }

    /// Returns whether a task-level or owning-group pause is currently active.
    pub fn is_paused(&self) -> bool {
        self.observer.shared.manual.is_paused()
    }

    /// Requests an early next run for a scheduled job using the policy frozen
    /// on its [`ScheduledJob`]. Retry backoff is never bypassed.
    pub fn run_now(&self) -> Result<TriggerOutcome, TaskCommandError> {
        self.observer.shared.run_now()
    }

    /// Waits for the authoritative terminal snapshot without owning the result
    /// payload.
    pub async fn wait_terminal(self) -> TaskSnapshot {
        self.observer.wait_terminal().await
    }
}

/// Unique owner of a task run's typed terminal result.
///
/// Dropping this handle detaches; it does not cancel the task.
pub struct TaskHandle<T, E> {
    observer: TaskObserver,
    result: oneshot::Receiver<TaskTerminal<T, E>>,
}

impl<T, E> TaskHandle<T, E> {
    /// Returns the unique run identity.
    pub fn id(&self) -> TaskRunId {
        self.observer.id()
    }

    /// Returns the reusable job identity.
    pub fn job_id(&self) -> JobId {
        self.observer.job_id()
    }

    /// Creates a payload-free observer.
    pub fn observer(&self) -> TaskObserver {
        self.observer.clone()
    }

    /// Creates a cloneable cancellation/control handle.
    pub fn controller(&self) -> TaskController {
        TaskController {
            observer: self.observer.clone(),
        }
    }

    /// Wraps this result owner in a guard that requests cancellation if dropped
    /// before being disarmed or joined.
    pub fn cancel_on_drop(self, reason: CancelReason) -> CancelOnDrop<T, E> {
        let guard = CancelGuard {
            shared: Arc::clone(&self.observer.shared),
            reason,
            armed: true,
        };
        CancelOnDrop {
            guard,
            handle: self,
        }
    }

    /// Consumes the unique result owner and waits for the typed terminal.
    pub async fn join(mut self) -> TaskTerminal<T, E> {
        receive_terminal(&self.observer, &mut self.result).await
    }

    /// Requests cooperative cancellation and waits for the actual terminal;
    /// cancellation is not assumed to win a race with completion.
    pub async fn cancel_and_wait(mut self, reason: CancelReason) -> TaskTerminal<T, E> {
        self.observer.shared.cancel(reason);
        receive_terminal(&self.observer, &mut self.result).await
    }
}

async fn receive_terminal<T, E>(
    observer: &TaskObserver,
    result: &mut oneshot::Receiver<TaskTerminal<T, E>>,
) -> TaskTerminal<T, E> {
    match result.await {
        Ok(terminal) => terminal,
        Err(_) => {
            if observer.shared.snapshot().state() == TaskState::ExecutorError {
                return TaskTerminal::ExecutorError {
                    code: ExecutorErrorCode::CompletionAlreadyPublished,
                };
            }
            observer.shared.publish_terminal(TaskState::ExecutorStopped);
            TaskTerminal::ExecutorStopped
        }
    }
}

/// Opt-in RAII wrapper that requests cancellation while armed when dropped.
pub struct CancelOnDrop<T, E> {
    // Keep the guard first so cancellation is requested before the result
    // receiver is dropped when an in-progress `join` future is abandoned.
    guard: CancelGuard,
    handle: TaskHandle<T, E>,
}

struct CancelGuard {
    shared: Arc<RunShared>,
    reason: CancelReason,
    armed: bool,
}

impl<T, E> CancelOnDrop<T, E> {
    /// Returns the guarded run identity.
    pub fn id(&self) -> TaskRunId {
        self.handle.id()
    }

    /// Creates a payload-free observer for the guarded run.
    pub fn observer(&self) -> TaskObserver {
        self.handle.observer()
    }

    /// Creates a cloneable controller for the guarded run.
    pub fn controller(&self) -> TaskController {
        self.handle.controller()
    }

    /// Returns the cancellation reason used if this guard is dropped armed.
    pub fn reason(&self) -> CancelReason {
        self.guard.reason
    }

    /// Disables cancellation-on-drop and returns the original result owner.
    pub fn disarm(mut self) -> TaskHandle<T, E> {
        self.guard.armed = false;
        self.handle
    }

    /// Waits for the typed terminal without issuing the guard's cancellation.
    pub async fn join(mut self) -> TaskTerminal<T, E> {
        let terminal = receive_terminal(&self.handle.observer, &mut self.handle.result).await;
        self.guard.armed = false;
        terminal
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.cancel(self.reason);
        }
    }
}

#[non_exhaustive]
/// A scheduler submission failure that returns ownership of the original job.
pub enum TrySubmitError<J> {
    /// Reject backpressure found the destination lane at capacity.
    Full(J),
    /// Scheduler shutdown has started but has not yet fully terminated.
    ShuttingDown(J),
    /// The scheduler or selected lane is closed.
    Closed(J),
    /// The selected lane name is not registered.
    LaneNotFound(J),
}

impl<J> TrySubmitError<J> {
    /// Returns the payload-free rejection category.
    pub fn reason(&self) -> SubmissionFailure {
        match self {
            Self::Full(_) => SubmissionFailure::Full,
            Self::ShuttingDown(_) => SubmissionFailure::ShuttingDown,
            Self::Closed(_) => SubmissionFailure::Closed,
            Self::LaneNotFound(_) => SubmissionFailure::LaneNotFound,
        }
    }

    /// Recovers the job rejected by submission.
    pub fn into_job(self) -> J {
        match self {
            Self::Full(job)
            | Self::ShuttingDown(job)
            | Self::Closed(job)
            | Self::LaneNotFound(job) => job,
        }
    }
}

impl<J> fmt::Debug for TrySubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full(_) => "Full(..)",
            Self::ShuttingDown(_) => "ShuttingDown(..)",
            Self::Closed(_) => "Closed(..)",
            Self::LaneNotFound(_) => "LaneNotFound(..)",
        })
    }
}

impl<J> fmt::Display for TrySubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("the lane queue is full"),
            Self::ShuttingDown(_) => f.write_str("the Scheduler is shutting down"),
            Self::Closed(_) => f.write_str("the Scheduler or lane is closed"),
            Self::LaneNotFound(_) => f.write_str("the requested lane does not exist"),
        }
    }
}

impl<J> std::error::Error for TrySubmitError<J> {}

/// Error returned by asynchronous submission.
pub type SubmitError<J> = TrySubmitError<J>;
type LaneLookup<T, E> = Result<(Job<T, E>, Arc<LaneInner>), TrySubmitError<Job<T, E>>>;

struct LaneInner {
    config: LaneConfig,
    generation: u64,
    queue_slots: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    closed: AtomicBool,
}

struct KeyRegistryState {
    current: HashMap<Arc<str>, KeyEntry>,
    generations: HashMap<Arc<str>, u64>,
}

enum KeyEntry {
    Reserved {
        token: Uuid,
        generation: u64,
        previous: Option<(u64, Arc<RunShared>)>,
        stopped: bool,
    },
    Running {
        generation: u64,
        run: Arc<RunShared>,
    },
}

impl KeyEntry {
    fn status(&self) -> KeyStatus {
        match self {
            Self::Reserved {
                generation,
                previous,
                ..
            } => KeyStatus {
                generation: *generation,
                run_id: previous.as_ref().map(|(_, run)| run.run_id),
                replacing: true,
            },
            Self::Running { generation, run } => KeyStatus {
                generation: *generation,
                run_id: Some(run.run_id),
                replacing: false,
            },
        }
    }
}

impl KeyRegistryState {
    fn new() -> Self {
        Self {
            current: HashMap::new(),
            generations: HashMap::new(),
        }
    }

    fn remove_terminal(&mut self, key: &str) {
        if self.current.get(key).is_some_and(|entry| {
            matches!(entry, KeyEntry::Running { run, .. } if run.terminal.load(Ordering::Acquire))
        }) {
            self.current.remove(key);
        }
    }

    fn next_generation(&mut self, key: &Arc<str>) -> Option<u64> {
        let next = self
            .generations
            .get(key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)?;
        self.generations.insert(Arc::clone(key), next);
        Some(next)
    }
}

#[derive(Clone)]
struct KeyRegistration {
    registry: Weak<Mutex<KeyRegistryState>>,
    key: Arc<str>,
    generation: u64,
}

impl KeyRegistration {
    fn remove(&self, run_id: TaskRunId) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.current.get(&self.key).is_some_and(|entry| {
            matches!(entry, KeyEntry::Running { generation, run }
                if *generation == self.generation && run.run_id == run_id)
        }) {
            registry.current.remove(&self.key);
        }
    }
}

struct KeyReservation {
    registry: Arc<Mutex<KeyRegistryState>>,
    key: Arc<str>,
    token: Uuid,
    generation: u64,
    previous: Option<(u64, Arc<RunShared>)>,
    armed: bool,
}

impl KeyReservation {
    fn is_active(&self) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .get(&self.key)
            .is_some_and(|entry| {
                matches!(entry, KeyEntry::Reserved { token, stopped: false, .. } if *token == self.token)
            })
    }

    fn commit(mut self, run: &Arc<RunShared>) -> bool {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = registry.current.get(&self.key).is_some_and(|entry| {
            matches!(entry, KeyEntry::Reserved { token, stopped: false, .. } if *token == self.token)
        });
        if !active || run.terminal.load(Ordering::Acquire) {
            if registry.current.get(&self.key).is_some_and(
                |entry| matches!(entry, KeyEntry::Reserved { token, .. } if *token == self.token),
            ) {
                registry.current.remove(&self.key);
            }
            self.armed = false;
            return false;
        }
        *run.key_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(KeyRegistration {
            registry: Arc::downgrade(&self.registry),
            key: Arc::clone(&self.key),
            generation: self.generation,
        });
        registry.current.insert(
            Arc::clone(&self.key),
            KeyEntry::Running {
                generation: self.generation,
                run: Arc::clone(run),
            },
        );
        self.armed = false;
        true
    }
}

impl Drop for KeyReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !registry.current.get(&self.key).is_some_and(
            |entry| matches!(entry, KeyEntry::Reserved { token, .. } if *token == self.token),
        ) {
            return;
        }
        if registry.current.get(&self.key).is_some_and(|entry| {
            matches!(entry, KeyEntry::Reserved { token, stopped: true, .. } if *token == self.token)
        }) {
            registry.current.remove(&self.key);
            return;
        }
        match self.previous.take() {
            Some((generation, run)) if !run.terminal.load(Ordering::Acquire) => {
                registry
                    .current
                    .insert(Arc::clone(&self.key), KeyEntry::Running { generation, run });
            }
            _ => {
                registry.current.remove(&self.key);
            }
        }
    }
}

fn key_status(registry: &Arc<Mutex<KeyRegistryState>>, key: &str) -> Option<KeyStatus> {
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.remove_terminal(key);
    registry.current.get(key).map(KeyEntry::status)
}

fn stop_key(registry: &Arc<Mutex<KeyRegistryState>>, key: &str, reason: CancelReason) -> bool {
    let run = {
        let mut registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.remove_terminal(key);
        match registry.current.get_mut(key) {
            Some(KeyEntry::Running { run, .. }) => Some(Arc::clone(run)),
            Some(KeyEntry::Reserved {
                previous, stopped, ..
            }) => {
                *stopped = true;
                previous.as_ref().map(|(_, run)| Arc::clone(run))
            }
            None => return false,
        }
    };
    run.is_none_or(|run| {
        let _ = run.cancel(reason);
        true
    })
}

enum ReserveKeyError {
    Busy(KeyStatus),
    GenerationExhausted,
}

fn reserve_key(
    registry: Arc<Mutex<KeyRegistryState>>,
    key: Arc<str>,
    replace: bool,
) -> Result<KeyReservation, ReserveKeyError> {
    let mut state = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.remove_terminal(&key);
    if let Some(current) = state.current.get(&key) {
        if !replace || matches!(current, KeyEntry::Reserved { .. }) {
            return Err(ReserveKeyError::Busy(current.status()));
        }
    }
    let previous = match state.current.remove(&key) {
        Some(KeyEntry::Running { generation, run }) => Some((generation, run)),
        Some(entry @ KeyEntry::Reserved { .. }) => {
            let current = entry.status();
            state.current.insert(Arc::clone(&key), entry);
            return Err(ReserveKeyError::Busy(current));
        }
        None => None,
    };
    let generation = match state.next_generation(&key) {
        Some(generation) => generation,
        None => {
            if let Some((generation, run)) = previous {
                state
                    .current
                    .insert(Arc::clone(&key), KeyEntry::Running { generation, run });
            }
            return Err(ReserveKeyError::GenerationExhausted);
        }
    };
    let token = Uuid::new_v4();
    state.current.insert(
        Arc::clone(&key),
        KeyEntry::Reserved {
            token,
            generation,
            previous: previous.clone(),
            stopped: false,
        },
    );
    drop(state);
    Ok(KeyReservation {
        registry,
        key,
        token,
        generation,
        previous,
        armed: true,
    })
}

struct GroupInner {
    name: Arc<str>,
    generation: u64,
    state: AtomicU8,
    paused: AtomicBool,
    members: Mutex<HashMap<TaskRunId, Arc<RunShared>>>,
    keys: Arc<Mutex<KeyRegistryState>>,
    scheduler: Weak<SchedulerInner>,
    runtime: Handle,
    observability: Arc<Observability>,
    shutdown_policy: ShutdownPolicy,
    shutdown: Mutex<Option<SchedulerShutdown>>,
}

impl GroupInner {
    fn state(&self) -> GroupState {
        match self.state.load(Ordering::Acquire) {
            OPEN => GroupState::Open,
            CLOSING => GroupState::Closing,
            _ => GroupState::Terminated,
        }
    }

    fn best_effort_close(&self) {
        if self
            .state
            .compare_exchange(OPEN, CLOSING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let runs = self
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in &runs {
            run.cancel(CancelReason::GroupShutdown);
        }
        if runs.is_empty() {
            self.state.store(TERMINATED, Ordering::Release);
        }
    }

    fn mark_closing(&self) {
        let _ = self
            .state
            .compare_exchange(OPEN, CLOSING, Ordering::AcqRel, Ordering::Acquire);
        if self
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            self.state.store(TERMINATED, Ordering::Release);
        }
    }

    fn remove_member(&self, run_id: TaskRunId, shared: &Arc<RunShared>) {
        let mut members = self
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if members
            .get(&run_id)
            .is_some_and(|current| Arc::ptr_eq(current, shared))
        {
            members.remove(&run_id);
        }
        if members.is_empty() && self.state.load(Ordering::Acquire) == CLOSING {
            self.state.store(TERMINATED, Ordering::Release);
        }
    }
}

#[derive(Clone)]
/// Generational task scope with isolated submission, keyed ownership, pause,
/// cancellation, and shutdown.
pub struct TaskGroup {
    inner: Arc<GroupInner>,
}

impl TaskGroup {
    pub(crate) fn bound_runtime(&self) -> Handle {
        self.inner.runtime.clone()
    }

    /// Returns the stable group name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns the generation assigned when this name was created.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Returns the current group lifecycle state.
    pub fn state(&self) -> GroupState {
        self.inner.state()
    }

    /// Returns registered group members without a terminal state.
    pub fn active_task_count(&self) -> usize {
        self.inner
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Returns whether group-level cooperative pause is active.
    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
    }

    /// Pauses current and future members at Scheduler-controlled boundaries.
    /// Already-running user futures are allowed to return normally.
    pub fn pause(&self) -> usize {
        let scheduler = self.inner.scheduler.upgrade();
        let _submit = scheduler.as_ref().map(|scheduler| {
            scheduler
                .submit_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        self.inner.paused.store(true, Ordering::Release);
        self.inner
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|run| run.pause_scope() == Ok(true))
            .count()
    }

    /// Resumes current and future members and returns how many existing member
    /// pause flags changed.
    pub fn resume(&self) -> usize {
        let scheduler = self.inner.scheduler.upgrade();
        let _submit = scheduler.as_ref().map(|scheduler| {
            scheduler
                .submit_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        self.inner.paused.store(false, Ordering::Release);
        self.inner
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|run| run.resume_scope() == Ok(true))
            .count()
    }

    /// Submits without waiting for lane capacity and atomically joins this
    /// group on success.
    pub fn try_submit<T, E>(
        &self,
        job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, TrySubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let Some(inner) = self.inner.scheduler.upgrade() else {
            return Err(TrySubmitError::Closed(job));
        };
        Scheduler { inner }.try_submit_to_group(job, Arc::clone(&self.inner))
    }

    /// Submits according to lane backpressure and atomically joins this group
    /// on success.
    pub async fn submit<T, E>(
        &self,
        job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, SubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let Some(inner) = self.inner.scheduler.upgrade() else {
            return Err(TrySubmitError::Closed(job));
        };
        Scheduler { inner }
            .submit_to_group(job, Arc::clone(&self.inner))
            .await
    }

    /// Returns current group-local keyed ownership after removing terminal
    /// entries.
    pub fn key_status(&self, key: &str) -> Option<KeyStatus> {
        key_status(&self.inner.keys, key)
    }

    /// Atomically submits a group-local keyed run only when the key is absent.
    pub fn start_if_absent<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        mut job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, KeyedSubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let key = key.into();
        let reservation = match reserve_key(Arc::clone(&self.inner.keys), Arc::clone(&key), false) {
            Ok(reservation) => reservation,
            Err(ReserveKeyError::Busy(current)) => {
                return Err(KeyedSubmitError::Occupied {
                    job,
                    current: Box::new(current),
                });
            }
            Err(ReserveKeyError::GenerationExhausted) => {
                return Err(KeyedSubmitError::GenerationExhausted(job));
            }
        };
        job.key = Some(key);
        job.key_generation = Some(reservation.generation);
        let handle = self.try_submit(job).map_err(KeyedSubmitError::Submit)?;
        reservation.commit(&handle.observer.shared);
        Ok(handle)
    }

    /// Requests cooperative user cancellation for the current group-local key.
    pub fn stop(&self, key: &str) -> bool {
        stop_key(&self.inner.keys, key, CancelReason::User)
    }

    /// Atomically replaces the current group-local key according to `policy`.
    pub async fn replace<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        job: Job<T, E>,
        policy: ReplacePolicy,
    ) -> Result<TaskHandle<T, E>, ReplaceError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let Some(inner) = self.inner.scheduler.upgrade() else {
            return Err(ReplaceError::Submit(TrySubmitError::Closed(job)));
        };
        replace_keyed(
            Scheduler { inner },
            Some(Arc::clone(&self.inner)),
            Arc::clone(&self.inner.keys),
            current_run_id(),
            key.into(),
            job,
            policy,
        )
        .await
    }

    fn start_shutdown(&self) -> watch::Receiver<SchedulerShutdownReport> {
        if let Some(scheduler) = self.inner.scheduler.upgrade() {
            let _submit = scheduler
                .submit_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.start_shutdown_locked()
        } else {
            self.start_shutdown_locked()
        }
    }

    fn start_shutdown_locked(&self) -> watch::Receiver<SchedulerShutdownReport> {
        let mut shutdown = self
            .inner
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = shutdown.as_ref() {
            return existing.progress.clone();
        }
        self.inner.state.store(CLOSING, Ordering::Release);
        let runs = self
            .inner
            .members
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in &runs {
            run.cancel(CancelReason::GroupShutdown);
        }
        let (progress_tx, progress_rx) =
            watch::channel(SchedulerShutdownReport::collect(&runs, 0, false, false));
        let group = Arc::downgrade(&self.inner);
        let policy = self.inner.shutdown_policy;
        let observability = Arc::clone(&self.inner.observability);
        let guard = ShutdownCoordinatorGuard::group(runs, progress_tx, group);
        let coordinator =
            self.inner
                .runtime
                .spawn(coordinate_group_shutdown(guard, policy, observability));
        *shutdown = Some(SchedulerShutdown {
            progress: progress_rx.clone(),
            _coordinator: coordinator,
        });
        progress_rx
    }

    /// Starts group shutdown once and waits for the shared authoritative
    /// report, except that a member calling this directly receives an immediate
    /// self-join-safe progress report.
    pub async fn shutdown(&self) -> SchedulerShutdownReport {
        let progress = self.start_shutdown();
        if current_run_id().is_some_and(|run_id| {
            self.inner
                .members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&run_id)
        }) {
            return progress.borrow().clone();
        }
        wait_for_settled_report(progress).await
    }

    /// Observes shared group shutdown with a caller-local deadline that does
    /// not change the group-wide policy.
    pub async fn shutdown_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SchedulerShutdownReport, ShutdownWaitError> {
        let mut progress = self.start_shutdown();
        if current_run_id().is_some_and(|run_id| {
            self.inner
                .members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&run_id)
        }) {
            return Ok(progress.borrow_and_update().clone());
        }
        match tokio::time::timeout(timeout, wait_for_settled_report(progress.clone())).await {
            Ok(report) => Ok(report),
            Err(_) => Err(ShutdownWaitError {
                report: progress.borrow_and_update().clone(),
            }),
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) <= 2 {
            self.inner.best_effort_close();
        }
    }
}

struct SchedulerShutdown {
    progress: watch::Receiver<SchedulerShutdownReport>,
    _coordinator: JoinHandle<()>,
}

impl LaneInner {
    fn new(config: LaneConfig, generation: u64) -> Self {
        Self {
            queue_slots: Arc::new(Semaphore::new(config.queue_capacity)),
            execution: Arc::new(Semaphore::new(config.concurrency)),
            config,
            generation,
            closed: AtomicBool::new(false),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.queue_slots.close();
        self.execution.close();
    }
}

struct SchedulerInner {
    runtime: Handle,
    global: Arc<Semaphore>,
    lanes: RwLock<HashMap<Arc<str>, Arc<LaneInner>>>,
    registry: Mutex<HashMap<TaskRunId, Arc<RunShared>>>,
    keys: Arc<Mutex<KeyRegistryState>>,
    submit_gate: Mutex<()>,
    lifecycle: AtomicU8,
    next_lane_generation: AtomicU64,
    groups: Mutex<HashMap<Arc<str>, Arc<GroupInner>>>,
    next_group_generation: AtomicU64,
    observability: Arc<Observability>,
    shutdown_policy: ShutdownPolicy,
    shutdown: Mutex<Option<SchedulerShutdown>>,
}

impl SchedulerInner {
    fn best_effort_close(&self) {
        if self
            .lifecycle
            .compare_exchange(OPEN, CLOSING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let lanes = self
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for lane in lanes.values() {
            lane.close();
        }
        drop(lanes);
        let groups = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for group in groups.values() {
            group.mark_closing();
        }
        drop(groups);
        let runs = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            run.cancel(CancelReason::Shutdown);
        }
    }
}

/// Builder for a runtime-bound [`Scheduler`].
pub struct SchedulerBuilder {
    runtime: Option<Handle>,
    global_concurrency: usize,
    default_lane: LaneConfig,
    event_capacity: usize,
    metrics_hook: Option<Arc<dyn MetricsHook>>,
    shutdown_policy: ShutdownPolicy,
}

impl SchedulerBuilder {
    /// Creates a builder with one default lane, bounded observations, and a
    /// five-second cooperative grace period.
    pub fn new() -> Self {
        Self {
            runtime: None,
            global_concurrency: Semaphore::MAX_PERMITS.min(1024),
            default_lane: LaneConfig::default(),
            event_capacity: 256,
            metrics_hook: None,
            shutdown_policy: ShutdownPolicy::default(),
        }
    }

    /// Binds all internal tasks to an explicit Tokio runtime.
    pub fn runtime_handle(mut self, runtime: Handle) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Sets the scheduler-wide simultaneous attempt limit.
    pub fn global_concurrency(mut self, concurrency: usize) -> Self {
        self.global_concurrency = concurrency;
        self
    }

    /// Replaces the configuration of the initial `default` lane.
    pub fn default_lane(mut self, config: LaneConfig) -> Self {
        self.default_lane = config;
        self
    }

    /// Sets the bounded capacity used independently by task event subscribers
    /// and the optional metrics hook queue.
    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity;
        self
    }

    /// Installs a best-effort metrics consumer. It is invoked outside task
    /// execution, and a slow or panicking hook cannot change task outcomes.
    pub fn metrics_hook(mut self, hook: Arc<dyn MetricsHook>) -> Self {
        self.metrics_hook = Some(hook);
        self
    }

    /// Freezes the policy shared by scheduler and group shutdown coordinators.
    pub fn shutdown_policy(mut self, policy: ShutdownPolicy) -> Self {
        self.shutdown_policy = policy;
        self
    }

    /// Validates configuration and creates the scheduler.
    pub fn build(self) -> Result<Scheduler, SchedulerBuildError> {
        const MAX_EVENT_CAPACITY: usize = 1_048_576;
        if self.global_concurrency == 0 || self.global_concurrency > Semaphore::MAX_PERMITS {
            return Err(SchedulerBuildError::InvalidGlobalConcurrency {
                concurrency: self.global_concurrency,
                maximum: Semaphore::MAX_PERMITS,
            });
        }
        if self.event_capacity == 0 || self.event_capacity > MAX_EVENT_CAPACITY {
            return Err(SchedulerBuildError::InvalidEventCapacity {
                capacity: self.event_capacity,
                maximum: MAX_EVENT_CAPACITY,
            });
        }
        let runtime = match self.runtime {
            Some(runtime) => runtime,
            None => Handle::try_current().map_err(|_| SchedulerBuildError::RuntimeUnavailable)?,
        };
        let mut lanes = HashMap::new();
        lanes.insert(
            Arc::<str>::from("default"),
            Arc::new(LaneInner::new(self.default_lane, 1)),
        );
        let observability = Arc::new(Observability::new(
            &runtime,
            self.event_capacity,
            self.metrics_hook,
        ));
        Ok(Scheduler {
            inner: Arc::new(SchedulerInner {
                runtime,
                global: Arc::new(Semaphore::new(self.global_concurrency)),
                lanes: RwLock::new(lanes),
                registry: Mutex::new(HashMap::new()),
                keys: Arc::new(Mutex::new(KeyRegistryState::new())),
                submit_gate: Mutex::new(()),
                lifecycle: AtomicU8::new(OPEN),
                next_lane_generation: AtomicU64::new(1),
                groups: Mutex::new(HashMap::new()),
                next_group_generation: AtomicU64::new(0),
                observability,
                shutdown_policy: self.shutdown_policy,
                shutdown: Mutex::new(None),
            }),
        })
    }
}

impl Default for SchedulerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
/// Runtime-bound asynchronous task scheduler with typed terminals, lanes,
/// task groups, keyed ownership, and coordinated shutdown.
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

impl Scheduler {
    /// Starts a scheduler builder.
    pub fn builder() -> SchedulerBuilder {
        SchedulerBuilder::new()
    }

    pub(crate) fn bound_runtime(&self) -> Handle {
        self.inner.runtime.clone()
    }

    pub(crate) fn lock_submission_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn submission_failure(&self) -> Option<SubmissionFailure> {
        match self.inner.lifecycle.load(Ordering::Acquire) {
            OPEN => None,
            CLOSING => Some(SubmissionFailure::ShuttingDown),
            _ => Some(SubmissionFailure::Closed),
        }
    }

    /// Subscribes to a bounded, best-effort stream of task lifecycle events.
    /// Slow subscribers skip overwritten events and increment the scheduler's
    /// dropped-delivery counter.
    pub fn subscribe_events(&self) -> EventReceiver {
        self.inner.observability.subscribe()
    }

    /// Captures an eventually consistent operational view. Counts are read
    /// from separate registries and are not an atomic cross-field transaction;
    /// `snapshot_version` identifies this observation, not a database commit.
    pub fn snapshot(&self) -> SchedulerSnapshot {
        let runs = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active_tasks = runs.len();
        let mut queued_tasks = 0;
        let mut running_tasks = 0;
        let mut waiting_tasks = 0;
        let mut cancel_requested_tasks = 0;
        for run in runs.values() {
            match run.snapshot().state() {
                TaskState::Queued | TaskState::WaitingForPermit => queued_tasks += 1,
                TaskState::Running => running_tasks += 1,
                TaskState::WaitingForRetry | TaskState::WaitingForSchedule | TaskState::Paused => {
                    waiting_tasks += 1;
                }
                TaskState::CancelRequested => cancel_requested_tasks += 1,
                _ => {}
            }
        }
        drop(runs);
        let shutting_down = self.inner.lifecycle.load(Ordering::Acquire) != OPEN;
        let lanes = self
            .inner
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let groups = self
            .inner
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let (snapshot_version, _) = self.inner.observability.observe();
        SchedulerSnapshot::new(SchedulerSnapshotData {
            snapshot_version,
            active_tasks,
            queued_tasks,
            running_tasks,
            waiting_tasks,
            cancel_requested_tasks,
            shutting_down,
            lanes,
            groups,
            dropped_event_deliveries: self.inner.observability.dropped_deliveries(),
            observation_failures: self.inner.observability.observation_failures(),
        })
    }

    /// Creates the next generation of a named task group when no live
    /// generation owns that name.
    pub fn create_group(&self, name: impl Into<Arc<str>>) -> Result<TaskGroup, GroupError> {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.lifecycle.load(Ordering::Acquire) != OPEN {
            return Err(GroupError::SchedulerClosed);
        }
        let name = name.into();
        let mut groups = self
            .inner
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = groups.get(&name) {
            let state = existing.state();
            if state != GroupState::Terminated {
                return Err(GroupError::NameInUse {
                    name: name.to_string(),
                    generation: existing.generation,
                    state,
                });
            }
        }
        let generation = self
            .inner
            .next_group_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| GroupError::GenerationExhausted)?
            + 1;
        let group = Arc::new(GroupInner {
            name: Arc::clone(&name),
            generation,
            state: AtomicU8::new(OPEN),
            paused: AtomicBool::new(false),
            members: Mutex::new(HashMap::new()),
            keys: Arc::new(Mutex::new(KeyRegistryState::new())),
            scheduler: Arc::downgrade(&self.inner),
            runtime: self.inner.runtime.clone(),
            observability: Arc::clone(&self.inner.observability),
            shutdown_policy: self.inner.shutdown_policy,
            shutdown: Mutex::new(None),
        });
        groups.insert(name, Arc::clone(&group));
        Ok(TaskGroup { inner: group })
    }

    /// Returns current scheduler-local keyed ownership after removing terminal
    /// entries.
    pub fn key_status(&self, key: &str) -> Option<KeyStatus> {
        key_status(&self.inner.keys, key)
    }

    /// Atomically submits a scheduler-local keyed run only when the key is
    /// absent.
    pub fn start_if_absent<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        mut job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, KeyedSubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let key = key.into();
        let reservation = match reserve_key(Arc::clone(&self.inner.keys), Arc::clone(&key), false) {
            Ok(reservation) => reservation,
            Err(ReserveKeyError::Busy(current)) => {
                return Err(KeyedSubmitError::Occupied {
                    job,
                    current: Box::new(current),
                });
            }
            Err(ReserveKeyError::GenerationExhausted) => {
                return Err(KeyedSubmitError::GenerationExhausted(job));
            }
        };
        job.key = Some(key);
        job.key_generation = Some(reservation.generation);
        let handle = self.try_submit(job).map_err(KeyedSubmitError::Submit)?;
        reservation.commit(&handle.observer.shared);
        Ok(handle)
    }

    /// Requests cooperative user cancellation for the current scheduler-local
    /// key.
    pub fn stop(&self, key: &str) -> bool {
        stop_key(&self.inner.keys, key, CancelReason::User)
    }

    /// Atomically replaces the current scheduler-local key according to
    /// `policy`.
    pub async fn replace<T, E>(
        &self,
        key: impl Into<Arc<str>>,
        job: Job<T, E>,
        policy: ReplacePolicy,
    ) -> Result<TaskHandle<T, E>, ReplaceError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        replace_keyed(
            self.clone(),
            None,
            Arc::clone(&self.inner.keys),
            current_run_id(),
            key.into(),
            job,
            policy,
        )
        .await
    }

    /// Creates a named lane or verifies that an existing live lane has exactly
    /// the requested configuration.
    pub fn ensure_lane(
        &self,
        name: impl Into<Arc<str>>,
        config: LaneConfig,
    ) -> Result<(), LaneError> {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.lifecycle.load(Ordering::Acquire) != OPEN {
            return Err(LaneError::SchedulerClosed);
        }
        let name = name.into();
        let mut lanes = self
            .inner
            .lanes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match lanes.get(&name) {
            Some(existing) if existing.closed.load(Ordering::Acquire) => Err(LaneError::Closed {
                name: name.to_string(),
                generation: existing.generation,
            }),
            Some(existing) if existing.config == config => Ok(()),
            Some(existing) => Err(LaneError::ConfigConflict {
                name: name.to_string(),
                existing: existing.config.clone(),
                requested: config,
            }),
            None => {
                let generation = self
                    .inner
                    .next_lane_generation
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        current.checked_add(1)
                    })
                    .map_err(|_| LaneError::GenerationExhausted)?
                    + 1;
                lanes.insert(name, Arc::new(LaneInner::new(config, generation)));
                Ok(())
            }
        }
    }

    /// Returns the currently registered generation for a lane name.
    pub fn lane_generation(&self, name: &str) -> Option<u64> {
        self.inner
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|lane| lane.generation)
    }

    /// Atomically closes a lane to new submissions, requests cancellation of
    /// its active generation, and returns the accepted cancellation count.
    pub fn close_lane(&self, name: &str) -> Result<usize, LaneError> {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.lifecycle.load(Ordering::Acquire) != OPEN {
            return Err(LaneError::SchedulerClosed);
        }
        let lane = self
            .inner
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
            .ok_or_else(|| LaneError::NotFound {
                name: name.to_owned(),
            })?;
        lane.close();

        let runs = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|run| run.lane.as_ref() == name && run.lane_generation == lane.generation)
            .cloned()
            .collect::<Vec<_>>();
        let cancelled = runs
            .iter()
            .filter(|run| run.cancel(CancelReason::LaneClosed))
            .count();
        Ok(cancelled)
    }

    /// Deletes a closed lane only after all references to its generation are
    /// released, returning the deleted generation.
    pub fn delete_lane(&self, name: &str) -> Result<u64, LaneError> {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.lifecycle.load(Ordering::Acquire) != OPEN {
            return Err(LaneError::SchedulerClosed);
        }
        let mut lanes = self
            .inner
            .lanes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lane = lanes.get(name).ok_or_else(|| LaneError::NotFound {
            name: name.to_owned(),
        })?;
        if !lane.closed.load(Ordering::Acquire) {
            return Err(LaneError::StillOpen {
                name: name.to_owned(),
                generation: lane.generation,
            });
        }
        let active = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|run| run.lane.as_ref() == name && run.lane_generation == lane.generation);
        if active || Arc::strong_count(lane) != 1 {
            return Err(LaneError::Busy {
                name: name.to_owned(),
                generation: lane.generation,
            });
        }
        let generation = lane.generation;
        lanes.remove(name);
        Ok(generation)
    }

    /// Returns registered task runs without a terminal state.
    pub fn active_task_count(&self) -> usize {
        self.inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn start_shutdown(&self) -> watch::Receiver<SchedulerShutdownReport> {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut shutdown = self
            .inner
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = shutdown.as_ref() {
            return existing.progress.clone();
        }

        self.inner.lifecycle.store(CLOSING, Ordering::Release);
        let lanes = self
            .inner
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for lane in lanes.values() {
            lane.close();
        }
        drop(lanes);
        let groups = self
            .inner
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for group in groups.values() {
            group.mark_closing();
        }
        drop(groups);

        let runs = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in &runs {
            run.cancel(CancelReason::Shutdown);
        }

        let initial = SchedulerShutdownReport::collect(&runs, 0, false, false);
        let (progress_tx, progress_rx) = watch::channel(initial);
        let policy = self.inner.shutdown_policy;
        let scheduler = Arc::downgrade(&self.inner);
        let observability = Arc::clone(&self.inner.observability);
        let guard = ShutdownCoordinatorGuard::scheduler(runs, progress_tx, scheduler);
        let coordinator =
            self.inner
                .runtime
                .spawn(coordinate_shutdown(guard, policy, observability));
        *shutdown = Some(SchedulerShutdown {
            progress: progress_rx.clone(),
            _coordinator: coordinator,
        });
        progress_rx
    }

    /// Starts the builder-defined shutdown policy once. Concurrent and later
    /// callers observe the same coordinator and authoritative report stream.
    pub async fn shutdown(&self) -> SchedulerShutdownReport {
        let progress = self.start_shutdown();
        if current_run_id().is_some_and(|run_id| {
            self.inner
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&run_id)
        }) {
            return progress.borrow().clone();
        }
        wait_for_settled_report(progress).await
    }

    /// Applies a deadline only to this caller's observation. It never changes
    /// the policy frozen by [`SchedulerBuilder::shutdown_policy`].
    pub async fn shutdown_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SchedulerShutdownReport, ShutdownWaitError> {
        let mut progress = self.start_shutdown();
        if current_run_id().is_some_and(|run_id| {
            self.inner
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&run_id)
        }) {
            return Ok(progress.borrow_and_update().clone());
        }
        let wait = wait_for_settled_report(progress.clone());
        match tokio::time::timeout(timeout, wait).await {
            Ok(report) => Ok(report),
            Err(_) => Err(ShutdownWaitError {
                report: progress.borrow_and_update().clone(),
            }),
        }
    }

    fn closed_submit_error<T, E>(&self, job: Job<T, E>) -> TrySubmitError<Job<T, E>> {
        if self.inner.lifecycle.load(Ordering::Acquire) == CLOSING {
            TrySubmitError::ShuttingDown(job)
        } else {
            TrySubmitError::Closed(job)
        }
    }

    fn lane<T, E>(&self, job: Job<T, E>) -> LaneLookup<T, E> {
        match self.inner.lifecycle.load(Ordering::Acquire) {
            OPEN => {}
            CLOSING => return Err(TrySubmitError::ShuttingDown(job)),
            _ => return Err(TrySubmitError::Closed(job)),
        }
        let lane = self
            .inner
            .lanes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&job.lane)
            .cloned();
        match lane {
            Some(lane) if !lane.closed.load(Ordering::Acquire) => Ok((job, lane)),
            Some(_) => Err(TrySubmitError::Closed(job)),
            None => Err(TrySubmitError::LaneNotFound(job)),
        }
    }

    /// Submits without waiting for lane capacity.
    pub fn try_submit<T, E>(
        &self,
        job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, TrySubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (job, lane) = self.lane(job)?;
        let queue_slot = match Arc::clone(&lane.queue_slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(TrySubmitError::Full(job));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(self.closed_submit_error(job));
            }
        };
        self.commit(job, lane, queue_slot, None)
    }

    async fn prepare_submit<T, E>(
        &self,
        job: Job<T, E>,
    ) -> Result<(Job<T, E>, Arc<LaneInner>, OwnedSemaphorePermit), TrySubmitError<Job<T, E>>> {
        let (job, lane) = self.lane(job)?;
        let queue_slot = if lane.config.backpressure == Backpressure::Reject {
            match Arc::clone(&lane.queue_slots).try_acquire_owned() {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(TrySubmitError::Full(job));
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(self.closed_submit_error(job));
                }
            }
        } else {
            match Arc::clone(&lane.queue_slots).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return Err(self.closed_submit_error(job)),
            }
        };
        Ok((job, lane, queue_slot))
    }

    /// Submits according to the selected lane's backpressure policy.
    pub async fn submit<T, E>(
        &self,
        job: Job<T, E>,
    ) -> Result<TaskHandle<T, E>, SubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (job, lane) = self.lane(job)?;
        if lane.config.backpressure == Backpressure::Reject {
            let queue_slot = match Arc::clone(&lane.queue_slots).try_acquire_owned() {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(TrySubmitError::Full(job));
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(self.closed_submit_error(job));
                }
            };
            return self.commit(job, lane, queue_slot, None);
        }
        let queue_slot = match Arc::clone(&lane.queue_slots).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return Err(self.closed_submit_error(job)),
        };
        self.commit(job, lane, queue_slot, None)
    }

    fn try_submit_to_group<T, E>(
        &self,
        job: Job<T, E>,
        group: Arc<GroupInner>,
    ) -> Result<TaskHandle<T, E>, TrySubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (job, lane) = self.lane(job)?;
        let queue_slot = match Arc::clone(&lane.queue_slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(TrySubmitError::Full(job));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(self.closed_submit_error(job));
            }
        };
        self.commit(job, lane, queue_slot, Some(group))
    }

    async fn submit_to_group<T, E>(
        &self,
        job: Job<T, E>,
        group: Arc<GroupInner>,
    ) -> Result<TaskHandle<T, E>, SubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (job, lane) = self.lane(job)?;
        let queue_slot = if lane.config.backpressure == Backpressure::Reject {
            match Arc::clone(&lane.queue_slots).try_acquire_owned() {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(TrySubmitError::Full(job));
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(self.closed_submit_error(job));
                }
            }
        } else {
            match Arc::clone(&lane.queue_slots).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return Err(self.closed_submit_error(job)),
            }
        };
        self.commit(job, lane, queue_slot, Some(group))
    }

    fn commit<T, E>(
        &self,
        job: Job<T, E>,
        lane: Arc<LaneInner>,
        queue_slot: OwnedSemaphorePermit,
        group: Option<Arc<GroupInner>>,
    ) -> Result<TaskHandle<T, E>, TrySubmitError<Job<T, E>>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let _submit = self
            .inner
            .submit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lifecycle = self.inner.lifecycle.load(Ordering::Acquire);
        if lifecycle != OPEN {
            return Err(if lifecycle == CLOSING {
                TrySubmitError::ShuttingDown(job)
            } else {
                TrySubmitError::Closed(job)
            });
        }
        if lane.closed.load(Ordering::Acquire)
            || group
                .as_ref()
                .is_some_and(|group| group.state.load(Ordering::Acquire) != OPEN)
        {
            return Err(TrySubmitError::Closed(job));
        }

        let mut registry = self
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run_id = loop {
            let candidate = TaskRunId::new();
            if !registry.contains_key(&candidate) {
                break candidate;
            }
        };
        let Job {
            id: job_id,
            key,
            key_generation,
            lane: lane_name,
            kind,
            retry,
            schedule,
            retry_exhausted_action,
        } = job;
        let trigger_policy = kind.trigger_policy();
        let shared = Arc::new(RunShared::new(
            RunMetadata {
                run_id,
                job_id,
                key_generation,
                lane: lane_name,
                lane_generation: lane.generation,
            },
            group.as_ref().map(Arc::downgrade),
            Arc::clone(&self.inner.observability),
            trigger_policy,
        ));
        if group
            .as_ref()
            .is_some_and(|group| group.paused.load(Ordering::Acquire))
        {
            shared.manual.pause_scope();
        }
        registry.insert(run_id, Arc::clone(&shared));
        if let Some(group) = &group {
            group
                .members
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(run_id, Arc::clone(&shared));
        }
        drop(registry);
        shared.publish_submitted();

        let children = Arc::new(TrackedChildren::new(self.inner.runtime.clone()));
        let context = TaskContext {
            run_id,
            job_id,
            key,
            key_generation,
            run_index: 0,
            attempt: 1,
            cancellation: Arc::clone(&shared.cancellation),
            children: Arc::clone(&children),
            scheduler: Arc::downgrade(&self.inner),
            group: group.as_ref().map(Arc::downgrade),
        };
        let global = Arc::clone(&self.inner.global);
        let queue = Arc::clone(&lane.queue_slots);
        let execution = Arc::clone(&lane.execution);
        let execution_shared = Arc::clone(&shared);
        let submitted_at = tokio::time::Instant::now();
        let actual = self
            .inner
            .runtime
            .spawn(CURRENT_RUN_ID.scope(run_id, async move {
                execute_job(
                    kind,
                    retry,
                    schedule,
                    retry_exhausted_action,
                    context,
                    ExecutionResources {
                        shared: execution_shared,
                        children,
                        queue_slot,
                        queue,
                        global,
                        lane: execution,
                        submitted_at,
                    },
                )
                .await
            }));
        *shared
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(actual.abort_handle());

        let (result_tx, result) = oneshot::channel();
        let monitor_shared = Arc::clone(&shared);
        let scheduler = Arc::downgrade(&self.inner);
        let completion =
            RunCompletion::new(Arc::clone(&monitor_shared), scheduler, run_id, result_tx);
        self.inner.runtime.spawn(async move {
            let terminal = match actual.await {
                Ok(terminal) => terminal,
                Err(error) if error.is_panic() => {
                    if let Some(reason) = monitor_shared.cancellation.claim_outcome() {
                        TaskTerminal::Cancelled { reason }
                    } else {
                        let payload = error.into_panic();
                        let message = if let Some(message) = payload.downcast_ref::<&'static str>()
                        {
                            Some((*message).to_string())
                        } else {
                            payload.downcast_ref::<String>().cloned()
                        };
                        TaskTerminal::Panicked(PanicInfo { message })
                    }
                }
                Err(_) if monitor_shared.abort_requested.load(Ordering::Acquire) => {
                    TaskTerminal::Aborted
                }
                Err(_) => TaskTerminal::ExecutorStopped,
            };
            completion.finish(terminal);
        });

        Ok(TaskHandle {
            observer: TaskObserver {
                snapshots: shared.snapshot.subscribe(),
                shared,
            },
            result,
        })
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.best_effort_close();
        }
    }
}

async fn replace_keyed<T, E>(
    scheduler: Scheduler,
    group: Option<Arc<GroupInner>>,
    registry: Arc<Mutex<KeyRegistryState>>,
    caller_run_id: Option<TaskRunId>,
    key: Arc<str>,
    job: Job<T, E>,
    policy: ReplacePolicy,
) -> Result<TaskHandle<T, E>, ReplaceError<Job<T, E>>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    if group
        .as_ref()
        .is_some_and(|group| group.state.load(Ordering::Acquire) != OPEN)
    {
        return Err(ReplaceError::Submit(TrySubmitError::Closed(job)));
    }
    let (mut job, lane, queue_slot) = scheduler
        .prepare_submit(job)
        .await
        .map_err(ReplaceError::Submit)?;
    let reservation = match reserve_key(registry, Arc::clone(&key), true) {
        Ok(reservation) => reservation,
        Err(ReserveKeyError::Busy(current)) => {
            return Err(ReplaceError::Busy { job, current });
        }
        Err(ReserveKeyError::GenerationExhausted) => {
            return Err(ReplaceError::GenerationExhausted(job));
        }
    };
    let previous = reservation.previous.clone();
    if let Some((previous_generation, previous_run)) = &previous {
        let previous_status = KeyStatus {
            generation: *previous_generation,
            run_id: Some(previous_run.run_id),
            replacing: false,
        };
        if caller_run_id == Some(previous_run.run_id)
            && policy.mode == ReplaceMode::AfterConfirmedStop
        {
            return Err(ReplaceError::SelfReplacementRequiresOverlap(job));
        }
        previous_run.cancel(CancelReason::Replaced);
        if policy.mode == ReplaceMode::AfterConfirmedStop
            && tokio::time::timeout(policy.grace_period, previous_run.wait_terminal())
                .await
                .is_err()
        {
            match policy.timeout_action {
                ReplaceTimeoutAction::Fail => {
                    return Err(ReplaceError::TimedOut {
                        job,
                        previous: previous_status,
                    });
                }
                ReplaceTimeoutAction::ContinueWait => {
                    previous_run.wait_terminal().await;
                }
                ReplaceTimeoutAction::Abort {
                    confirmation_timeout,
                } => {
                    previous_run.request_abort();
                    if tokio::time::timeout(confirmation_timeout, previous_run.wait_terminal())
                        .await
                        .is_err()
                    {
                        return Err(ReplaceError::TimedOut {
                            job,
                            previous: previous_status,
                        });
                    }
                }
            }
        }
    }

    if !reservation.is_active() {
        return Err(ReplaceError::Superseded(job));
    }
    job.key = Some(key);
    job.key_generation = Some(reservation.generation);
    let handle = scheduler
        .commit(job, lane, queue_slot, group)
        .map_err(ReplaceError::Submit)?;
    if !reservation.commit(&handle.observer.shared) {
        handle.observer.shared.cancel(CancelReason::User);
    }
    Ok(handle)
}

async fn wait_for_settled_report(
    mut progress: watch::Receiver<SchedulerShutdownReport>,
) -> SchedulerShutdownReport {
    loop {
        let report = progress.borrow_and_update().clone();
        if report.settled {
            return report;
        }
        if progress.changed().await.is_err() {
            let mut report = progress.borrow().clone();
            report.settled = true;
            return report;
        }
    }
}

async fn wait_for_runs(runs: &[Arc<RunShared>]) {
    for run in runs {
        run.wait_terminal().await;
    }
}

enum ShutdownScope {
    Scheduler(Weak<SchedulerInner>),
    Group(Weak<GroupInner>),
}

struct ShutdownCoordinatorGuard {
    runs: Vec<Arc<RunShared>>,
    progress: watch::Sender<SchedulerShutdownReport>,
    scope: ShutdownScope,
    abort_requested: usize,
    grace_timed_out: bool,
    armed: bool,
}

impl ShutdownCoordinatorGuard {
    fn scheduler(
        runs: Vec<Arc<RunShared>>,
        progress: watch::Sender<SchedulerShutdownReport>,
        scheduler: Weak<SchedulerInner>,
    ) -> Self {
        Self {
            runs,
            progress,
            scope: ShutdownScope::Scheduler(scheduler),
            abort_requested: 0,
            grace_timed_out: false,
            armed: true,
        }
    }

    fn group(
        runs: Vec<Arc<RunShared>>,
        progress: watch::Sender<SchedulerShutdownReport>,
        group: Weak<GroupInner>,
    ) -> Self {
        Self {
            runs,
            progress,
            scope: ShutdownScope::Group(group),
            abort_requested: 0,
            grace_timed_out: false,
            armed: true,
        }
    }

    fn mark_terminated(&self) {
        match &self.scope {
            ShutdownScope::Scheduler(scheduler) => {
                if let Some(scheduler) = scheduler.upgrade() {
                    scheduler.lifecycle.store(TERMINATED, Ordering::Release);
                }
            }
            ShutdownScope::Group(group) => {
                if let Some(group) = group.upgrade() {
                    group.state.store(TERMINATED, Ordering::Release);
                }
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ShutdownCoordinatorGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A bound Tokio runtime drops spawned futures during shutdown. If the
        // coordinator is among them, no later poll can refresh its watch
        // report. Claim the remaining terminal states here so every waiter
        // observes the same final snapshot as the task completion guards.
        for run in &self.runs {
            run.publish_terminal(TaskState::ExecutorStopped);
        }
        self.mark_terminated();
        self.progress.send_replace(SchedulerShutdownReport::collect(
            &self.runs,
            self.abort_requested,
            self.grace_timed_out,
            true,
        ));
    }
}

async fn coordinate_shutdown(
    mut guard: ShutdownCoordinatorGuard,
    policy: ShutdownPolicy,
    observability: Arc<Observability>,
) {
    if tokio::time::timeout(policy.grace_period, wait_for_runs(&guard.runs))
        .await
        .is_ok()
    {
        guard.mark_terminated();
        observability.drain(policy.event_drain).await;
        guard
            .progress
            .send_replace(SchedulerShutdownReport::collect(
                &guard.runs,
                0,
                false,
                true,
            ));
        guard.disarm();
        return;
    }

    let mut abort_requested = 0;
    guard.grace_timed_out = true;
    if policy.abort_after_grace {
        for run in &guard.runs {
            if run.request_abort() {
                abort_requested += 1;
            }
        }
        if tokio::time::timeout(policy.abort_wait, wait_for_runs(&guard.runs))
            .await
            .is_ok()
        {
            guard.mark_terminated();
            observability.drain(policy.event_drain).await;
            guard
                .progress
                .send_replace(SchedulerShutdownReport::collect(
                    &guard.runs,
                    abort_requested,
                    true,
                    true,
                ));
            guard.abort_requested = abort_requested;
            guard.disarm();
            return;
        }
    }
    guard.abort_requested = abort_requested;

    observability.drain(policy.event_drain).await;
    guard
        .progress
        .send_replace(SchedulerShutdownReport::collect(
            &guard.runs,
            abort_requested,
            true,
            true,
        ));

    // Keep the authoritative coordinator alive after a caller-visible timeout.
    // A later shutdown call can observe the eventual confirmed final state.
    wait_for_runs(&guard.runs).await;
    guard.mark_terminated();
    observability.drain(policy.event_drain).await;
    guard
        .progress
        .send_replace(SchedulerShutdownReport::collect(
            &guard.runs,
            abort_requested,
            true,
            true,
        ));
    guard.disarm();
}

async fn coordinate_group_shutdown(
    mut guard: ShutdownCoordinatorGuard,
    policy: ShutdownPolicy,
    observability: Arc<Observability>,
) {
    if tokio::time::timeout(policy.grace_period, wait_for_runs(&guard.runs))
        .await
        .is_ok()
    {
        guard.mark_terminated();
        observability.drain(policy.event_drain).await;
        guard
            .progress
            .send_replace(SchedulerShutdownReport::collect(
                &guard.runs,
                0,
                false,
                true,
            ));
        guard.disarm();
        return;
    }

    let mut abort_requested = 0;
    guard.grace_timed_out = true;
    if policy.abort_after_grace {
        for run in &guard.runs {
            if run.request_abort() {
                abort_requested += 1;
            }
        }
        if tokio::time::timeout(policy.abort_wait, wait_for_runs(&guard.runs))
            .await
            .is_ok()
        {
            guard.mark_terminated();
            observability.drain(policy.event_drain).await;
            guard
                .progress
                .send_replace(SchedulerShutdownReport::collect(
                    &guard.runs,
                    abort_requested,
                    true,
                    true,
                ));
            guard.abort_requested = abort_requested;
            guard.disarm();
            return;
        }
    }
    guard.abort_requested = abort_requested;

    observability.drain(policy.event_drain).await;
    guard
        .progress
        .send_replace(SchedulerShutdownReport::collect(
            &guard.runs,
            abort_requested,
            true,
            true,
        ));
    wait_for_runs(&guard.runs).await;
    guard.mark_terminated();
    observability.drain(policy.event_drain).await;
    guard
        .progress
        .send_replace(SchedulerShutdownReport::collect(
            &guard.runs,
            abort_requested,
            true,
            true,
        ));
    guard.disarm();
}

enum WaitError {
    Cancelled(CancelReason),
    TimedOut,
    Closed,
    Paused,
}

async fn acquire_or_cancel_until(
    semaphore: Arc<Semaphore>,
    shared: &RunShared,
    deadline: Option<tokio::time::Instant>,
) -> Result<OwnedSemaphorePermit, WaitError> {
    loop {
        let pause_changed = shared.manual.pause_changed.notified();
        if shared.manual.is_paused() {
            return Err(WaitError::Paused);
        }
        if let Some(deadline) = deadline {
            if deadline <= tokio::time::Instant::now() {
                return Err(WaitError::TimedOut);
            }
            tokio::select! {
                biased;
                reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                _ = pause_changed => continue,
                _ = tokio::time::sleep_until(deadline) => return Err(WaitError::TimedOut),
                permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                    Ok(permit) => return Ok(permit),
                    Err(_) => return Err(WaitError::Closed),
                }
            }
        } else {
            tokio::select! {
                biased;
                reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                _ = pause_changed => continue,
                permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                    Ok(permit) => return Ok(permit),
                    Err(_) => return Err(WaitError::Closed),
                }
            }
        }
    }
}

enum AttemptOutcome<T, E> {
    Output(Result<T, E>),
    TimedOut(TimeoutScope),
}

async fn run_attempt<T, E>(
    future: Pin<Box<dyn Future<Output = Result<JobOutput<T>, E>> + Send + 'static>>,
    attempt_timeout: Option<Duration>,
    total_deadline: Option<tokio::time::Instant>,
) -> AttemptOutcome<JobOutput<T>, E> {
    let now = tokio::time::Instant::now();
    let attempt_deadline = attempt_timeout.and_then(|duration| now.checked_add(duration));
    let deadline = match (attempt_deadline, total_deadline) {
        (Some(attempt), Some(total)) if total <= attempt => {
            Some((total, TimeoutScope::TotalElapsed))
        }
        (Some(attempt), _) => Some((attempt, TimeoutScope::Attempt)),
        (None, Some(total)) => Some((total, TimeoutScope::TotalElapsed)),
        (None, None) => None,
    };
    match deadline {
        Some((deadline, scope)) => {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => AttemptOutcome::TimedOut(scope),
                output = future => AttemptOutcome::Output(output),
            }
        }
        None => AttemptOutcome::Output(future.await),
    }
}

async fn wait_while_paused(
    shared: &RunShared,
    total_deadline: Option<tokio::time::Instant>,
) -> Result<(), WaitError> {
    loop {
        let pause_changed = shared.manual.pause_changed.notified();
        if !shared.manual.is_paused() {
            return Ok(());
        }
        if total_deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
            return Err(WaitError::TimedOut);
        }
        shared.transition(TaskState::Paused);
        if let Some(deadline) = total_deadline {
            tokio::select! {
                biased;
                reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                _ = tokio::time::sleep_until(deadline) => return Err(WaitError::TimedOut),
                _ = pause_changed => {}
            }
        } else {
            tokio::select! {
                biased;
                reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                _ = pause_changed => {}
            }
        }
    }
}

async fn wait_controlled_delay(
    delay: Duration,
    shared: &RunShared,
    total_deadline: Option<tokio::time::Instant>,
    state: TaskState,
    allow_trigger: bool,
) -> Result<(), WaitError> {
    if let Some(reason) = shared.cancellation.reason() {
        return Err(WaitError::Cancelled(reason));
    }
    let delay_deadline = tokio::time::Instant::now().checked_add(delay);
    loop {
        let pause_changed = shared.manual.pause_changed.notified();
        let trigger_changed = shared.manual.trigger_changed.notified();
        if let Some(reason) = shared.cancellation.reason() {
            return Err(WaitError::Cancelled(reason));
        }
        if total_deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
            return Err(WaitError::TimedOut);
        }
        if shared.manual.is_paused() {
            let next_deadline = match (delay_deadline, total_deadline) {
                (Some(delay), Some(total)) => Some(delay.min(total)),
                (Some(delay), None) => Some(delay),
                (None, Some(total)) => Some(total),
                (None, None) => None,
            };
            shared.transition_with_delay(
                TaskState::Paused,
                next_deadline.map(|deadline| {
                    deadline.saturating_duration_since(tokio::time::Instant::now())
                }),
            );
            if let Some(total) = total_deadline {
                tokio::select! {
                    biased;
                    reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                    _ = tokio::time::sleep_until(total) => return Err(WaitError::TimedOut),
                    _ = pause_changed => continue,
                }
            } else {
                tokio::select! {
                    biased;
                    reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                    _ = pause_changed => continue,
                }
            }
        }
        if allow_trigger && shared.manual.take_trigger() {
            return Ok(());
        }
        if delay.is_zero() {
            shared.transition_with_delay(state, Some(Duration::ZERO));
            tokio::select! {
                biased;
                reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
                _ = pause_changed => continue,
                _ = trigger_changed, if allow_trigger => continue,
                () = tokio::task::yield_now() => return Ok(()),
            }
        }
        enum Wake {
            Delay,
            Timeout,
            Continue,
        }
        let (deadline, wake) = match (delay_deadline, total_deadline) {
            (Some(delay), Some(total)) if total <= delay => (total, Wake::Timeout),
            (Some(delay), _) => (delay, Wake::Delay),
            (None, Some(total)) => (total, Wake::Timeout),
            (None, None) => (
                tokio::time::Instant::now() + Duration::from_secs(24 * 60 * 60),
                Wake::Continue,
            ),
        };
        shared.transition_with_delay(
            state,
            Some(deadline.saturating_duration_since(tokio::time::Instant::now())),
        );
        tokio::select! {
            biased;
            reason = shared.cancellation.cancelled() => return Err(WaitError::Cancelled(reason)),
            _ = pause_changed => continue,
            _ = trigger_changed, if allow_trigger => continue,
            _ = tokio::time::sleep_until(deadline) => match wake {
                Wake::Delay => {
                    // A manual trigger racing the natural deadline is folded
                    // into this same run instead of creating an accidental
                    // immediate follow-up run.
                    if allow_trigger {
                        let _commands = shared
                            .command_gate
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        shared.manual.take_trigger();
                    }
                    return Ok(())
                },
                Wake::Timeout => return Err(WaitError::TimedOut),
                Wake::Continue => continue,
            }
        }
    }
}

fn finalize_terminal<T, E>(shared: &RunShared, terminal: TaskTerminal<T, E>) -> TaskTerminal<T, E> {
    match shared.cancellation.claim_outcome() {
        Some(reason) => TaskTerminal::Cancelled { reason },
        None => terminal,
    }
}

fn retry_deadline<E>(
    retry: Option<&Arc<RetryPolicy<E>>>,
    started_at: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    retry
        .and_then(|policy| policy.max_elapsed())
        .and_then(|duration| started_at.checked_add(duration))
}

enum ScheduleWaitError {
    Cancelled(CancelReason),
    Schedule(ScheduleError),
}

async fn advance_schedule<T>(
    cursor: &mut ScheduleCursor,
    control: TaskControl<T>,
    context: &mut TaskContext,
    shared: &RunShared,
    schedule_anchor: tokio::time::Instant,
) -> Result<tokio::time::Instant, ScheduleWaitError> {
    let next_run_index = context
        .run_index
        .checked_add(1)
        .ok_or(ScheduleWaitError::Schedule(
            ScheduleError::RunIndexExhausted,
        ))?;
    let elapsed = tokio::time::Instant::now().saturating_duration_since(schedule_anchor);
    let delay = cursor
        .next_delay(&control, elapsed)
        .map_err(ScheduleWaitError::Schedule)?;
    drop(control);
    tokio::time::Instant::now()
        .checked_add(delay)
        .ok_or(ScheduleWaitError::Schedule(ScheduleError::DurationOverflow))?;
    match wait_controlled_delay(delay, shared, None, TaskState::WaitingForSchedule, true).await {
        Ok(()) => {
            context.run_index = next_run_index;
            context.attempt = 1;
            Ok(tokio::time::Instant::now())
        }
        Err(WaitError::Cancelled(reason)) => Err(ScheduleWaitError::Cancelled(reason)),
        Err(WaitError::TimedOut | WaitError::Closed | WaitError::Paused) => {
            unreachable!("schedule waits have no deadline or semaphore")
        }
    }
}

struct ExecutionResources {
    shared: Arc<RunShared>,
    children: Arc<TrackedChildren>,
    queue_slot: OwnedSemaphorePermit,
    queue: Arc<Semaphore>,
    global: Arc<Semaphore>,
    lane: Arc<Semaphore>,
    submitted_at: tokio::time::Instant,
}

async fn execute_job<T, E>(
    mut kind: JobKind<T, E>,
    retry: Option<Arc<RetryPolicy<E>>>,
    schedule: Option<Arc<Schedule>>,
    retry_exhausted_action: RetryExhaustedAction,
    mut context: TaskContext,
    resources: ExecutionResources,
) -> TaskTerminal<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let ExecutionResources {
        shared,
        children,
        queue_slot,
        queue,
        global,
        lane,
        submitted_at,
    } = resources;
    let _children_guard = TrackedChildrenGuard(Arc::clone(&children));
    let mut queue_slot = Some(queue_slot);
    let mut schedule_cursor = schedule.as_ref().map(|schedule| schedule.cursor());
    let delayed_first_run = schedule
        .as_ref()
        .is_some_and(|schedule| matches!(schedule.first_run(), FirstRun::After(_)));
    if let (Some(schedule), Some(cursor)) = (schedule.as_ref(), schedule_cursor.as_ref()) {
        if matches!(schedule.first_run(), FirstRun::After(_)) {
            drop(queue_slot.take());
            match wait_controlled_delay(
                cursor.first_delay(),
                &shared,
                None,
                TaskState::WaitingForSchedule,
                true,
            )
            .await
            {
                Ok(()) => {}
                Err(WaitError::Cancelled(reason)) => {
                    return TaskTerminal::Cancelled { reason };
                }
                Err(WaitError::TimedOut | WaitError::Closed | WaitError::Paused) => {
                    unreachable!("initial schedule waits have no deadline or semaphore")
                }
            }
        }
    }
    let retry_started_at = if delayed_first_run {
        tokio::time::Instant::now()
    } else {
        submitted_at
    };
    let mut total_deadline = retry_deadline(retry.as_ref(), retry_started_at);
    let mut pending_error = None;
    let mut attempt: u32 = 1;

    loop {
        match wait_while_paused(&shared, total_deadline).await {
            Ok(()) => {}
            Err(WaitError::Cancelled(reason)) => return TaskTerminal::Cancelled { reason },
            Err(WaitError::TimedOut) => {
                return finalize_terminal(
                    &shared,
                    TaskTerminal::TimedOut {
                        scope: TimeoutScope::TotalElapsed,
                        attempts: attempt.saturating_sub(1),
                        last_error: pending_error,
                    },
                );
            }
            Err(WaitError::Closed | WaitError::Paused) => {
                unreachable!("pause waits do not use semaphores")
            }
        }
        if queue_slot.is_none() {
            shared.transition(TaskState::Queued);
            queue_slot = Some(
                match acquire_or_cancel_until(Arc::clone(&queue), &shared, total_deadline).await {
                    Ok(permit) => permit,
                    Err(WaitError::Cancelled(reason)) => {
                        return TaskTerminal::Cancelled { reason };
                    }
                    Err(WaitError::TimedOut) => {
                        return finalize_terminal(
                            &shared,
                            TaskTerminal::TimedOut {
                                scope: TimeoutScope::TotalElapsed,
                                attempts: attempt.saturating_sub(1),
                                last_error: pending_error,
                            },
                        );
                    }
                    Err(WaitError::Closed) => {
                        return finalize_terminal(&shared, TaskTerminal::ExecutorStopped);
                    }
                    Err(WaitError::Paused) => continue,
                },
            );
        }
        shared.transition(TaskState::WaitingForPermit);
        let lane_permit =
            match acquire_or_cancel_until(Arc::clone(&lane), &shared, total_deadline).await {
                Ok(permit) => permit,
                Err(WaitError::Cancelled(reason)) => return TaskTerminal::Cancelled { reason },
                Err(WaitError::TimedOut) => {
                    return finalize_terminal(
                        &shared,
                        TaskTerminal::TimedOut {
                            scope: TimeoutScope::TotalElapsed,
                            attempts: attempt - 1,
                            last_error: pending_error,
                        },
                    );
                }
                Err(WaitError::Closed) => {
                    return finalize_terminal(&shared, TaskTerminal::ExecutorStopped);
                }
                Err(WaitError::Paused) => {
                    drop(queue_slot.take());
                    continue;
                }
            };
        let global_permit =
            match acquire_or_cancel_until(Arc::clone(&global), &shared, total_deadline).await {
                Ok(permit) => permit,
                Err(WaitError::Cancelled(reason)) => return TaskTerminal::Cancelled { reason },
                Err(WaitError::TimedOut) => {
                    return finalize_terminal(
                        &shared,
                        TaskTerminal::TimedOut {
                            scope: TimeoutScope::TotalElapsed,
                            attempts: attempt - 1,
                            last_error: pending_error,
                        },
                    );
                }
                Err(WaitError::Closed) => {
                    return finalize_terminal(&shared, TaskTerminal::ExecutorStopped);
                }
                Err(WaitError::Paused) => {
                    drop(queue_slot.take());
                    continue;
                }
            };
        {
            let _commands = shared
                .command_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if shared.manual.is_paused() {
                drop(global_permit);
                drop(lane_permit);
                drop(queue_slot.take());
                continue;
            }
            if let Some(reason) = shared.cancellation.reason() {
                return TaskTerminal::Cancelled { reason };
            }
            shared.transition(TaskState::Running);
        }
        drop(queue_slot.take());
        context.attempt = attempt;
        let future = match kind.start(context.clone()) {
            Ok(future) => future,
            Err(code) => {
                crate::diagnostic::error(
                    code.as_str(),
                    "scheduler",
                    format_args!("failed to start job run_id={:?}", context.run_id),
                );
                drop(lane_permit);
                drop(global_permit);
                return finalize_terminal(&shared, TaskTerminal::ExecutorError { code });
            }
        };
        let output = run_attempt(
            future,
            retry.as_ref().and_then(|policy| policy.attempt_timeout()),
            total_deadline,
        )
        .await;
        drop(lane_permit);
        drop(global_permit);

        match output {
            AttemptOutcome::TimedOut(scope) => {
                return finalize_terminal(
                    &shared,
                    TaskTerminal::TimedOut {
                        scope,
                        attempts: attempt,
                        last_error: pending_error,
                    },
                );
            }
            AttemptOutcome::Output(Ok(JobOutput::Value(value))) => {
                return finalize_terminal(&shared, TaskTerminal::Completed(value));
            }
            AttemptOutcome::Output(Ok(JobOutput::Control(TaskControl::Complete(value)))) => {
                return finalize_terminal(&shared, TaskTerminal::Completed(value));
            }
            AttemptOutcome::Output(Ok(JobOutput::Control(control))) => {
                let Some(cursor) = schedule_cursor.as_mut() else {
                    let code = ExecutorErrorCode::ScheduleCursorMissing;
                    crate::diagnostic::error(
                        code.as_str(),
                        "scheduler",
                        format_args!("scheduled output has no cursor run_id={:?}", context.run_id),
                    );
                    return finalize_terminal(&shared, TaskTerminal::ExecutorError { code });
                };
                match advance_schedule(cursor, control, &mut context, &shared, submitted_at).await {
                    Ok(started_at) => {
                        attempt = 1;
                        pending_error = None;
                        total_deadline = retry_deadline(retry.as_ref(), started_at);
                    }
                    Err(ScheduleWaitError::Cancelled(reason)) => {
                        return TaskTerminal::Cancelled { reason };
                    }
                    Err(ScheduleWaitError::Schedule(error)) => {
                        return finalize_terminal(&shared, TaskTerminal::ScheduleError(error));
                    }
                }
            }
            AttemptOutcome::Output(Err(error)) => {
                let Some(policy) = retry.as_ref() else {
                    return finalize_terminal(&shared, TaskTerminal::Failed(error));
                };
                if !policy.should_retry(&error) {
                    return finalize_terminal(&shared, TaskTerminal::Failed(error));
                }
                if attempt >= policy.max_attempts() {
                    if retry_exhausted_action == RetryExhaustedAction::ContinueSchedule {
                        if let Some(cursor) = schedule_cursor.as_mut() {
                            drop(error);
                            match advance_schedule(
                                cursor,
                                TaskControl::<T>::Continue,
                                &mut context,
                                &shared,
                                submitted_at,
                            )
                            .await
                            {
                                Ok(started_at) => {
                                    attempt = 1;
                                    pending_error = None;
                                    total_deadline = retry_deadline(retry.as_ref(), started_at);
                                    continue;
                                }
                                Err(ScheduleWaitError::Cancelled(reason)) => {
                                    return TaskTerminal::Cancelled { reason };
                                }
                                Err(ScheduleWaitError::Schedule(error)) => {
                                    return finalize_terminal(
                                        &shared,
                                        TaskTerminal::ScheduleError(error),
                                    );
                                }
                            }
                        }
                    }
                    return finalize_terminal(
                        &shared,
                        TaskTerminal::RetriesExhausted {
                            attempts: attempt,
                            last_error: error,
                        },
                    );
                }

                let delay = policy.delay_after(&error, attempt).unwrap_or_else(|error| {
                    panic!("retry delay source violated its contract: {error}")
                });
                pending_error = Some(error);
                match wait_controlled_delay(
                    delay,
                    &shared,
                    total_deadline,
                    TaskState::WaitingForRetry,
                    false,
                )
                .await
                {
                    Ok(()) => {
                        attempt += 1;
                    }
                    Err(WaitError::Cancelled(reason)) => {
                        return TaskTerminal::Cancelled { reason };
                    }
                    Err(WaitError::TimedOut) => {
                        return finalize_terminal(
                            &shared,
                            TaskTerminal::TimedOut {
                                scope: TimeoutScope::TotalElapsed,
                                attempts: attempt,
                                last_error: pending_error,
                            },
                        );
                    }
                    Err(WaitError::Closed) => unreachable!("retry waits do not use semaphores"),
                    Err(WaitError::Paused) => unreachable!("controlled waits absorb pause"),
                }
            }
        }
    }
}

struct RunCompletion<T, E> {
    shared: Arc<RunShared>,
    scheduler: Weak<SchedulerInner>,
    run_id: TaskRunId,
    result: Option<oneshot::Sender<TaskTerminal<T, E>>>,
}

impl<T, E> RunCompletion<T, E> {
    fn new(
        shared: Arc<RunShared>,
        scheduler: Weak<SchedulerInner>,
        run_id: TaskRunId,
        result: oneshot::Sender<TaskTerminal<T, E>>,
    ) -> Self {
        Self {
            shared,
            scheduler,
            run_id,
            result: Some(result),
        }
    }

    fn finish(mut self, terminal: TaskTerminal<T, E>) {
        let Some(result) = self.result.take() else {
            let code = ExecutorErrorCode::CompletionAlreadyPublished;
            crate::diagnostic::error(
                code.as_str(),
                "scheduler",
                format_args!("run completion sender is missing run_id={:?}", self.run_id),
            );
            self.shared.publish_terminal(TaskState::ExecutorError);
            remove_run_registrations(&self.shared, &self.scheduler, self.run_id);
            return;
        };
        publish_and_remove(terminal, &self.shared, &self.scheduler, self.run_id, result);
    }
}

impl<T, E> Drop for RunCompletion<T, E> {
    fn drop(&mut self) {
        let Some(result) = self.result.take() else {
            return;
        };
        // Tokio drops spawned futures when their bound runtime shuts down. The
        // completion owner is part of that future, so its drop path is the
        // last reliable place to publish a terminal and release registries.
        publish_and_remove(
            TaskTerminal::ExecutorStopped,
            &self.shared,
            &self.scheduler,
            self.run_id,
            result,
        );
    }
}

fn publish_and_remove<T, E>(
    terminal: TaskTerminal<T, E>,
    shared: &Arc<RunShared>,
    scheduler: &Weak<SchedulerInner>,
    run_id: TaskRunId,
    result: oneshot::Sender<TaskTerminal<T, E>>,
) {
    let terminal = if shared.publish_terminal(terminal.state()) {
        terminal
    } else if shared.snapshot().state() == TaskState::ExecutorStopped {
        // A shutdown coordinator guard can win the terminal CAS while the
        // task's completion future is concurrently being dropped or finishing.
        // Keep the owned result channel consistent with that authoritative
        // state instead of exposing two different terminal outcomes.
        TaskTerminal::ExecutorStopped
    } else {
        terminal
    };
    remove_run_registrations(shared, scheduler, run_id);
    let _ = result.send(terminal);
}

fn remove_run_registrations(
    shared: &Arc<RunShared>,
    scheduler: &Weak<SchedulerInner>,
    run_id: TaskRunId,
) {
    if let Some(scheduler) = Weak::upgrade(scheduler) {
        let mut registry = scheduler
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .get(&run_id)
            .is_some_and(|current| Arc::ptr_eq(current, shared))
        {
            registry.remove(&run_id);
        }
    }
    if let Some(group) = shared.group.as_ref().and_then(Weak::upgrade) {
        group.remove_member(run_id, shared);
    }
    let registration = {
        shared
            .key_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };
    if let Some(registration) = registration {
        registration.remove(run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancelReason, Cancellation, ExecutorErrorCode, Job, JobKind, KeyedSubmitError, LaneConfig,
        LaneError, RunCompletion, RunShared, ScheduledJob, Scheduler, TaskState, TaskTerminal,
        TrackedChildren,
    };
    use crate::{FirstRun, Schedule, TaskControl};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn consumed_one_shot_job_returns_executor_error_instead_of_panicking() {
        crate::test_log::init();
        let scheduler = Scheduler::builder().build().unwrap();
        let mut job = Job::once(|_| async { Ok::<_, ()>(()) });
        job.kind = JobKind::Once(None);

        let terminal = scheduler.submit(job).await.unwrap().join().await;
        assert!(matches!(
            terminal,
            TaskTerminal::ExecutorError {
                code: ExecutorErrorCode::JobAlreadyConsumed
            }
        ));
        assert!(crate::test_log::contains("BB-EXEC-001"));
    }

    #[tokio::test]
    async fn missing_schedule_cursor_returns_executor_error_instead_of_panicking() {
        crate::test_log::init();
        let scheduler = Scheduler::builder().build().unwrap();
        let schedule =
            Schedule::fixed_delay(Duration::from_millis(1), FirstRun::Immediate).unwrap();
        let mut job = ScheduledJob::new(schedule, |_| async {
            Ok::<_, ()>(TaskControl::<()>::Continue)
        })
        .instantiate();
        job.schedule = None;

        let terminal = scheduler.submit(job).await.unwrap().join().await;
        assert!(matches!(
            terminal,
            TaskTerminal::ExecutorError {
                code: ExecutorErrorCode::ScheduleCursorMissing
            }
        ));
        assert!(crate::test_log::contains("BB-EXEC-002"));
    }

    #[tokio::test]
    async fn missing_completion_sender_is_logged_without_panicking() {
        crate::test_log::init();
        let scheduler = Scheduler::builder().build().unwrap();
        let run_id = super::TaskRunId::new();
        let shared = Arc::new(RunShared::new(
            super::RunMetadata {
                run_id,
                job_id: super::JobId::new(),
                key_generation: None,
                lane: Arc::from("default"),
                lane_generation: 1,
            },
            None,
            Arc::new(super::Observability::new(
                &tokio::runtime::Handle::current(),
                16,
                None,
            )),
            None,
        ));
        let completion = RunCompletion::<(), ()> {
            shared: Arc::clone(&shared),
            scheduler: Arc::downgrade(&scheduler.inner),
            run_id,
            result: None,
        };
        scheduler
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, Arc::clone(&shared));

        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            completion.finish(TaskTerminal::Completed(()));
        }));
        assert!(finished.is_ok());
        assert!(crate::test_log::contains("BB-EXEC-003"));
        assert_eq!(shared.snapshot().state(), TaskState::ExecutorError);
        assert!(!scheduler
            .inner
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&run_id));

        let observer = super::TaskObserver {
            snapshots: shared.snapshot.subscribe(),
            shared,
        };
        let (closed_sender, mut closed_result) =
            tokio::sync::oneshot::channel::<TaskTerminal<(), ()>>();
        drop(closed_sender);
        assert!(matches!(
            super::receive_terminal(&observer, &mut closed_result).await,
            TaskTerminal::ExecutorError {
                code: ExecutorErrorCode::CompletionAlreadyPublished
            }
        ));
    }

    #[test]
    fn completion_claim_wins_later_cancellation() {
        let cancellation = Cancellation::new();
        assert_eq!(cancellation.claim_outcome(), None);
        assert!(!cancellation.cancel(CancelReason::User));
        assert_eq!(cancellation.reason(), None);
    }

    #[test]
    fn cancellation_claim_wins_later_completion() {
        let cancellation = Cancellation::new();
        assert!(cancellation.cancel(CancelReason::Shutdown));
        assert_eq!(cancellation.claim_outcome(), Some(CancelReason::Shutdown));
    }

    #[tokio::test]
    async fn non_terminal_transition_cannot_regress_terminal_snapshot() {
        let shared = RunShared::new(
            super::RunMetadata {
                run_id: super::TaskRunId::new(),
                job_id: super::JobId::new(),
                key_generation: None,
                lane: Arc::from("default"),
                lane_generation: 1,
            },
            None,
            Arc::new(super::Observability::new(
                &tokio::runtime::Handle::current(),
                16,
                None,
            )),
            None,
        );
        assert!(shared.publish_terminal(TaskState::Completed));

        shared.transition(TaskState::Running);

        assert_eq!(shared.snapshot().state(), TaskState::Completed);
    }

    #[tokio::test]
    async fn lane_generation_exhaustion_is_explicit_and_does_not_insert() {
        let scheduler = Scheduler::builder().build().unwrap();
        scheduler
            .inner
            .next_lane_generation
            .store(u64::MAX, Ordering::Release);

        assert_eq!(
            scheduler.ensure_lane("overflow", LaneConfig::default()),
            Err(LaneError::GenerationExhausted)
        );
        assert_eq!(scheduler.lane_generation("overflow"), None);
    }

    #[tokio::test]
    async fn key_generation_exhaustion_is_explicit_and_does_not_reserve() {
        let scheduler = Scheduler::builder().build().unwrap();
        scheduler
            .inner
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generations
            .insert(Arc::from("overflow"), u64::MAX);

        assert!(matches!(
            scheduler.start_if_absent("overflow", Job::once(|_| async { Ok::<_, ()>(()) })),
            Err(KeyedSubmitError::GenerationExhausted(_))
        ));
        assert_eq!(scheduler.key_status("overflow"), None);
    }

    #[tokio::test]
    async fn completed_tracked_children_are_removed_before_parent_finishes() {
        let children = Arc::new(TrackedChildren::new(tokio::runtime::Handle::current()));

        for _ in 0..64 {
            children.spawn(async {}).unwrap().await.unwrap();
        }

        assert_eq!(
            children
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .handles
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn unpolled_aborted_children_are_removed_before_parent_finishes() {
        let children = Arc::new(TrackedChildren::new(tokio::runtime::Handle::current()));

        for _ in 0..64 {
            let handle = children
                .spawn(std::future::pending::<()>())
                .expect("open context accepts child");
            handle.abort();
            assert!(handle.await.unwrap_err().is_cancelled());
        }

        assert_eq!(
            children
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .handles
                .len(),
            0
        );
    }

    #[test]
    fn stopped_runtime_does_not_deadlock_tracked_child_registration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let children = Arc::new(TrackedChildren::new(runtime.handle().clone()));
        drop(runtime);
        let (finished, completion) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| children.spawn(async {})));
            let handles = children
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .handles
                .len();
            let _ = finished.send((result.is_ok(), handles));
        });

        assert_eq!(
            completion.recv_timeout(Duration::from_secs(1)),
            Ok((true, 0)),
            "a stopped runtime must neither panic nor deadlock child registration"
        );
    }
}
