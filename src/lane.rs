use crate::error::{BeaverError, BeaverResult};
use crate::execution::{
    self, CancelReason, ExecutionRegistry, ExecutionStart, ExecutorLifetime, PreparedExecution,
    TaskControlHandle, TaskHandle, TaskSpec,
};
use crate::ids::{ExecutionId, LaneId};
use crate::recurring::{self, RecurringSpec};
use crate::retry::{self, RetrySpec};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Scheduling priority used when a lane has more ready work than running
/// capacity. Priority affects start selection only; it never changes result
/// ordering. Every eighth dispatch is reserved for the oldest ready entry so
/// continuously arriving high-priority work cannot starve older work.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Priority(u8);

impl Priority {
    pub const LOWEST: Self = Self(0);
    pub const NORMAL: Self = Self(3);
    pub const HIGHEST: Self = Self(7);

    pub fn new(value: u8) -> Result<Self, InvalidPriority> {
        if value <= Self::HIGHEST.0 {
            Ok(Self(value))
        } else {
            Err(InvalidPriority { value })
        }
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidPriority {
    pub value: u8,
}

impl fmt::Display for InvalidPriority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "priority {} is outside the supported 0..=7 range",
            self.value
        )
    }
}

impl std::error::Error for InvalidPriority {}

/// Crate-owned, bounded ordering key. User `Hash`/`Eq` implementations are
/// never invoked while a lane lock is held.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OrderingKey(Arc<[u8]>);

impl OrderingKey {
    pub const MAX_BYTES: usize = 256;

    pub fn new(bytes: impl AsRef<[u8]>) -> Result<Self, InvalidOrderingKey> {
        let bytes = bytes.as_ref();
        if bytes.len() > Self::MAX_BYTES {
            return Err(InvalidOrderingKey {
                length: bytes.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        Ok(Self(Arc::from(bytes)))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<String> for OrderingKey {
    type Error = InvalidOrderingKey;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value.into_bytes())
    }
}

impl TryFrom<&str> for OrderingKey {
    type Error = InvalidOrderingKey;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidOrderingKey {
    pub length: usize,
    pub maximum: usize,
}

impl fmt::Display for InvalidOrderingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ordering key is {} bytes; maximum is {} bytes",
            self.length, self.maximum
        )
    }
}

impl std::error::Error for InvalidOrderingKey {}

/// Per-submission lane scheduling metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnOptions {
    priority: Priority,
    ordering_key: Option<OrderingKey>,
}

impl SpawnOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn ordering_key(mut self, ordering_key: OrderingKey) -> Self {
        self.ordering_key = Some(ordering_key);
        self
    }

    pub fn configured_priority(&self) -> Priority {
        self.priority
    }

    pub fn configured_ordering_key(&self) -> Option<&OrderingKey> {
        self.ordering_key.as_ref()
    }
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            priority: Priority::NORMAL,
            ordering_key: None,
        }
    }
}

/// Ownership policy for a lane generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LaneLifetime {
    Executor,
    Explicit,
}

/// Immutable lane configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneConfig {
    name: String,
    capacity: usize,
    concurrency: usize,
    lifetime: LaneLifetime,
}

impl LaneConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            capacity: 256,
            concurrency: 1,
            lifetime: LaneLifetime::Executor,
        }
    }

    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    pub fn lifetime(mut self, lifetime: LaneLifetime) -> Self {
        self.lifetime = lifetime;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn queue_capacity(&self) -> usize {
        self.capacity
    }

    pub fn max_concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn lane_lifetime(&self) -> LaneLifetime {
        self.lifetime
    }

    pub(crate) fn validate(&self) -> BeaverResult<()> {
        if self.capacity == 0 {
            return Err(BeaverError::InvalidLaneCapacity);
        }
        if self.concurrency == 0 {
            return Err(BeaverError::InvalidLaneConcurrency);
        }
        Ok(())
    }
}

/// Admission error for the public Lane API.
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnError {
    QueueFull,
    LaneClosing,
    ExecutorUnavailable,
    ExecutorShuttingDown,
    AdmissionTimedOut,
    AdmissionDeadlineExceeded,
    TimerUnavailable,
    WaitingProducerLimitReached,
    OrderingKeyLimitReached,
    SequenceExhausted,
    Internal(BeaverError),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("lane queue is full"),
            Self::LaneClosing => formatter.write_str("lane is closing"),
            Self::ExecutorUnavailable => formatter.write_str("lane runtime is unavailable"),
            Self::ExecutorShuttingDown => formatter.write_str("executor is shutting down"),
            Self::AdmissionTimedOut => formatter.write_str("lane admission timed out"),
            Self::AdmissionDeadlineExceeded => {
                formatter.write_str("retry deadline elapsed before admission")
            }
            Self::TimerUnavailable => formatter.write_str("Tokio time driver is unavailable"),
            Self::WaitingProducerLimitReached => {
                formatter.write_str("lane waiting-producer limit reached")
            }
            Self::OrderingKeyLimitReached => formatter.write_str("lane ordering-key limit reached"),
            Self::SequenceExhausted => formatter.write_str("lane admission sequence exhausted"),
            Self::Internal(error) => write!(formatter, "lane admission failed: {error}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error),
            _ => None,
        }
    }
}

