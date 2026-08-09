use crate::scheduler::{CancelReason, JobId, TaskRunId, TaskState};
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, Notify};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The publication phase represented by a [`TaskEvent`].
pub enum TaskEventKind {
    /// A run was accepted into a scheduler lane.
    Submitted,
    /// A non-terminal state transition was published.
    StateChanged,
    /// The run's single authoritative terminal state was published.
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A bounded, payload-free task lifecycle event.
///
/// Events intentionally exclude task keys, result/error values, panic text,
/// and closure captures. Delivery is best effort; [`TaskSnapshot`](crate::TaskSnapshot)
/// and `TaskTerminal` remain authoritative.
pub struct TaskEvent {
    sequence: u64,
    observed_at: Duration,
    kind: TaskEventKind,
    run_id: TaskRunId,
    job_id: JobId,
    lane: Arc<str>,
    lane_generation: u64,
    group_generation: Option<u64>,
    state: TaskState,
    cancel_reason: Option<CancelReason>,
    next_wake_at: Option<Duration>,
}

impl TaskEvent {
    /// Returns the scheduler-wide monotonic publication sequence.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the monotonic offset from scheduler creation.
    pub fn observed_at(&self) -> Duration {
        self.observed_at
    }

    /// Returns the lifecycle publication phase.
    pub fn kind(&self) -> TaskEventKind {
        self.kind
    }

    /// Returns the unique identity of this submitted run.
    pub fn run_id(&self) -> TaskRunId {
        self.run_id
    }

    /// Returns the identity shared by instantiations of the same reusable job.
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    /// Returns the lane name captured at submission.
    pub fn lane(&self) -> &str {
        &self.lane
    }

    /// Returns the lane generation captured at submission.
    pub fn lane_generation(&self) -> u64 {
        self.lane_generation
    }

    /// Returns the task-group generation, when submitted through a group.
    pub fn group_generation(&self) -> Option<u64> {
        self.group_generation
    }

    /// Returns the state published by this event.
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// Returns the first accepted cancellation reason, if any.
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        self.cancel_reason
    }

    /// Returns the planned wake-up offset from scheduler creation for retry
    /// and schedule waits. It is absent for execution and permit waits.
    pub fn next_wake_at(&self) -> Option<Duration> {
        self.next_wake_at
    }
}

/// A synchronous metrics consumer dispatched away from task execution.
///
/// One serial dispatcher invokes the hook, so hooks should still return
/// promptly. A panic is isolated and counted. A blocked call does not block
/// task execution, but it delays every later hook call, can fill the bounded
/// queue and can cause event delivery drops.
pub trait MetricsHook: Send + Sync + 'static {
    /// Receives a best-effort event away from the task execution path.
    fn on_event(&self, event: &TaskEvent);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Indicates that every sender for an event stream has been dropped.
pub struct EventRecvError;

impl fmt::Display for EventRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task event stream is closed")
    }
}

impl std::error::Error for EventRecvError {}

/// A bounded task-event subscription.
///
/// If this receiver falls behind, overwritten events are skipped and counted
/// in [`SchedulerSnapshot::dropped_event_deliveries`].
pub struct EventReceiver {
    receiver: broadcast::Receiver<TaskEvent>,
    dropped_deliveries: Arc<AtomicU64>,
}

