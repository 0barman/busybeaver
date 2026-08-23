use crate::execution::{
    self, Cancelled, ChildHandle, ExecutionRegistry, PreparedExecution, SpawnChildError, TaskExit,
    TaskFailure, WorkContext,
};
use crate::ids::{AttemptId, TaskSpecId};
use crate::AbortPolicy;
use crate::PanicSource;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::time::Instant;

type BoxAttemptFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>;
type RetryFactory<T, E> = dyn Fn(AttemptContext) -> BoxAttemptFuture<T, E> + Send + Sync + 'static;
type RetryPredicate<E> =
    dyn for<'a> Fn(RetryDecisionContext<'a, E>) -> bool + Send + Sync + 'static;

const MAX_ATTEMPTS: u32 = 1_000_000;

/// Delay policy applied only between failed attempts.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Backoff {
    None,
    Fixed(Duration),
    Explicit(Vec<Duration>),
    Exponential {
        base: Duration,
        multiplier: f64,
        cap: Duration,
    },
}

impl Backoff {
    pub fn fixed(delay: Duration) -> Self {
        Self::Fixed(delay)
    }

    pub fn explicit(delays: impl IntoIterator<Item = Duration>) -> Self {
        Self::Explicit(delays.into_iter().collect())
    }

    pub fn exponential(base: Duration, multiplier: f64, cap: Duration) -> Self {
        Self::Exponential {
            base,
            multiplier,
            cap,
        }
    }
}

/// Deterministic, explicitly-seeded proportional jitter.
#[derive(Clone, Copy, Debug)]
pub struct Jitter {
    seed: u64,
    ratio: f64,
}

impl Jitter {
    pub fn seeded(seed: u64, ratio: f64) -> Self {
        Self { seed, ratio }
    }

    pub(crate) fn is_valid(self) -> bool {
        self.ratio.is_finite() && (0.0..=1.0).contains(&self.ratio)
    }

    pub(crate) fn apply(self, delay: Duration, stream_index: u64) -> Option<Duration> {
        if !self.is_valid() {
            return None;
        }
        let mut state = self.seed ^ stream_index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let unit = state as f64 / u64::MAX as f64;
        let factor = (1.0 - self.ratio) + (2.0 * self.ratio * unit);
        Duration::try_from_secs_f64(delay.as_secs_f64() * factor).ok()
    }
}

/// Retry configuration validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryBuildError {
    InvalidAttemptCount,
    AttemptCountTooLarge,
    RetryPolicyRequired,
    DelayCountMismatch { expected: usize, actual: usize },
    InvalidBackoffMultiplier,
    BackoffCapBelowBase,
    InvalidJitterRatio,
    ZeroAttemptTimeout,
    ConflictingDeadline,
    DurationOverflow,
}

impl fmt::Display for RetryBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAttemptCount => formatter.write_str("max_attempts must be at least one"),
            Self::AttemptCountTooLarge => formatter.write_str("max_attempts exceeds the SDK limit"),
            Self::RetryPolicyRequired => formatter.write_str(
                "multiple attempts require retry_if or an explicit retry_all_errors acknowledgement",
            ),
            Self::DelayCountMismatch { expected, actual } => write!(
                formatter,
                "retry delay count mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidBackoffMultiplier => {
                formatter.write_str("exponential multiplier must be finite and at least one")
            }
            Self::BackoffCapBelowBase => {
                formatter.write_str("exponential backoff cap must not be below its base")
            }
            Self::InvalidJitterRatio => {
                formatter.write_str("jitter ratio must be finite and between zero and one")
            }
            Self::ZeroAttemptTimeout => {
                formatter.write_str("attempt timeout must be greater than zero")
            }
            Self::ConflictingDeadline => {
                formatter.write_str("deadline and overall_timeout are mutually exclusive")
            }
            Self::DurationOverflow => formatter.write_str("retry duration arithmetic overflowed"),
        }
    }
}

impl std::error::Error for RetryBuildError {}

/// Runtime policy stage isolated by a panic boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RetryPolicyStage {
    Predicate,
    Backoff,
}

/// Typed retry-specific business failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum RetryFailure<E> {
    NonRetryable {
        error: E,
    },
    Exhausted {
        last_error: E,
    },
    AttemptTimedOut {
        attempt: u32,
        previous_error: Option<E>,
        may_have_side_effects: bool,
    },
}

