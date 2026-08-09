use crate::{EvictionPolicy, Job, Scheduler, SubmissionFailure, TaskTerminal};
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{oneshot, Notify};

const MAX_INITIAL_PENDING_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects how a full dispatch queue handles a newly submitted job.
pub enum QueueOverflowPolicy {
    /// Rejects the incoming job and leaves all pending jobs unchanged.
    RejectNewest,
    /// Evicts the pending job with the oldest insertion ticket.
    EvictOldest,
    /// Evicts the oldest pending job at the lowest priority, but only when
    /// the incoming job has a strictly higher priority.
    EvictLowestPriority,
    /// Replaces a pending job with the same key; distinct keys are rejected
    /// when the queue is full.
    CoalesceByKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Validated capacity, concurrency, overflow, and aging settings for a
/// [`DispatchQueue`].
pub struct DispatchQueueConfig {
    pending_capacity: usize,
    concurrency: usize,
    overflow: QueueOverflowPolicy,
    key_concurrency: Option<NonZeroUsize>,
    aging_interval: NonZeroUsize,
}

impl DispatchQueueConfig {
    /// Largest supported number of pending jobs in one dispatch queue.
    pub const MAX_PENDING_CAPACITY: usize = 1_048_576;

    /// Creates a configuration with reject-newest overflow and no per-key
    /// concurrency limit.
    pub fn new(
        pending_capacity: usize,
        concurrency: usize,
    ) -> Result<Self, DispatchQueueConfigError> {
        if pending_capacity == 0 {
            return Err(DispatchQueueConfigError::ZeroPendingCapacity);
        }
        if pending_capacity > Self::MAX_PENDING_CAPACITY {
            return Err(DispatchQueueConfigError::PendingCapacityTooLarge {
                capacity: pending_capacity,
                maximum: Self::MAX_PENDING_CAPACITY,
            });
        }
        if concurrency == 0 {
            return Err(DispatchQueueConfigError::ZeroConcurrency);
        }
        Ok(Self {
            pending_capacity,
            concurrency,
            overflow: QueueOverflowPolicy::RejectNewest,
            key_concurrency: None,
            aging_interval: NonZeroUsize::MIN,
        })
    }

    /// Sets the full-queue policy.
    pub fn overflow(mut self, overflow: QueueOverflowPolicy) -> Self {
        self.overflow = overflow;
        self
    }

    /// Limits concurrently dispatched jobs sharing one key.
    pub fn key_concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.key_concurrency = Some(concurrency);
        self
    }

    /// Sets the number of dispatch decisions required for one priority point
    /// of aging. The default is one.
    pub fn aging_interval(mut self, interval: NonZeroUsize) -> Self {
        self.aging_interval = interval;
        self
    }

    /// Returns the maximum number of jobs that may remain pending.
    pub fn pending_capacity(self) -> usize {
        self.pending_capacity
    }

    /// Returns the queue-wide dispatch concurrency limit.
    pub fn concurrency(self) -> usize {
        self.concurrency
    }

    /// Returns the configured overflow policy.
    pub fn overflow_policy(self) -> QueueOverflowPolicy {
        self.overflow
    }

    /// Returns the optional per-key concurrency limit.
    pub fn key_concurrency_limit(self) -> Option<NonZeroUsize> {
        self.key_concurrency
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Explains why a dispatch queue configuration is invalid.
pub enum DispatchQueueConfigError {
    /// The pending capacity was zero.
    ZeroPendingCapacity,
    /// The pending capacity exceeded the defensive queue limit.
    PendingCapacityTooLarge {
        /// Rejected pending capacity.
        capacity: usize,
        /// Largest supported pending capacity.
        maximum: usize,
    },
    /// The dispatch concurrency was zero.
    ZeroConcurrency,
}

impl DispatchQueueConfigError {
    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::ZeroPendingCapacity => "BB-DISPATCH-CONFIG-001",
            Self::PendingCapacityTooLarge { .. } => "BB-DISPATCH-CONFIG-002",
            Self::ZeroConcurrency => "BB-DISPATCH-CONFIG-003",
        }
    }
}

impl fmt::Display for DispatchQueueConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPendingCapacity => {
                f.write_str("dispatch pending capacity must be greater than zero")
            }
            Self::PendingCapacityTooLarge { capacity, maximum } => write!(
                f,
                "dispatch pending capacity {capacity} exceeds maximum {maximum}"
            ),
            Self::ZeroConcurrency => f.write_str("dispatch concurrency must be greater than zero"),
        }
    }
}

