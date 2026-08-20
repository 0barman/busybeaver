use crate::{ExecutionId, LaneId, LaneStats, TaskExitSummary, TaskSnapshot};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// Bounded executor resources. Limits are immutable after construction so
/// admission can fail explicitly without partially publishing a resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub max_active_executions: usize,
    pub max_lanes: usize,
    pub max_scopes: usize,
    pub max_slots: usize,
    pub max_event_subscribers: usize,
    pub max_children_per_execution: usize,
    pub max_waiting_producers_per_lane: usize,
    pub max_ordering_keys_per_lane: usize,
    pub terminal_history_capacity: usize,
    pub terminal_history_ttl: Option<Duration>,
    pub event_capacity: usize,
    pub max_tag_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_active_executions: 100_000,
            max_lanes: 1_024,
            max_scopes: 4_096,
            max_slots: 4_096,
            max_event_subscribers: 64,
            max_children_per_execution: 1_024,
            max_waiting_producers_per_lane: 4_096,
            max_ordering_keys_per_lane: 65_536,
            terminal_history_capacity: 1_024,
            terminal_history_ttl: Some(Duration::from_secs(60 * 60)),
            event_capacity: 1_024,
            max_tag_bytes: 256,
        }
    }
}

impl ResourceLimits {
    pub(crate) fn validate(&self) -> Result<(), InvalidResourceLimits> {
        let values = [
            ("max_active_executions", self.max_active_executions),
            ("max_lanes", self.max_lanes),
            ("max_scopes", self.max_scopes),
            ("max_slots", self.max_slots),
            ("max_event_subscribers", self.max_event_subscribers),
            (
                "max_children_per_execution",
                self.max_children_per_execution,
            ),
            (
                "max_waiting_producers_per_lane",
                self.max_waiting_producers_per_lane,
            ),
            (
                "max_ordering_keys_per_lane",
                self.max_ordering_keys_per_lane,
            ),
            ("event_capacity", self.event_capacity),
            ("max_tag_bytes", self.max_tag_bytes),
        ];
        if let Some((field, _)) = values.into_iter().find(|(_, value)| *value == 0) {
            return Err(InvalidResourceLimits { field });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidResourceLimits {
    pub field: &'static str,
}

impl fmt::Display for InvalidResourceLimits {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "resource limit '{}' must be non-zero",
            self.field
        )
    }
}

impl std::error::Error for InvalidResourceLimits {}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskEvent {
    Admitted(TaskSnapshot),
    StateChanged(TaskSnapshot),
    Terminal {
        execution_id: ExecutionId,
        summary: TaskExitSummary,
    },
}

pub struct EventStream {
    pub(crate) receiver: broadcast::Receiver<TaskEvent>,
    pub(crate) subscribers: Arc<AtomicUsize>,
}

impl EventStream {
    pub async fn recv(&mut self) -> Result<TaskEvent, EventRecvError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Closed => EventRecvError::Closed,
            broadcast::error::RecvError::Lagged(skipped) => EventRecvError::Lagged { skipped },
        })
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventRecvError {
    Closed,
    Lagged { skipped: u64 },
}

impl fmt::Display for EventRecvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("task event stream is closed"),
            Self::Lagged { skipped } => {
                write!(formatter, "task event stream lagged by {skipped} events")
            }
        }
    }
}

impl std::error::Error for EventRecvError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventSubscribeError {
    SubscriberLimitReached,
}

impl fmt::Display for EventSubscribeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("task event subscriber limit reached")
    }
}

impl std::error::Error for EventSubscribeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRecord {
    pub execution_id: ExecutionId,
    pub summary: TaskExitSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneSnapshot {
    pub lane_id: LaneId,
    pub name: String,
    pub stats: LaneStats,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorSnapshot {
    pub active: Vec<TaskSnapshot>,
    pub terminal_history: Vec<TerminalRecord>,
    pub lanes: Vec<LaneSnapshot>,
    pub scope_count: usize,
    pub slot_count: usize,
    pub event_subscribers: usize,
}
