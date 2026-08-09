use std::fmt;
use std::sync::Arc;
use std::time::Duration;

type RetryPredicate<E> = Arc<dyn Fn(&E) -> bool + Send + Sync + 'static>;
type DelayOverride<E> = Arc<dyn Fn(&E, u32) -> Option<Duration> + Send + Sync + 'static>;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Assigns one delay to failures through an inclusive attempt number.
pub struct BackoffRange {
    through_attempt: u32,
    delay: Duration,
}

impl BackoffRange {
    /// Creates an inclusive range endpoint; attempt zero is invalid.
    pub fn new(through_attempt: u32, delay: Duration) -> Result<Self, RetryPolicyError> {
        if through_attempt == 0 {
            return Err(RetryPolicyError::InvalidRange);
        }
        Ok(Self {
            through_attempt,
            delay,
        })
    }

    /// Returns the last failed attempt covered by this range.
    pub fn through_attempt(&self) -> u32 {
        self.through_attempt
    }

    /// Returns the delay assigned to the range.
    pub fn delay(&self) -> Duration {
        self.delay
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BackoffKind {
    None,
    Fixed(Duration),
    Exponential {
        initial: Duration,
        factor: u32,
        maximum: Option<Duration>,
    },
    Sequence {
        delays: Arc<[Duration]>,
        repeat_last: bool,
    },
    Ranges(Arc<[BackoffRange]>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Calculates the delay after a failed attempt.
pub struct Backoff {
    kind: BackoffKind,
}

impl Backoff {
    /// Creates a policy that retries without an added delay.
    pub fn none() -> Self {
        Self {
            kind: BackoffKind::None,
        }
    }

    /// Creates a constant delay between attempts.
    pub fn fixed(delay: Duration) -> Self {
        Self {
            kind: BackoffKind::Fixed(delay),
        }
    }

    /// Creates an exponential delay starting at `initial`, multiplying by
    /// `factor` after each failure, and optionally capping at `maximum`.
    pub fn exponential(
        initial: Duration,
        factor: u32,
        maximum: Option<Duration>,
    ) -> Result<Self, RetryPolicyError> {
        if factor == 0 {
            return Err(RetryPolicyError::InvalidExponentialFactor);
        }
        if maximum.is_some_and(|maximum| maximum < initial) {
            return Err(RetryPolicyError::InvalidExponentialMaximum);
        }
        Ok(Self {
            kind: BackoffKind::Exponential {
                initial,
                factor,
                maximum,
            },
        })
    }

    /// Creates an attempt-indexed delay sequence.
    ///
    /// Once exhausted, the last delay is reused when `repeat_last` is true;
    /// otherwise later retries have no added delay.
    pub fn sequence(
        delays: impl Into<Vec<Duration>>,
        repeat_last: bool,
    ) -> Result<Self, RetryPolicyError> {
        let delays = delays.into();
        if delays.is_empty() {
            return Err(RetryPolicyError::EmptySequence);
        }
        Ok(Self {
            kind: BackoffKind::Sequence {
                delays: delays.into(),
                repeat_last,
            },
        })
    }

    /// Creates strictly increasing inclusive attempt ranges.
    pub fn ranges(ranges: impl Into<Vec<BackoffRange>>) -> Result<Self, RetryPolicyError> {
        let ranges = ranges.into();
        if ranges.is_empty()
            || ranges
                .windows(2)
                .any(|pair| pair[0].through_attempt >= pair[1].through_attempt)
        {
            return Err(RetryPolicyError::InvalidRange);
        }
        Ok(Self {
            kind: BackoffKind::Ranges(ranges.into()),
        })
    }

    pub(crate) fn delay_after(&self, failed_attempt: u32) -> Result<Duration, RetryPolicyError> {
        match &self.kind {
            BackoffKind::None => Ok(Duration::ZERO),
            BackoffKind::Fixed(delay) => Ok(*delay),
            BackoffKind::Exponential {
                initial,
                factor,
                maximum,
            } => {
                let exponent = failed_attempt.saturating_sub(1);
                if exponent == 0 || initial.is_zero() || *factor == 1 {
                    return Ok(maximum.map_or(*initial, |maximum| (*initial).min(maximum)));
                }

                let mut delay = *initial;
                for _ in 0..exponent {
                    delay = match delay.checked_mul(*factor) {
                        Some(delay) => delay,
                        None => return maximum.ok_or(RetryPolicyError::DurationOverflow),
                    };
                    if let Some(maximum) = maximum {
                        if delay >= *maximum {
                            return Ok(*maximum);
                        }
                    }
                }
                Ok(delay)
            }
            BackoffKind::Sequence {
                delays,
                repeat_last,
            } => {
                let index = failed_attempt.saturating_sub(1) as usize;
                match delays.get(index) {
                    Some(delay) => Ok(*delay),
                    None if *repeat_last => delays.last().copied().ok_or_else(|| {
                        invalid_backoff_state(
                            RetryPolicyError::EmptySequence,
                            "delay_after",
                            failed_attempt,
                        )
                    }),
                    None => Ok(Duration::ZERO),
                }
            }
            BackoffKind::Ranges(ranges) => ranges
                .iter()
                .find(|range| failed_attempt <= range.through_attempt)
                .or_else(|| ranges.last())
                .map(|range| range.delay)
                .ok_or_else(|| {
                    invalid_backoff_state(
                        RetryPolicyError::InvalidRange,
                        "delay_after",
                        failed_attempt,
                    )
                }),
        }
    }

    fn maximum_delay_through(
        &self,
        failed_attempts: u32,
    ) -> Result<Option<Duration>, RetryPolicyError> {
        if failed_attempts == 0 {
            return Ok(None);
        }

        let maximum = match &self.kind {
            BackoffKind::None => Duration::ZERO,
            BackoffKind::Fixed(delay) => *delay,
            BackoffKind::Exponential { .. } => self.delay_after(failed_attempts)?,
            BackoffKind::Sequence { delays, .. } => {
                let used = usize::try_from(failed_attempts)
                    .unwrap_or(usize::MAX)
                    .min(delays.len());
                delays.iter().take(used).copied().max().ok_or_else(|| {
                    invalid_backoff_state(
                        RetryPolicyError::EmptySequence,
                        "maximum_delay_through",
                        failed_attempts,
                    )
                })?
            }
            BackoffKind::Ranges(ranges) => ranges
                .iter()
                .take_while(|range| range.through_attempt < failed_attempts)
                .chain(
                    ranges
                        .iter()
                        .find(|range| failed_attempts <= range.through_attempt),
                )
                .map(|range| range.delay)
                .max()
                .or_else(|| ranges.iter().map(|range| range.delay).max())
                .ok_or_else(|| {
                    invalid_backoff_state(
                        RetryPolicyError::InvalidRange,
                        "maximum_delay_through",
                        failed_attempts,
                    )
                })?,
        };
        Ok(Some(maximum))
    }
}

fn invalid_backoff_state(
    error: RetryPolicyError,
    operation: &'static str,
    failed_attempts: u32,
) -> RetryPolicyError {
    crate::diagnostic::error(
        error.code(),
        "retry",
        format_args!(
            "invalid backoff state operation={operation} failed_attempts={failed_attempts}"
        ),
    );
    error
}

impl Default for Backoff {
    fn default() -> Self {
        Self::none()
    }
}

/// Supplies deterministic or random additive jitter for retry delays.
pub trait JitterSource: Send + Sync + 'static {
    /// Samples a duration no greater than `upper_bound`.
    fn sample(&self, upper_bound: Duration) -> Duration;
}

#[derive(Clone)]
struct Jitter {
    maximum: Duration,
    source: Arc<dyn JitterSource>,
}

/// Immutable retry decisions for jobs whose error type is `E`.
///
/// `max_attempts` includes the initial attempt. Backoff, delay overrides, and
/// jitter apply only before a subsequent attempt.
pub struct RetryPolicy<E> {
    max_attempts: u32,
    backoff: Backoff,
    retry_if: RetryPredicate<E>,
    delay_override: Option<DelayOverride<E>>,
    jitter: Option<Jitter>,
    max_elapsed: Option<Duration>,
    attempt_timeout: Option<Duration>,
}

impl<E> Clone for RetryPolicy<E> {
    fn clone(&self) -> Self {
        Self {
            max_attempts: self.max_attempts,
            backoff: self.backoff.clone(),
            retry_if: Arc::clone(&self.retry_if),
            delay_override: self.delay_override.clone(),
            jitter: self.jitter.clone(),
            max_elapsed: self.max_elapsed,
            attempt_timeout: self.attempt_timeout,
        }
    }
}

impl<E> fmt::Debug for RetryPolicy<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("backoff", &self.backoff)
            .field("has_retry_predicate", &true)
            .field("has_delay_override", &self.delay_override.is_some())
            .field("has_jitter", &self.jitter.is_some())
            .field("max_elapsed", &self.max_elapsed)
            .field("attempt_timeout", &self.attempt_timeout)
            .finish()
    }
}

impl<E> RetryPolicy<E> {
    /// Starts a builder with the maximum total number of attempts.
    pub fn builder(max_attempts: u32) -> RetryPolicyBuilder<E> {
        RetryPolicyBuilder {
            max_attempts,
            backoff: Backoff::none(),
            retry_if: Arc::new(|_| true),
            delay_override: None,
            jitter: None,
            max_elapsed: None,
            attempt_timeout: None,
        }
    }

