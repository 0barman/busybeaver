use crate::lane::{Lane, SpawnError};
use crate::{CancelReason, ExecutionId, TaskControlHandle, TaskHandle, TaskSpec, TaskSpecId};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::oneshot;

#[cfg(test)]
#[derive(Clone)]
struct QueueFullTestHook {
    reached: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SlotKey(Arc<str>);

impl SlotKey {
    pub const MAX_BYTES: usize = 256;

    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSlotKey> {
        let value = value.into();
        if value.len() > Self::MAX_BYTES {
            return Err(InvalidSlotKey {
                length: value.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSlotKey {
    pub length: usize,
    pub maximum: usize,
}

impl fmt::Display for InvalidSlotKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "slot key is {} bytes; maximum is {} bytes",
            self.length, self.maximum
        )
    }
}

impl std::error::Error for InvalidSlotKey {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReplacePolicy {
    StrictSingleInstance,
    AvailabilityFirst,
}

struct SlotState {
    revision: Option<u64>,
    spec_id: Option<TaskSpecId>,
    transaction: u64,
    current: Option<TaskControlHandle>,
    last: Option<TaskControlHandle>,
    closed: bool,
}

pub(crate) struct SlotCore {
    key: SlotKey,
    lane: Lane,
    state: Mutex<SlotState>,
    #[cfg(test)]
    queue_full_test_hook: Mutex<Option<QueueFullTestHook>>,
}

#[derive(Clone)]
pub struct TaskSlot {
    pub(crate) core: Arc<SlotCore>,
}

impl fmt::Debug for TaskSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskSlot")
            .field("key", &self.key())
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl TaskSlot {
    pub(crate) fn new(key: SlotKey, lane: Lane) -> Self {
        Self {
            core: Arc::new(SlotCore {
                key,
                lane,
                state: Mutex::new(SlotState {
                    revision: None,
                    spec_id: None,
                    transaction: 0,
                    current: None,
                    last: None,
                    closed: false,
                }),
                #[cfg(test)]
                queue_full_test_hook: Mutex::new(None),
            }),
        }
    }

    #[cfg(test)]
    fn install_queue_full_test_hook(
        &self,
        reached: Arc<tokio::sync::Notify>,
        resume: Arc<tokio::sync::Notify>,
    ) {
        let mut hook = self
            .core
            .queue_full_test_hook
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        *hook = Some(QueueFullTestHook { reached, resume });
    }

    pub fn key(&self) -> &SlotKey {
        &self.core.key
    }

    pub fn lane(&self) -> &Lane {
        &self.core.lane
    }

    pub fn snapshot(&self) -> TaskSlotSnapshot {
        let state = self
            .core
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        TaskSlotSnapshot {
            revision: state.revision,
            current_execution: state.current.as_ref().map(TaskControlHandle::execution_id),
            transaction: state.transaction,
            closed: state.closed,
        }
    }

    /// Accepts a newest-wins replace transaction synchronously and returns a
    /// cancellation-safe result future. The owned supervisor continues if the
    /// future is dropped.
    pub fn replace<T, E>(
        &self,
        revision: u64,
        spec: TaskSpec<T, E>,
        policy: ReplacePolicy,
    ) -> Result<ReplaceHandle<T, E>, ReplaceError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let (transaction, previous) = {
            let mut state = self
                .core
                .state
                .lock()
                .map_err(|_| ReplaceError::LockPoisoned)?;
            if state.closed {
                return Err(ReplaceError::Closed);
            }
            if matches!(policy, ReplacePolicy::StrictSingleInstance) {
                if let Some(current) = &state.current {
                    if crate::execution::would_join(current.execution_id()) {
                        return Err(ReplaceError::WouldJoin {
                            execution_id: current.execution_id(),
                        });
                    }
                }
            }
            match state.revision {
                Some(current) if revision < current => {
                    return Err(ReplaceError::StaleRevision {
                        current,
                        proposed: revision,
                    });
                }
                Some(current) if revision == current => {
                    if state.spec_id != Some(spec.id()) {
                        return Err(ReplaceError::RevisionConflict { revision });
                    }
                    let Some(control) = state.current.clone().or_else(|| state.last.clone()) else {
                        return Err(ReplaceError::RevisionInProgress { revision });
                    };
                    let _ = sender.send(ReplaceOutcome::Existing { revision, control });
                    return Ok(ReplaceHandle { revision, receiver });
                }
                _ => {}
            }
            state.transaction = state
                .transaction
                .checked_add(1)
                .ok_or(ReplaceError::SequenceExhausted)?;
            state.revision = Some(revision);
            state.spec_id = Some(spec.id());
            (state.transaction, state.current.clone())
        };

        let core = Arc::clone(&self.core);
        drop(self.core.lane.runtime().spawn(async move {
            run_replace(core, transaction, revision, spec, policy, previous, sender).await;
        }));
        Ok(ReplaceHandle { revision, receiver })
    }

    pub fn cancel_current(&self, reason: CancelReason) -> Option<ExecutionId> {
        let current = self
            .core
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .current
            .clone();
        current.map(|control| {
            let id = control.execution_id();
            control.cancel(reason);
            id
        })
    }

    pub fn close(&self) -> Option<ExecutionId> {
        let current = {
            let mut state = self
                .core
                .state
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard);
            state.closed = true;
            if let Some(next) = state.transaction.checked_add(1) {
                state.transaction = next;
            } else {
                crate::internal::log_internal_error(
                    "BB-SLOT-CLOSE-SEQUENCE-OVERFLOW",
                    "closed task slot could not advance its exhausted transaction sequence",
                );
            }
            let current = state.current.take();
            state.last = None;
            current
        };
        current.map(|control| {
            let id = control.execution_id();
            control.cancel(CancelReason::Replaced);
            id
        })
    }
}

async fn run_replace<T, E>(
    core: Arc<SlotCore>,
    transaction: u64,
    revision: u64,
    spec: TaskSpec<T, E>,
    policy: ReplacePolicy,
    previous: Option<TaskControlHandle>,
    sender: oneshot::Sender<ReplaceOutcome<T, E>>,
) where
    T: Send + 'static,
    E: Send + 'static,
{
    if matches!(policy, ReplacePolicy::StrictSingleInstance) {
        if let Some(previous) = &previous {
            previous.cancel(CancelReason::Replaced);
            previous.wait().await;
        }
    }

    let handle = loop {
        let capacity_changed = core.lane.capacity_change_notified();
        tokio::pin!(capacity_changed);
        capacity_changed.as_mut().enable();
        let admitted = {
            let state = core
                .state
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard);
            if state.closed || state.transaction != transaction || state.revision != Some(revision)
            {
                let _ = sender.send(ReplaceOutcome::Superseded { revision });
                return;
            }
            core.lane.try_spawn(spec.clone())
        };
        match admitted {
            Ok(handle) => break handle,
            Err(SpawnError::QueueFull) => {
                #[cfg(test)]
                pause_after_queue_full_for_test(&core).await;
                capacity_changed.await;
            }
            Err(error) => {
                let _ = sender.send(ReplaceOutcome::AdmissionFailed { revision, error });
                return;
            }
        }
    };

