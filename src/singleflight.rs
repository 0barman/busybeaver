use crate::{Job, Scheduler, SubmissionFailure, TaskContext, TaskGroup, TaskHandle, TaskTerminal};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::watch;

#[derive(Clone)]
enum FlightScope {
    Scheduler(Scheduler),
    Group(TaskGroup),
}

impl FlightScope {
    fn runtime(&self) -> Handle {
        match self {
            Self::Scheduler(scheduler) => scheduler.bound_runtime(),
            Self::Group(group) => group.bound_runtime(),
        }
    }

    async fn submit<T, E>(&self, job: Job<T, E>) -> Result<TaskHandle<T, E>, SubmissionFailure>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        match self {
            Self::Scheduler(scheduler) => {
                scheduler.submit(job).await.map_err(|error| error.reason())
            }
            Self::Group(group) => group.submit(job).await.map_err(|error| error.reason()),
        }
    }
}

struct FlightEntry<T, E> {
    result: watch::Sender<Option<Arc<TaskTerminal<T, E>>>>,
}

impl<T, E> FlightEntry<T, E> {
    fn new() -> Self {
        let (result, _) = watch::channel(None);
        Self { result }
    }

    async fn wait(&self) -> Arc<TaskTerminal<T, E>> {
        let mut result = self.result.subscribe();
        loop {
            if let Some(terminal) = result.borrow_and_update().clone() {
                return terminal;
            }
            if result.changed().await.is_err() {
                return Arc::new(TaskTerminal::ExecutorStopped);
            }
        }
    }
}

struct SingleflightInner<K, T, E> {
    scope: FlightScope,
    entries: Mutex<HashMap<K, Arc<FlightEntry<T, E>>>>,
}

struct FlightCompletion<K, T, E>
where
    K: Eq + Hash,
{
    inner: Arc<SingleflightInner<K, T, E>>,
    key: Option<K>,
    entry: Arc<FlightEntry<T, E>>,
}

impl<K, T, E> FlightCompletion<K, T, E>
where
    K: Eq + Hash,
{
    fn new(inner: Arc<SingleflightInner<K, T, E>>, key: K, entry: Arc<FlightEntry<T, E>>) -> Self {
        Self {
            inner,
            key: Some(key),
            entry,
        }
    }

    fn finish(mut self, terminal: TaskTerminal<T, E>) {
        self.publish(Arc::new(terminal));
    }

    fn publish(&mut self, terminal: Arc<TaskTerminal<T, E>>) {
        let Some(key) = self.key.take() else {
            return;
        };
        let mut entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
        {
            entries.remove(&key);
        }
        drop(entries);
        self.entry.result.send_replace(Some(terminal));
    }
}

impl<K, T, E> Drop for FlightCompletion<K, T, E>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        self.publish(Arc::new(TaskTerminal::ExecutorStopped));
    }
}

/// A typed, per-scope singleflight registry.
///
/// Concurrent calls with the same key share one Scheduler task and one
/// `Arc<TaskTerminal<T, E>>`. Results are not cached: the key is removed as
/// soon as that execution publishes its terminal. Dropping any or all waiter
/// futures does not cancel the leader; scope cancellation still does.
pub struct Singleflight<K, T, E> {
    inner: Arc<SingleflightInner<K, T, E>>,
}

impl<K, T, E> Clone for Singleflight<K, T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, T, E> Singleflight<K, T, E>
where
    K: Clone + Eq + Hash + Send + 'static,
    T: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    fn new(scope: FlightScope) -> Self {
        Self {
            inner: Arc::new(SingleflightInner {
                scope,
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Joins the current execution for `key` or atomically starts one.
    ///
    /// A follower's `operation` is dropped without being invoked. Submission
    /// failure is represented as [`TaskTerminal::SubmissionFailed`], since
    /// this high-level API consumes the operation rather than returning a Job.
    pub async fn run<F, Fut>(&self, key: K, operation: F) -> Arc<TaskTerminal<T, E>>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        let (entry, leader) = {
            let mut entries = self
                .inner
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match entries.get(&key) {
                Some(entry) => (Arc::clone(entry), false),
                None => {
                    let entry = Arc::new(FlightEntry::new());
                    entries.insert(key.clone(), Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if leader {
            let inner = Arc::clone(&self.inner);
            let leader_entry = Arc::clone(&entry);
            let scope = inner.scope.clone();
            let runtime = scope.runtime();
            let completion = FlightCompletion::new(inner, key, leader_entry);
            runtime.spawn(async move {
                let terminal = match scope.submit(Job::once(operation)).await {
                    Ok(handle) => handle.join().await,
                    Err(reason) => TaskTerminal::SubmissionFailed { reason },
                };
                completion.finish(terminal);
            });
        } else {
            drop(operation);
        }

        entry.wait().await
    }

    /// Returns the number of keys whose leader has not yet published a
    /// terminal result.
    pub fn in_flight_count(&self) -> usize {
        self.inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Scheduler {
    /// Creates an uncached typed singleflight registry scoped to this
    /// scheduler.
    pub fn singleflight<K, T, E>(&self) -> Singleflight<K, T, E>
    where
        K: Clone + Eq + Hash + Send + 'static,
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
    {
        Singleflight::new(FlightScope::Scheduler(self.clone()))
    }
}

impl TaskGroup {
    /// Creates an uncached typed singleflight registry whose leaders belong to
    /// this task group.
    pub fn singleflight<K, T, E>(&self) -> Singleflight<K, T, E>
    where
        K: Clone + Eq + Hash + Send + 'static,
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
    {
        Singleflight::new(FlightScope::Group(self.clone()))
    }
}