    /// Returns the maximum total number of attempts, including the first.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Returns the base backoff calculation.
    pub fn backoff(&self) -> &Backoff {
        &self.backoff
    }

    /// Returns the optional deadline covering attempts and retry waits.
    pub fn max_elapsed(&self) -> Option<Duration> {
        self.max_elapsed
    }

    /// Returns the optional timeout applied independently to each attempt.
    pub fn attempt_timeout(&self) -> Option<Duration> {
        self.attempt_timeout
    }

    pub(crate) fn should_retry(&self, error: &E) -> bool {
        (self.retry_if)(error)
    }

    pub(crate) fn delay_after(
        &self,
        error: &E,
        failed_attempt: u32,
    ) -> Result<Duration, RetryPolicyError> {
        let base = self
            .delay_override
            .as_ref()
            .and_then(|override_delay| override_delay(error, failed_attempt))
            .map_or_else(
                || self.backoff.delay_after(failed_attempt),
                Result::<_, RetryPolicyError>::Ok,
            )?;
        let sampled_jitter = self.jitter.as_ref().map_or(Duration::ZERO, |jitter| {
            jitter.source.sample(jitter.maximum)
        });
        if self
            .jitter
            .as_ref()
            .is_some_and(|jitter| sampled_jitter > jitter.maximum)
        {
            return Err(RetryPolicyError::JitterOutOfRange);
        }
        let delay = base
            .checked_add(sampled_jitter)
            .ok_or(RetryPolicyError::DurationOverflow)?;
        validate_timer_delay(delay)?;
        Ok(delay)
    }
}

/// Builder for a validated [`RetryPolicy`].
pub struct RetryPolicyBuilder<E> {
    max_attempts: u32,
    backoff: Backoff,
    retry_if: RetryPredicate<E>,
    delay_override: Option<DelayOverride<E>>,
    jitter: Option<Jitter>,
    max_elapsed: Option<Duration>,
    attempt_timeout: Option<Duration>,
}

impl<E: 'static> RetryPolicyBuilder<E> {
    /// Sets the base delay calculation.
    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Retries only errors for which `predicate` returns true.
    pub fn retry_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&E) -> bool + Send + Sync + 'static,
    {
        self.retry_if = Arc::new(predicate);
        self
    }

    /// Installs a per-error, per-failed-attempt delay override.
    /// Returning `None` retains the configured base backoff.
    pub fn delay_override<F>(mut self, delay_override: F) -> Self
    where
        F: Fn(&E, u32) -> Option<Duration> + Send + Sync + 'static,
    {
        self.delay_override = Some(Arc::new(delay_override));
        self
    }

    /// Adds jitter sampled from `source`, rejecting samples above `maximum`.
    pub fn jitter<S>(mut self, maximum: Duration, source: S) -> Self
    where
        S: JitterSource,
    {
        self.jitter = Some(Jitter {
            maximum,
            source: Arc::new(source),
        });
        self
    }

    /// Limits total elapsed execution and retry-wait time.
    pub fn max_elapsed(mut self, duration: Duration) -> Self {
        self.max_elapsed = Some(duration);
        self
    }

    /// Limits each individual job attempt.
    pub fn attempt_timeout(mut self, duration: Duration) -> Self {
        self.attempt_timeout = Some(duration);
        self
    }

    /// Validates and builds the retry policy.
    pub fn build(self) -> Result<RetryPolicy<E>, RetryPolicyError> {
        if self.max_attempts == 0 {
            return Err(RetryPolicyError::ZeroAttempts);
        }
        if self.max_elapsed == Some(Duration::ZERO) {
            return Err(RetryPolicyError::ZeroMaxElapsed);
        }
        if self.attempt_timeout == Some(Duration::ZERO) {
            return Err(RetryPolicyError::ZeroAttemptTimeout);
        }
        if self
            .max_elapsed
            .into_iter()
            .chain(self.attempt_timeout)
            .any(|duration| tokio::time::Instant::now().checked_add(duration).is_none())
        {
            return Err(RetryPolicyError::DurationOverflow);
        }
        if let Some(delay) = self
            .backoff
            .maximum_delay_through(self.max_attempts.saturating_sub(1))?
        {
            let delay = delay
                .checked_add(
                    self.jitter
                        .as_ref()
                        .map_or(Duration::ZERO, |jitter| jitter.maximum),
                )
                .ok_or(RetryPolicyError::DurationOverflow)?;
            validate_timer_delay(delay)?;
        }
        Ok(RetryPolicy {
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            retry_if: self.retry_if,
            delay_override: self.delay_override,
            jitter: self.jitter,
            max_elapsed: self.max_elapsed,
            attempt_timeout: self.attempt_timeout,
        })
    }
}

