use crate::error::{BeaverError, BeaverResult};
use crate::listener::WorkListener;
use crate::task::{Task, TaskId};
use crate::work::Work;
use crate::work_fn::BoxWork;
use std::collections::BinaryHeap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// A task that retries with a total attempt count and range-based time intervals.
///
/// **Important: the first attempt is executed immediately (no sleep).**
/// Starting from the second attempt, the executor sleeps `intervals[attempt - 1]`
/// milliseconds before each execution. If the interval for an attempt is zero,
/// no sleep is performed.
///
/// This differs from [`TimeIntervalTask`](crate::time_interval_task::TimeIntervalTask),
/// which sleeps before *every* attempt including the first.
pub struct RangeIntervalTask {
    pub(crate) id: TaskId,
    pub(crate) work: BoxWork,
    /// Total number of retry attempts (including the first run).
    pub(crate) total_retries: u32,
    /// Private adaptive delay lookup. The first attempt remains immediate;
    /// index i is the sleep after the i-th failure and before attempt i+1.
    intervals: RangeIntervals,
    pub(crate) tag: Option<String>,
    pub(crate) listener: Option<Arc<dyn WorkListener>>,
    pub(crate) interrupted: AtomicBool,
}

struct RangeDelaySegment {
    start_inclusive: u64,
    end_exclusive: u64,
    duration_millis: u64,
}

enum RangeIntervals {
    Dense(Box<[u64]>),
    Segments(Box<[RangeDelaySegment]>),
}

impl RangeIntervals {
    fn delay_at(&self, attempt_index: usize) -> u64 {
        match self {
            Self::Dense(intervals) => intervals
                .get(attempt_index)
                .copied()
                .map_or(0, |value| value),
            Self::Segments(segments) => {
                let attempt_index = match u64::try_from(attempt_index) {
                    Ok(index) => index,
                    Err(_) => {
                        crate::internal::log_internal_error(
                            "BB-RANGE-ATTEMPT-INDEX-OVERFLOW",
                            "range interval attempt index did not fit its private lookup type",
                        );
                        return 0;
                    }
                };
                let position =
                    segments.partition_point(|segment| segment.start_inclusive <= attempt_index);
                let Some(index) = position.checked_sub(1) else {
                    return 0;
                };
                segments.get(index).map_or(0, |segment| {
                    if attempt_index < segment.end_exclusive {
                        segment.duration_millis
                    } else {
                        0
                    }
                })
            }
        }
    }
}

impl RangeIntervalTask {
    pub(crate) fn interval_millis(&self, attempt_index: usize) -> Option<u64> {
        let total = usize::try_from(self.total_retries).map_or(usize::MAX, |value| value);
        if attempt_index >= total {
            return None;
        }
        Some(self.intervals.delay_at(attempt_index))
    }
}

/// One range: [start_inclusive, end_inclusive] -> duration in milliseconds.
#[derive(Clone, Debug)]
pub struct RangeIntervalRange {
    pub start_inclusive: u32,
    pub end_inclusive: u32,
    pub duration_millis: u64,
}

/// Builder for retry tasks with range-based time intervals.
pub struct RangeIntervalBuilder {
    work: Option<BoxWork>,
    total_retries: u32,
    ranges: Vec<RangeIntervalRange>,
    tag: Option<String>,
    listener: Option<Arc<dyn WorkListener>>,
}

impl RangeIntervalBuilder {
    /// Creates a new `RangeIntervalBuilder` with the given work and total retry count.
    ///
    /// The work will be retried at most `total_retries` times. Use `.add_range()` to specify
    /// different sleep durations for different attempt index ranges. If no range is added
    /// or an attempt index falls outside all ranges, the sleep duration is 0 (no sleep).
    ///
    /// # Arguments
    ///
    /// * `work` - The work to be executed. Must implement [`Work`] + `Send` + `'static`.
    ///   If the work's async code panics or crashes, it is reported via the listener's `on_error`
    ///   and does not affect other tasks.
    /// * `total_retries` - **Total number of attempts, including the first execution.**
    ///   E.g. `total_retries = 20` means attempt indices `0..20` (20 calls in total);
    ///   `total_retries = 1` means the work runs exactly once and is never retried.
    ///   Despite the name, this is *not* "retries on top of the first attempt".
    pub fn new<W>(work: W, total_retries: u32) -> Self
    where
        W: Work + Send + 'static,
    {
        RangeIntervalBuilder {
            work: Some(Box::new(work)),
            total_retries,
            ranges: Vec::new(),
            tag: None,
            listener: None,
        }
    }

