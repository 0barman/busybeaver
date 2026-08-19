use std::fmt;
use std::sync::PoisonError;

/// Unified error type for the library.
#[derive(Debug, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum BeaverError {
    /// Required field is missing when building a task.
    BuilderMissingField(&'static str),
    /// Queue is full, cannot enqueue.
    QueueFull,
    /// Execution thread has been released, cannot enqueue.
    DamReleased,
    /// Internal lock was poisoned (usually caused by a panic).
    LockPoisoned,
    /// No execution thread available.
    NoDam,
    /// The executor has begun its irreversible shutdown transition.
    ExecutorShuttingDown,
    /// One or more workers did not terminate before the shutdown deadline.
    ShutdownTimedOut,
    /// A supervised lane worker failed while shutting down.
    WorkerFailed(String),
    /// Public lane queue capacity must be at least one.
    InvalidLaneCapacity,
    /// Public lane concurrency must be at least one.
    InvalidLaneConcurrency,
    /// A lane name already exists with a different immutable configuration.
    LaneConfigConflict { name: String },
    /// Range interval task: number of interval ranges exceeds total retry count.
    RangeIntervalRangesExceedTotal { total: u32, ranges_count: usize },
}

impl fmt::Display for BeaverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BeaverError::BuilderMissingField(field) => {
                write!(f, "builder missing required field: {field}")
            }
            BeaverError::QueueFull => write!(f, "task queue is full"),
            BeaverError::DamReleased => write!(f, "execution thread has been released"),
            BeaverError::LockPoisoned => write!(f, "internal lock poisoned"),
            BeaverError::NoDam => write!(f, "no execution thread available"),
            BeaverError::ExecutorShuttingDown => write!(f, "executor is shutting down"),
            BeaverError::ShutdownTimedOut => {
                write!(f, "executor shutdown deadline was exceeded")
            }
            BeaverError::WorkerFailed(message) => {
                write!(f, "executor worker failed: {message}")
            }
            BeaverError::InvalidLaneCapacity => {
                write!(f, "lane queue capacity must be at least one")
            }
            BeaverError::InvalidLaneConcurrency => {
                write!(f, "lane concurrency must be at least one")
            }
            BeaverError::LaneConfigConflict { name } => {
                write!(
                    f,
                    "lane '{name}' already exists with a different configuration"
                )
            }
            BeaverError::RangeIntervalRangesExceedTotal {
                total,
                ranges_count,
            } => {
                write!(
                    f,
                    "range interval ranges count ({ranges_count}) exceeds total retry count ({total})"
                )
            }
        }
    }
}

impl std::error::Error for BeaverError {}

impl<T> From<PoisonError<T>> for BeaverError {
    fn from(_: PoisonError<T>) -> Self {
        BeaverError::LockPoisoned
    }
}

/// Unified Result type for the library.
pub type BeaverResult<T> = Result<T, BeaverError>;

/// Runtime error, passed to the caller via Listener.
#[derive(Debug, Clone)]
pub enum RuntimeError {
    /// Internal lock was poisoned.
    LockPoisoned,
    /// An error occurred during task execution (e.g. panic in work, reported with message).
    TaskExecutionFailed(String),
    /// A bounded retry task (fixed-count / time-interval / range-interval) ran
    /// every permitted attempt and the last one still returned
    /// [`WorkResult::NeedRetry`](crate::WorkResult::NeedRetry) — i.e. it never
    /// succeeded. Reported via [`WorkListener::on_error`](crate::WorkListener::on_error)
    /// so that "retries exhausted without success" is distinct from the
    /// successful-completion signal [`WorkListener::on_complete`](crate::WorkListener::on_complete).
    /// Periodic tasks never produce this (they have no retry budget to exhaust).
    RetriesExhausted,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::LockPoisoned => write!(f, "internal lock poisoned during execution"),
            RuntimeError::TaskExecutionFailed(msg) => write!(f, "task execution failed: {msg}"),
            RuntimeError::RetriesExhausted => {
                write!(f, "task retries exhausted without success")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}
