use crate::dam::Dam;
use crate::error::{BeaverError, BeaverResult};
use crate::task::Task;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Handle;

/// BusyBeaver: Because sometimes your tasks need to run like a Busy Beaver — tirelessly attempting until they produce the maximum possible success (or hit their busy beaver bound).
///
/// BusyBeaver is a task-scheduling library that supports execution strategies based on counts, cycles, and custom time intervals. It streamlines periodic tasks in your codebase—such as heartbeats, metric reporting, scheduled polling, and automated cleanup—making them simpler and more reliable. At its core, BusyBeaver is an asynchronous task executor with configurable retry strategies, purpose-built for Rust async runtimes like Tokio. Whether a task needs to stop after exactly `N` executions, repeat every `X` milliseconds, or run at specific intervals within a defined time window, BusyBeaver handles the complexity elegantly. Equipped with built-in mechanisms like exponential backoff, retry limits, task listeners, and progress callbacks, it eliminates the need to manually write tedious tokio::time + loop + retry boilerplate in your asynchronous code.
///
/// # Lifecycle
///
/// Always call [`Beaver::destroy`] before letting a `Beaver` go out of scope.
/// Without it, a [`PeriodicBuilder`](crate::PeriodicBuilder) task that has not
/// returned [`WorkResult::Done`](crate::WorkResult::Done) yet may keep running
/// on the underlying runtime even after the `Beaver` is dropped.
///
/// # Creation Methods
///
/// 1. **Within a tokio runtime**:
/// ```ignore
/// #[tokio::main]
/// async fn main() {
///     let beaver = Beaver::new("default", 256);
///     beaver.enqueue(task).await.unwrap();
/// }
/// ```
///
/// 2. **With an external runtime handle** (can be called outside tokio runtime):
/// ```ignore
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
/// rt.block_on(beaver.enqueue(task)).unwrap();
/// ```
///
/// # Async API
///
/// [`enqueue`](Self::enqueue), [`enqueue_on_new_thread`](Self::enqueue_on_new_thread),
/// [`cancel_all`](Self::cancel_all), [`cancel_non_long_resident`](Self::cancel_non_long_resident),
/// [`release_thread_resource_by_name`](Self::release_thread_resource_by_name), and
/// [`destroy`](Self::destroy) are **async** and must be awaited on a Tokio runtime.
pub struct Beaver {
    default: Mutex<Option<Arc<Dam>>>,
    named: Mutex<HashMap<String, NamedEntry>>,
    handle: Option<Handle>,
}

struct NamedEntry {
    dam: Arc<Dam>,
    long_resident: bool,
}