/// Immutable information passed to the retry predicate.
pub struct RetryDecisionContext<'a, E> {
    pub error: &'a E,
    pub attempt: u32,
    pub elapsed: Duration,
    pub remaining_budget: Option<Duration>,
}

/// Context for one 1-based retry attempt.
#[derive(Clone)]
pub struct AttemptContext {
    work: WorkContext,
    accepted_at: Instant,
    deadline: Option<Instant>,
}

impl AttemptContext {
    pub fn id(&self) -> AttemptId {
        AttemptId {
            execution_id: self.work.execution_id(),
            number: self.work.attempt(),
        }
    }

    pub fn number(&self) -> u32 {
        self.work.attempt()
    }

    pub fn elapsed(&self) -> Duration {
        self.accepted_at.elapsed()
    }

    pub fn remaining_budget(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub fn is_cancelled(&self) -> bool {
        self.work.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.work.cancelled().await;
    }

    pub async fn sleep(&self, duration: Duration) -> Result<(), Cancelled> {
        self.work.sleep(duration).await
    }

    pub fn control(&self) -> crate::TaskControlHandle {
        self.work.control()
    }

    pub fn spawn_child<F, T, E>(&self, future: F) -> Result<ChildHandle<T, E>, SpawnChildError>
    where
        F: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        self.work.spawn_child(future)
    }
}

enum DeadlineConfig {
    None,
    Absolute(Instant),
    Overall(Duration),
}

#[derive(Clone)]
enum DelaySource {
    Zero,
    Fixed(Duration),
    Explicit(Arc<[Duration]>),
    Exponential {
        base: Duration,
        multiplier: f64,
        cap: Duration,
    },
}

#[derive(Clone)]
struct DelayPlan {
    source: DelaySource,
    jitter: Option<Jitter>,
    jitter_cap: Option<Duration>,
    len: usize,
}

impl DelayPlan {
    fn build(
        backoff: Backoff,
        retry_count: usize,
        jitter: Option<Jitter>,
    ) -> Result<Self, RetryBuildError> {
        let (source, jitter_cap) = match backoff {
            Backoff::None => (DelaySource::Zero, None),
            Backoff::Fixed(delay) => (DelaySource::Fixed(delay), None),
            Backoff::Explicit(delays) => {
                if delays.len() != retry_count {
                    return Err(RetryBuildError::DelayCountMismatch {
                        expected: retry_count,
                        actual: delays.len(),
                    });
                }
                (DelaySource::Explicit(delays.into()), None)
            }
            Backoff::Exponential {
                base,
                multiplier,
                cap,
            } => {
                if !multiplier.is_finite() || multiplier < 1.0 {
                    return Err(RetryBuildError::InvalidBackoffMultiplier);
                }
                if cap < base {
                    return Err(RetryBuildError::BackoffCapBelowBase);
                }
                (
                    DelaySource::Exponential {
                        base,
                        multiplier,
                        cap,
                    },
                    Some(cap),
                )
            }
        };
        let plan = Self {
            source,
            jitter,
            jitter_cap,
            len: retry_count,
        };
        for delay in plan.base_iter() {
            delay?;
        }
        if let Some(jitter) = jitter {
            if !jitter.is_valid() {
                return Err(RetryBuildError::InvalidJitterRatio);
            }
            for delay in plan.iter() {
                delay?;
            }
        }
        Ok(plan)
    }

    fn base_iter(&self) -> BaseDelayIter<'_> {
        match &self.source {
            DelaySource::Zero => BaseDelayIter::Repeated {
                delay: Duration::ZERO,
                remaining: self.len,
            },
            DelaySource::Fixed(delay) => BaseDelayIter::Repeated {
                delay: *delay,
                remaining: self.len,
            },
            DelaySource::Explicit(delays) => BaseDelayIter::Explicit(delays.iter()),
            DelaySource::Exponential {
                base,
                multiplier,
                cap,
            } => BaseDelayIter::Exponential {
                remaining: self.len,
                seconds: base.as_secs_f64(),
                multiplier: *multiplier,
                cap_seconds: cap.as_secs_f64(),
            },
        }
    }

    fn iter(&self) -> DelayIter<'_> {
        DelayIter {
            base: self.base_iter(),
            jitter: self.jitter.map(|jitter| JitterState {
                state: jitter.seed,
                ratio: jitter.ratio,
            }),
            cap: self.jitter_cap,
        }
    }

    fn materialize_for_debug(&self) -> Vec<Duration> {
        let mut delays = Vec::with_capacity(self.len);
        for delay in self.iter() {
            match delay {
                Ok(delay) => delays.push(delay),
                Err(_) => {
                    crate::internal::log_internal_error(
                        "BB-RETRY-DELAY-DEBUG-INVALID",
                        "validated retry delay plan could not be materialized for Debug",
                    );
                    break;
                }
            }
        }
        delays
    }
}

