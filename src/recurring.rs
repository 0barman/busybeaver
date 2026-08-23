use crate::execution::{self, ExecutionKind, ExecutionRegistry, PreparedExecution};
use crate::retry::{self, Jitter, RetrySpec};
use crate::{
    AbortPolicy, Cancelled, ExecutorStopReason, PanicSource, TaskExit, TaskFailure, TaskSpecId,
    WorkContext,
};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::time::Instant;

type BoxTickFuture<T, E> =
    Pin<Box<dyn Future<Output = Result<TickOutcome<T>, E>> + Send + 'static>>;
type TickFactory<T, E> = dyn Fn(TickContext) -> BoxTickFuture<T, E> + Send + Sync + 'static;
type ScheduleFunction = dyn Fn(ScheduleDecisionContext) -> Option<Duration> + Send + Sync + 'static;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TickOutcome<T> {
    Continue,
    Stop(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ScheduleMode {
    FixedDelay,
    FixedRate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MissedTickPolicy {
    Burst,
    Skip,
    Delay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResumePolicy {
    Continue,
    RunImmediately,
    SkipMissed,
}

#[derive(Clone, Copy, Debug)]
pub struct ScheduleDecisionContext {
    pub completed_ticks: u64,
    pub last_started_at: Instant,
    pub last_finished_at: Instant,
    pub observed_missed_ticks: u64,
    pub resume_observed: bool,
}

#[derive(Clone)]
enum SchedulePlan {
    Fixed {
        interval: Duration,
        mode: ScheduleMode,
    },
    Steps {
        delays: Arc<[Duration]>,
        repeat_last: bool,
    },
    Dynamic(Arc<ScheduleFunction>),
}

/// Validated recurring schedule with an independent initial delay.
#[derive(Clone)]
pub struct Schedule {
    initial_delay: Duration,
    plan: SchedulePlan,
    missed_tick_policy: MissedTickPolicy,
    resume_policy: ResumePolicy,
    jitter: Option<Jitter>,
}

impl fmt::Debug for Schedule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Schedule")
            .field("initial_delay", &self.initial_delay)
            .field("missed_tick_policy", &self.missed_tick_policy)
            .field("resume_policy", &self.resume_policy)
            .field("jitter", &self.jitter)
            .finish_non_exhaustive()
    }
}

impl Schedule {
    pub fn fixed_delay(interval: Duration) -> Self {
        Self::fixed(interval, ScheduleMode::FixedDelay)
    }

    pub fn fixed_rate(interval: Duration) -> Self {
        Self::fixed(interval, ScheduleMode::FixedRate)
    }

    fn fixed(interval: Duration, mode: ScheduleMode) -> Self {
        Self {
            initial_delay: Duration::ZERO,
            plan: SchedulePlan::Fixed { interval, mode },
            missed_tick_policy: MissedTickPolicy::Skip,
            resume_policy: ResumePolicy::Continue,
            jitter: None,
        }
    }

    pub fn steps(delays: impl IntoIterator<Item = Duration>) -> Self {
        Self {
            initial_delay: Duration::ZERO,
            plan: SchedulePlan::Steps {
                delays: delays.into_iter().collect::<Vec<_>>().into(),
                repeat_last: false,
            },
            missed_tick_policy: MissedTickPolicy::Skip,
            resume_policy: ResumePolicy::Continue,
            jitter: None,
        }
    }

    pub fn dynamic<F>(function: F) -> Self
    where
        F: Fn(ScheduleDecisionContext) -> Option<Duration> + Send + Sync + 'static,
    {
        Self {
            initial_delay: Duration::ZERO,
            plan: SchedulePlan::Dynamic(Arc::new(function)),
            missed_tick_policy: MissedTickPolicy::Skip,
            resume_policy: ResumePolicy::Continue,
            jitter: None,
        }
    }

    pub fn initial_delay(mut self, delay: Duration) -> Self {
        self.initial_delay = delay;
        self
    }

    pub fn repeat_last(mut self) -> Self {
        if let SchedulePlan::Steps { repeat_last, .. } = &mut self.plan {
            *repeat_last = true;
        }
        self
    }

    pub fn missed_tick_policy(mut self, policy: MissedTickPolicy) -> Self {
        self.missed_tick_policy = policy;
        self
    }

    pub fn resume_policy(mut self, policy: ResumePolicy) -> Self {
        self.resume_policy = policy;
        self
    }

    /// Applies deterministic proportional jitter to the wait before each tick.
    pub fn jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = Some(jitter);
        self
    }

    pub fn configured_initial_delay(&self) -> Duration {
        self.initial_delay
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TickFailurePolicy {
    Stop,
    Continue {
        max_consecutive_failures: u32,
        delay: Duration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    max_restarts: u32,
    window: Duration,
    backoff: Duration,
}

impl RestartPolicy {
    pub fn new(max_restarts: u32) -> Self {
        Self {
            max_restarts,
            window: Duration::from_secs(60),
            backoff: Duration::ZERO,
        }
    }

    pub fn window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    pub fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    pub fn maximum_restarts(&self) -> u32 {
        self.max_restarts
    }

    pub fn configured_window(&self) -> Duration {
        self.window
    }

    pub fn configured_backoff(&self) -> Duration {
        self.backoff
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PanicPolicy {
    Stop,
    Restart(RestartPolicy),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecurringBuildError {
    EmptySchedule,
    ZeroFailureBudget,
    ZeroRestartBudget,
    ZeroRestartWindow,
    DurationOverflow,
    InvalidJitterRatio,
}

impl fmt::Display for RecurringBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchedule => formatter.write_str("step schedule must contain a delay"),
            Self::ZeroFailureBudget => {
                formatter.write_str("continued tick failures require a non-zero budget")
            }
            Self::ZeroRestartBudget => formatter.write_str("restart budget must be non-zero"),
            Self::ZeroRestartWindow => formatter.write_str("restart window must be non-zero"),
            Self::DurationOverflow => formatter.write_str("schedule duration overflows Instant"),
            Self::InvalidJitterRatio => {
                formatter.write_str("schedule jitter ratio must be finite and between zero and one")
            }
        }
    }
}

impl std::error::Error for RecurringBuildError {}

#[derive(Debug)]
#[non_exhaustive]
pub enum RecurringFailure<E> {
    TickFailed { tick: u64, error: E },
    ConsecutiveFailuresExceeded { tick: u64, error: E },
    ScheduleEnded,
    ScheduleOverflow { code: &'static str },
    CounterOverflow { counter: &'static str },
    RestartLimitExceeded,
}

impl<E> RecurringFailure<E> {
    /// Stable machine-readable code for recurring terminal failures.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TickFailed { .. } => "BB-RECURRING-TICK-FAILED",
            Self::ConsecutiveFailuresExceeded { .. } => {
                "BB-RECURRING-CONSECUTIVE-FAILURES-EXCEEDED"
            }
            Self::ScheduleEnded => "BB-RECURRING-SCHEDULE-ENDED",
            Self::ScheduleOverflow { code } => code,
            Self::CounterOverflow { .. } => "BB-RECURRING-COUNTER-OVERFLOW",
            Self::RestartLimitExceeded => "BB-RECURRING-RESTART-LIMIT-EXCEEDED",
        }
    }
}

enum RecurringOperation<T, E> {
    Tick(Arc<TickFactory<T, E>>),
    Retry(RetrySpec<TickOutcome<T>, E>),
}

impl<T, E> Clone for RecurringOperation<T, E> {
    fn clone(&self) -> Self {
        match self {
            Self::Tick(factory) => Self::Tick(Arc::clone(factory)),
            Self::Retry(spec) => Self::Retry(spec.clone()),
        }
    }
}

pub struct RecurringSpec<T, E> {
    id: TaskSpecId,
    operation: RecurringOperation<T, E>,
    schedule: Schedule,
    failure_policy: TickFailurePolicy,
    panic_policy: PanicPolicy,
    tag: Option<Arc<str>>,
    abort_policy: AbortPolicy,
    marker: PhantomData<fn() -> (T, E)>,
}

impl<T, E> Clone for RecurringSpec<T, E> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            operation: self.operation.clone(),
            schedule: self.schedule.clone(),
            failure_policy: self.failure_policy.clone(),
            panic_policy: self.panic_policy.clone(),
            tag: self.tag.clone(),
            abort_policy: self.abort_policy,
            marker: PhantomData,
        }
    }
}

