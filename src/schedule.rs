use std::fmt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects when the first scheduled run starts.
pub enum FirstRun {
    /// Starts as soon as permits are available.
    Immediate,
    /// Waits the supplied duration from submission.
    After(Duration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects how fixed-rate schedules recover after missing nominal ticks.
pub enum MissedTickBehavior {
    /// Skips every missed tick and waits for the next future tick.
    Skip,
    /// Reanchors the next tick one interval after the current time.
    Delay,
    /// Runs immediately for at most `max_catch_up` missed ticks, then skips
    /// any remaining backlog.
    Burst {
        /// Maximum consecutive immediate catch-up runs.
        max_catch_up: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
/// A monotonic offset from the start of a scheduled task run.
pub struct ScheduleTime(Duration);

impl ScheduleTime {
    /// Creates an absolute schedule offset relative to task start.
    pub fn after_start(offset: Duration) -> Self {
        Self(offset)
    }

    /// Returns the offset from task start.
    pub fn offset(self) -> Duration {
        self.0
    }
}

#[derive(Debug)]
/// Directs a scheduled job after one successful invocation.
pub enum TaskControl<T> {
    /// Continues using the schedule's configured next delay.
    Continue,
    /// Continues after a delay measured from this invocation's completion.
    ContinueAfter(Duration),
    /// Continues at a monotonic offset measured from task start.
    ContinueAt(ScheduleTime),
    /// Stops the schedule and publishes the contained successful value.
    Complete(T),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScheduleKind {
    FixedDelay(Duration),
    FixedRate(Duration),
    Sequence {
        delays: Arc<[Duration]>,
        repeat: bool,
    },
    Dynamic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A validated, non-reentrant sequence of job invocations.
pub struct Schedule {
    first_run: FirstRun,
    missed_tick: MissedTickBehavior,
    kind: ScheduleKind,
}

impl Schedule {
    /// Creates a schedule whose next interval starts after each invocation
    /// completes.
    pub fn fixed_delay(interval: Duration, first_run: FirstRun) -> Result<Self, ScheduleError> {
        Self::interval(ScheduleKind::FixedDelay(interval), interval, first_run)
    }

    /// Creates a schedule anchored to fixed monotonic offsets from task start.
    pub fn fixed_rate(interval: Duration, first_run: FirstRun) -> Result<Self, ScheduleError> {
        Self::interval(ScheduleKind::FixedRate(interval), interval, first_run)
    }

    fn interval(
        kind: ScheduleKind,
        interval: Duration,
        first_run: FirstRun,
    ) -> Result<Self, ScheduleError> {
        if interval.is_zero() {
            return Err(ScheduleError::ZeroInterval);
        }
        tokio::time::Instant::now()
            .checked_add(interval)
            .ok_or(ScheduleError::DurationOverflow)?;
        validate_first_run(first_run)?;
        Ok(Self {
            first_run,
            missed_tick: MissedTickBehavior::Skip,
            kind,
        })
    }

    /// Creates a fixed-delay sequence, optionally repeating from its start.
    pub fn sequence(
        delays: impl Into<Vec<Duration>>,
        repeat: bool,
        first_run: FirstRun,
    ) -> Result<Self, ScheduleError> {
        let delays = delays.into();
        if delays.is_empty() {
            return Err(ScheduleError::EmptySequence);
        }
        if repeat && delays.iter().all(Duration::is_zero) {
            return Err(ScheduleError::UnboundedZeroSequence);
        }
        if delays
            .iter()
            .any(|delay| tokio::time::Instant::now().checked_add(*delay).is_none())
        {
            return Err(ScheduleError::DurationOverflow);
        }
        validate_first_run(first_run)?;
        Ok(Self {
            first_run,
            missed_tick: MissedTickBehavior::Skip,
            kind: ScheduleKind::Sequence {
                delays: delays.into(),
                repeat,
            },
        })
    }

    /// Creates a schedule whose job must return [`TaskControl::ContinueAfter`]
    /// or [`TaskControl::ContinueAt`] after each non-terminal invocation.
    pub fn dynamic(first_run: FirstRun) -> Result<Self, ScheduleError> {
        validate_first_run(first_run)?;
        Ok(Self {
            first_run,
            missed_tick: MissedTickBehavior::Skip,
            kind: ScheduleKind::Dynamic,
        })
    }

    /// Sets fixed-rate recovery behavior after one or more missed ticks.
    pub fn missed_tick_behavior(
        mut self,
        behavior: MissedTickBehavior,
    ) -> Result<Self, ScheduleError> {
        if matches!(behavior, MissedTickBehavior::Burst { max_catch_up: 0 }) {
            return Err(ScheduleError::ZeroBurstLimit);
        }
        self.missed_tick = behavior;
        Ok(self)
    }

    /// Returns the first-run policy.
    pub fn first_run(&self) -> FirstRun {
        self.first_run
    }

    /// Returns the missed-tick policy.
    pub fn missed_tick(&self) -> MissedTickBehavior {
        self.missed_tick
    }

    pub(crate) fn cursor(&self) -> ScheduleCursor {
        ScheduleCursor {
            schedule: self.clone(),
            scheduled_offset: match self.first_run {
                FirstRun::Immediate => Duration::ZERO,
                FirstRun::After(delay) => delay,
            },
            sequence_index: 0,
            burst_used: 0,
        }
    }
}

fn validate_first_run(first_run: FirstRun) -> Result<(), ScheduleError> {
    if let FirstRun::After(delay) = first_run {
        tokio::time::Instant::now()
            .checked_add(delay)
            .ok_or(ScheduleError::DurationOverflow)?;
    }
    Ok(())
}

pub(crate) struct ScheduleCursor {
    schedule: Schedule,
    scheduled_offset: Duration,
    sequence_index: usize,
    burst_used: u32,
}

impl ScheduleCursor {
    pub(crate) fn first_delay(&self) -> Duration {
        self.scheduled_offset
    }

    pub(crate) fn next_delay<T>(
        &mut self,
        control: &TaskControl<T>,
        elapsed: Duration,
    ) -> Result<Duration, ScheduleError> {
        match control {
            TaskControl::Complete(_) => unreachable!("complete has no next schedule"),
            TaskControl::ContinueAfter(delay) => {
                self.scheduled_offset = elapsed
                    .checked_add(*delay)
                    .ok_or(ScheduleError::DurationOverflow)?;
                self.burst_used = 0;
                Ok(*delay)
            }
            TaskControl::ContinueAt(time) => {
                self.scheduled_offset = time.offset();
                self.burst_used = 0;
                Ok(time.offset().saturating_sub(elapsed))
            }
            TaskControl::Continue => self.next_default(elapsed),
        }
    }

    fn next_default(&mut self, elapsed: Duration) -> Result<Duration, ScheduleError> {
        match &self.schedule.kind {
            ScheduleKind::FixedDelay(interval) => {
                self.scheduled_offset = elapsed
                    .checked_add(*interval)
                    .ok_or(ScheduleError::DurationOverflow)?;
                self.burst_used = 0;
                Ok(*interval)
            }
            ScheduleKind::FixedRate(interval) => {
                let nominal = self
                    .scheduled_offset
                    .checked_add(*interval)
                    .ok_or(ScheduleError::DurationOverflow)?;
                if nominal > elapsed {
                    self.scheduled_offset = nominal;
                    self.burst_used = 0;
                    return Ok(nominal - elapsed);
                }
                match self.schedule.missed_tick {
                    MissedTickBehavior::Delay => {
                        self.scheduled_offset = elapsed
                            .checked_add(*interval)
                            .ok_or(ScheduleError::DurationOverflow)?;
                        self.burst_used = 0;
                        Ok(*interval)
                    }
                    MissedTickBehavior::Skip => {
                        self.burst_used = 0;
                        self.skip_past(elapsed, nominal, *interval)
                    }
                    MissedTickBehavior::Burst { max_catch_up } => {
                        if self.burst_used < max_catch_up {
                            self.burst_used += 1;
                            self.scheduled_offset = nominal;
                            Ok(Duration::ZERO)
                        } else {
                            self.burst_used = 0;
                            self.skip_past(elapsed, nominal, *interval)
                        }
                    }
                }
            }
            ScheduleKind::Sequence { delays, repeat } => {
                let delay = match delays.get(self.sequence_index) {
                    Some(delay) => *delay,
                    None if *repeat => {
                        self.sequence_index = 0;
                        delays[0]
                    }
                    None => return Err(ScheduleError::SequenceExhausted),
                };
                self.sequence_index += 1;
                self.scheduled_offset = elapsed
                    .checked_add(delay)
                    .ok_or(ScheduleError::DurationOverflow)?;
                self.burst_used = 0;
                Ok(delay)
            }
            ScheduleKind::Dynamic => Err(ScheduleError::DynamicControlRequired),
        }
    }

    fn skip_past(
        &mut self,
        elapsed: Duration,
        nominal: Duration,
        interval: Duration,
    ) -> Result<Duration, ScheduleError> {
        let missed_nanos = elapsed.saturating_sub(nominal).as_nanos();
        let interval_nanos = interval.as_nanos();
        let skips = missed_nanos
            .checked_div(interval_nanos)
            .and_then(|value| value.checked_add(1))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(ScheduleError::DurationOverflow)?;
        let advance = interval
            .checked_mul(skips)
            .ok_or(ScheduleError::DurationOverflow)?;
        self.scheduled_offset = nominal
            .checked_add(advance)
            .ok_or(ScheduleError::DurationOverflow)?;
        Ok(self.scheduled_offset - elapsed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// Explains invalid schedule configuration or a runtime schedule calculation
/// failure.
pub enum ScheduleError {
    /// A fixed interval was zero.
    ZeroInterval,
    /// A delay sequence contained no entries.
    EmptySequence,
    /// A repeating sequence contained only zero delays and could spin.
    UnboundedZeroSequence,
    /// A burst policy allowed zero catch-up runs.
    ZeroBurstLimit,
    /// A monotonic time or duration calculation overflowed.
    DurationOverflow,
    /// A non-repeating delay sequence has no next entry.
    SequenceExhausted,
    /// A dynamic schedule received the default `Continue` control.
    DynamicControlRequired,
    /// The monotonically increasing run index was exhausted.
    RunIndexExhausted,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ZeroInterval => "schedule interval must be greater than zero",
            Self::EmptySequence => "schedule delay sequence must not be empty",
            Self::UnboundedZeroSequence => {
                "a repeating schedule sequence cannot contain only zero delays"
            }
            Self::ZeroBurstLimit => "schedule burst limit must be greater than zero",
            Self::DurationOverflow => "schedule time exceeds the monotonic clock range",
            Self::SequenceExhausted => "schedule delay sequence was exhausted",
            Self::DynamicControlRequired => "dynamic schedule requires ContinueAfter or ContinueAt",
            Self::RunIndexExhausted => "schedule run index is exhausted",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ScheduleError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects what a scheduled job does when one invocation exhausts retries.
pub enum RetryExhaustedAction {
    /// Terminates the entire schedule as retries exhausted.
    Stop,
    /// Discards that invocation's final error and advances the schedule.
    ContinueSchedule,
}

#[cfg(test)]
mod tests {
    use super::{FirstRun, MissedTickBehavior, Schedule, ScheduleError, TaskControl};
    use std::time::Duration;

    #[test]
    fn fixed_delay_and_fixed_rate_diverge_after_slow_run() {
        let mut delay = Schedule::fixed_delay(Duration::from_secs(10), FirstRun::Immediate)
            .unwrap()
            .cursor();
        let mut rate = Schedule::fixed_rate(Duration::from_secs(10), FirstRun::Immediate)
            .unwrap()
            .cursor();

        assert_eq!(
            delay
                .next_delay(&TaskControl::<()>::Continue, Duration::from_secs(7))
                .unwrap(),
            Duration::from_secs(10)
        );
        assert_eq!(
            rate.next_delay(&TaskControl::<()>::Continue, Duration::from_secs(7))
                .unwrap(),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn missed_tick_behaviors_are_bounded() {
        let elapsed = Duration::from_secs(35);
        let mut skip = Schedule::fixed_rate(Duration::from_secs(10), FirstRun::Immediate)
            .unwrap()
            .missed_tick_behavior(MissedTickBehavior::Skip)
            .unwrap()
            .cursor();
        assert_eq!(
            skip.next_delay(&TaskControl::<()>::Continue, elapsed)
                .unwrap(),
            Duration::from_secs(5)
        );

        let mut delay = Schedule::fixed_rate(Duration::from_secs(10), FirstRun::Immediate)
            .unwrap()
            .missed_tick_behavior(MissedTickBehavior::Delay)
            .unwrap()
            .cursor();
        assert_eq!(
            delay
                .next_delay(&TaskControl::<()>::Continue, elapsed)
                .unwrap(),
            Duration::from_secs(10)
        );

        let mut burst = Schedule::fixed_rate(Duration::from_secs(10), FirstRun::Immediate)
            .unwrap()
            .missed_tick_behavior(MissedTickBehavior::Burst { max_catch_up: 2 })
            .unwrap()
            .cursor();
        assert_eq!(
            burst
                .next_delay(&TaskControl::<()>::Continue, elapsed)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            burst
                .next_delay(&TaskControl::<()>::Continue, elapsed)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            burst
                .next_delay(&TaskControl::<()>::Continue, elapsed)
                .unwrap(),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn invalid_schedules_are_rejected() {
        assert_eq!(
            Schedule::fixed_delay(Duration::ZERO, FirstRun::Immediate).unwrap_err(),
            ScheduleError::ZeroInterval
        );
        assert_eq!(
            Schedule::sequence(Vec::new(), false, FirstRun::Immediate).unwrap_err(),
            ScheduleError::EmptySequence
        );
        assert_eq!(
            Schedule::sequence(vec![Duration::ZERO], true, FirstRun::Immediate).unwrap_err(),
            ScheduleError::UnboundedZeroSequence
        );
    }
}
