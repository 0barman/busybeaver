use crate::error::BeaverError;
use crate::execution::{TaskControlHandle, TaskExitSummary, TaskSnapshot};
use crate::ids::ExecutionId;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ShutdownTimeoutAction {
    ReportAndKeepTracked,
    AbortAllowed,
    Wait,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ShutdownMode {
    CancelAll,
    DrainFinite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownOptions {
    mode: ShutdownMode,
    grace_period: Duration,
    on_timeout: ShutdownTimeoutAction,
}

impl ShutdownOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(mut self, mode: ShutdownMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn grace_period(mut self, grace_period: Duration) -> Self {
        self.grace_period = grace_period;
        self
    }

    pub fn on_timeout(mut self, action: ShutdownTimeoutAction) -> Self {
        self.on_timeout = action;
        self
    }

    pub fn shutdown_mode(&self) -> ShutdownMode {
        self.mode
    }

    pub fn configured_grace_period(&self) -> Duration {
        self.grace_period
    }

    pub fn timeout_action(&self) -> ShutdownTimeoutAction {
        self.on_timeout
    }
}

impl Default for ShutdownOptions {
    fn default() -> Self {
        Self {
            mode: ShutdownMode::CancelAll,
            grace_period: Duration::from_secs(5),
            on_timeout: ShutdownTimeoutAction::ReportAndKeepTracked,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CleanupPhase {
    TrackedChildren,
    LegacyCallback,
    ShutdownHook,
    WorkerJoin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CleanupProgress {
    NotRequired,
    Pending { phase: CleanupPhase },
    Finished(CleanupOutcome),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CleanupOutcome {
    NotRequired,
    Completed,
    Failed(String),
    ForcedCancelled { callback_invoked: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFailure {
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackFailure {
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskShutdownProgressRecord {
    pub execution_id: ExecutionId,
    pub snapshot_at_start: TaskSnapshot,
    pub grace_deadline_exceeded: bool,
    pub forced_cancellation_requested: bool,
    pub final_exit: Option<TaskExitSummary>,
    pub cleanup: CleanupProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskShutdownRecord {
    pub execution_id: ExecutionId,
    pub snapshot_at_start: TaskSnapshot,
    pub grace_deadline_exceeded: bool,
    pub forced_cancellation_requested: bool,
    pub final_exit: TaskExitSummary,
    pub cleanup: CleanupOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownReportSnapshot {
    pub tasks: Vec<TaskShutdownProgressRecord>,
    pub callback_failures: Vec<CallbackFailure>,
    pub worker_failures: Vec<WorkerFailure>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    pub tasks: Vec<TaskShutdownRecord>,
    pub callback_failures: Vec<CallbackFailure>,
    pub worker_failures: Vec<WorkerFailure>,
    pub elapsed: Duration,
}

#[derive(Clone)]
enum GraceCompletion {
    TimedOut {
        snapshot: Arc<ShutdownReportSnapshot>,
        pending: Vec<TaskControlHandle>,
    },
    Stopped(Arc<ShutdownReport>),
    TimerUnavailable,
    Failed(BeaverError),
}

pub(crate) struct ShutdownProcess {
    id: Uuid,
    options: ShutdownOptions,
    accepted_at: Instant,
    grace_deadline: Instant,
    grace: watch::Sender<Option<GraceCompletion>>,
    final_report: watch::Sender<Option<Result<Arc<ShutdownReport>, BeaverError>>>,
}

impl ShutdownProcess {
    pub(crate) fn new(options: ShutdownOptions) -> Result<Arc<Self>, ShutdownError> {
        let accepted_at = Instant::now();
        let grace_deadline = accepted_at
            .checked_add(options.grace_period)
            .ok_or(ShutdownError::InvalidGracePeriod)?;
        let (grace, _) = watch::channel(None);
        let (final_report, _) = watch::channel(None);
        Ok(Arc::new(Self {
            id: Uuid::new_v4(),
            options,
            accepted_at,
            grace_deadline,
            grace,
            final_report,
        }))
    }

    pub(crate) fn handle(self: &Arc<Self>) -> ShutdownHandle {
        ShutdownHandle {
            process: Arc::clone(self),
        }
    }

    pub(crate) fn options(&self) -> &ShutdownOptions {
        &self.options
    }

    pub(crate) fn accepted_at(&self) -> Instant {
        self.accepted_at
    }

    pub(crate) fn grace_deadline(&self) -> Instant {
        self.grace_deadline
    }

    pub(crate) fn publish_timeout(
        &self,
        snapshot: Arc<ShutdownReportSnapshot>,
        pending: Vec<TaskControlHandle>,
    ) {
        self.grace
            .send_replace(Some(GraceCompletion::TimedOut { snapshot, pending }));
    }

    pub(crate) fn publish_final(&self, report: Arc<ShutdownReport>) {
        if self.grace.borrow().is_none() {
            self.grace
                .send_replace(Some(GraceCompletion::Stopped(Arc::clone(&report))));
        }
        self.final_report.send_replace(Some(Ok(report)));
    }

    pub(crate) fn publish_timer_unavailable(&self) {
        if self.grace.borrow().is_none() {
            self.grace
                .send_replace(Some(GraceCompletion::TimerUnavailable));
        }
    }

    pub(crate) fn publish_failure(&self, error: BeaverError) {
        if self.grace.borrow().is_none() {
            self.grace
                .send_replace(Some(GraceCompletion::Failed(error.clone())));
        }
        self.final_report.send_replace(Some(Err(error)));
    }
}

#[derive(Clone)]
pub struct ShutdownHandle {
    process: Arc<ShutdownProcess>,
}

impl fmt::Debug for ShutdownHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownHandle")
            .field("id", &self.process.id)
            .field("options", &self.process.options)
            .finish()
    }
}

impl ShutdownHandle {
    pub fn id(&self) -> Uuid {
        self.process.id
    }

    pub fn effective_options(&self) -> &ShutdownOptions {
        &self.process.options
    }

    pub async fn wait_grace_outcome(&self) -> Result<ShutdownOutcome, ShutdownWaitError> {
        let mut grace = self.process.grace.subscribe();
        loop {
            if let Some(completion) = grace.borrow().clone() {
                return match completion {
                    GraceCompletion::TimedOut { snapshot, pending } => {
                        Ok(ShutdownOutcome::TimedOut {
                            snapshot,
                            pending,
                            shutdown: self.clone(),
                        })
                    }
                    GraceCompletion::Stopped(report) => Ok(ShutdownOutcome::Stopped(report)),
                    GraceCompletion::TimerUnavailable => Err(ShutdownWaitError::TimerUnavailable {
                        shutdown: self.clone(),
                    }),
                    GraceCompletion::Failed(error) => {
                        Err(ShutdownWaitError::SupervisorFailed(error))
                    }
                };
            }
            grace
                .changed()
                .await
                .map_err(|_| ShutdownWaitError::SupervisorUnavailable)?;
        }
    }

    pub async fn wait_final(&self) -> Result<Arc<ShutdownReport>, ShutdownWaitError> {
        let mut final_report = self.process.final_report.subscribe();
        loop {
            if let Some(report) = final_report.borrow().clone() {
                return report.map_err(ShutdownWaitError::SupervisorFailed);
            }
            final_report
                .changed()
                .await
                .map_err(|_| ShutdownWaitError::SupervisorUnavailable)?;
        }
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ShutdownOutcome {
    Stopped(Arc<ShutdownReport>),
    TimedOut {
        snapshot: Arc<ShutdownReportSnapshot>,
        pending: Vec<TaskControlHandle>,
        shutdown: ShutdownHandle,
    },
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ShutdownError {
    ConfigConflict {
        existing: ShutdownHandle,
        effective_options: ShutdownOptions,
    },
    InvalidGracePeriod,
    LockPoisoned,
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigConflict { .. } => {
                formatter.write_str("shutdown already started with different options")
            }
            Self::InvalidGracePeriod => formatter.write_str("shutdown grace period overflowed"),
            Self::LockPoisoned => formatter.write_str("shutdown lifecycle lock was poisoned"),
        }
    }
}

impl std::error::Error for ShutdownError {}

#[derive(Debug)]
#[non_exhaustive]
pub enum ShutdownWaitError {
    SupervisorFailed(BeaverError),
    SupervisorUnavailable,
    TimerUnavailable { shutdown: ShutdownHandle },
}

impl fmt::Display for ShutdownWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SupervisorFailed(error) => write!(formatter, "shutdown failed: {error}"),
            Self::SupervisorUnavailable => formatter.write_str("shutdown supervisor unavailable"),
            Self::TimerUnavailable { .. } => formatter.write_str(
                "Tokio time driver is unavailable; shutdown continues without a grace timer",
            ),
        }
    }
}

impl std::error::Error for ShutdownWaitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SupervisorFailed(error) => Some(error),
            Self::SupervisorUnavailable | Self::TimerUnavailable { .. } => None,
        }
    }
}