impl<T, E> RecurringSpec<T, E> {
    pub fn id(&self) -> TaskSpecId {
        self.id
    }
}

pub struct RecurringBuilder<T, E> {
    operation: RecurringOperation<T, E>,
    schedule: Schedule,
    failure_policy: TickFailurePolicy,
    panic_policy: PanicPolicy,
    tag: Option<Arc<str>>,
    abort_policy: AbortPolicy,
}

impl<T, E> RecurringBuilder<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    pub fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(TickContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TickOutcome<T>, E>> + Send + 'static,
    {
        Self {
            operation: RecurringOperation::Tick(Arc::new(move |context| {
                Box::pin(factory(context))
            })),
            schedule: Schedule::fixed_delay(Duration::from_secs(1)),
            failure_policy: TickFailurePolicy::Stop,
            panic_policy: PanicPolicy::Stop,
            tag: None,
            abort_policy: AbortPolicy::CooperativeOnly,
        }
    }

    pub fn from_retry(spec: RetrySpec<TickOutcome<T>, E>) -> Self {
        Self {
            operation: RecurringOperation::Retry(spec),
            schedule: Schedule::fixed_delay(Duration::from_secs(1)),
            failure_policy: TickFailurePolicy::Stop,
            panic_policy: PanicPolicy::Stop,
            tag: None,
            abort_policy: AbortPolicy::CooperativeOnly,
        }
    }

    pub fn schedule(mut self, schedule: Schedule) -> Self {
        self.schedule = schedule;
        self
    }

    pub fn initial_delay(mut self, delay: Duration) -> Self {
        self.schedule.initial_delay = delay;
        self
    }

    pub fn tick_failure_policy(mut self, policy: TickFailurePolicy) -> Self {
        self.failure_policy = policy;
        self
    }

    pub fn panic_policy(mut self, policy: PanicPolicy) -> Self {
        self.panic_policy = policy;
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

    pub fn build(self) -> Result<RecurringSpec<T, E>, RecurringBuildError> {
        if let SchedulePlan::Steps { delays, .. } = &self.schedule.plan {
            if delays.is_empty() {
                return Err(RecurringBuildError::EmptySchedule);
            }
        }
        if let TickFailurePolicy::Continue {
            max_consecutive_failures,
            ..
        } = self.failure_policy
        {
            if max_consecutive_failures == 0 {
                return Err(RecurringBuildError::ZeroFailureBudget);
            }
        }
        if let PanicPolicy::Restart(policy) = &self.panic_policy {
            if policy.max_restarts == 0 {
                return Err(RecurringBuildError::ZeroRestartBudget);
            }
            if policy.window.is_zero() {
                return Err(RecurringBuildError::ZeroRestartWindow);
            }
        }
        if self
            .schedule
            .jitter
            .is_some_and(|jitter| !jitter.is_valid())
        {
            return Err(RecurringBuildError::InvalidJitterRatio);
        }
        Instant::now()
            .checked_add(self.schedule.initial_delay)
            .ok_or(RecurringBuildError::DurationOverflow)?;
        Ok(RecurringSpec {
            id: TaskSpecId::new(),
            operation: self.operation,
            schedule: self.schedule,
            failure_policy: self.failure_policy,
            panic_policy: self.panic_policy,
            tag: self.tag,
            abort_policy: self.abort_policy,
            marker: PhantomData,
        })
    }
}

#[derive(Clone)]
pub struct TickContext {
    work: WorkContext,
    tick: u64,
    scheduled_at: Instant,
    observed_missed_ticks: u64,
}

impl TickContext {
    pub fn number(&self) -> u64 {
        self.tick
    }

    pub fn scheduled_at(&self) -> Instant {
        self.scheduled_at
    }

    pub fn observed_missed_ticks(&self) -> u64 {
        self.observed_missed_ticks
    }

    pub fn work(&self) -> &WorkContext {
        &self.work
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
}

pub(crate) fn prepare_recurring<T, E>(
    registry: &Arc<ExecutionRegistry>,
    runtime: Handle,
    spec: RecurringSpec<T, E>,
) -> crate::BeaverResult<PreparedExecution<T, E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    let id = spec.id;
    let tag = spec.tag.clone();
    let abort_policy = spec.abort_policy;
    let prepared = execution::prepare_exit_future(registry, runtime, id, tag, move |work| {
        run_recurring(spec, work)
    })?;
    prepared.start.set_kind(ExecutionKind::Recurring);
    prepared.start.set_abort_policy(abort_policy);
    Ok(prepared)
}

async fn run_recurring<T, E>(spec: RecurringSpec<T, E>, work: WorkContext) -> TaskExit<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    if !spec.schedule.initial_delay.is_zero()
        && work.sleep(spec.schedule.initial_delay).await.is_err()
    {
        return cancelled_exit(&work);
    }

    let anchor = Instant::now();
    let mut scheduled_at = anchor;
    let mut tick = 1u64;
    let mut observed_missed_ticks = 0u64;
    let mut consecutive_failures = 0u32;
    let mut restarts = VecDeque::new();
    let mut resume_epoch = work.resume_epoch();

    loop {
        if work.is_cancelled() {
            return cancelled_exit(&work);
        }
        let tick_work = work.for_attempt(u32::try_from(tick).map_or(u32::MAX, |value| value));
        tick_work.begin_attempt();
        let context = TickContext {
            work: tick_work.clone(),
            tick,
            scheduled_at,
            observed_missed_ticks,
        };

        let outcome = run_tick(&spec.operation, context, &tick_work).await;
        tick_work.close_attempt_children_and_wait().await;
        match outcome {
            TickRun::Outcome(TickOutcome::Stop(value)) => return TaskExit::Completed(value),
            TickRun::Outcome(TickOutcome::Continue) => consecutive_failures = 0,
            TickRun::Failure(error) => match spec.failure_policy {
                TickFailurePolicy::Stop => {
                    return TaskExit::Failed(TaskFailure::Recurring(
                        RecurringFailure::TickFailed { tick, error },
                    ));
                }
                TickFailurePolicy::Continue {
                    max_consecutive_failures,
                    delay,
                } => {
                    let Some(next_failures) = consecutive_failures.checked_add(1) else {
                        return TaskExit::Failed(TaskFailure::Recurring(
                            RecurringFailure::CounterOverflow {
                                counter: "consecutive_failures",
                            },
                        ));
                    };
                    consecutive_failures = next_failures;
                    if consecutive_failures > max_consecutive_failures {
                        return TaskExit::Failed(TaskFailure::Recurring(
                            RecurringFailure::ConsecutiveFailuresExceeded { tick, error },
                        ));
                    }
                    if tick_work.sleep(delay).await.is_err() {
                        return cancelled_exit(&tick_work);
                    }
                }
            },
            TickRun::Exit(exit) => return map_tick_exit(exit),
            TickRun::Panicked { source, message } => match &spec.panic_policy {
                PanicPolicy::Stop => return TaskExit::Panicked { source, message },
                PanicPolicy::Restart(policy) => {
                    let now = Instant::now();
                    while restarts
                        .front()
                        .is_some_and(|at| now.duration_since(*at) > policy.window)
                    {
                        restarts.pop_front();
                    }
                    if restarts.len() >= policy.max_restarts as usize {
                        return TaskExit::Failed(TaskFailure::Recurring(
                            RecurringFailure::RestartLimitExceeded,
                        ));
                    }
                    restarts.push_back(now);
                    if tick_work.sleep(policy.backoff).await.is_err() {
                        return cancelled_exit(&tick_work);
                    }
                    continue;
                }
            },
        }

        let finished_at = Instant::now();
        let current_resume = work.resume_epoch();
        let resume_observed = current_resume != resume_epoch;
        resume_epoch = current_resume;
        let next = catch_unwind(AssertUnwindSafe(|| {
            next_target(
                &spec.schedule,
                anchor,
                tick,
                scheduled_at,
                finished_at,
                observed_missed_ticks,
                resume_observed,
            )
        }));
        let (target, missed) = match next {
            Ok(Ok(Some(next))) => next,
            Ok(Ok(None)) => {
                return TaskExit::Failed(TaskFailure::Recurring(RecurringFailure::ScheduleEnded));
            }
            Ok(Err(code)) => {
                crate::internal::log_internal_error(
                    code,
                    "recurring schedule time arithmetic overflowed",
                );
                return TaskExit::Failed(TaskFailure::Recurring(
                    RecurringFailure::ScheduleOverflow { code },
                ));
            }
            Err(payload) => {
                let message = panic_message(payload);
                if execution::is_timer_unavailable_message(&message) {
                    return TaskExit::Failed(TaskFailure::TimerUnavailable);
                }
                return TaskExit::Panicked {
                    source: PanicSource::Schedule,
                    message,
                };
            }
        };
        observed_missed_ticks = missed;
        let now = Instant::now();
        let mut delay = target.saturating_duration_since(now);
        if let Some(jitter) = spec.schedule.jitter {
            let Some(jittered) = jitter.apply(delay, tick) else {
                return TaskExit::Panicked {
                    source: PanicSource::Schedule,
                    message: "schedule jitter overflow".to_string(),
                };
            };
            delay = jittered;
            let Some(jittered_target) = now.checked_add(delay) else {
                return TaskExit::Panicked {
                    source: PanicSource::Schedule,
                    message: "schedule jitter Instant overflow".to_string(),
                };
            };
            scheduled_at = jittered_target;
        } else {
            scheduled_at = target;
        }
        if matches!(spec.schedule.resume_policy, ResumePolicy::RunImmediately) && !delay.is_zero() {
            tokio::select! {
                biased;
                resumed = work.resumed_after(resume_epoch) => {
                    match resumed {
                        Ok(epoch) => {
                            resume_epoch = epoch;
                            scheduled_at = Instant::now();
                            observed_missed_ticks = 0;
                        }
                        Err(_) => return cancelled_exit(&work),
                    }
                }
                slept = work.sleep(delay) => {
                    if slept.is_err() {
                        return cancelled_exit(&work);
                    }
                }
            }
        } else if work.sleep(delay).await.is_err() {
            return cancelled_exit(&work);
        }
        let Some(next_tick) = tick.checked_add(1) else {
            return TaskExit::Failed(TaskFailure::Recurring(RecurringFailure::CounterOverflow {
                counter: "tick",
            }));
        };
        tick = next_tick;
    }
}