fn validate_timer_delay(delay: Duration) -> Result<(), RetryPolicyError> {
    tokio::time::Instant::now()
        .checked_add(delay)
        .map(|_| ())
        .ok_or(RetryPolicyError::DurationOverflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Explains why retry configuration or delay calculation failed.
pub enum RetryPolicyError {
    /// `max_attempts` was zero.
    ZeroAttempts,
    /// The total elapsed limit was zero.
    ZeroMaxElapsed,
    /// The per-attempt timeout was zero.
    ZeroAttemptTimeout,
    /// A sequence backoff contained no delays.
    EmptySequence,
    /// Attempt ranges were empty, zero-based, or not strictly increasing.
    InvalidRange,
    /// An exponential multiplier was zero.
    InvalidExponentialFactor,
    /// An exponential maximum was lower than its initial delay.
    InvalidExponentialMaximum,
    /// A configured or calculated duration cannot be represented.
    DurationOverflow,
    /// A jitter source returned a value above its promised maximum.
    JitterOutOfRange,
}

impl RetryPolicyError {
    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ZeroAttempts => "BB-RETRY-001",
            Self::ZeroMaxElapsed => "BB-RETRY-002",
            Self::ZeroAttemptTimeout => "BB-RETRY-003",
            Self::EmptySequence => "BB-RETRY-004",
            Self::InvalidRange => "BB-RETRY-005",
            Self::InvalidExponentialFactor => "BB-RETRY-006",
            Self::InvalidExponentialMaximum => "BB-RETRY-007",
            Self::DurationOverflow => "BB-RETRY-008",
            Self::JitterOutOfRange => "BB-RETRY-009",
        }
    }
}