impl std::error::Error for DispatchQueueConfigError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Per-submission priority and optional key used by [`DispatchQueue`].
pub struct DispatchOptions {
    priority: u8,
    key: Option<Arc<str>>,
}

impl DispatchOptions {
    /// Sets the base priority; larger values are dispatched first.
    pub fn priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    /// Associates a stable key used for coalescing and per-key concurrency.
    pub fn with_key(mut self, key: impl Into<Arc<str>>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Returns the base priority.
    pub fn priority_value(&self) -> u8 {
        self.priority
    }

    /// Returns the dispatch key, if configured.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

#[non_exhaustive]
/// A non-blocking dispatch submission failure that returns ownership of the
/// original job.
pub enum DispatchSubmitError<J> {
    /// The dispatch queue no longer accepts work.
    Closed(J),
    /// The pending queue is full and no configured eviction was possible.
    Full(J),
    /// Coalesce-by-key was selected but this submission has no key.
    KeyRequired(J),
    /// The monotonic insertion ticket space was exhausted.
    SequenceExhausted(J),
    /// Internal queue state contradicted the validated configuration.
    InvariantViolation(J),
}

impl<J> DispatchSubmitError<J> {
    /// Recovers the job rejected by the dispatch queue.
    pub fn into_job(self) -> J {
        match self {
            Self::Closed(job)
            | Self::Full(job)
            | Self::KeyRequired(job)
            | Self::SequenceExhausted(job)
            | Self::InvariantViolation(job) => job,
        }
    }

    /// Returns a stable, payload-free code suitable for logs and metrics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Closed(_) => "BB-DISPATCH-001",
            Self::Full(_) => "BB-DISPATCH-002",
            Self::KeyRequired(_) => "BB-DISPATCH-003",
            Self::SequenceExhausted(_) => "BB-DISPATCH-004",
            Self::InvariantViolation(_) => "BB-DISPATCH-006",
        }
    }
}

impl<J> fmt::Debug for DispatchSubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => f.write_str("Closed(..)"),
            Self::Full(_) => f.write_str("Full(..)"),
            Self::KeyRequired(_) => f.write_str("KeyRequired(..)"),
            Self::SequenceExhausted(_) => f.write_str("SequenceExhausted(..)"),
            Self::InvariantViolation(_) => f.write_str("InvariantViolation(..)"),
        }
    }
}

impl<J> fmt::Display for DispatchSubmitError<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => f.write_str("dispatch queue is closed"),
            Self::Full(_) => f.write_str("dispatch pending queue is full"),
            Self::KeyRequired(_) => f.write_str("coalesce-by-key requires a dispatch key"),
            Self::SequenceExhausted(_) => f.write_str("dispatch sequence space is exhausted"),
            Self::InvariantViolation(_) => {
                f.write_str("dispatch queue internal state is inconsistent")
            }
        }
    }
}

impl<J> std::error::Error for DispatchSubmitError<J> {}

fn dispatch_invariant<J>(
    job: J,
    overflow: QueueOverflowPolicy,
    pending: usize,
    capacity: usize,
) -> DispatchSubmitError<J> {
    crate::diagnostic::error(
        "BB-DISPATCH-006",
        "dispatch",
        format_args!(
            "full queue has no eviction candidate overflow={overflow:?} pending={pending} capacity={capacity}"
        ),
    );
    DispatchSubmitError::InvariantViolation(job)
}

/// Owns the single terminal result of a job submitted through a
/// [`DispatchQueue`].
pub struct DispatchTaskHandle<T, E> {
    result: oneshot::Receiver<TaskTerminal<T, E>>,
}

impl<T, E> DispatchTaskHandle<T, E> {
    /// Waits for the dispatched job's terminal result.
    ///
    /// If the bound runtime stops before publication, the result is
    /// [`TaskTerminal::ExecutorStopped`].
    pub async fn join(self) -> TaskTerminal<T, E> {
        self.result.await.unwrap_or(TaskTerminal::ExecutorStopped)
    }
}

struct Pending<T, E> {
    job: Job<T, E>,
    options: DispatchOptions,
    ticket: u64,
    enqueued_epoch: u64,
    result: oneshot::Sender<TaskTerminal<T, E>>,
}

