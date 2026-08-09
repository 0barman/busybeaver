use crate::fixed_count_task::FixedCountTask;
use crate::listener::isolate_callback;
use crate::periodic_task::PeriodicTask;
use crate::range_interval_task::RangeIntervalTask;
use crate::time_interval_task::TimeIntervalTask;
use uuid::Uuid;

/// Unique task identifier (16-byte UUID, no heap allocation).
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct TaskId(Uuid);

impl TaskId {
    #[inline]
    pub(crate) fn new() -> Self {
        TaskId(Uuid::new_v4())
    }

    /// Returns the underlying UUID, e.g. for logging or persistence.
    #[inline]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A legacy task definition accepted by [`Beaver`](crate::Beaver).
pub enum Task {
    /// A bounded task with one interval between attempts.
    TimeInterval(TimeIntervalTask),
    /// A bounded task with attempt-specific interval ranges.
    RangeInterval(RangeIntervalTask),
    /// A bounded task that runs a fixed number of immediate attempts.
    FixedCount(FixedCountTask),
    /// An unbounded periodic task that runs until interrupted.
    Periodic(PeriodicTask),
}

impl Task {
    /// Returns the task identity shared by legacy re-enqueues of this value.
    pub fn id(&self) -> &TaskId {
        match self {
            Task::TimeInterval(s) => &s.id,
            Task::RangeInterval(s) => &s.id,
            Task::FixedCount(s) => &s.id,
            Task::Periodic(s) => &s.id,
        }
    }
    #[inline]
    /// Returns the configured tag, or an empty string when absent.
    pub fn tag(&self) -> &str {
        match self {
            Task::TimeInterval(s) => s.tag.as_deref().unwrap_or(""),
            Task::RangeInterval(s) => s.tag.as_deref().unwrap_or(""),
            Task::FixedCount(s) => s.tag.as_deref().unwrap_or(""),
            Task::Periodic(s) => s.tag.as_deref().unwrap_or(""),
        }
    }
    #[inline]
    /// Returns whether interruption has been requested.
    pub fn interrupted(&self) -> bool {
        match self {
            Task::TimeInterval(s) => s.control.is_cancelled(),
            Task::RangeInterval(s) => s.control.is_cancelled(),
            Task::FixedCount(s) => s.control.is_cancelled(),
            Task::Periodic(s) => s.control.is_cancelled(),
        }
    }
    #[inline]
    pub(crate) fn set_interrupted(&self, v: bool) {
        if v {
            match self {
                Task::TimeInterval(s) => {
                    s.control.cancel();
                }
                Task::RangeInterval(s) => {
                    s.control.cancel();
                }
                Task::FixedCount(s) => {
                    s.control.cancel();
                }
                Task::Periodic(s) => {
                    s.control.cancel();
                }
            }
        }
    }

    pub(crate) async fn wait_or_cancel(&self, duration: std::time::Duration) -> bool {
        match self {
            Task::TimeInterval(s) => s.control.wait(duration).await,
            Task::RangeInterval(s) => s.control.wait(duration).await,
            Task::FixedCount(s) => s.control.wait(duration).await,
            Task::Periodic(s) => s.control.wait(duration).await,
        }
    }
    /// Marks as interrupted and calls the listener's `on_interrupt` if present.
    pub(crate) fn interrupt(&self) {
        self.set_interrupted(true);
        match self {
            Task::TimeInterval(s) => {
                if let Some(l) = &s.listener {
                    isolate_callback(|| l.on_interrupt());
                }
            }
            Task::RangeInterval(s) => {
                if let Some(l) = &s.listener {
                    isolate_callback(|| l.on_interrupt());
                }
            }
            Task::FixedCount(s) => {
                if let Some(l) = &s.listener {
                    isolate_callback(|| l.on_interrupt());
                }
            }
            Task::Periodic(s) => {
                if let Some(l) = &s.listener {
                    isolate_callback(|| l.on_interrupt());
                }
            }
        }
    }
}