impl fmt::Display for RetryPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ZeroAttempts => "retry max_attempts must be at least one",
            Self::ZeroMaxElapsed => "retry max_elapsed must be greater than zero",
            Self::ZeroAttemptTimeout => "retry attempt timeout must be greater than zero",
            Self::EmptySequence => "retry backoff sequence must not be empty",
            Self::InvalidRange => "retry backoff ranges must be non-empty and strictly increasing",
            Self::InvalidExponentialFactor => "retry exponential factor must be at least one",
            Self::InvalidExponentialMaximum => {
                "retry exponential maximum must not be less than its initial delay"
            }
            Self::DurationOverflow => "retry delay exceeds Duration range",
            Self::JitterOutOfRange => "jitter source returned more than its configured maximum",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RetryPolicyError {}

#[cfg(test)]
mod tests {
    use super::{Backoff, BackoffKind, BackoffRange, RetryPolicy, RetryPolicyError};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn invalid_internal_backoff_state_returns_typed_errors_and_logs_codes() {
        crate::test_log::init();
        let empty_sequence = Backoff {
            kind: BackoffKind::Sequence {
                delays: Arc::from([]),
                repeat_last: true,
            },
        };
        assert_eq!(
            empty_sequence.delay_after(2),
            Err(RetryPolicyError::EmptySequence)
        );
        assert_eq!(
            empty_sequence.maximum_delay_through(1),
            Err(RetryPolicyError::EmptySequence)
        );
        assert!(crate::test_log::contains("BB-RETRY-004"));

        let empty_ranges = Backoff {
            kind: BackoffKind::Ranges(Arc::from([])),
        };
        assert_eq!(
            empty_ranges.delay_after(1),
            Err(RetryPolicyError::InvalidRange)
        );
        assert_eq!(
            empty_ranges.maximum_delay_through(1),
            Err(RetryPolicyError::InvalidRange)
        );
        assert!(crate::test_log::contains("BB-RETRY-005"));
    }