struct DispatchState<T, E> {
    pending: Vec<Pending<T, E>>,
    active: usize,
    active_by_key: HashMap<Arc<str>, usize>,
    epoch: u64,
    next_ticket: u64,
    closed: bool,
}

struct DispatchInner<T, E> {
    scheduler: Scheduler,
    config: DispatchQueueConfig,
    state: Mutex<DispatchState<T, E>>,
    changed: Arc<Notify>,
}

struct DispatchLoopGuard<T, E> {
    inner: Weak<DispatchInner<T, E>>,
}

impl<T, E> Drop for DispatchLoopGuard<T, E> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.close();
        }
    }
}

struct DispatchCompletion<T, E> {
    inner: Arc<DispatchInner<T, E>>,
    key: Option<Arc<str>>,
    result: Option<oneshot::Sender<TaskTerminal<T, E>>>,
}

impl<T, E> DispatchCompletion<T, E> {
    fn new(
        inner: Arc<DispatchInner<T, E>>,
        key: Option<Arc<str>>,
        result: oneshot::Sender<TaskTerminal<T, E>>,
    ) -> Self {
        Self {
            inner,
            key,
            result: Some(result),
        }
    }

    fn finish(mut self, terminal: TaskTerminal<T, E>) {
        self.publish(terminal);
    }

    fn publish(&mut self, terminal: TaskTerminal<T, E>) {
        let Some(result) = self.result.take() else {
            return;
        };
        self.inner.complete(self.key.take());
        let _ = result.send(terminal);
    }
}

impl<T, E> Drop for DispatchCompletion<T, E> {
    fn drop(&mut self) {
        self.publish(TaskTerminal::ExecutorStopped);
    }
}

fn select_pending_index<T, E>(
    state: &DispatchState<T, E>,
    config: DispatchQueueConfig,
) -> Option<usize> {
    let epoch = state.epoch;
    let aging_interval = config.aging_interval.get() as u64;
    let key_limit = config.key_concurrency.map(NonZeroUsize::get);
    state
        .pending
        .iter()
        .enumerate()
        .filter(|(_, pending)| {
            pending.options.key.as_ref().is_none_or(|key| {
                key_limit
                    .is_none_or(|limit| state.active_by_key.get(key).copied().unwrap_or(0) < limit)
            })
        })
        .max_by(|(_, left), (_, right)| {
            let left_age = epoch.saturating_sub(left.enqueued_epoch) / aging_interval;
            let right_age = epoch.saturating_sub(right.enqueued_epoch) / aging_interval;
            let left_priority = u64::from(left.options.priority).saturating_add(left_age);
            let right_priority = u64::from(right.options.priority).saturating_add(right_age);
            left_priority
                .cmp(&right_priority)
                .then_with(|| right.ticket.cmp(&left.ticket))
        })
        .map(|(index, _)| index)
}

impl<T, E> DispatchInner<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    fn start(inner: &Arc<Self>) {
        let weak = Arc::downgrade(inner);
        let runtime = inner.scheduler.bound_runtime();
        let runtime_guard = DispatchLoopGuard {
            inner: weak.clone(),
        };
        runtime.spawn(async move {
            let _runtime_guard = runtime_guard;
            Self::dispatch_loop(weak).await;
        });
    }

    async fn dispatch_loop(weak: Weak<Self>) {
        loop {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let changed = Arc::clone(&inner.changed);
            let notified = changed.notified();
            if let Some(pending) = inner.take_next() {
                inner.start_pending(pending);
                continue;
            }
            let closed_and_idle = {
                let state = inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.closed && state.active == 0
            };
            drop(inner);
            if closed_and_idle {
                return;
            }
            notified.await;
        }
    }

    fn take_next(&self) -> Option<Pending<T, E>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.active >= self.config.concurrency {
            return None;
        }
        let selected = select_pending_index(&state, self.config)?;
        let pending = state.pending.remove(selected);
        state.active += 1;
        state.epoch = state.epoch.saturating_add(1);
        if let Some(key) = &pending.options.key {
            *state.active_by_key.entry(Arc::clone(key)).or_insert(0) += 1;
        }
        Some(pending)
    }

    fn start_pending(self: &Arc<Self>, pending: Pending<T, E>) {
        let inner = Arc::clone(self);
        let runtime = self.scheduler.bound_runtime();
        let Pending {
            job,
            options,
            result,
            ..
        } = pending;
        let scheduler = inner.scheduler.clone();
        let completion = DispatchCompletion::new(inner, options.key, result);
        runtime.spawn(async move {
            let terminal = match scheduler.submit(job).await {
                Ok(handle) => handle.join().await,
                Err(error) => TaskTerminal::SubmissionFailed {
                    reason: error.reason(),
                },
            };
            completion.finish(terminal);
        });
    }
}

