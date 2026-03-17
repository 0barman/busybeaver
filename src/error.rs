use std::fmt;
use std::sync::PoisonError;

/// Unified error type for the library.
#[derive(Debug)]
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
    /// Range interval task: number of interval ranges exceeds total retry count.
    RangeIntervalRangesExceedTotal { total: u32, ranges_count: usize },
}

impl fmt::Display for BeaverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BeaverError::BuilderMissingField(field) => {
                write!(f, "builder missing required field: {}", field)
            }
            BeaverError::QueueFull => write!(f, "task queue is full"),
            BeaverError::DamReleased => write!(f, "execution thread has been released"),
            BeaverError::LockPoisoned => write!(f, "internal lock poisoned"),
            BeaverError::NoDam => write!(f, "no execution thread available"),
            BeaverError::RangeIntervalRangesExceedTotal {
                total,
                ranges_count,
            } => {
                write!(
                    f,
                    "range interval ranges count ({}) exceeds total retry count ({})",
                    ranges_count, total
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
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::LockPoisoned => write!(f, "internal lock poisoned during execution"),
            RuntimeError::TaskExecutionFailed(msg) => write!(f, "task execution failed: {}", msg),
        }
    }
}

impl std::error::Error for RuntimeError {}