    /// Adds a range of attempt indices and the sleep duration to use after a failure
    /// in that range before the next attempt.
    ///
    /// **Semantics**: `start_inclusive..=end_inclusive` denotes which attempt indices (0-based).
    /// For each failure in that range, the task sleeps the given `duration` before the next attempt.
    /// Use `Duration::ZERO` to skip sleep.
    ///
    /// **Example**: With `total_retries = 20`,
    /// - `.add_range(0, 5, Duration::from_millis(100))` → after failures at attempts 0–5, wait 100ms before retry;
    /// - `.add_range(6, 15, Duration::from_millis(500))` → after failures at attempts 6–15, wait 500ms before retry;
    /// - Attempts 16–19 fall in no range, so interval is 0 (no sleep).
    ///
    /// **Overlapping ranges**: If multiple ranges cover the same attempt index, **the later range
    /// overwrites the earlier**. E.g. `add_range(6, 10, ...)` then `add_range(10, 19, ...)` means
    /// the interval for attempt 10 is determined by the second call.
    ///
    /// **Range beyond total**: Valid attempt indices are `0..total_retries`. If `end_inclusive >= total_retries`,
    /// only indices up to `total_retries - 1` are applied; the rest are ignored (no error). E.g. with
    /// total 20, `add_range(10, 30, ...)` effectively applies to indices 10–19.
    ///
    /// **Range count**: If the number of `add_range` calls exceeds `total_retries`, [`build`](Self::build)
    /// returns [`BeaverError::RangeIntervalRangesExceedTotal`].
    pub fn add_range(
        mut self,
        start_inclusive: u32,
        end_inclusive: u32,
        duration: Duration,
    ) -> Self {
        self.ranges.push(RangeIntervalRange {
            start_inclusive,
            end_inclusive,
            duration_millis: u64::try_from(duration.as_millis()).map_or(u64::MAX, |value| value),
        });
        self
    }

    /// Sets the task tag for identification.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Sets the lifecycle event listener.
    pub fn listener(mut self, listener: Arc<dyn WorkListener>) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Builds the task. Returns an error if required fields are missing or if the number
    /// of interval ranges exceeds the total retry count.
    pub fn build(self) -> BeaverResult<Arc<Task>> {
        let work = self.work.ok_or(BeaverError::BuilderMissingField("work"))?;

        if self.ranges.len() > self.total_retries as usize {
            return Err(BeaverError::RangeIntervalRangesExceedTotal {
                total: self.total_retries,
                ranges_count: self.ranges.len(),
            });
        }

        let intervals = compile_intervals(self.total_retries, &self.ranges);

        Ok(Arc::new(Task::RangeInterval(RangeIntervalTask {
            id: TaskId::new(),
            work,
            total_retries: self.total_retries,
            intervals,
            tag: self.tag,
            listener: self.listener,
            interrupted: AtomicBool::new(false),
        })))
    }
}