    let control = handle.control();
    let execution_id = control.execution_id();
    {
        let mut state = core
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if state.closed || state.transaction != transaction || state.revision != Some(revision) {
            control.cancel(CancelReason::Replaced);
            let _ = sender.send(ReplaceOutcome::Superseded { revision });
            return;
        }
        state.current = Some(control.clone());
        state.last = Some(control.clone());
    }

    if matches!(policy, ReplacePolicy::AvailabilityFirst) {
        if let Some(previous) = previous {
            previous.cancel(CancelReason::Replaced);
        }
    }

    let monitor_core = Arc::clone(&core);
    let monitor_control = control.clone();
    drop(core.lane.runtime().spawn(async move {
        monitor_control.wait().await;
        let mut state = monitor_core
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if state.transaction == transaction
            && state
                .current
                .as_ref()
                .is_some_and(|current| current.execution_id() == execution_id)
        {
            state.current = None;
        }
    }));
    let _ = sender.send(ReplaceOutcome::Replaced { revision, handle });
}

#[cfg(test)]
async fn pause_after_queue_full_for_test(core: &SlotCore) {
    let hook = core
        .queue_full_test_hook
        .lock()
        .map_or_else(crate::internal::recover_poison, |guard| guard)
        .take();
    if let Some(hook) = hook {
        hook.reached.notify_one();
        hook.resume.notified().await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSlotSnapshot {
    pub revision: Option<u64>,
    pub current_execution: Option<ExecutionId>,
    pub transaction: u64,
    pub closed: bool,
}

#[non_exhaustive]
pub enum ReplaceOutcome<T, E> {
    Replaced {
        revision: u64,
        handle: TaskHandle<T, E>,
    },
    Existing {
        revision: u64,
        control: TaskControlHandle,
    },
    Superseded {
        revision: u64,
    },
    AdmissionFailed {
        revision: u64,
        error: SpawnError,
    },
}

#[must_use = "dropping a replace handle does not cancel the accepted transaction"]
pub struct ReplaceHandle<T, E> {
    revision: u64,
    receiver: oneshot::Receiver<ReplaceOutcome<T, E>>,
}

impl<T, E> ReplaceHandle<T, E> {
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

impl<T, E> Future for ReplaceHandle<T, E> {
    type Output = Result<ReplaceOutcome<T, E>, ReplaceWaitError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver)
            .poll(context)
            .map(|result| result.map_err(|_| ReplaceWaitError::SupervisorUnavailable))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReplaceError {
    Closed,
    StaleRevision { current: u64, proposed: u64 },
    RevisionConflict { revision: u64 },
    RevisionInProgress { revision: u64 },
    WouldJoin { execution_id: ExecutionId },
    SequenceExhausted,
    LockPoisoned,
}

impl fmt::Display for ReplaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("task slot is closed"),
            Self::StaleRevision { current, proposed } => write!(
                formatter,
                "slot revision {proposed} is stale; current revision is {current}"
            ),
            Self::RevisionConflict { revision } => {
                write!(
                    formatter,
                    "slot revision {revision} has a different task definition"
                )
            }
            Self::RevisionInProgress { revision } => {
                write!(
                    formatter,
                    "slot revision {revision} is still being admitted"
                )
            }
            Self::WouldJoin { execution_id } => write!(
                formatter,
                "strict replacement would wait for current execution {execution_id} from within that execution"
            ),
            Self::SequenceExhausted => formatter.write_str("slot transaction sequence exhausted"),
            Self::LockPoisoned => formatter.write_str("task slot state lock was poisoned"),
        }
    }
}

impl std::error::Error for ReplaceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceWaitError {
    SupervisorUnavailable,
}

impl fmt::Display for ReplaceWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("task slot replace supervisor is unavailable")
    }
}