impl From<BeaverError> for SpawnError {
    fn from(error: BeaverError) -> Self {
        match error {
            BeaverError::ExecutorShuttingDown => Self::ExecutorShuttingDown,
            other => Self::Internal(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneStats {
    pub queued_live: usize,
    pub running: usize,
    pub available_queue_capacity: usize,
    pub waiting_producers: usize,
    pub blocked_by_ordering_key: usize,
    pub ready_by_priority: [usize; 8],
}

struct QueueEntry {
    start: ExecutionStart,
    options: SpawnOptions,
    sequence: u64,
}

struct LaneState {
    closing: bool,
    runtime_unavailable: bool,
    queue: VecDeque<ExecutionId>,
    entries: HashMap<ExecutionId, QueueEntry>,
    active: HashMap<ExecutionId, TaskControlHandle>,
    running: usize,
    waiting_producers: usize,
    waiting_queue: BTreeMap<u64, Arc<Notify>>,
    next_waiter_sequence: u64,
    next_sequence: u64,
    dispatch_count: u64,
    active_ordering_keys: HashSet<OrderingKey>,
    running_ordering_keys: HashMap<ExecutionId, OrderingKey>,
    ordering_key_refcounts: HashMap<OrderingKey, usize>,
}

#[derive(Clone, Copy)]
struct ReadyCandidate {
    queue_index: usize,
    execution_id: ExecutionId,
    priority: Priority,
    sequence: u64,
}

fn select_ready_candidate(
    dispatch_count: u64,
    candidates: impl Iterator<Item = ReadyCandidate>,
) -> Option<ReadyCandidate> {
    let select_oldest = dispatch_count % 8 == 7;
    let mut selected: Option<ReadyCandidate> = None;
    for candidate in candidates {
        let replace = selected.is_none_or(|current| {
            if select_oldest {
                candidate.sequence < current.sequence
            } else {
                candidate.priority > current.priority
                    || (candidate.priority == current.priority
                        && candidate.sequence < current.sequence)
            }
        });
        if replace {
            selected = Some(candidate);
        }
    }
    selected
}

struct LaneShared {
    id: LaneId,
    config: LaneConfig,
    runtime: Handle,
    state: Mutex<LaneState>,
    work_changed: Notify,
    capacity_changed: Notify,
    max_waiting_producers: usize,
    max_ordering_keys: usize,
    #[cfg(test)]
    targeted_waiter_notifications: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    suppress_targeted_waiter_notifications: std::sync::atomic::AtomicBool,
}

struct WaitingProducer<'a> {
    shared: &'a LaneShared,
    ticket: u64,
    signal: Arc<Notify>,
    active: bool,
}

impl<'a> WaitingProducer<'a> {
    fn new(shared: &'a LaneShared) -> Result<Self, SpawnError> {
        let mut state = shared.lock_state();
        if state.waiting_producers >= shared.max_waiting_producers {
            return Err(SpawnError::WaitingProducerLimitReached);
        }
        let ticket = state.next_waiter_sequence;
        state.next_waiter_sequence = state
            .next_waiter_sequence
            .checked_add(1)
            .ok_or(SpawnError::SequenceExhausted)?;
        state.waiting_producers = state
            .waiting_producers
            .checked_add(1)
            .ok_or(SpawnError::SequenceExhausted)?;
        let signal = Arc::new(Notify::new());
        state.waiting_queue.insert(ticket, Arc::clone(&signal));
        drop(state);
        Ok(Self {
            shared,
            ticket,
            signal,
            active: true,
        })
    }

    fn ticket(&self) -> u64 {
        self.ticket
    }

    fn notification(&self) -> std::pin::Pin<Box<tokio::sync::futures::OwnedNotified>> {
        let mut notification = Box::pin(Arc::clone(&self.signal).notified_owned());
        notification.as_mut().enable();
        notification
    }

    fn finish(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.shared.lock_state();
        let was_front = state
            .waiting_queue
            .first_key_value()
            .map(|(ticket, _)| ticket)
            == Some(&self.ticket);
        let removed = state.waiting_queue.remove(&self.ticket).is_some();
        if !removed {
            crate::internal::log_internal_error(
                "BB-LANE-WAITER-TICKET-MISSING",
                "active waiting-producer ticket was missing from the waiter queue",
            );
        } else if let Some(next) = state.waiting_producers.checked_sub(1) {
            state.waiting_producers = next;
        } else {
            crate::internal::log_internal_error(
                "BB-LANE-WAITER-COUNT-UNDERFLOW",
                "lane waiting-producer counter underflowed",
            );
        }
        let next_signal = was_front
            .then(|| state.waiting_queue.first_key_value())
            .flatten()
            .map(|(_, signal)| Arc::clone(signal));
        let notify_capacity_observers = removed
            && was_front
            && state.waiting_queue.is_empty()
            && state.entries.len() < self.shared.config.capacity;
        self.active = false;
        drop(state);
        if let Some(signal) = next_signal {
            self.shared.notify_waiting_producer(&signal);
        }
        if notify_capacity_observers {
            self.shared.capacity_changed.notify_waiters();
        }
    }
}

impl Drop for WaitingProducer<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

fn decrement_ordering_key_refcount(state: &mut LaneState, key: &OrderingKey) {
    let remove = match state.ordering_key_refcounts.get_mut(key) {
        Some(count) => match count.checked_sub(1) {
            Some(0) => true,
            Some(next) => {
                *count = next;
                false
            }
            None => {
                crate::internal::log_internal_error(
                    "BB-LANE-ORDERING-KEY-COUNT-UNDERFLOW",
                    "lane ordering-key reference counter underflowed",
                );
                false
            }
        },
        None => {
            crate::internal::log_internal_error(
                "BB-LANE-ORDERING-KEY-COUNT-MISSING",
                "lane ordering-key reference was missing during lifecycle cleanup",
            );
            false
        }
    };
    if remove {
        state.ordering_key_refcounts.remove(key);
    }
}

impl LaneShared {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, LaneState> {
        self.state
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
    }

    fn notify_waiting_producer(&self, signal: &Arc<Notify>) {
        #[cfg(test)]
        if self
            .suppress_targeted_waiter_notifications
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        #[cfg(test)]
        self.targeted_waiter_notifications
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        signal.notify_one();
    }

    fn notify_waiting_head(&self) {
        let signal = self
            .lock_state()
            .waiting_queue
            .first_key_value()
            .map(|(_, signal)| Arc::clone(signal));
        if let Some(signal) = signal {
            self.notify_waiting_producer(&signal);
        }
    }

    fn notify_all_waiting_producers(&self) {
        let signals = self
            .lock_state()
            .waiting_queue
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for signal in signals {
            self.notify_waiting_producer(&signal);
        }
    }

    #[cfg(test)]
    fn targeted_waiter_notification_count(&self) -> usize {
        self.targeted_waiter_notifications
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn remove_queued(&self, execution_id: ExecutionId) {
        let removed = {
            let mut state = self.lock_state();
            let removed = state.entries.remove(&execution_id);
            if removed.is_some() {
                state.queue.retain(|id| *id != execution_id);
                state.active.remove(&execution_id);
                if let Some(key) = removed
                    .as_ref()
                    .and_then(|entry| entry.options.ordering_key.as_ref())
                {
                    decrement_ordering_key_refcount(&mut state, key);
                }
            }
            removed
        };
        if removed.is_some() {
            // Dropping the queued start invokes its runner guard outside the
            // lane lock, committing the already-selected cancellation exit.
            // A user future destructor must not unwind through lane cleanup.
            let _ = catch_unwind(AssertUnwindSafe(|| drop(removed)));
            self.capacity_changed.notify_one();
            self.notify_waiting_head();
            self.work_changed.notify_one();
        }
    }

    fn take_startable(&self) -> (Vec<ExecutionStart>, bool) {
        let mut state = self.lock_state();
        let mut starts = Vec::new();
        while state.running < self.config.concurrency {
            let candidate = select_ready_candidate(
                state.dispatch_count,
                state.queue.iter().copied().enumerate().filter_map(
                    |(queue_index, execution_id)| {
                        let entry = state.entries.get(&execution_id)?;
                        let blocked = entry
                            .options
                            .ordering_key
                            .as_ref()
                            .is_some_and(|key| state.active_ordering_keys.contains(key));
                        (!blocked).then_some(ReadyCandidate {
                            queue_index,
                            execution_id,
                            priority: entry.options.priority,
                            sequence: entry.sequence,
                        })
                    },
                ),
            );
            let Some(candidate) = candidate else {
                break;
            };
            let execution_id = candidate.execution_id;
            let Some(next_running) = state.running.checked_add(1) else {
                crate::internal::log_internal_error(
                    "BB-LANE-RUNNING-OVERFLOW",
                    "lane running execution counter overflowed",
                );
                state.closing = true;
                break;
            };
            let Some(removed_id) = state.queue.remove(candidate.queue_index) else {
                crate::internal::log_internal_error(
                    "BB-LANE-QUEUE-INDEX-MISSING",
                    "selected lane queue index disappeared while the lane lock was held",
                );
                state.closing = true;
                break;
            };
            if removed_id != execution_id {
                crate::internal::log_internal_error(
                    "BB-LANE-QUEUE-ID-MISMATCH",
                    "selected lane queue entry changed while the lane lock was held",
                );
                state.closing = true;
                break;
            }
            let Some(entry) = state.entries.remove(&execution_id) else {
                crate::internal::log_internal_error(
                    "BB-LANE-QUEUE-ENTRY-MISSING",
                    "selected lane queue entry had no corresponding execution record",
                );
                state.closing = true;
                break;
            };
            if let Some(key) = entry.options.ordering_key {
                state.active_ordering_keys.insert(key.clone());
                state.running_ordering_keys.insert(execution_id, key);
            }
            state.running = next_running;
            state.dispatch_count = state.dispatch_count.checked_add(1).map_or_else(
                || {
                    crate::internal::log_internal_error(
                        "BB-LANE-DISPATCH-COUNT-OVERFLOW",
                        "lane dispatch counter wrapped to preserve fairness scheduling",
                    );
                    0
                },
                |next| next,
            );
            starts.push(entry.start);
        }
        let should_exit = state.closing && state.active.is_empty();
        drop(state);
        if !starts.is_empty() {
            self.capacity_changed.notify_waiters();
            self.notify_waiting_head();
        }
        (starts, should_exit)
    }

    fn execution_finished(&self, execution_id: ExecutionId) {
        let mut state = self.lock_state();
        if state.active.remove(&execution_id).is_some() {
            if let Some(next) = state.running.checked_sub(1) {
                state.running = next;
            } else {
                crate::internal::log_internal_error(
                    "BB-LANE-RUNNING-UNDERFLOW",
                    "lane running counter underflowed while completing an active execution",
                );
            }
        }
        if let Some(key) = state.running_ordering_keys.remove(&execution_id) {
            state.active_ordering_keys.remove(&key);
            decrement_ordering_key_refcount(&mut state, &key);
        }
        drop(state);
        self.work_changed.notify_one();
    }

    fn request_close(&self) -> Vec<TaskControlHandle> {
        let controls = {
            let mut state = self.lock_state();
            state.closing = true;
            state.active.values().cloned().collect()
        };
        self.work_changed.notify_waiters();
        self.capacity_changed.notify_waiters();
        self.notify_all_waiting_producers();
        controls
    }

    fn stats(&self) -> LaneStats {
        let state = self.lock_state();
        let mut ready_by_priority = [0usize; 8];
        let mut blocked_by_ordering_key = 0usize;
        for entry in state.entries.values() {
            if entry
                .options
                .ordering_key
                .as_ref()
                .is_some_and(|key| state.active_ordering_keys.contains(key))
            {
                if let Some(next) = blocked_by_ordering_key.checked_add(1) {
                    blocked_by_ordering_key = next;
                } else {
                    crate::internal::log_internal_error(
                        "BB-LANE-BLOCKED-STATS-OVERFLOW",
                        "lane blocked-by-key statistics counter overflowed",
                    );
                }
            } else {
                let priority = usize::from(entry.options.priority.value());
                if let Some(count) = ready_by_priority.get_mut(priority) {
                    if let Some(next) = count.checked_add(1) {
                        *count = next;
                    } else {
                        crate::internal::log_internal_error(
                            "BB-LANE-READY-STATS-OVERFLOW",
                            "lane ready-by-priority statistics counter overflowed",
                        );
                    }
                } else {
                    crate::internal::log_internal_error(
                        "BB-LANE-PRIORITY-OUT-OF-RANGE",
                        "lane entry contains an invalid internal priority",
                    );
                }
            }
        }
        let available_queue_capacity = self
            .config
            .capacity
            .checked_sub(state.entries.len())
            .map_or_else(
                || {
                    crate::internal::log_internal_error(
                        "BB-LANE-CAPACITY-INVARIANT",
                        "lane queue contains more live entries than its configured capacity",
                    );
                    0
                },
                |available| available,
            );
        LaneStats {
            queued_live: state.entries.len(),
            running: state.running,
            available_queue_capacity,
            waiting_producers: state.waiting_producers,
            blocked_by_ordering_key,
            ready_by_priority,
        }
    }

    fn dispatcher_stopped(&self) {
        let (queued, running_controls) = {
            let mut state = self.lock_state();
            state.closing = true;
            state.runtime_unavailable = true;
            state.queue.clear();
            let queued = std::mem::take(&mut state.entries);
            let running_controls = state
                .active
                .iter()
                .filter(|(execution_id, _)| !queued.contains_key(execution_id))
                .map(|(_, control)| control.clone())
                .collect::<Vec<_>>();
            state.active.clear();
            state.running = 0;
            state.active_ordering_keys.clear();
            state.running_ordering_keys.clear();
            state.ordering_key_refcounts.clear();
            (queued, running_controls)
        };

        // Queued starts own their RunnerGuard. Dropping them outside the lane
        // lock commits ExecutorStopped even when the dispatcher future was
        // never polled by a runtime that disappeared during admission.
        for entry in queued.into_values() {
            let _ = catch_unwind(AssertUnwindSafe(|| drop(entry)));
        }
        for control in running_controls {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                control.cancel(CancelReason::ExecutorShutdown)
            }));
        }
        self.capacity_changed.notify_waiters();
        self.notify_all_waiting_producers();
        self.work_changed.notify_waiters();
    }
}

struct DispatcherGuard {
    shared: Arc<LaneShared>,
}

impl Drop for DispatcherGuard {
    fn drop(&mut self) {
        self.shared.dispatcher_stopped();
    }
}

async fn run_dispatcher(shared: Arc<LaneShared>) {
    loop {
        let notified = shared.work_changed.notified();
        let (starts, should_exit) = shared.take_startable();
        if should_exit {
            return;
        }
        if starts.is_empty() {
            notified.await;
            continue;
        }

        for start in starts {
            let execution_id = start.execution_id();
            let control = start.control();
            start.start();
            let monitor_shared = Arc::clone(&shared);
            shared.runtime.spawn(async move {
                control.wait().await;
                monitor_shared.execution_finished(execution_id);
            });
        }
    }
}

pub(crate) struct LaneCore {
    shared: Arc<LaneShared>,
    registry: Arc<ExecutionRegistry>,
    admission_open: Arc<Mutex<bool>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
}

impl LaneCore {
    pub(crate) fn new(
        config: LaneConfig,
        runtime: Handle,
        registry: Arc<ExecutionRegistry>,
        admission_open: Arc<Mutex<bool>>,
    ) -> Arc<Self> {
        let shared = Arc::new(LaneShared {
            id: LaneId::new(),
            config,
            runtime: runtime.clone(),
            state: Mutex::new(LaneState {
                closing: false,
                runtime_unavailable: false,
                queue: VecDeque::new(),
                entries: HashMap::new(),
                active: HashMap::new(),
                running: 0,
                waiting_producers: 0,
                waiting_queue: BTreeMap::new(),
                next_waiter_sequence: 0,
                next_sequence: 0,
                dispatch_count: 0,
                active_ordering_keys: HashSet::new(),
                running_ordering_keys: HashMap::new(),
                ordering_key_refcounts: HashMap::new(),
            }),
            work_changed: Notify::new(),
            capacity_changed: Notify::new(),
            max_waiting_producers: registry.limits().max_waiting_producers_per_lane,
            max_ordering_keys: registry.limits().max_ordering_keys_per_lane,
            #[cfg(test)]
            targeted_waiter_notifications: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            suppress_targeted_waiter_notifications: std::sync::atomic::AtomicBool::new(false),
        });
        let dispatcher_shared = Arc::clone(&shared);
        let dispatcher_guard = DispatcherGuard {
            shared: Arc::clone(&shared),
        };
        let dispatcher = runtime.spawn(async move {
            let _guard = dispatcher_guard;
            run_dispatcher(dispatcher_shared).await;
        });
        Arc::new(Self {
            shared,
            registry,
            admission_open,
            dispatcher: Mutex::new(Some(dispatcher)),
        })
    }