enum BaseDelayIter<'a> {
    Repeated {
        delay: Duration,
        remaining: usize,
    },
    Explicit(std::slice::Iter<'a, Duration>),
    Exponential {
        remaining: usize,
        seconds: f64,
        multiplier: f64,
        cap_seconds: f64,
    },
}

impl Iterator for BaseDelayIter<'_> {
    type Item = Result<Duration, RetryBuildError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Repeated { delay, remaining } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining = remaining.saturating_sub(1);
                Some(Ok(*delay))
            }
            Self::Explicit(delays) => delays.next().copied().map(Ok),
            Self::Exponential {
                remaining,
                seconds,
                multiplier,
                cap_seconds,
            } => {
                if *remaining == 0 {
                    return None;
                }
                *remaining = remaining.saturating_sub(1);
                let delay = Duration::try_from_secs_f64((*seconds).min(*cap_seconds))
                    .map_err(|_| RetryBuildError::DurationOverflow);
                *seconds = (*seconds * *multiplier).min(*cap_seconds);
                if !seconds.is_finite() {
                    return Some(Err(RetryBuildError::DurationOverflow));
                }
                Some(delay)
            }
        }
    }
}

struct JitterState {
    state: u64,
    ratio: f64,
}

struct DelayIter<'a> {
    base: BaseDelayIter<'a>,
    jitter: Option<JitterState>,
    cap: Option<Duration>,
}

impl Iterator for DelayIter<'_> {
    type Item = Result<Duration, RetryBuildError>;

    fn next(&mut self) -> Option<Self::Item> {
        let delay = match self.base.next()? {
            Ok(delay) => delay,
            Err(error) => return Some(Err(error)),
        };
        let Some(jitter) = self.jitter.as_mut() else {
            return Some(Ok(delay));
        };
        jitter.state ^= jitter.state << 13;
        jitter.state ^= jitter.state >> 7;
        jitter.state ^= jitter.state << 17;
        let unit = jitter.state as f64 / u64::MAX as f64;
        let factor = (1.0 - jitter.ratio) + (2.0 * jitter.ratio * unit);
        let jittered = match Duration::try_from_secs_f64(delay.as_secs_f64() * factor) {
            Ok(delay) => delay,
            Err(_) => return Some(Err(RetryBuildError::DurationOverflow)),
        };
        Some(Ok(self.cap.map_or(jittered, |cap| jittered.min(cap))))
    }
}

impl Clone for DeadlineConfig {
    fn clone(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::Absolute(deadline) => Self::Absolute(*deadline),
            Self::Overall(duration) => Self::Overall(*duration),
        }
    }
}

/// Reusable validated typed retry definition.
pub struct RetrySpec<T, E> {
    id: TaskSpecId,
    factory: Arc<RetryFactory<T, E>>,
    predicate: Option<Arc<RetryPredicate<E>>>,
    retry_all_errors: bool,
    max_attempts: u32,
    delays: DelayPlan,
    attempt_timeout: Option<Duration>,
    retry_timed_out_attempts: bool,
    deadline: DeadlineConfig,
    tag: Option<Arc<str>>,
    abort_policy: AbortPolicy,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for RetrySpec<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            factory: Arc::clone(&self.factory),
            predicate: self.predicate.clone(),
            retry_all_errors: self.retry_all_errors,
            max_attempts: self.max_attempts,
            delays: self.delays.clone(),
            attempt_timeout: self.attempt_timeout,
            retry_timed_out_attempts: self.retry_timed_out_attempts,
            deadline: self.deadline.clone(),
            tag: self.tag.clone(),
            abort_policy: self.abort_policy,
            marker: PhantomData,
        }
    }
}