impl<T, E> DispatchInner<T, E> {
    fn complete(&self, key: Option<Arc<str>>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        if let Some(key) = key {
            if let Some(active) = state.active_by_key.get_mut(&key) {
                *active -= 1;
                if *active == 0 {
                    state.active_by_key.remove(&key);
                }
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn close(&self) -> usize {
        self.close_with(|| TaskTerminal::ExecutorStopped)
    }

    fn close_for_submission_failure(&self, reason: SubmissionFailure) -> usize {
        self.close_with(|| TaskTerminal::SubmissionFailed { reason })
    }

    fn close_with(&self, mut terminal: impl FnMut() -> TaskTerminal<T, E>) -> usize {
        let pending = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return 0;
            }
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        let count = pending.len();
        for pending in pending {
            let _ = pending.result.send(terminal());
        }
        self.changed.notify_waiters();
        count
    }
}

/// An opt-in bounded priority/eviction layer in front of a [`Scheduler`].
///
/// Only pending jobs are eligible for eviction. Once dispatched, a job is
/// owned by Scheduler and can stop only through its normal terminal protocol.
pub struct DispatchQueue<T, E> {
    inner: Arc<DispatchInner<T, E>>,
    owners: Arc<()>,
}

impl<T, E> Clone for DispatchQueue<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            owners: Arc::clone(&self.owners),
        }
    }
}

