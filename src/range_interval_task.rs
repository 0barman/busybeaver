use crate::error::{BeaverError, BeaverResult};
use crate::listener::WorkListener;
use crate::task::{Task, TaskId};
use crate::work::Work;
use crate::work_fn::BoxWork;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// A task that retries with a total count and range-based time intervals: for each attempt
/// in a given range, waits the specified duration after failure before the next execution.
/// If the duration for a range is zero, no sleep is performed.
pub struct RangeIntervalTask {
    pub(crate) id: TaskId,
    pub(crate) work: BoxWork,
    /// Total number of retry attempts (including the first run).
    pub(crate) total_retries: u32,
    /// Precomputed interval in milliseconds for each attempt index. Length = total_retries.
    /// intervals[i] is the sleep before the (i+1)-th execution (i.e. after the i-th failure).
    pub(crate) intervals: Box<[u64]>,
    pub(crate) tag: Option<Box<String>>,
    pub(crate) listener: Option<Arc<dyn WorkListener>>,
    pub(crate) interrupted: AtomicBool,
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
    tag: Option<Box<String>>,
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
    /// * `total_retries` - Maximum number of attempts (e.g. 20 means attempts 0..20).
    pub fn new<W>(work: W, total_retries: u32) -> Self
    where
        W: Work + Send + 'static,
    {
        RangeIntervalBuilder {
            work: Some(Box::pin(work)),
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
            duration_millis: duration.as_millis().min(u64::MAX as u128) as u64,
        });
        self
    }

    /// Sets the task tag for identification.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Box::new(tag.into()));
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

        let total = self.total_retries as usize;
        let mut intervals = vec![0u64; total];
        for r in &self.ranges {
            let start = r.start_inclusive as usize;
            let end = (r.end_inclusive as usize).min(total.saturating_sub(1));
            for i in start..=end {
                if i < total {
                    intervals[i] = r.duration_millis;
                }
            }
        }

        Ok(Arc::new(Task::RangeInterval(RangeIntervalTask {
            id: TaskId::new(),
            work,
            total_retries: self.total_retries,
            intervals: intervals.into_boxed_slice(),
            tag: self.tag,
            listener: self.listener,
            interrupted: AtomicBool::new(false),
        })))
    }
}