impl<T, E> fmt::Debug for RetrySpec<T, E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let delays = self.delays.materialize_for_debug();
        formatter
            .debug_struct("RetrySpec")
            .field("id", &self.id)
            .field("max_attempts", &self.max_attempts)
            .field("delays", &delays)
            .field("attempt_timeout", &self.attempt_timeout)
            .field("retry_timed_out_attempts", &self.retry_timed_out_attempts)
            .finish_non_exhaustive()
    }
}

impl<T, E> RetrySpec<T, E> {
    pub fn id(&self) -> TaskSpecId {
        self.id
    }

    pub(crate) fn deadline_at(&self, accepted_at: Instant) -> Result<Option<Instant>, ()> {
        match self.deadline {
            DeadlineConfig::None => Ok(None),
            DeadlineConfig::Absolute(deadline) => Ok(Some(deadline)),
            DeadlineConfig::Overall(duration) => {
                accepted_at.checked_add(duration).map(Some).ok_or(())
            }
        }
    }
}

/// Builder for an immutable reusable typed retry definition.
pub struct RetryBuilder<T, E> {
    factory: Arc<RetryFactory<T, E>>,
    predicate: Option<Arc<RetryPredicate<E>>>,
    retry_all_errors: bool,
    max_attempts: u32,
    backoff: Backoff,
    jitter: Option<Jitter>,
    attempt_timeout: Option<Duration>,
    retry_timed_out_attempts: bool,
    deadline: Option<Instant>,
    overall_timeout: Option<Duration>,
    tag: Option<Arc<str>>,
    abort_policy: AbortPolicy,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> RetryBuilder<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    pub fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(AttemptContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        Self {
            factory: Arc::new(move |context| Box::pin(factory(context))),
            predicate: None,
            retry_all_errors: false,
            max_attempts: 1,
            backoff: Backoff::None,
            jitter: None,
            attempt_timeout: None,
            retry_timed_out_attempts: false,
            deadline: None,
            overall_timeout: None,
            tag: None,
            abort_policy: AbortPolicy::CooperativeOnly,
            marker: PhantomData,
        }
    }

    pub fn max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    pub fn delays(mut self, delays: impl IntoIterator<Item = Duration>) -> Self {
        self.backoff = Backoff::explicit(delays);
        self
    }

    pub fn fixed_delay(mut self, delay: Duration) -> Self {
        self.backoff = Backoff::fixed(delay);
        self
    }

    pub fn jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = Some(jitter);
        self
    }

    pub fn retry_if<F>(mut self, predicate: F) -> Self
    where
        F: for<'a> Fn(RetryDecisionContext<'a, E>) -> bool + Send + Sync + 'static,
    {
        self.predicate = Some(Arc::new(predicate));
        self.retry_all_errors = false;
        self
    }

    pub fn retry_all_errors(mut self) -> Self {
        self.predicate = None;
        self.retry_all_errors = true;
        self
    }

    pub fn attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = Some(timeout);
        self
    }

    pub fn retry_timed_out_attempts(mut self) -> Self {
        self.retry_timed_out_attempts = true;
        self
    }

    pub fn deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn overall_timeout(mut self, timeout: Duration) -> Self {
        self.overall_timeout = Some(timeout);
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Arc::from(tag.into()));
        self
    }

    pub fn abort_policy(mut self, policy: AbortPolicy) -> Self {
        self.abort_policy = policy;
        self
    }

    pub fn build(self) -> Result<RetrySpec<T, E>, RetryBuildError> {
        if self.max_attempts == 0 {
            return Err(RetryBuildError::InvalidAttemptCount);
        }
        if self.max_attempts > MAX_ATTEMPTS {
            return Err(RetryBuildError::AttemptCountTooLarge);
        }
        if self.max_attempts > 1 && self.predicate.is_none() && !self.retry_all_errors {
            return Err(RetryBuildError::RetryPolicyRequired);
        }
        if self.deadline.is_some() && self.overall_timeout.is_some() {
            return Err(RetryBuildError::ConflictingDeadline);
        }
        if self.attempt_timeout == Some(Duration::ZERO) {
            return Err(RetryBuildError::ZeroAttemptTimeout);
        }
        if let Some(timeout) = self.attempt_timeout {
            Instant::now()
                .checked_add(timeout)
                .ok_or(RetryBuildError::DurationOverflow)?;
        }
        if let Some(timeout) = self.overall_timeout {
            Instant::now()
                .checked_add(timeout)
                .ok_or(RetryBuildError::DurationOverflow)?;
        }

        let retries = self
            .max_attempts
            .checked_sub(1)
            .ok_or(RetryBuildError::InvalidAttemptCount)?;
        let retry_count =
            usize::try_from(retries).map_err(|_| RetryBuildError::AttemptCountTooLarge)?;
        let delays = DelayPlan::build(self.backoff, retry_count, self.jitter)?;
        let deadline = match (self.deadline, self.overall_timeout) {
            (Some(deadline), None) => DeadlineConfig::Absolute(deadline),
            (None, Some(timeout)) => DeadlineConfig::Overall(timeout),
            (None, None) => DeadlineConfig::None,
            (Some(_), Some(_)) => return Err(RetryBuildError::ConflictingDeadline),
        };
        Ok(RetrySpec {
            id: TaskSpecId::new(),
            factory: self.factory,
            predicate: self.predicate,
            retry_all_errors: self.retry_all_errors,
            max_attempts: self.max_attempts,
            delays,
            attempt_timeout: self.attempt_timeout,
            retry_timed_out_attempts: self.retry_timed_out_attempts,
            deadline,
            tag: self.tag,
            abort_policy: self.abort_policy,
            marker: PhantomData,
        })
    }
}