enum TickRun<T, E> {
    Outcome(TickOutcome<T>),
    Failure(E),
    Exit(TaskExit<TickOutcome<T>, E>),
    Panicked {
        source: PanicSource,
        message: String,
    },
}

async fn run_tick<T, E>(
    operation: &RecurringOperation<T, E>,
    context: TickContext,
    work: &WorkContext,
) -> TickRun<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    match operation {
        RecurringOperation::Tick(factory) => {
            let future = catch_unwind(AssertUnwindSafe(|| factory(context)));
            let future = match future {
                Ok(future) => future,
                Err(payload) => {
                    return TickRun::Panicked {
                        source: PanicSource::Factory,
                        message: panic_message(payload),
                    };
                }
            };
            match tokio::spawn(future).await {
                Ok(Ok(outcome)) => TickRun::Outcome(outcome),
                Ok(Err(error)) => TickRun::Failure(error),
                Err(error) if error.is_panic() => {
                    let Some(payload) = crate::internal::take_join_panic(
                        error,
                        "BB-RECURRING-JOIN-PANIC-MISCLASSIFIED",
                    ) else {
                        return TickRun::Exit(TaskExit::ExecutorStopped {
                            reason: ExecutorStopReason::InternalInvariantViolation,
                        });
                    };
                    let message = panic_message(payload);
                    if execution::is_timer_unavailable_message(&message) {
                        TickRun::Exit(TaskExit::Failed(TaskFailure::TimerUnavailable))
                    } else {
                        TickRun::Panicked {
                            source: PanicSource::WorkFuture,
                            message,
                        }
                    }
                }
                Err(_) => TickRun::Exit(TaskExit::ExecutorStopped {
                    reason: ExecutorStopReason::RuntimeUnavailable,
                }),
            }
        }
        RecurringOperation::Retry(spec) => {
            let accepted_at = Instant::now();
            let deadline = match spec.deadline_at(accepted_at) {
                Ok(deadline) => deadline,
                Err(()) => {
                    return TickRun::Exit(TaskExit::Failed(TaskFailure::DeadlineExceeded {
                        last_error: None,
                    }));
                }
            };
            match retry::run_retry(spec.clone(), work.clone(), accepted_at, deadline).await {
                TaskExit::Completed(outcome) => TickRun::Outcome(outcome),
                exit => TickRun::Exit(exit),
            }
        }
    }
}