    pub(crate) fn config(&self) -> &LaneConfig {
        &self.shared.config
    }

    pub(crate) fn request_close_and_cancel(&self) {
        let controls = self.shared.request_close();
        for control in controls {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                control.cancel(CancelReason::ExecutorShutdown)
            }));
        }
    }

    pub(crate) fn request_close(&self) {
        self.shared.request_close();
    }

    pub(crate) fn take_dispatcher(&self) -> Option<JoinHandle<()>> {
        self.dispatcher
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard)
            .take()
    }

    fn insert_prepared<T, E>(
        &self,
        prepared: PreparedExecution<T, E>,
        options: SpawnOptions,
        waiter_ticket: Option<u64>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let PreparedExecution { handle, start } = prepared;
        let execution_id = start.execution_id();
        let control = handle.control();
        start.set_lane_id(self.shared.id);
        let weak_shared: Weak<LaneShared> = Arc::downgrade(&self.shared);
        start.install_cancel_hook(Box::new(move || {
            if let Some(shared) = weak_shared.upgrade() {
                shared.remove_queued(execution_id);
            }
        }));

        let mut state = self.shared.lock_state();
        if state.runtime_unavailable {
            drop(state);
            drop(start);
            return Err(SpawnError::ExecutorUnavailable);
        }
        if state.closing {
            drop(state);
            drop(start);
            return Err(SpawnError::LaneClosing);
        }
        if state.entries.len() >= self.shared.config.capacity {
            drop(state);
            drop(start);
            return Err(SpawnError::QueueFull);
        }
        match waiter_ticket {
            Some(ticket)
                if state
                    .waiting_queue
                    .first_key_value()
                    .map(|(ticket, _)| ticket)
                    != Some(&ticket) =>
            {
                drop(state);
                drop(start);
                return Err(SpawnError::QueueFull);
            }
            None if !state.waiting_queue.is_empty() => {
                drop(state);
                drop(start);
                return Err(SpawnError::QueueFull);
            }
            _ => {}
        }
        let next_key_refcount = if let Some(key) = options.ordering_key.as_ref() {
            if let Some(count) = state.ordering_key_refcounts.get(key) {
                let Some(next) = count.checked_add(1) else {
                    drop(state);
                    drop(start);
                    return Err(SpawnError::SequenceExhausted);
                };
                Some((key.clone(), next))
            } else if state.ordering_key_refcounts.len() >= self.shared.max_ordering_keys {
                drop(state);
                drop(start);
                return Err(SpawnError::OrderingKeyLimitReached);
            } else {
                Some((key.clone(), 1))
            }
        } else {
            None
        };
        let sequence = state.next_sequence;
        let Some(next_sequence) = state.next_sequence.checked_add(1) else {
            drop(state);
            drop(start);
            return Err(SpawnError::SequenceExhausted);
        };
        state.next_sequence = next_sequence;
        if let Some((key, count)) = next_key_refcount {
            state.ordering_key_refcounts.insert(key, count);
        }
        state.active.insert(execution_id, control.clone());
        state.entries.insert(
            execution_id,
            QueueEntry {
                start,
                options,
                sequence,
            },
        );
        state.queue.push_back(execution_id);
        drop(state);
        if let Err(error) = control.register() {
            self.shared.remove_queued(execution_id);
            return Err(error.into());
        }
        self.shared.work_changed.notify_one();
        Ok(handle)
    }
}