impl EventReceiver {
    /// Waits for the next available event, transparently skipping lagged
    /// entries while recording their count.
    pub async fn recv(&mut self) -> Result<TaskEvent, EventRecvError> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Ok(event),
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    self.dropped_deliveries.fetch_add(count, Ordering::Relaxed);
                }
                Err(broadcast::error::RecvError::Closed) => return Err(EventRecvError),
            }
        }
    }

    /// Returns the next event without waiting, or `None` when none is ready.
    pub fn try_recv(&mut self) -> Result<Option<TaskEvent>, EventRecvError> {
        loop {
            match self.receiver.try_recv() {
                Ok(event) => return Ok(Some(event)),
                Err(broadcast::error::TryRecvError::Empty) => return Ok(None),
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    self.dropped_deliveries.fetch_add(count, Ordering::Relaxed);
                }
                Err(broadcast::error::TryRecvError::Closed) => return Err(EventRecvError),
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// An eventually consistent, payload-free view of Scheduler resources.
///
/// Counts come from separate internal registries and therefore do not form an
/// atomic cross-field transaction. `snapshot_version` identifies the capture.
pub struct SchedulerSnapshot {
    snapshot_version: u64,
    active_tasks: usize,
    queued_tasks: usize,
    running_tasks: usize,
    waiting_tasks: usize,
    cancel_requested_tasks: usize,
    shutting_down: bool,
    lanes: usize,
    groups: usize,
    dropped_event_deliveries: u64,
    observation_failures: u64,
}

impl SchedulerSnapshot {
    pub(crate) fn new(data: SchedulerSnapshotData) -> Self {
        Self {
            snapshot_version: data.snapshot_version,
            active_tasks: data.active_tasks,
            queued_tasks: data.queued_tasks,
            running_tasks: data.running_tasks,
            waiting_tasks: data.waiting_tasks,
            cancel_requested_tasks: data.cancel_requested_tasks,
            shutting_down: data.shutting_down,
            lanes: data.lanes,
            groups: data.groups,
            dropped_event_deliveries: data.dropped_event_deliveries,
            observation_failures: data.observation_failures,
        }
    }

    /// Returns the observation sequence assigned to this capture.
    pub fn snapshot_version(&self) -> u64 {
        self.snapshot_version
    }

    /// Returns the number of registered non-terminal task runs.
    pub fn active_tasks(&self) -> usize {
        self.active_tasks
    }

    /// Returns registered runs waiting to begin or acquire permits.
    pub fn queued_tasks(&self) -> usize {
        self.queued_tasks
    }

    /// Returns registered runs currently executing user work.
    pub fn running_tasks(&self) -> usize {
        self.running_tasks
    }

    /// Returns registered runs waiting on pause, backoff, or schedule time.
    pub fn waiting_tasks(&self) -> usize {
        self.waiting_tasks
    }

    /// Returns registered runs for which cancellation was requested but no
    /// terminal has yet been published.
    pub fn cancel_requested_tasks(&self) -> usize {
        self.cancel_requested_tasks
    }

    /// Returns whether scheduler shutdown has started.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Returns the number of currently registered lane names.
    pub fn lanes(&self) -> usize {
        self.lanes
    }

    /// Returns the number of currently registered task-group names.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// Returns the cumulative number of event and hook deliveries dropped by
    /// bounded observation channels.
    pub fn dropped_event_deliveries(&self) -> u64 {
        self.dropped_event_deliveries
    }

    /// Returns the cumulative count of hook failures and observation sequence
    /// exhaustion.
    pub fn observation_failures(&self) -> u64 {
        self.observation_failures
    }
}

pub(crate) struct SchedulerSnapshotData {
    pub(crate) snapshot_version: u64,
    pub(crate) active_tasks: usize,
    pub(crate) queued_tasks: usize,
    pub(crate) running_tasks: usize,
    pub(crate) waiting_tasks: usize,
    pub(crate) cancel_requested_tasks: usize,
    pub(crate) shutting_down: bool,
    pub(crate) lanes: usize,
    pub(crate) groups: usize,
    pub(crate) dropped_event_deliveries: u64,
    pub(crate) observation_failures: u64,
}

pub(crate) struct EventData {
    pub(crate) run_id: TaskRunId,
    pub(crate) job_id: JobId,
    pub(crate) lane: Arc<str>,
    pub(crate) lane_generation: u64,
    pub(crate) group_generation: Option<u64>,
    pub(crate) state: TaskState,
    pub(crate) cancel_reason: Option<CancelReason>,
    pub(crate) next_wake_at: Option<Duration>,
}

pub(crate) struct Observability {
    started_at: tokio::time::Instant,
    publication: Mutex<()>,
    sequence: AtomicU64,
    events: broadcast::Sender<TaskEvent>,
    hook: Option<mpsc::Sender<TaskEvent>>,
    outstanding_hooks: Arc<AtomicUsize>,
    hook_progress: Arc<Notify>,
    dropped_deliveries: Arc<AtomicU64>,
    observation_failures: Arc<AtomicU64>,
}

impl Observability {
    pub(crate) fn new(
        runtime: &Handle,
        capacity: usize,
        hook: Option<Arc<dyn MetricsHook>>,
    ) -> Self {
        let (events, _) = broadcast::channel(capacity);
        let dropped_deliveries = Arc::new(AtomicU64::new(0));
        let observation_failures = Arc::new(AtomicU64::new(0));
        let outstanding_hooks = Arc::new(AtomicUsize::new(0));
        let hook_progress = Arc::new(Notify::new());
        let hook_sender = hook.map(|hook| {
            let (sender, mut receiver) = mpsc::channel::<TaskEvent>(capacity);
            let outstanding = Arc::clone(&outstanding_hooks);
            let progress = Arc::clone(&hook_progress);
            let failures = Arc::clone(&observation_failures);
            runtime.spawn(async move {
                while let Some(event) = receiver.recv().await {
                    let hook = Arc::clone(&hook);
                    let panicked = tokio::task::spawn_blocking(move || {
                        catch_unwind(AssertUnwindSafe(|| hook.on_event(&event))).is_err()
                    })
                    .await
                    .unwrap_or(true);
                    if panicked {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    outstanding.fetch_sub(1, Ordering::AcqRel);
                    progress.notify_waiters();
                }
            });
            sender
        });
        Self {
            started_at: tokio::time::Instant::now(),
            publication: Mutex::new(()),
            sequence: AtomicU64::new(0),
            events,
            hook: hook_sender,
            outstanding_hooks,
            hook_progress,
            dropped_deliveries,
            observation_failures,
        }
    }

    fn next_stamp(&self) -> (u64, Duration, bool) {
        let (sequence, publishable) =
            match self
                .sequence
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                }) {
                Ok(previous) => (previous + 1, true),
                Err(_) => {
                    self.observation_failures.fetch_add(1, Ordering::Relaxed);
                    (u64::MAX, false)
                }
            };
        (
            sequence,
            tokio::time::Instant::now().saturating_duration_since(self.started_at),
            publishable,
        )
    }

    pub(crate) fn observe(&self) -> (u64, Duration) {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (sequence, observed_at, _) = self.next_stamp();
        (sequence, observed_at)
    }

    pub(crate) fn publish(
        &self,
        kind: TaskEventKind,
        build: impl FnOnce(u64, Duration) -> EventData,
    ) -> (u64, Duration) {
        let _publication = self
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (sequence, observed_at, publishable) = self.next_stamp();
        let data = build(sequence, observed_at);
        if !publishable {
            self.dropped_deliveries.fetch_add(1, Ordering::Relaxed);
            return (sequence, observed_at);
        }
        let event = TaskEvent {
            sequence,
            observed_at,
            kind,
            run_id: data.run_id,
            job_id: data.job_id,
            lane: data.lane,
            lane_generation: data.lane_generation,
            group_generation: data.group_generation,
            state: data.state,
            cancel_reason: data.cancel_reason,
            next_wake_at: data.next_wake_at,
        };
        let _ = self.events.send(event.clone());
        if let Some(hook) = &self.hook {
            self.outstanding_hooks.fetch_add(1, Ordering::AcqRel);
            match hook.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.outstanding_hooks.fetch_sub(1, Ordering::AcqRel);
                    self.dropped_deliveries.fetch_add(1, Ordering::Relaxed);
                    self.hook_progress.notify_waiters();
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.outstanding_hooks.fetch_sub(1, Ordering::AcqRel);
                    self.dropped_deliveries.fetch_add(1, Ordering::Relaxed);
                    self.observation_failures.fetch_add(1, Ordering::Relaxed);
                    self.hook_progress.notify_waiters();
                }
            }
        }
        (sequence, observed_at)
    }

    pub(crate) fn subscribe(&self) -> EventReceiver {
        EventReceiver {
            receiver: self.events.subscribe(),
            dropped_deliveries: Arc::clone(&self.dropped_deliveries),
        }
    }

    pub(crate) fn dropped_deliveries(&self) -> u64 {
        self.dropped_deliveries.load(Ordering::Relaxed)
    }

    pub(crate) fn observation_failures(&self) -> u64 {
        self.observation_failures.load(Ordering::Relaxed)
    }

    pub(crate) async fn drain(&self, timeout: Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.hook_progress.notified();
                if self.outstanding_hooks.load(Ordering::Acquire) == 0 {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::Observability;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn observation_sequence_exhaustion_is_explicit_and_never_wraps() {
        let observation = Observability::new(&tokio::runtime::Handle::current(), 1, None);
        observation.sequence.store(u64::MAX - 1, Ordering::Release);

        assert_eq!(observation.observe().0, u64::MAX);
        assert_eq!(observation.observation_failures(), 0);
        assert_eq!(observation.observe().0, u64::MAX);
        assert_eq!(observation.observation_failures(), 1);
        assert_eq!(observation.sequence.load(Ordering::Acquire), u64::MAX);
    }
}