impl std::error::Error for ReplaceWaitError {}

#[cfg(test)]
mod tests {
    use super::{ReplaceOutcome, ReplacePolicy, SlotKey, TaskSlot};
    use crate::{
        Beaver, CancelReason, LaneConfig, OrderingKey, ResourceLimits, SpawnError, SpawnOptions,
        TaskSpec,
    };
    use std::error::Error;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Notify;

    type TestResult = Result<(), Box<dyn Error>>;

    fn test_error(message: impl Into<String>) -> Box<dyn Error> {
        Box::new(std::io::Error::other(message.into()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slot_capacity_release_between_queue_full_and_wait_registration_completes() -> TestResult
    {
        let beaver = Beaver::new("slot-lost-wake", 4)?;
        let lane = beaver.create_lane(
            LaneConfig::new("slot-lost-wake-lane")
                .capacity(1)
                .concurrency(1),
        )?;

        let running_started = Arc::new(Notify::new());
        let release_running = Arc::new(Notify::new());
        let running_started_task = Arc::clone(&running_started);
        let release_running_task = Arc::clone(&release_running);
        let running = lane.try_spawn(TaskSpec::new(move |_| {
            let running_started = Arc::clone(&running_started_task);
            let release_running = Arc::clone(&release_running_task);
            async move {
                running_started.notify_one();
                release_running.notified().await;
                Ok::<_, ()>(())
            }
        }))?;
        running_started.notified().await;

        let queued_started = Arc::new(Notify::new());
        let release_queued = Arc::new(Notify::new());
        let queued_started_task = Arc::clone(&queued_started);
        let release_queued_task = Arc::clone(&release_queued);
        let queued = lane.try_spawn(TaskSpec::new(move |_| {
            let queued_started = Arc::clone(&queued_started_task);
            let release_queued = Arc::clone(&release_queued_task);
            async move {
                queued_started.notify_one();
                release_queued.notified().await;
                Ok::<_, ()>(())
            }
        }))?;
        let slot = TaskSlot::new(SlotKey::new("slot-lost-wake-key")?, lane.clone());
        let queue_full_reached = Arc::new(Notify::new());
        let resume_replace = Arc::new(Notify::new());
        slot.install_queue_full_test_hook(
            Arc::clone(&queue_full_reached),
            Arc::clone(&resume_replace),
        );

        let replace = slot.replace(
            1,
            TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
            ReplacePolicy::AvailabilityFirst,
        )?;
        queue_full_reached.notified().await;

        release_running.notify_one();
        running.wait().await;
        queued_started.notified().await;
        resume_replace.notify_one();

        let observed = tokio::time::timeout(Duration::from_millis(250), replace).await;
        let mut replacement_control = None;
        let result = match observed {
            Ok(Ok(ReplaceOutcome::Replaced { handle, .. })) => {
                replacement_control = Some(handle.control());
                Ok(())
            }
            Ok(Ok(_)) => Err(test_error(
                "slot replacement returned an unexpected outcome",
            )),
            Ok(Err(error)) => Err(test_error(format!(
                "slot replacement supervisor failed: {error}"
            ))),
            Err(_) => Err(test_error(
                "slot replacement lost the capacity notification and did not complete",
            )),
        };

        if let Some(control) = replacement_control {
            control.cancel(CancelReason::UserRequested);
            control.wait().await;
        }
        queued.control().cancel(CancelReason::UserRequested);
        release_queued.notify_one();
        queued.wait().await;
        slot.close();
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slot_is_woken_when_last_formal_waiter_rejects_available_capacity() -> TestResult {
        let limits = ResourceLimits {
            max_ordering_keys_per_lane: 1,
            ..ResourceLimits::default()
        };
        let beaver = Beaver::builder("slot-formal-waiter-handoff", 4)
            .resource_limits(limits)
            .build()?;
        let lane = beaver.create_lane(
            LaneConfig::new("slot-formal-waiter-handoff-lane")
                .capacity(1)
                .concurrency(1),
        )?;
        let key_a = OrderingKey::new("key-a")?;
        let key_b = OrderingKey::new("key-b")?;

        let running_started = Arc::new(Notify::new());
        let release_running = Arc::new(Notify::new());
        let running_started_task = Arc::clone(&running_started);
        let release_running_task = Arc::clone(&release_running);
        let running = lane.try_spawn_with_options(
            TaskSpec::new(move |_| {
                let running_started = Arc::clone(&running_started_task);
                let release_running = Arc::clone(&release_running_task);
                async move {
                    running_started.notify_one();
                    release_running.notified().await;
                    Ok::<_, ()>(())
                }
            }),
            SpawnOptions::default().ordering_key(key_a.clone()),
        )?;
        running_started.notified().await;
        let mut queued = lane.try_spawn_with_options(
            TaskSpec::new(|context| async move {
                context.cancelled().await;
                Ok::<_, ()>(())
            }),
            SpawnOptions::default().ordering_key(key_a),
        )?;

        let slot = TaskSlot::new(
            SlotKey::new("slot-formal-waiter-handoff-key")?,
            lane.clone(),
        );
        let first_queue_full = Arc::new(Notify::new());
        let resume_first_replace = Arc::new(Notify::new());
        slot.install_queue_full_test_hook(
            Arc::clone(&first_queue_full),
            Arc::clone(&resume_first_replace),
        );
        let replace = slot.replace(
            1,
            TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
            ReplacePolicy::AvailabilityFirst,
        )?;
        first_queue_full.notified().await;
        resume_first_replace.notify_one();

        let waiting_lane = lane.clone();
        let formal_waiter = tokio::spawn(async move {
            waiting_lane
                .spawn_with_options(
                    TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
                    SpawnOptions::default().ordering_key(key_b),
                )
                .await
        });
        for _ in 0..10_000_usize {
            if lane.stats().waiting_producers == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        if lane.stats().waiting_producers != 1 {
            return Err(test_error("formal lane waiter was not registered"));
        }

        let second_queue_full = Arc::new(Notify::new());
        let resume_second_replace = Arc::new(Notify::new());
        slot.install_queue_full_test_hook(
            Arc::clone(&second_queue_full),
            Arc::clone(&resume_second_replace),
        );
        lane.suppress_targeted_waiter_notifications_for_test(true);
        queued.control().cancel(CancelReason::UserRequested);
        let _ = queued.join().await?;
        tokio::time::timeout(Duration::from_secs(1), second_queue_full.notified())
            .await
            .map_err(|_| test_error("slot did not consume the initial capacity notification"))?;

        lane.suppress_targeted_waiter_notifications_for_test(false);
        lane.notify_waiting_head_for_test();
        let formal_result = tokio::time::timeout(Duration::from_secs(1), formal_waiter)
            .await
            .map_err(|_| test_error("formal waiter did not finish after targeted notification"))?
            .map_err(|error| test_error(format!("formal waiter task failed: {error}")))?;
        if !matches!(formal_result, Err(SpawnError::OrderingKeyLimitReached)) {
            return Err(test_error("formal waiter returned an unexpected result"));
        }

        resume_second_replace.notify_one();
        let outcome = tokio::time::timeout(Duration::from_millis(250), replace)
            .await
            .map_err(|_| {
                test_error(
                    "slot remained asleep after the last formal waiter left available capacity",
                )
            })??;
        let replacement = match outcome {
            ReplaceOutcome::Replaced { handle, .. } => handle,
            _ => {
                return Err(test_error(
                    "slot replacement returned an unexpected outcome",
                ))
            }
        };
        replacement.control().cancel(CancelReason::UserRequested);
        replacement.wait().await;
        running.control().cancel(CancelReason::UserRequested);
        release_running.notify_one();
        running.wait().await;
        slot.close();
        beaver.destroy().await?;
        Ok(())
    }
}