impl<T, E> DispatchQueue<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    fn new(scheduler: Scheduler, config: DispatchQueueConfig) -> Self {
        let inner = Arc::new(DispatchInner {
            scheduler,
            config,
            state: Mutex::new(DispatchState {
                pending: Vec::with_capacity(
                    config.pending_capacity.min(MAX_INITIAL_PENDING_CAPACITY),
                ),
                active: 0,
                active_by_key: HashMap::new(),
                epoch: 0,
                next_ticket: 0,
                closed: false,
            }),
            changed: Arc::new(Notify::new()),
        });
        DispatchInner::start(&inner);
        Self {
            inner,
            owners: Arc::new(()),
        }
    }

    /// Adds a job without waiting for pending capacity.
    ///
    /// The returned handle also resolves when a pending entry is evicted or
    /// when the queue/runtime closes. If Scheduler rejects the job after this
    /// queue has returned a handle, the handle resolves as
    /// [`TaskTerminal::SubmissionFailed`] with the original rejection reason.
    /// Once Scheduler shutdown has started, this method instead returns
    /// [`DispatchSubmitError::Closed`] synchronously.
    pub fn submit(
        &self,
        job: Job<T, E>,
        options: DispatchOptions,
    ) -> Result<DispatchTaskHandle<T, E>, DispatchSubmitError<Job<T, E>>> {
        let submission = self.inner.scheduler.lock_submission_gate();
        if let Some(reason) = self.inner.scheduler.submission_failure() {
            drop(submission);
            self.inner.close_for_submission_failure(reason);
            return Err(DispatchSubmitError::Closed(job));
        }
        let mut evicted = None;
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(DispatchSubmitError::Closed(job));
        }
        if self.inner.config.overflow == QueueOverflowPolicy::CoalesceByKey {
            let Some(key) = options.key.as_ref() else {
                return Err(DispatchSubmitError::KeyRequired(job));
            };
            if let Some(index) = state
                .pending
                .iter()
                .position(|pending| pending.options.key.as_ref() == Some(key))
            {
                evicted = Some((state.pending.remove(index), EvictionPolicy::Coalesced));
            } else if state.pending.len() >= self.inner.config.pending_capacity {
                return Err(DispatchSubmitError::Full(job));
            }
        } else if state.pending.len() >= self.inner.config.pending_capacity {
            match self.inner.config.overflow {
                QueueOverflowPolicy::RejectNewest => {
                    return Err(DispatchSubmitError::Full(job));
                }
                QueueOverflowPolicy::EvictOldest => {
                    let Some(index) = state
                        .pending
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, pending)| pending.ticket)
                        .map(|(index, _)| index)
                    else {
                        return Err(dispatch_invariant(
                            job,
                            self.inner.config.overflow,
                            state.pending.len(),
                            self.inner.config.pending_capacity,
                        ));
                    };
                    evicted = Some((state.pending.remove(index), EvictionPolicy::Oldest));
                }
                QueueOverflowPolicy::EvictLowestPriority => {
                    let Some((index, lowest)) =
                        state
                            .pending
                            .iter()
                            .enumerate()
                            .min_by(|(_, left), (_, right)| {
                                left.options
                                    .priority
                                    .cmp(&right.options.priority)
                                    .then_with(|| left.ticket.cmp(&right.ticket))
                            })
                    else {
                        return Err(dispatch_invariant(
                            job,
                            self.inner.config.overflow,
                            state.pending.len(),
                            self.inner.config.pending_capacity,
                        ));
                    };
                    if options.priority <= lowest.options.priority {
                        return Err(DispatchSubmitError::Full(job));
                    }
                    evicted = Some((state.pending.remove(index), EvictionPolicy::LowestPriority));
                }
                QueueOverflowPolicy::CoalesceByKey => unreachable!("handled above"),
            }
        }
        let Some(ticket) = state.next_ticket.checked_add(1) else {
            if let Some((pending, _)) = evicted {
                state.pending.push(pending);
            }
            return Err(DispatchSubmitError::SequenceExhausted(job));
        };
        state.next_ticket = ticket;
        let (result, receiver) = oneshot::channel();
        let enqueued_epoch = state.epoch;
        state.pending.push(Pending {
            job,
            options,
            ticket,
            enqueued_epoch,
            result,
        });
        drop(state);
        drop(submission);
        if let Some((pending, policy)) = evicted {
            let _ = pending.result.send(TaskTerminal::Evicted { policy });
        }
        self.inner.changed.notify_waiters();
        Ok(DispatchTaskHandle { result: receiver })
    }

    /// Returns the number of jobs not yet handed to the scheduler.
    pub fn pending_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .len()
    }

    /// Returns the number of jobs dispatched by this queue and not yet
    /// terminal.
    pub fn active_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }

    /// Stops accepting work and resolves every pending handle as
    /// `ExecutorStopped`. Already-dispatched Scheduler tasks are not aborted.
    pub fn close(&self) -> usize {
        self.inner.close()
    }
}

impl<T, E> Drop for DispatchQueue<T, E> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.owners) == 1 {
            let _ = self.inner.close();
        }
    }
}