#[cfg(test)]
fn build_delays(backoff: Backoff, retry_count: usize) -> Result<Vec<Duration>, RetryBuildError> {
    match backoff {
        Backoff::None => Ok(vec![Duration::ZERO; retry_count]),
        Backoff::Fixed(delay) => Ok(vec![delay; retry_count]),
        Backoff::Explicit(delays) => {
            if delays.len() != retry_count {
                return Err(RetryBuildError::DelayCountMismatch {
                    expected: retry_count,
                    actual: delays.len(),
                });
            }
            Ok(delays)
        }
        Backoff::Exponential {
            base,
            multiplier,
            cap,
        } => {
            if !multiplier.is_finite() || multiplier < 1.0 {
                return Err(RetryBuildError::InvalidBackoffMultiplier);
            }
            if cap < base {
                return Err(RetryBuildError::BackoffCapBelowBase);
            }
            let mut delays = Vec::with_capacity(retry_count);
            let mut seconds = base.as_secs_f64();
            let cap_seconds = cap.as_secs_f64();
            for _ in 0..retry_count {
                let delay = Duration::try_from_secs_f64(seconds.min(cap_seconds))
                    .map_err(|_| RetryBuildError::DurationOverflow)?;
                delays.push(delay);
                seconds = (seconds * multiplier).min(cap_seconds);
                if !seconds.is_finite() {
                    return Err(RetryBuildError::DurationOverflow);
                }
            }
            Ok(delays)
        }
    }
}

#[cfg(test)]
fn apply_jitter(
    delays: &mut [Duration],
    jitter: Jitter,
    cap: Option<Duration>,
) -> Result<(), RetryBuildError> {
    if !jitter.ratio.is_finite() || !(0.0..=1.0).contains(&jitter.ratio) {
        return Err(RetryBuildError::InvalidJitterRatio);
    }
    let mut state = jitter.seed;
    for delay in delays {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let unit = (state as f64) / (u64::MAX as f64);
        let factor = (1.0 - jitter.ratio) + (2.0 * jitter.ratio * unit);
        let jittered = Duration::try_from_secs_f64(delay.as_secs_f64() * factor)
            .map_err(|_| RetryBuildError::DurationOverflow)?;
        *delay = cap.map_or(jittered, |cap| jittered.min(cap));
    }
    Ok(())
}

pub(crate) fn prepare_retry<T, E>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    spec: RetrySpec<T, E>,
    accepted_at: Instant,
    deadline: Option<Instant>,
) -> crate::BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let task_spec_id = spec.id;
    let tag = spec.tag.clone();
    let abort_policy = spec.abort_policy;
    let deadline_runtime = runtime.clone();
    let prepared =
        execution::prepare_exit_future(registry, runtime, task_spec_id, tag, move |work| {
            run_retry(spec, work, accepted_at, deadline)
        })?;
    if let Some(deadline) = deadline {
        let control = prepared.handle.control();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            deadline_runtime.spawn(async move {
                tokio::select! {
                    biased;
                    _ = control.wait() => {}
                    _ = tokio::time::sleep_until(deadline) => {
                        control.mark_deadline();
                    }
                }
            })
        }));
    }
    prepared.start.set_abort_policy(abort_policy);
    Ok(prepared)
}