fn compile_intervals(total_retries: u32, ranges: &[RangeIntervalRange]) -> RangeIntervals {
    let total = u64::from(total_retries);
    if total == 0 {
        return RangeIntervals::Dense(Vec::new().into_boxed_slice());
    }

    let mut normalized = Vec::with_capacity(ranges.len());
    let mut boundaries = Vec::with_capacity(
        ranges
            .len()
            .checked_mul(2)
            .and_then(|count| count.checked_add(2))
            .map_or(ranges.len(), |count| count),
    );
    boundaries.push(0);
    boundaries.push(total);
    for (priority, range) in ranges.iter().enumerate() {
        let start = u64::from(range.start_inclusive);
        let end_exclusive = u64::from(range.end_inclusive).saturating_add(1).min(total);
        if start >= end_exclusive {
            continue;
        }
        normalized.push((start, end_exclusive, priority, range.duration_millis));
        boundaries.push(start);
        boundaries.push(end_exclusive);
    }
    normalized.sort_unstable_by_key(|(start, _, priority, _)| (*start, *priority));
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut active = BinaryHeap::new();
    let mut next_range = 0usize;
    let mut segments: Vec<RangeDelaySegment> = Vec::new();
    for window in boundaries.windows(2) {
        let Some((&start, remainder)) = window.split_first() else {
            continue;
        };
        let Some(&end_exclusive) = remainder.first() else {
            continue;
        };
        while normalized
            .get(next_range)
            .is_some_and(|(range_start, _, _, _)| *range_start <= start)
        {
            let Some((_, range_end, priority, duration)) = normalized.get(next_range).copied()
            else {
                break;
            };
            active.push((priority, range_end, duration));
            let Some(next) = next_range.checked_add(1) else {
                crate::internal::log_internal_error(
                    "BB-RANGE-COMPILE-INDEX-OVERFLOW",
                    "range interval compiler index overflowed",
                );
                break;
            };
            next_range = next;
        }
        while active
            .peek()
            .is_some_and(|(_, range_end, _)| *range_end <= start)
        {
            active.pop();
        }
        let duration_millis = active.peek().map_or(0, |(_, _, duration)| *duration);
        if duration_millis == 0 || start >= end_exclusive {
            continue;
        }
        if let Some(last) = segments.last_mut() {
            if last.end_exclusive == start && last.duration_millis == duration_millis {
                last.end_exclusive = end_exclusive;
                continue;
            }
        }
        segments.push(RangeDelaySegment {
            start_inclusive: start,
            end_exclusive,
            duration_millis,
        });
    }

    let total_usize = usize::try_from(total).map_or(usize::MAX, |value| value);
    let dense_bytes = total_usize
        .checked_mul(std::mem::size_of::<u64>())
        .map_or(usize::MAX, |value| value);
    let segment_bytes = segments
        .len()
        .checked_mul(std::mem::size_of::<RangeDelaySegment>())
        .map_or(usize::MAX, |value| value);
    if total_usize <= 4_096 || dense_bytes <= segment_bytes {
        let mut dense = vec![0u64; total_usize];
        let mut valid = true;
        for segment in &segments {
            let bounds = usize::try_from(segment.start_inclusive)
                .and_then(|start| usize::try_from(segment.end_exclusive).map(|end| (start, end)));
            let Ok((start, end)) = bounds else {
                valid = false;
                break;
            };
            let Some(intervals) = dense.get_mut(start..end) else {
                valid = false;
                break;
            };
            intervals.fill(segment.duration_millis);
        }
        if valid {
            return RangeIntervals::Dense(dense.into_boxed_slice());
        }
        crate::internal::log_internal_error(
            "BB-RANGE-DENSE-MATERIALIZATION-FAILED",
            "validated range segments did not fit their dense representation",
        );
    }
    RangeIntervals::Segments(segments.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::{compile_intervals, RangeIntervalRange, RangeIntervals};
    use std::error::Error;

    type TestResult = Result<(), Box<dyn Error>>;

    fn test_error(message: impl Into<String>) -> Box<dyn Error> {
        Box::new(std::io::Error::other(message.into()))
    }

    fn dense_oracle(total: u32, ranges: &[RangeIntervalRange]) -> TestResultWith<Vec<u64>> {
        let total = usize::try_from(total)?;
        let mut intervals = vec![0u64; total];
        for range in ranges {
            let Some(last_index) = total.checked_sub(1) else {
                break;
            };
            let start = usize::try_from(range.start_inclusive)?;
            let end = usize::try_from(range.end_inclusive)?.min(last_index);
            let Some(end_exclusive) = end.checked_add(1) else {
                return Err(test_error("dense oracle end index overflowed"));
            };
            if let Some(selected) = intervals.get_mut(start..end_exclusive) {
                selected.fill(range.duration_millis);
            }
        }
        Ok(intervals)
    }

    type TestResultWith<T> = Result<T, Box<dyn Error>>;

    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn compiled_intervals_match_dense_later_wins_oracle() -> TestResult {
        let mut random = 0xC0FF_EE12_3456_7890u64;
        for total in 0..=32u32 {
            for _ in 0..64 {
                let range_count = if total == 0 {
                    0
                } else {
                    usize::try_from(next_random(&mut random) % u64::from(total + 1))?
                };
                let mut ranges = Vec::with_capacity(range_count);
                for _ in 0..range_count {
                    let domain = u64::from(total).saturating_add(8);
                    let start = u32::try_from(next_random(&mut random) % domain)?;
                    let end = u32::try_from(next_random(&mut random) % domain)?;
                    let duration_millis = next_random(&mut random) % 9;
                    ranges.push(RangeIntervalRange {
                        start_inclusive: start,
                        end_inclusive: end,
                        duration_millis,
                    });
                }
                let expected = dense_oracle(total, &ranges)?;
                let actual = compile_intervals(total, &ranges);
                for (index, expected_delay) in expected.into_iter().enumerate() {
                    let actual_delay = actual.delay_at(index);
                    if actual_delay != expected_delay {
                        return Err(test_error(format!(
                            "range plan mismatch: total={total}, index={index}, expected={expected_delay}, actual={actual_delay}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn huge_sparse_range_uses_compact_segments() -> TestResult {
        let ranges = [RangeIntervalRange {
            start_inclusive: 1,
            end_inclusive: 1,
            duration_millis: 5,
        }];
        let intervals = compile_intervals(u32::MAX, &ranges);
        match &intervals {
            RangeIntervals::Segments(segments) if segments.len() == 1 => {}
            RangeIntervals::Segments(segments) => {
                return Err(test_error(format!(
                    "huge sparse range compiled to {} segments instead of one",
                    segments.len()
                )));
            }
            RangeIntervals::Dense(_) => {
                return Err(test_error(
                    "huge sparse range unexpectedly allocated a dense table",
                ));
            }
        }
        if intervals.delay_at(0) != 0 || intervals.delay_at(1) != 5 || intervals.delay_at(2) != 0 {
            return Err(test_error(
                "huge sparse range returned an incorrect compact lookup",
            ));
        }
        Ok(())
    }
}