impl Scheduler {
    /// Creates an opt-in bounded priority dispatch layer bound to this
    /// scheduler and its runtime.
    pub fn dispatch_queue<T, E>(&self, config: DispatchQueueConfig) -> DispatchQueue<T, E>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        DispatchQueue::new(self.clone(), config)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        select_pending_index, DispatchInner, DispatchOptions, DispatchQueue, DispatchQueueConfig,
        DispatchState, DispatchSubmitError, Pending, QueueOverflowPolicy,
        MAX_INITIAL_PENDING_CAPACITY,
    };
    use crate::{Job, Scheduler, SubmissionFailure, TaskTerminal};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;
    use tokio::sync::Notify;

    fn pending(priority: u8, ticket: u64, enqueued_epoch: u64) -> Pending<(), ()> {
        let (result, _) = oneshot::channel();
        Pending {
            job: Job::once(|_| async { Ok(()) }),
            options: DispatchOptions::default().priority(priority),
            ticket,
            enqueued_epoch,
            result,
        }
    }

    #[tokio::test]
    async fn empty_full_queue_invariant_returns_job_and_logs_for_both_eviction_policies() {
        crate::test_log::init();
        for overflow in [
            QueueOverflowPolicy::EvictOldest,
            QueueOverflowPolicy::EvictLowestPriority,
        ] {
            let scheduler = Scheduler::builder().build().unwrap();
            let invalid_config = DispatchQueueConfig {
                pending_capacity: 0,
                concurrency: 1,
                overflow,
                key_concurrency: None,
                aging_interval: std::num::NonZeroUsize::MIN,
            };
            let queue: DispatchQueue<(), ()> = scheduler.dispatch_queue(invalid_config);
            let result = queue.submit(
                Job::once(|_| async { Ok::<_, ()>(()) }),
                DispatchOptions::default().priority(1),
            );
            let Err(error) = result else {
                std::panic::panic_any("invalid queue state unexpectedly accepted a job");
            };
            assert_eq!(error.code(), "BB-DISPATCH-006");
            assert!(matches!(error, DispatchSubmitError::InvariantViolation(_)));
            assert_eq!(queue.pending_count(), 0);
            drop(queue);
            assert!(scheduler.shutdown().await.is_complete());
        }
        assert!(crate::test_log::contains("BB-DISPATCH-006"));
    }

    #[test]
    fn aging_eventually_beats_a_stream_of_newer_high_priority_work() {
        let config = DispatchQueueConfig::new(16, 1).unwrap();
        let mut state = DispatchState {
            pending: vec![pending(0, 1, 0)],
            active: 0,
            active_by_key: HashMap::new(),
            epoch: 0,
            next_ticket: 1,
            closed: false,
        };
        for ticket in 2..=5 {
            state.epoch += 1;
            state.pending.push(pending(3, ticket, state.epoch));
            let index = select_pending_index(&state, config).unwrap();
            let selected = state.pending.remove(index);
            if selected.ticket == 1 {
                assert!(ticket <= 5);
                return;
            }
        }
        panic!("aged low-priority work was starved");
    }

    #[tokio::test]
    async fn maximum_pending_capacity_uses_bounded_initial_allocation() {
        let scheduler = Scheduler::builder().build().unwrap();
        let config = DispatchQueueConfig::new(DispatchQueueConfig::MAX_PENDING_CAPACITY, 1)
            .expect("the documented upper boundary must remain valid");
        let queue: DispatchQueue<(), ()> = scheduler.dispatch_queue(config);

        let allocated = queue
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .capacity();

        assert_eq!(allocated, MAX_INITIAL_PENDING_CAPACITY);
        drop(queue);
        assert!(scheduler.shutdown().await.is_complete());
    }

    #[tokio::test]
    async fn exhausted_sequence_does_not_mutate_the_queue() {
        let scheduler = Scheduler::builder().build().unwrap();
        let config = DispatchQueueConfig::new(1, 1).unwrap();
        let inner = Arc::new(DispatchInner {
            scheduler,
            config,
            state: Mutex::new(DispatchState {
                pending: Vec::new(),
                active: 0,
                active_by_key: HashMap::new(),
                epoch: 0,
                next_ticket: u64::MAX,
                closed: false,
            }),
            changed: Arc::new(Notify::new()),
        });
        let queue = DispatchQueue {
            inner,
            owners: Arc::new(()),
        };

        let result = queue.submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default(),
        );
        assert!(matches!(
            result,
            Err(DispatchSubmitError::SequenceExhausted(_))
        ));
        assert_eq!(queue.pending_count(), 0);
    }

    #[tokio::test]
    async fn scheduler_shutdown_closes_existing_pending_with_typed_failure() {
        let scheduler = Scheduler::builder().build().unwrap();
        let config = DispatchQueueConfig::new(2, 1).unwrap();
        let inner = Arc::new(DispatchInner {
            scheduler: scheduler.clone(),
            config,
            state: Mutex::new(DispatchState {
                pending: Vec::new(),
                active: 0,
                active_by_key: HashMap::new(),
                epoch: 0,
                next_ticket: 0,
                closed: false,
            }),
            changed: Arc::new(Notify::new()),
        });
        let queue = DispatchQueue {
            inner,
            owners: Arc::new(()),
        };
        let pending = queue
            .submit(
                Job::once(|_| async { Ok::<_, ()>(()) }),
                DispatchOptions::default(),
            )
            .unwrap();
        assert_eq!(queue.pending_count(), 1);
        assert!(scheduler.shutdown().await.is_complete());

        assert!(matches!(
            queue.submit(
                Job::once(|_| async { Ok::<_, ()>(()) }),
                DispatchOptions::default(),
            ),
            Err(DispatchSubmitError::Closed(_))
        ));
        assert!(matches!(
            pending.join().await,
            TaskTerminal::SubmissionFailed {
                reason: SubmissionFailure::Closed
            }
        ));
    }
}