fn map_tick_exit<T, E>(exit: TaskExit<TickOutcome<T>, E>) -> TaskExit<T, E> {
    match exit {
        TaskExit::Completed(TickOutcome::Stop(value)) => TaskExit::Completed(value),
        TaskExit::Completed(TickOutcome::Continue) => {
            TaskExit::Failed(TaskFailure::Recurring(RecurringFailure::ScheduleEnded))
        }
        TaskExit::Failed(failure) => TaskExit::Failed(failure),
        TaskExit::Cancelled { reason } => TaskExit::Cancelled { reason },
        TaskExit::Aborted { preceding_stop } => TaskExit::Aborted { preceding_stop },
        TaskExit::Panicked { source, message } => TaskExit::Panicked { source, message },
        TaskExit::ExecutorStopped { reason } => TaskExit::ExecutorStopped { reason },
    }
}

fn next_target(
    schedule: &Schedule,
    anchor: Instant,
    completed_ticks: u64,
    last_started_at: Instant,
    last_finished_at: Instant,
    previous_missed: u64,
    resume_observed: bool,
) -> Result<Option<(Instant, u64)>, &'static str> {
    if resume_observed && matches!(schedule.resume_policy, ResumePolicy::RunImmediately) {
        return Ok(Some((Instant::now(), 0)));
    }
    match &schedule.plan {
        SchedulePlan::Fixed { interval, mode } => match mode {
            ScheduleMode::FixedDelay => last_finished_at
                .checked_add(*interval)
                .map(|at| Some((at, 0)))
                .ok_or("BB-SCHEDULE-FIXED-DELAY-OVERFLOW"),
            ScheduleMode::FixedRate => {
                let nanos = interval
                    .as_nanos()
                    .checked_mul(u128::from(completed_ticks))
                    .ok_or("BB-SCHEDULE-FIXED-RATE-MULTIPLY-OVERFLOW")?;
                let elapsed =
                    duration_from_nanos(nanos).ok_or("BB-SCHEDULE-FIXED-RATE-DURATION-OVERFLOW")?;
                let mut target = anchor
                    .checked_add(elapsed)
                    .ok_or("BB-SCHEDULE-FIXED-RATE-INSTANT-OVERFLOW")?;
                let now = Instant::now();
                if target > now {
                    return Ok(Some((target, 0)));
                }
                if interval.is_zero() {
                    return Ok(Some((now, 0)));
                }
                match if resume_observed
                    && matches!(schedule.resume_policy, ResumePolicy::SkipMissed)
                {
                    MissedTickPolicy::Skip
                } else {
                    schedule.missed_tick_policy
                } {
                    MissedTickPolicy::Burst => Ok(Some((target, 1))),
                    MissedTickPolicy::Delay => last_finished_at
                        .checked_add(*interval)
                        .map(|at| Some((at, 1)))
                        .ok_or("BB-SCHEDULE-MISSED-DELAY-OVERFLOW"),
                    MissedTickPolicy::Skip => {
                        let behind = now.duration_since(target);
                        let skipped = behind
                            .as_nanos()
                            .checked_div(interval.as_nanos())
                            .and_then(|value| value.checked_add(1))
                            .ok_or("BB-SCHEDULE-MISSED-COUNT-OVERFLOW")?;
                        let skipped_u64 = u64::try_from(skipped)
                            .map_err(|_| "BB-SCHEDULE-MISSED-COUNT-OVERFLOW")?;
                        let advance_nanos = interval
                            .as_nanos()
                            .checked_mul(skipped)
                            .ok_or("BB-SCHEDULE-MISSED-ADVANCE-OVERFLOW")?;
                        let advance = duration_from_nanos(advance_nanos)
                            .ok_or("BB-SCHEDULE-MISSED-DURATION-OVERFLOW")?;
                        target = target
                            .checked_add(advance)
                            .ok_or("BB-SCHEDULE-MISSED-INSTANT-OVERFLOW")?;
                        Ok(Some((target, skipped_u64)))
                    }
                }
            }
        },
        SchedulePlan::Steps {
            delays,
            repeat_last,
        } => {
            let step = completed_ticks
                .checked_sub(1)
                .ok_or("BB-SCHEDULE-STEP-COUNTER-UNDERFLOW")?;
            let index = usize::try_from(step).map_err(|_| "BB-SCHEDULE-STEP-INDEX-OVERFLOW")?;
            let delay = delays.get(index).copied().or_else(|| {
                if *repeat_last {
                    delays.last().copied()
                } else {
                    None
                }
            });
            let Some(delay) = delay else {
                return Ok(None);
            };
            last_finished_at
                .checked_add(delay)
                .map(|at| Some((at, 0)))
                .ok_or("BB-SCHEDULE-STEP-INSTANT-OVERFLOW")
        }
        SchedulePlan::Dynamic(function) => match function(ScheduleDecisionContext {
            completed_ticks,
            last_started_at,
            last_finished_at,
            observed_missed_ticks: previous_missed,
            resume_observed,
        }) {
            Some(delay) => last_finished_at
                .checked_add(delay)
                .map(|at| Some((at, 0)))
                .ok_or("BB-SCHEDULE-DYNAMIC-INSTANT-OVERFLOW"),
            None => Ok(None),
        },
    }
}

fn duration_from_nanos(nanos: u128) -> Option<Duration> {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let seconds = u64::try_from(nanos / NANOS_PER_SECOND).ok()?;
    let subsecond_nanos = u32::try_from(nanos % NANOS_PER_SECOND).ok()?;
    Some(Duration::new(seconds, subsecond_nanos))
}

fn cancelled_exit<T, E>(work: &WorkContext) -> TaskExit<T, E> {
    match work.stop_cause() {
        Some(crate::StopCauseSummary::Cancel(reason)) => TaskExit::Cancelled { reason },
        Some(crate::StopCauseSummary::Deadline) => {
            TaskExit::Failed(TaskFailure::DeadlineExceeded { last_error: None })
        }
        None => TaskExit::ExecutorStopped {
            reason: ExecutorStopReason::RunnerCancelled,
        },
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

#[cfg(test)]
mod tests;