impl Drop for LaneCore {
    fn drop(&mut self) {
        self.request_close_and_cancel();
        if let Some(dispatcher) = self
            .dispatcher
            .get_mut()
            .map_or_else(crate::internal::recover_poison, |dispatcher| dispatcher)
            .take()
        {
            dispatcher.abort();
        }
    }
}

/// A cloneable handle to one immutable lane generation.
#[derive(Clone)]
pub struct Lane {
    pub(crate) core: Arc<LaneCore>,
    pub(crate) _lifetime: Arc<ExecutorLifetime>,
}

impl Lane {
    pub fn id(&self) -> LaneId {
        self.core.shared.id
    }

    pub fn name(&self) -> &str {
        self.core.shared.config.name()
    }

    pub fn config(&self) -> &LaneConfig {
        &self.core.shared.config
    }

    pub fn stats(&self) -> LaneStats {
        self.core.shared.stats()
    }

    pub(crate) fn runtime(&self) -> Handle {
        self.core.shared.runtime.clone()
    }

    pub(crate) fn capacity_change_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.core.shared.capacity_changed.notified()
    }

    #[cfg(test)]
    pub(crate) fn suppress_targeted_waiter_notifications_for_test(&self, suppress: bool) {
        self.core
            .shared
            .suppress_targeted_waiter_notifications
            .store(suppress, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn notify_waiting_head_for_test(&self) {
        self.core.shared.notify_waiting_head();
    }

    fn check_try_admission_for(
        &self,
        waiter_ticket: Option<u64>,
    ) -> Result<std::sync::MutexGuard<'_, bool>, SpawnError> {
        let admission = self
            .core
            .admission_open
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        if !*admission {
            return Err(SpawnError::ExecutorShuttingDown);
        }
        let state = self.core.shared.lock_state();
        if state.runtime_unavailable {
            return Err(SpawnError::ExecutorUnavailable);
        }
        if state.closing {
            return Err(SpawnError::LaneClosing);
        }
        if state.entries.len() >= self.core.shared.config.capacity {
            return Err(SpawnError::QueueFull);
        }
        match waiter_ticket {
            Some(ticket)
                if state
                    .waiting_queue
                    .first_key_value()
                    .map(|(ticket, _)| ticket)
                    != Some(&ticket) =>
            {
                return Err(SpawnError::QueueFull);
            }
            None if !state.waiting_queue.is_empty() => return Err(SpawnError::QueueFull),
            _ => {}
        }
        drop(state);
        Ok(admission)
    }

    fn check_try_admission(&self) -> Result<std::sync::MutexGuard<'_, bool>, SpawnError> {
        self.check_try_admission_for(None)
    }

    pub(crate) fn try_spawn_scoped<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        scope_id: crate::ScopeId,
        generation: u64,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let admission = self.check_try_admission()?;
        let prepared =
            execution::prepare_spec(&self.core.registry, self.core.shared.runtime.clone(), spec)?;
        prepared.start.set_scope(scope_id, generation);
        let result = self
            .core
            .insert_prepared(prepared, SpawnOptions::default(), None);
        drop(admission);
        result
    }

    pub(crate) fn try_spawn_future_scoped<T, E, F>(
        &self,
        future: F,
        scope_id: crate::ScopeId,
        generation: u64,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let admission = self.check_try_admission()?;
        let prepared = execution::prepare_future(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            future,
        )?;
        prepared.start.set_scope(scope_id, generation);
        let result = self
            .core
            .insert_prepared(prepared, SpawnOptions::default(), None);
        drop(admission);
        result
    }

    pub(crate) fn try_spawn_retry_scoped<T, E>(
        &self,
        spec: RetrySpec<T, E>,
        scope_id: crate::ScopeId,
        generation: u64,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let accepted_at = Instant::now();
        let deadline = spec
            .deadline_at(accepted_at)
            .map_err(|()| SpawnError::AdmissionDeadlineExceeded)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(SpawnError::AdmissionDeadlineExceeded);
        }
        let admission = self.check_try_admission()?;
        let prepared = retry::prepare_retry(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            spec,
            accepted_at,
            deadline,
        )?;
        prepared.start.set_scope(scope_id, generation);
        let result = self
            .core
            .insert_prepared(prepared, SpawnOptions::default(), None);
        drop(admission);
        result
    }

    pub(crate) fn try_spawn_recurring_scoped<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
        scope_id: crate::ScopeId,
        generation: u64,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let admission = self.check_try_admission()?;
        let prepared = recurring::prepare_recurring(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            spec,
        )?;
        prepared.start.set_scope(scope_id, generation);
        let result = self
            .core
            .insert_prepared(prepared, SpawnOptions::default(), None);
        drop(admission);
        result
    }

    pub fn try_spawn<T, E>(&self, spec: TaskSpec<T, E>) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        self.try_spawn_with_options(spec, SpawnOptions::default())
    }

    pub fn try_spawn_with_options<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        options: SpawnOptions,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        self.try_spawn_with_options_for(spec, options, None)
    }

    fn try_spawn_with_options_for<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        options: SpawnOptions,
        waiter_ticket: Option<u64>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let admission = self.check_try_admission_for(waiter_ticket)?;
        let prepared =
            execution::prepare_spec(&self.core.registry, self.core.shared.runtime.clone(), spec)?;
        let result = self.core.insert_prepared(prepared, options, waiter_ticket);
        drop(admission);
        result
    }

    pub fn try_spawn_future<T, E, F>(&self, future: F) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.try_spawn_future_with_options(future, SpawnOptions::default())
    }

    pub fn try_spawn_future_with_options<T, E, F>(
        &self,
        future: F,
        options: SpawnOptions,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let admission = self.check_try_admission()?;
        let prepared = execution::prepare_future(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            future,
        )?;
        let result = self.core.insert_prepared(prepared, options, None);
        drop(admission);
        result
    }

    fn try_spawn_retry_at<T, E>(
        &self,
        spec: RetrySpec<T, E>,
        accepted_at: Instant,
        deadline: Option<Instant>,
        waiter_ticket: Option<u64>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(SpawnError::AdmissionDeadlineExceeded);
        }
        let admission = self.check_try_admission_for(waiter_ticket)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(SpawnError::AdmissionDeadlineExceeded);
        }
        let prepared = retry::prepare_retry(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            spec,
            accepted_at,
            deadline,
        )?;
        let result = self
            .core
            .insert_prepared(prepared, SpawnOptions::default(), waiter_ticket);
        drop(admission);
        result
    }

    pub fn try_spawn_retry<T, E>(
        &self,
        spec: RetrySpec<T, E>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let accepted_at = Instant::now();
        let deadline = spec
            .deadline_at(accepted_at)
            .map_err(|()| SpawnError::AdmissionDeadlineExceeded)?;
        self.try_spawn_retry_at(spec, accepted_at, deadline, None)
    }

    pub fn try_spawn_recurring<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        self.try_spawn_recurring_with_options(spec, SpawnOptions::default())
    }

    pub fn try_spawn_recurring_with_options<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
        options: SpawnOptions,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        self.try_spawn_recurring_with_options_for(spec, options, None)
    }

    fn try_spawn_recurring_with_options_for<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
        options: SpawnOptions,
        waiter_ticket: Option<u64>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let admission = self.check_try_admission_for(waiter_ticket)?;
        let prepared = recurring::prepare_recurring(
            &self.core.registry,
            self.core.shared.runtime.clone(),
            spec,
        )?;
        let result = self.core.insert_prepared(prepared, options, waiter_ticket);
        drop(admission);
        result
    }

    pub async fn spawn_recurring<T, E>(
        &self,
        spec: RecurringSpec<T, E>,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let mut waiter = None;
        loop {
            let notification = waiter.as_ref().map(WaitingProducer::notification);
            let ticket = waiter.as_ref().map(WaitingProducer::ticket);
            match self.try_spawn_recurring_with_options_for(
                spec.clone(),
                SpawnOptions::default(),
                ticket,
            ) {
                Ok(handle) => {
                    if let Some(waiter) = waiter.as_mut() {
                        waiter.finish();
                    }
                    return Ok(handle);
                }
                Err(SpawnError::QueueFull) => {
                    if waiter.is_none() {
                        waiter = Some(WaitingProducer::new(&self.core.shared)?);
                        continue;
                    }
                    if let Some(notification) = notification {
                        notification.await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn spawn_retry<T, E>(
        &self,
        spec: RetrySpec<T, E>,
    ) -> impl Future<Output = Result<TaskHandle<T, E>, SpawnError>> + Send + '_
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let accepted_at = Instant::now();
        let deadline = spec.deadline_at(accepted_at);
        async move {
            let deadline = deadline.map_err(|()| SpawnError::AdmissionDeadlineExceeded)?;
            let mut waiter = None;
            loop {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(SpawnError::AdmissionDeadlineExceeded);
                }
                let notification = waiter.as_ref().map(WaitingProducer::notification);
                let ticket = waiter.as_ref().map(WaitingProducer::ticket);
                match self.try_spawn_retry_at(spec.clone(), accepted_at, deadline, ticket) {
                    Ok(handle) => {
                        if let Some(waiter) = waiter.as_mut() {
                            waiter.finish();
                        }
                        return Ok(handle);
                    }
                    Err(SpawnError::QueueFull) => {
                        if waiter.is_none() {
                            waiter = Some(WaitingProducer::new(&self.core.shared)?);
                            continue;
                        }
                        if let Some(notification) = notification {
                            if let Some(deadline) = deadline {
                                let timer = catch_unwind(AssertUnwindSafe(|| {
                                    tokio::time::sleep_until(deadline)
                                }))
                                .map_err(|_| SpawnError::TimerUnavailable)?;
                                tokio::select! {
                                    biased;
                                    _ = timer => {
                                        return Err(SpawnError::AdmissionDeadlineExceeded);
                                    }
                                    _ = notification => {}
                                }
                            } else {
                                notification.await;
                            }
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    pub async fn spawn<T, E>(&self, spec: TaskSpec<T, E>) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        self.spawn_with_options(spec, SpawnOptions::default()).await
    }

    pub async fn spawn_with_options<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        options: SpawnOptions,
    ) -> Result<TaskHandle<T, E>, SpawnError>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let mut waiter = None;
        loop {
            let notification = waiter.as_ref().map(WaitingProducer::notification);
            let ticket = waiter.as_ref().map(WaitingProducer::ticket);
            match self.try_spawn_with_options_for(spec.clone(), options.clone(), ticket) {
                Ok(handle) => {
                    if let Some(waiter) = waiter.as_mut() {
                        waiter.finish();
                    }
                    return Ok(handle);
                }
                Err(SpawnError::QueueFull) => {
                    if waiter.is_none() {
                        waiter = Some(WaitingProducer::new(&self.core.shared)?);
                        continue;
                    }
                    if let Some(notification) = notification {
                        notification.await;
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn spawn_timeout<T, E>(
        &self,
        spec: TaskSpec<T, E>,
        timeout: Duration,
    ) -> impl Future<Output = Result<TaskHandle<T, E>, SpawnError>> + Send + '_
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let deadline = Instant::now().checked_add(timeout);
        async move {
            let Some(deadline) = deadline else {
                return Err(SpawnError::AdmissionTimedOut);
            };
            let timeout = catch_unwind(AssertUnwindSafe(|| {
                tokio::time::timeout_at(deadline, self.spawn(spec))
            }))
            .map_err(|_| SpawnError::TimerUnavailable)?;
            match timeout.await {
                Ok(result) => result,
                Err(_) => Err(SpawnError::AdmissionTimedOut),
            }
        }
    }

    /// Closes admission without cancelling already admitted executions.
    pub fn close(&self) {
        let _admission = self
            .core
            .admission_open
            .lock()
            .map_or_else(crate::internal::recover_poison, |guard| guard);
        self.core.shared.request_close();
    }

    pub async fn close_and_cancel(&self) {
        let controls = {
            let _admission = self
                .core
                .admission_open
                .lock()
                .map_or_else(crate::internal::recover_poison, |guard| guard);
            self.core.shared.request_close()
        };
        for control in &controls {
            control.cancel(CancelReason::LaneClosing);
        }
        for control in controls {
            control.wait().await;
        }
    }

    pub fn cancel_snapshot(&self, reason: CancelReason) -> Vec<ExecutionId> {
        let controls: Vec<_> = self
            .core
            .shared
            .lock_state()
            .active
            .values()
            .cloned()
            .collect();
        controls
            .into_iter()
            .map(|control| {
                let id = control.execution_id();
                control.cancel(reason.clone());
                id
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "lane_tests.rs"]
mod tests;