    #[test]
    fn backoff_boundaries_are_exact() {
        let exponential =
            Backoff::exponential(Duration::from_secs(2), 3, Some(Duration::from_secs(10))).unwrap();
        assert_eq!(exponential.delay_after(1).unwrap(), Duration::from_secs(2));
        assert_eq!(exponential.delay_after(2).unwrap(), Duration::from_secs(6));
        assert_eq!(exponential.delay_after(3).unwrap(), Duration::from_secs(10));

        let sequence =
            Backoff::sequence(vec![Duration::from_secs(1), Duration::from_secs(4)], true).unwrap();
        assert_eq!(sequence.delay_after(1).unwrap(), Duration::from_secs(1));
        assert_eq!(sequence.delay_after(3).unwrap(), Duration::from_secs(4));

        let ranges = Backoff::ranges(vec![
            BackoffRange::new(2, Duration::from_secs(5)).unwrap(),
            BackoffRange::new(4, Duration::from_secs(9)).unwrap(),
        ])
        .unwrap();
        assert_eq!(ranges.delay_after(2).unwrap(), Duration::from_secs(5));
        assert_eq!(ranges.delay_after(3).unwrap(), Duration::from_secs(9));
        assert_eq!(ranges.delay_after(8).unwrap(), Duration::from_secs(9));
    }

    #[test]
    fn invalid_retry_configuration_is_rejected() {
        assert_eq!(
            RetryPolicy::<()>::builder(0).build().unwrap_err(),
            RetryPolicyError::ZeroAttempts
        );
        assert_eq!(
            Backoff::sequence(Vec::new(), false).unwrap_err(),
            RetryPolicyError::EmptySequence
        );
        assert_eq!(
            Backoff::exponential(Duration::from_secs(2), 0, None).unwrap_err(),
            RetryPolicyError::InvalidExponentialFactor
        );
        assert_eq!(
            RetryPolicy::<()>::builder(1)
                .attempt_timeout(Duration::ZERO)
                .build()
                .unwrap_err(),
            RetryPolicyError::ZeroAttemptTimeout
        );
        assert_eq!(
            RetryPolicy::<()>::builder(1)
                .max_elapsed(Duration::MAX)
                .build()
                .unwrap_err(),
            RetryPolicyError::DurationOverflow
        );
        assert_eq!(
            RetryPolicy::<()>::builder(2)
                .backoff(Backoff::fixed(Duration::MAX))
                .build()
                .unwrap_err(),
            RetryPolicyError::DurationOverflow
        );
    }

    #[test]
    fn dynamic_retry_delay_is_checked_against_the_monotonic_clock() {
        let policy = RetryPolicy::builder(2)
            .delay_override(|_: &(), _| Some(Duration::MAX))
            .build()
            .unwrap();

        assert_eq!(
            policy.delay_after(&(), 1),
            Err(RetryPolicyError::DurationOverflow)
        );
    }

    #[test]
    fn very_large_attempt_limit_does_not_make_policy_construction_linear() {
        let (finished, completion) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = RetryPolicy::<()>::builder(u32::MAX).build();
            let _ = finished.send(result.is_ok());
        });

        assert_eq!(
            completion.recv_timeout(Duration::from_secs(1)),
            Ok(true),
            "policy construction must not iterate over every possible attempt"
        );
    }

    #[test]
    fn capped_exponential_backoff_handles_extreme_attempt_numbers() {
        let maximum = Duration::from_secs(1);
        let policy = RetryPolicy::<()>::builder(u32::MAX)
            .backoff(Backoff::exponential(Duration::from_millis(1), 2, Some(maximum)).unwrap())
            .build()
            .unwrap();

        assert_eq!(policy.backoff().delay_after(u32::MAX).unwrap(), maximum);
    }
}
