use crate::lane::{Lane, SpawnError};
use crate::{
    CancelReason, ExecutionId, RecurringSpec, RetrySpec, ScopeId, TaskControlHandle, TaskHandle,
    TaskSpec,
};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::watch;

struct GenerationCore {
    id: ScopeId,
    number: u64,
    admission_open: AtomicBool,
    controls: Mutex<HashMap<ExecutionId, TaskControlHandle>>,
    children: Mutex<Vec<Weak<ScopeInner>>>,
}

impl GenerationCore {
    fn new(number: u64) -> Arc<Self> {
        Arc::new(Self {
            id: ScopeId::new(),
            number,
            admission_open: AtomicBool::new(true),
            controls: Mutex::new(HashMap::new()),
            children: Mutex::new(Vec::new()),
        })
    }

    fn controls(&self) -> Vec<TaskControlHandle> {
        self.controls
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .values()
            .cloned()
            .collect()
    }

    fn descendants(&self) -> Vec<Arc<ScopeInner>> {
        self.children
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
}

struct ScopeState {
    current: Arc<GenerationCore>,
    next_generation: u64,
    rotating: bool,
    rotation_ticket: u64,
    closed: bool,
}

pub(crate) struct ScopeInner {
    name: Arc<str>,
    lane: Lane,
    state: Mutex<ScopeState>,
}

/// A named family whose current generation can be rotated atomically.
#[derive(Clone)]
pub struct Scope {
    pub(crate) inner: Arc<ScopeInner>,
}

impl fmt::Debug for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Scope")
            .field("name", &self.name())
            .field("current", &self.current().id())
            .finish()
    }
}

impl Scope {
    pub(crate) fn new(name: impl Into<String>, lane: Lane) -> Self {
        Self {
            inner: Arc::new(ScopeInner {
                name: Arc::from(name.into()),
                lane,
                state: Mutex::new(ScopeState {
                    current: GenerationCore::new(1),
                    next_generation: 2,
                    rotating: false,
                    rotation_ticket: 0,
                    closed: false,
                }),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn lane(&self) -> &Lane {
        &self.inner.lane
    }

    pub fn current(&self) -> ScopeGeneration {
        let current = self
            .inner
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .current
            .clone();
        ScopeGeneration {
            family: Arc::downgrade(&self.inner),
            core: current,
            lane: self.inner.lane.clone(),
        }
    }

    pub fn child(&self, name: impl Into<String>, lane: Lane) -> Result<Self, ScopeError> {
        let child = Self::new(name, lane);
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| ScopeError::LockPoisoned)?;
        if state.closed || !state.current.admission_open.load(Ordering::Acquire) {
            return Err(ScopeError::Closed);
        }
        state
            .current
            .children
            .lock()
            .map_err(|_| ScopeError::LockPoisoned)?
            .push(Arc::downgrade(&child.inner));
        Ok(child)
    }

    /// Synchronously closes old-generation admission and starts an owned
    /// rotation supervisor. Dropping the returned handle does not roll back
    /// the accepted transaction.
    pub fn rotate(&self, policy: RotationPolicy) -> Result<RotationHandle, ScopeError> {
        let (old, new, ticket) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| ScopeError::LockPoisoned)?;
            if state.closed {
                return Err(ScopeError::Closed);
            }
            if state.rotating {
                return Err(ScopeError::RotationInProgress);
            }
            if matches!(policy, RotationPolicy::Strict) {
                if let Some(control) = state
                    .current
                    .controls()
                    .into_iter()
                    .find(|control| crate::execution::would_join(control.execution_id()))
                {
                    return Err(ScopeError::WouldJoin {
                        execution_id: control.execution_id(),
                    });
                }
            }
            let next_ticket = state
                .rotation_ticket
                .checked_add(1)
                .ok_or(ScopeError::SequenceExhausted)?;
            let next_generation = state
                .next_generation
                .checked_add(1)
                .ok_or(ScopeError::SequenceExhausted)?;
            state.rotating = true;
            state.rotation_ticket = next_ticket;
            let ticket = state.rotation_ticket;
            state.current.admission_open.store(false, Ordering::Release);
            let old = Arc::clone(&state.current);
            let new = GenerationCore::new(state.next_generation);
            state.next_generation = next_generation;
            if matches!(policy, RotationPolicy::AvailabilityFirst) {
                state.current = Arc::clone(&new);
            }
            (old, new, ticket)
        };

        let process = Arc::new(RotationProcess::new(old.id, new.id));
        let handle = RotationHandle {
            process: Arc::clone(&process),
        };
        let inner = Arc::clone(&self.inner);
        drop(self.inner.lane.runtime().spawn(async move {
            run_rotation(inner, old, new, ticket, policy, process).await;
        }));
        Ok(handle)
    }

    pub fn cancel(&self, reason: CancelReason) -> Vec<ExecutionId> {
        let controls = collect_scope_controls(&self.inner, true);
        controls
            .into_iter()
            .map(|control| {
                let id = control.execution_id();
                control.cancel(reason.clone());
                id
            })
            .collect()
    }

    pub async fn cancel_and_wait(&self, reason: CancelReason) {
        let controls = collect_scope_controls(&self.inner, true);
        for control in &controls {
            control.cancel(reason.clone());
        }
        for control in controls {
            control.wait().await;
        }
    }

    pub fn close(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        state.closed = true;
        state.current.admission_open.store(false, Ordering::Release);
    }
}

fn collect_scope_controls(inner: &Arc<ScopeInner>, close: bool) -> Vec<TaskControlHandle> {
    let (generation, children) = {
        let mut state = inner
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if close {
            state.closed = true;
            state.current.admission_open.store(false, Ordering::Release);
        }
        (Arc::clone(&state.current), state.current.descendants())
    };
    let mut controls = generation.controls();
    for child in children {
        controls.extend(collect_scope_controls(&child, close));
    }
    controls
}

#[derive(Clone)]
pub struct ScopeGeneration {
    family: Weak<ScopeInner>,
    core: Arc<GenerationCore>,
    lane: Lane,
}

impl fmt::Debug for ScopeGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopeGeneration")
            .field("id", &self.id())
            .field("number", &self.number())
            .finish()
    }
}

