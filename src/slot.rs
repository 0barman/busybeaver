use crate::lane::{Lane, SpawnError};
use crate::{CancelReason, ExecutionId, TaskControlHandle, TaskHandle, TaskSpec, TaskSpecId};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::oneshot;

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
            }),
        }
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
            Err(SpawnError::QueueFull) => core.lane.wait_for_capacity_change().await,
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