pub(crate) async fn run_retry<T, E>(
    spec: RetrySpec<T, E>,
    base_work: WorkContext,
    accepted_at: Instant,
    deadline: Option<Instant>,
) -> TaskExit<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let mut previous_error = None;
    let mut delays = spec.delays.iter();
    for number in 1..=spec.max_attempts {
        let work = base_work.for_attempt(number);
        work.begin_attempt();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            work.mark_deadline();
            return deadline_exit(previous_error);
        }
        if let Some(exit) = selected_stop_exit(&work, &mut previous_error) {
            return exit;
        }

        let context = AttemptContext {
            work: work.clone(),
            accepted_at,
            deadline,
        };
        let future = catch_unwind(AssertUnwindSafe(|| (spec.factory)(context)));
        let outcome = match future {
            Ok(future) => wait_attempt(future, work.clone(), deadline, spec.attempt_timeout).await,
            Err(payload) => {
                let message = panic_message(payload);
                if execution::is_timer_unavailable_message(&message) {
                    AttemptOutcome::TimerUnavailable
                } else {
                    AttemptOutcome::Panicked {
                        source: PanicSource::Factory,
                        message,
                    }
                }
            }
        };
        work.close_attempt_children_and_wait().await;

        if let Some(exit) = selected_stop_exit(&work, &mut previous_error) {
            return exit;
        }
        match outcome {
            AttemptOutcome::Completed(value) => return TaskExit::Completed(value),
            AttemptOutcome::Panicked { source, message } => {
                return TaskExit::Panicked { source, message };
            }
            AttemptOutcome::ExecutorStopped => {
                return TaskExit::ExecutorStopped {
                    reason: crate::ExecutorStopReason::RuntimeUnavailable,
                };
            }
            AttemptOutcome::TimerUnavailable => {
                return TaskExit::Failed(TaskFailure::TimerUnavailable);
            }
            AttemptOutcome::Deadline => {
                work.mark_deadline();
                return deadline_exit(previous_error);
            }
            AttemptOutcome::TimedOut => {
                if !spec.retry_timed_out_attempts || number == spec.max_attempts {
                    return TaskExit::Failed(TaskFailure::Retry(RetryFailure::AttemptTimedOut {
                        attempt: number,
                        previous_error,
                        may_have_side_effects: true,
                    }));
                }
            }
            AttemptOutcome::Failed(error) => {
                if number == spec.max_attempts {
                    return TaskExit::Failed(TaskFailure::Retry(RetryFailure::Exhausted {
                        last_error: error,
                    }));
                }
                let decision = RetryDecisionContext {
                    error: &error,
                    attempt: number,
                    elapsed: accepted_at.elapsed(),
                    remaining_budget: deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now())),
                };
                let retry = if spec.retry_all_errors {
                    Ok(true)
                } else {
                    let Some(predicate) = spec.predicate.as_ref() else {
                        crate::internal::log_internal_error(
                            "BB-RETRY-PREDICATE-MISSING",
                            "validated retry execution is missing its predicate",
                        );
                        return TaskExit::ExecutorStopped {
                            reason: crate::ExecutorStopReason::InternalInvariantViolation,
                        };
                    };
                    catch_unwind(AssertUnwindSafe(|| predicate(decision)))
                };
                match retry {
                    Ok(false) => {
                        return TaskExit::Failed(TaskFailure::Retry(RetryFailure::NonRetryable {
                            error,
                        }));
                    }
                    Ok(true) => previous_error = Some(error),
                    Err(_) => {
                        return TaskExit::Failed(TaskFailure::PolicyPanicked {
                            stage: RetryPolicyStage::Predicate,
                            last_error: Some(error),
                        });
                    }
                }
            }
        }

        let Some(delay) = delays.next() else {
            crate::internal::log_internal_error(
                "BB-RETRY-DELAY-MISSING",
                "validated retry execution is missing a between-attempt delay",
            );
            return TaskExit::ExecutorStopped {
                reason: crate::ExecutorStopReason::InternalInvariantViolation,
            };
        };
        let delay = match delay {
            Ok(delay) => delay,
            Err(_) => {
                crate::internal::log_internal_error(
                    "BB-RETRY-DELAY-INVALID",
                    "validated retry delay plan failed during execution",
                );
                return TaskExit::ExecutorStopped {
                    reason: crate::ExecutorStopReason::InternalInvariantViolation,
                };
            }
        };
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else if work.sleep(delay).await.is_err() {
            if let Some(exit) = selected_stop_exit(&work, &mut previous_error) {
                return exit;
            }
            return TaskExit::Failed(TaskFailure::PolicyPanicked {
                stage: RetryPolicyStage::Backoff,
                last_error: previous_error,
            });
        }
        if let Some(exit) = selected_stop_exit(&work, &mut previous_error) {
            return exit;
        }
    }
    crate::internal::log_internal_error(
        "BB-RETRY-LOOP-FELL-THROUGH",
        "validated retry loop exited without a terminal outcome",
    );
    TaskExit::ExecutorStopped {
        reason: crate::ExecutorStopReason::InternalInvariantViolation,
    }
}