impl Beaver {
    /// Creates a new Beaver instance.
    ///
    /// **Note**: Must be called within a tokio runtime context.
    ///
    /// * `name` - The name of the default thread created, which internally initializes a separate Tokio channel. Calling the enqueue(&self, task: Arc<Task>) method allows you to send tasks to this channel for execution.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///     Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///     The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime context.
    ///
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`tokio::sync::Semaphore::MAX_PERMITS`].
    pub fn new(name: impl Into<String>, buffer: usize) -> Self {
        let default = Mutex::new(Some(Arc::new(Dam::new(name, buffer))));
        Self {
            default,
            named: Mutex::new(HashMap::new()),
            handle: None,
        }
    }

    /// Creates a Beaver instance with a specified tokio runtime handle.
    ///
    /// Can be called outside a tokio runtime context.
    /// * `name` - The name of the default thread created, which internally initializes a separate Tokio channel. Calling the enqueue(&self, task: Arc<Task>) method allows you to send tasks to this channel for execution.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///     Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///     The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`tokio::sync::Semaphore::MAX_PERMITS`].
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let rt = tokio::runtime::Runtime::new().unwrap();
    /// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
    /// rt.block_on(beaver.enqueue(task)).unwrap();
    /// ```
    pub fn new_with_handle(name: impl Into<String>, buffer: usize, handle: Handle) -> Self {
        let default = Mutex::new(Some(Arc::new(Dam::with_handle(
            name,
            buffer,
            handle.clone(),
        ))));

        Self {
            default,
            named: Mutex::new(HashMap::new()),
            handle: Some(handle),
        }
    }

    /// Enqueues a task to be executed on the default execution thread.
    #[inline]
    pub async fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        match default {
            Some(d) => d.enqueue(task).await,
            None => Err(BeaverError::NoDam),
        }
    }

    /// Enqueues a task to a named execution thread; creates it if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `task` - The task to enqueue for execution.
    /// * `name` - The name of the execution thread where the task will run.
    ///   If a thread with this name doesn't exist, a new one will be created.
    /// * `buffer` - The channel buffers up to the provided number of messages.
    ///     Once full, enqueue returns [`BeaverError::QueueFull`] immediately.
    ///     The provided buffer capacity must be at least 1.
    /// * `long_resident` - Whether the task should be "long-resident":
    ///   - `true`: Background task (e.g., heartbeat, periodic sync) that should
    ///     not be cancelled during normal cleanup. The task still follows its
    ///     retry schedule.
    ///   - `false`: Temporary task (e.g., request-related retry) that can be
    ///     cleaned up when the request ends.
    pub async fn enqueue_on_new_thread(
        &self,
        task: Arc<Task>,
        name: impl Into<String>,
        buffer: usize,
        long_resident: bool,
    ) -> BeaverResult<()> {
        let name = name.into();
        let handle = self.handle.clone();
        let dam = {
            let mut named = self.named.lock()?;
            let entry = named.entry(name.clone()).or_insert_with(|| {
                let dam = match handle {
                    Some(h) => Dam::with_handle(&name, buffer, h),
                    None => Dam::new(&name, buffer),
                };
                NamedEntry {
                    dam: Arc::new(dam),
                    long_resident: false,
                }
            });
            entry.long_resident = long_resident;
            Arc::clone(&entry.dam)
        };
        dam.enqueue(task).await
    }

    /// Cancels all pending and running tasks on all execution threads.
    ///
    /// This includes tasks enqueued via [`enqueue`](Self::enqueue) on the default thread
    /// and all named threads.
    pub async fn cancel_all(&self) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        if let Some(d) = default {
            let _ = d.cancel_all().await;
        }
        let named_dams: Vec<Arc<Dam>> = {
            let mut named = self.named.lock()?;
            let dams = named.values().map(|e| Arc::clone(&e.dam)).collect();
            named.clear();
            dams
        };
        for dam in named_dams {
            let _ = dam.cancel_all().await;
        }
        Ok(())
    }

    /// Cancels all non-long-resident tasks.
    ///
    /// Long-resident tasks (e.g., heartbeat, periodic sync) are preserved.
    pub async fn cancel_non_long_resident(&self) -> BeaverResult<()> {
        let default = { self.default.lock()?.as_ref().cloned() };
        if let Some(d) = default {
            let _ = d.cancel_all().await;
        }
        let to_cancel: Vec<Arc<Dam>> = {
            let mut named = self.named.lock()?;
            let keys: Vec<String> = named
                .iter()
                .filter(|(_, e)| !e.long_resident)
                .map(|(k, _)| k.clone())
                .collect();
            let mut dams = Vec::with_capacity(keys.len());
            for k in keys {
                if let Some(e) = named.remove(&k) {
                    dams.push(e.dam);
                }
            }
            dams
        };
        for dam in to_cancel {
            let _ = dam.cancel_all().await;
        }
        Ok(())
    }

    /// Releases a named execution thread and all its resources by name.
    ///
    /// The thread must have been created via [`enqueue_on_new_thread`](Self::enqueue_on_new_thread).
    pub async fn release_thread_resource_by_name(
        &self,
        name: impl Into<String>,
    ) -> BeaverResult<()> {
        let removed = { self.named.lock()?.remove(&name.into()) };
        if let Some(e) = removed {
            let _ = e.dam.release().await;
        }
        Ok(())
    }

    /// Destroys the Beaver instance and all its resources.
    ///
    /// This includes the default execution thread and all named threads, and cancels
    /// **all** tasks (both normal and long-resident). Call this before letting a
    /// Beaver go out of scope so that no background threads or resources keep running
    /// after the Beaver is dropped.
    pub async fn destroy(&self) -> BeaverResult<()> {
        let default = { self.default.lock()?.take() };
        if let Some(d) = default {
            let _ = d.release().await;
        }
        let named_dams: Vec<Arc<Dam>> = {
            let mut named = self.named.lock()?;
            named.drain().map(|(_, v)| v.dam).collect()
        };
        for dam in named_dams {
            let _ = dam.release().await;
        }
        Ok(())
    }
}