impl ScopeGeneration {
    pub fn id(&self) -> ScopeId {
        self.core.id
    }

    pub fn number(&self) -> u64 {
        self.core.number
    }

    pub fn ensure_current(&self) -> Result<(), ScopeSpawnError> {
        let family = self.family.upgrade().ok_or(ScopeSpawnError::ScopeClosed)?;
        let state = family
            .state
            .lock()
            .map_err(|_| ScopeSpawnError::LockPoisoned)?;
        if state.closed {
            return Err(ScopeSpawnError::ScopeClosed);
        }
        if state.current.id != self.core.id || !self.core.admission_open.load(Ordering::Acquire) {
            return Err(ScopeSpawnError::StaleGeneration);
        }
        Ok(())
    }

    pub fn try_spawn<T, E>(&self, spec: TaskSpec<T, E>) -> Result<TaskHandle<T, E>, ScopeSpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let family = self.family.upgrade().ok_or(ScopeSpawnError::ScopeClosed)?;
        let state = family
            .state
            .lock()
            .map_err(|_| ScopeSpawnError::LockPoisoned)?;
        self.check_current(&state)?;
        let handle = self
            .lane
            .try_spawn_scoped(spec, self.core.id, self.core.number)?;
        self.track(handle.control());
        drop(state);
        Ok(handle)
    }

    pub fn try_spawn_future<T, E, F>(&self, future: F) -> Result<TaskHandle<T, E>, ScopeSpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let family = self.family.upgrade().ok_or(ScopeSpawnError::ScopeClosed)?;
        let state = family
            .state
            .lock()
            .map_err(|_| ScopeSpawnError::LockPoisoned)?;
        self.check_current(&state)?;
        let handle = self
            .lane
            .try_spawn_future_scoped(future, self.core.id, self.core.number)?;
        self.track(handle.control());
        drop(state);
        Ok(handle)
    }

    pub fn try_spawn_retry<T, E>(
        &self,
        spec: RetrySpec<T, E>,
    ) -> Result<TaskHandle<T, E>, ScopeSpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let family = self.family.upgrade().ok_or(ScopeSpawnError::ScopeClosed)?;
        let state = family
            .state
            .lock()
            .map_err(|_| ScopeSpawnError::LockPoisoned)?;
        self.check_current(&state)?;
        let handle = self
            .lane
            .try_spawn_retry_scoped(spec, self.core.id, self.core.number)?;
        self.track(handle.control());
        drop(state);
        Ok(handle)
    }

    pub fn try_spawn_recurring<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
    ) -> Result<TaskHandle<T, E>, ScopeSpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let family = self.family.upgrade().ok_or(ScopeSpawnError::ScopeClosed)?;
        let state = family
            .state
            .lock()
            .map_err(|_| ScopeSpawnError::LockPoisoned)?;
        self.check_current(&state)?;
        let handle = self
            .lane
            .try_spawn_recurring_scoped(spec, self.core.id, self.core.number)?;
        self.track(handle.control());
        drop(state);
        Ok(handle)
    }

    fn check_current(&self, state: &ScopeState) -> Result<(), ScopeSpawnError> {
        if state.closed {
            return Err(ScopeSpawnError::ScopeClosed);
        }
        if state.current.id != self.core.id || !self.core.admission_open.load(Ordering::Acquire) {
            return Err(ScopeSpawnError::StaleGeneration);
        }
        Ok(())
    }

    fn track(&self, control: TaskControlHandle) {
        let execution_id = control.execution_id();
        self.core
            .controls
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .insert(execution_id, control.clone());
        let core = Arc::clone(&self.core);
        drop(self.lane.runtime().spawn(async move {
            control.wait().await;
            core.controls
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard)
                .remove(&execution_id);
        }));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RotationPolicy {
    Strict,
    AvailabilityFirst,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RotationOutcome {
    Rotated { old: ScopeId, new: ScopeId },
    Cancelled { old: ScopeId, reserved: ScopeId },
    Failed,
}

struct RotationProcess {
    old: ScopeId,
    new: ScopeId,
    cancelled: AtomicBool,
    result: watch::Sender<Option<RotationOutcome>>,
}

impl RotationProcess {
    fn new(old: ScopeId, new: ScopeId) -> Self {
        let (result, _) = watch::channel(None);
        Self {
            old,
            new,
            cancelled: AtomicBool::new(false),
            result,
        }
    }
}

#[derive(Clone)]
pub struct RotationHandle {
    process: Arc<RotationProcess>,
}

impl RotationHandle {
    pub fn old_scope_id(&self) -> ScopeId {
        self.process.old
    }

    pub fn reserved_scope_id(&self) -> ScopeId {
        self.process.new
    }

    pub fn cancel_pending_new(&self) -> bool {
        !self.process.cancelled.swap(true, Ordering::AcqRel)
    }

    pub async fn wait(&self) -> RotationOutcome {
        let mut result = self.process.result.subscribe();
        loop {
            if let Some(outcome) = result.borrow().clone() {
                return outcome;
            }
            if result.changed().await.is_err() {
                return RotationOutcome::Failed;
            }
        }
    }
}

async fn run_rotation(
    inner: Arc<ScopeInner>,
    old: Arc<GenerationCore>,
    new: Arc<GenerationCore>,
    ticket: u64,
    policy: RotationPolicy,
    process: Arc<RotationProcess>,
) {
    let controls = old.controls();
    for control in &controls {
        control.cancel(CancelReason::ScopeCancelled);
    }
    for child in old.descendants() {
        for control in collect_scope_controls(&child, true) {
            control.cancel(CancelReason::ScopeCancelled);
            control.wait().await;
        }
    }
    if matches!(policy, RotationPolicy::Strict) {
        for control in controls {
            control.wait().await;
        }
    }

    let outcome = {
        let mut state = inner
            .state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if state.rotation_ticket != ticket {
            RotationOutcome::Failed
        } else if process.cancelled.load(Ordering::Acquire) {
            new.admission_open.store(false, Ordering::Release);
            state.closed = true;
            state.rotating = false;
            RotationOutcome::Cancelled {
                old: old.id,
                reserved: new.id,
            }
        } else {
            if matches!(policy, RotationPolicy::Strict) {
                state.current = Arc::clone(&new);
            }
            state.rotating = false;
            RotationOutcome::Rotated {
                old: old.id,
                new: new.id,
            }
        }
    };
    process.result.send_replace(Some(outcome));
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ScopeError {
    Closed,
    RotationInProgress,
    WouldJoin { execution_id: ExecutionId },
    SequenceExhausted,
    LockPoisoned,
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("scope is closed"),
            Self::RotationInProgress => {
                formatter.write_str("scope rotation is already in progress")
            }
            Self::WouldJoin { execution_id } => write!(
                formatter,
                "strict scope rotation would wait for current execution {execution_id} from within that execution"
            ),
            Self::SequenceExhausted => formatter.write_str("scope generation sequence exhausted"),
            Self::LockPoisoned => formatter.write_str("scope state lock was poisoned"),
        }
    }
}

impl std::error::Error for ScopeError {}

#[derive(Debug)]
#[non_exhaustive]
pub enum ScopeSpawnError {
    ScopeClosed,
    StaleGeneration,
    LockPoisoned,
    Lane(SpawnError),
}

impl fmt::Display for ScopeSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeClosed => formatter.write_str("scope is closed"),
            Self::StaleGeneration => formatter.write_str("scope generation is stale"),
            Self::LockPoisoned => formatter.write_str("scope state lock was poisoned"),
            Self::Lane(error) => write!(formatter, "scope admission failed: {error}"),
        }
    }
}

impl std::error::Error for ScopeSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lane(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SpawnError> for ScopeSpawnError {
    fn from(error: SpawnError) -> Self {
        Self::Lane(error)
    }
}