enum AttemptOutcome<T, E> {
    Completed(T),
    Failed(E),
    TimedOut,
    Deadline,
    Panicked {
        source: PanicSource,
        message: String,
    },
    ExecutorStopped,
    TimerUnavailable,
}

async fn wait_attempt<T, E>(
    future: BoxAttemptFuture<T, E>,
    work: WorkContext,
    deadline: Option<Instant>,
    attempt_timeout: Option<Duration>,
) -> AttemptOutcome<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let mut task = tokio::spawn(future);
    let attempt_deadline = match attempt_timeout {
        Some(timeout) => match Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => return AttemptOutcome::TimedOut,
        },
        None => None,
    };
    let joined = match (deadline, attempt_deadline) {
        (Some(deadline), Some(attempt_deadline)) => {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    work.mark_deadline();
                    task.abort();
                    let _ = (&mut task).await;
                    return AttemptOutcome::Deadline;
                }
                _ = tokio::time::sleep_until(attempt_deadline) => {
                    task.abort();
                    let _ = (&mut task).await;
                    return AttemptOutcome::TimedOut;
                }
                result = &mut task => result,
            }
        }
        (Some(deadline), None) => {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    work.mark_deadline();
                    task.abort();
                    let _ = (&mut task).await;
                    return AttemptOutcome::Deadline;
                }
                result = &mut task => result,
            }
        }
        (None, Some(attempt_deadline)) => {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(attempt_deadline) => {
                    task.abort();
                    let _ = (&mut task).await;
                    return AttemptOutcome::TimedOut;
                }
                result = &mut task => result,
            }
        }
        (None, None) => (&mut task).await,
    };
    match joined {
        Ok(Ok(value)) => AttemptOutcome::Completed(value),
        Ok(Err(error)) => AttemptOutcome::Failed(error),
        Err(error) if error.is_panic() => {
            let Some(payload) =
                crate::internal::take_join_panic(error, "BB-RETRY-JOIN-PANIC-MISCLASSIFIED")
            else {
                return AttemptOutcome::ExecutorStopped;
            };
            let message = panic_message(payload);
            if execution::is_timer_unavailable_message(&message) {
                AttemptOutcome::TimerUnavailable
            } else {
                AttemptOutcome::Panicked {
                    source: PanicSource::WorkFuture,
                    message,
                }
            }
        }
        Err(_) => AttemptOutcome::ExecutorStopped,
    }
}

fn selected_stop_exit<T, E>(
    work: &WorkContext,
    last_error: &mut Option<E>,
) -> Option<TaskExit<T, E>> {
    match work.stop_cause() {
        Some(crate::StopCauseSummary::Cancel(reason)) => Some(TaskExit::Cancelled { reason }),
        Some(crate::StopCauseSummary::Deadline) => Some(deadline_exit(last_error.take())),
        None => None,
    }
}

fn deadline_exit<T, E>(last_error: Option<E>) -> TaskExit<T, E> {
    TaskExit::Failed(TaskFailure::DeadlineExceeded { last_error })
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

#[cfg(test)]
mod tests;
