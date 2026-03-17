use crate::fixed_count_task::FixedCountTask;
use crate::periodic_task::PeriodicTask;
use crate::range_interval_task::RangeIntervalTask;
use crate::time_interval_task::TimeIntervalTask;
use std::sync::atomic::Ordering;
use uuid::Uuid;

/// Unique task identifier (16-byte UUID, no heap allocation).
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct TaskId(Uuid);

impl TaskId {
    #[inline]
    pub(crate) fn new() -> Self {
        TaskId(Uuid::new_v4())
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

pub enum Task {
    TimeInterval(TimeIntervalTask),
    RangeInterval(RangeIntervalTask),
    FixedCount(FixedCountTask),
    Periodic(PeriodicTask),
}

impl Task {
    pub fn id(&self) -> &TaskId {
        match self {
            Task::TimeInterval(s) => &s.id,
            Task::RangeInterval(s) => &s.id,
            Task::FixedCount(s) => &s.id,
            Task::Periodic(s) => &s.id,
        }
    }
    #[inline]
    pub fn tag(&self) -> &str {
        match self {
            Task::TimeInterval(s) => s.tag.as_ref().map(|s| s.as_str()).unwrap_or(""),
            Task::RangeInterval(s) => s.tag.as_ref().map(|s| s.as_str()).unwrap_or(""),
            Task::FixedCount(s) => s.tag.as_ref().map(|s| s.as_str()).unwrap_or(""),
            Task::Periodic(s) => s.tag.as_ref().map(|s| s.as_str()).unwrap_or(""),
        }
    }
    #[inline]
    pub fn interrupted(&self) -> bool {
        match self {
            Task::TimeInterval(s) => s.interrupted.load(Ordering::Relaxed),
            Task::RangeInterval(s) => s.interrupted.load(Ordering::Relaxed),
            Task::FixedCount(s) => s.interrupted.load(Ordering::Relaxed),
            Task::Periodic(s) => s.interrupted.load(Ordering::Relaxed),
        }
    }
    #[inline]
    pub(crate) fn set_interrupted(&self, v: bool) {
        match self {
            Task::TimeInterval(s) => s.interrupted.store(v, Ordering::Release),
            Task::RangeInterval(s) => s.interrupted.store(v, Ordering::Release),
            Task::FixedCount(s) => s.interrupted.store(v, Ordering::Release),
            Task::Periodic(s) => s.interrupted.store(v, Ordering::Release),
        }
    }
    /// Marks as interrupted and calls the listener's `on_interrupt` if present.
    pub(crate) fn interrupt(&self) {
        self.set_interrupted(true);
        match self {
            Task::TimeInterval(s) => {
                if let Some(l) = &s.listener {
                    l.on_interrupt();
                }
            }
            Task::RangeInterval(s) => {
                if let Some(l) = &s.listener {
                    l.on_interrupt();
                }
            }
            Task::FixedCount(s) => {
                if let Some(l) = &s.listener {
                    l.on_interrupt();
                }
            }
            Task::Periodic(s) => {
                if let Some(l) = &s.listener {
                    l.on_interrupt();
                }
            }
        }
    }
}
