use std::fmt;
use std::sync::PoisonError;

/// Unified error type for the library.
#[derive(Debug)]
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
    /// Range interval task: number of interval ranges exceeds total retry count.
    RangeIntervalRangesExceedTotal {
        /// Configured total attempt count.
        total: u32,
        /// Number of configured interval ranges.
        ranges_count: usize,
    },
    /// A legacy API that already returns [`BeaverResult`] rejected configuration
    /// while creating an execution lane.
    InvalidConfiguration(ValidationError),
}

impl BeaverError {
    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BuilderMissingField(_) => "BB-LEGACY-001",
            Self::QueueFull => "BB-LEGACY-002",
            Self::DamReleased => "BB-LEGACY-003",
            Self::LockPoisoned => "BB-LEGACY-004",
            Self::NoDam => "BB-LEGACY-005",
            Self::RangeIntervalRangesExceedTotal { .. } => "BB-LEGACY-006",
            Self::InvalidConfiguration(error) => error.code(),
        }
    }
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
            BeaverError::InvalidConfiguration(error) => {
                write!(f, "invalid execution lane configuration: {error}")
            }
        }
    }
}

impl std::error::Error for BeaverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfiguration(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ValidationError> for BeaverError {
    fn from(error: ValidationError) -> Self {
        Self::InvalidConfiguration(error)
    }
}

impl<T> From<PoisonError<T>> for BeaverError {
    fn from(_: PoisonError<T>) -> Self {
        BeaverError::LockPoisoned
    }
}

/// Unified Result type for the library.
pub type BeaverResult<T> = Result<T, BeaverError>;

/// Strict configuration validation errors.
///
/// Legacy constructors and `build` methods keep their historical normalization
/// behavior. New `try_new` and `build_strict` entry points return this type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationError {
    /// A required builder field was not supplied.
    BuilderMissingField(&'static str),
    /// An attempt count was zero.
    ZeroAttempts,
    /// A schedule contained no intervals.
    EmptySchedule,
    /// A periodic interval was zero.
    ZeroInterval,
    /// A range started after it ended.
    InvalidRange {
        /// Inclusive range start.
        start: u32,
        /// Inclusive range end.
        end: u32,
    },
    /// More ranges were supplied than the bounded task can consume.
    TooManyRanges {
        /// Configured total attempt count.
        total: u32,
        /// Number of configured ranges.
        ranges_count: usize,
    },
    /// A duration could not be represented by the scheduler clock.
    DurationOverflow,
    /// A queue capacity was zero or exceeded Tokio's permit limit.
    InvalidCapacity {
        /// Rejected capacity.
        capacity: usize,
        /// Largest supported capacity.
        maximum: usize,
    },
    /// Construction required an active Tokio runtime but none was available.
    RuntimeUnavailable,
    /// Internal scheduler defaults could not produce a valid executor.
    ExecutorConfiguration,
}

impl ValidationError {
    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BuilderMissingField(_) => "BB-VAL-001",
            Self::ZeroAttempts => "BB-VAL-002",
            Self::EmptySchedule => "BB-VAL-003",
            Self::ZeroInterval => "BB-VAL-004",
            Self::InvalidRange { .. } => "BB-VAL-005",
            Self::TooManyRanges { .. } => "BB-VAL-006",
            Self::DurationOverflow => "BB-VAL-007",
            Self::InvalidCapacity { .. } => "BB-VAL-008",
            Self::RuntimeUnavailable => "BB-VAL-009",
            Self::ExecutorConfiguration => "BB-VAL-010",
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuilderMissingField(field) => {
                write!(f, "builder missing required field: {field}")
            }
            Self::ZeroAttempts => write!(f, "attempt count must be greater than zero"),
            Self::EmptySchedule => write!(f, "schedule must contain at least one interval"),
            Self::ZeroInterval => write!(f, "periodic interval must be greater than zero"),
            Self::InvalidRange { start, end } => {
                write!(f, "range start ({start}) must not exceed end ({end})")
            }
            Self::TooManyRanges {
                total,
                ranges_count,
            } => write!(
                f,
                "range count ({ranges_count}) exceeds total attempt count ({total})"
            ),
            Self::DurationOverflow => write!(f, "duration does not fit the scheduler clock"),
            Self::InvalidCapacity { capacity, maximum } => write!(
                f,
                "queue capacity ({capacity}) must be between 1 and {maximum}"
            ),
            Self::RuntimeUnavailable => write!(f, "no active Tokio runtime is available"),
            Self::ExecutorConfiguration => {
                write!(f, "internal executor configuration is invalid")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

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

impl RuntimeError {
    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::LockPoisoned => "BB-RUN-001",
            Self::TaskExecutionFailed(_) => "BB-RUN-002",
            Self::RetriesExhausted => "BB-RUN-003",
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::LockPoisoned => write!(f, "internal lock poisoned during execution"),
            RuntimeError::TaskExecutionFailed(msg) => write!(f, "task execution failed: {}", msg),
            RuntimeError::RetriesExhausted => {
                write!(f, "task retries exhausted without success")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}
