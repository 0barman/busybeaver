use crate::dam::Dam;
use crate::error::{BeaverError, BeaverResult};
use crate::task::Task;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;

/// BusyBeaver: Because sometimes your tasks need to run like a Busy Beaver — tirelessly attempting until they produce the maximum possible success (or hit their busy beaver bound).
/// This Beaver executes your Futures independently of your worker threads, supporting scheduling strategies such as time intervals, execution count intervals, specific-time polling policies, and more.
///
/// # Creation Methods
///
/// 1. **Within a tokio runtime**:
/// ```ignore
/// #[tokio::main]
/// async fn main() {
///     let beaver = Beaver::new("default", 256);
/// }
/// ```
///
/// 2. **With an external runtime handle** (can be called outside tokio runtime):
/// ```ignore
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
/// ```
pub struct Beaver {
    default: Mutex<Option<Arc<Dam>>>,
    named: Mutex<HashMap<String, NamedEntry>>,
    handle: Option<Handle>,
}

impl Beaver {
    /// Creates a new Beaver instance.
    ///
    /// **Note**: Must be called within a tokio runtime context.
    ///
    /// * `name` - The name of the default thread created, which internally initializes a separate Tokio channel. Calling the enqueue(&self, task: Arc<Task>) method allows you to send tasks to this channel for execution.
    /// * `buffer` - The channel will buffer up to the provided number of messages.  Once the
    ///     buffer is full, attempts to send new messages will wait until a message is
    ///     received from the channel. The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime context.
    ///
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`Semaphore::MAX_PERMITS`].
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
    /// * `buffer` - The channel will buffer up to the provided number of messages.  Once the
    ///     buffer is full, attempts to send new messages will wait until a message is
    ///     received from the channel. The provided buffer capacity must be at least 1.
    ///
    /// # Panics
    /// Panics if the buffer capacity is 0, or too large. Currently the maximum
    /// capacity is [`Semaphore::MAX_PERMITS`].
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let rt = tokio::runtime::Runtime::new().unwrap();
    /// let beaver = Beaver::new_with_handle("default", 256, rt.handle().clone());
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
    pub fn enqueue(&self, task: Arc<Task>) -> BeaverResult<()> {
        let guard = self.default.lock()?;
        match guard.as_ref() {
            Some(d) => d.enqueue(task),
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
    /// * `buffer` - The channel will buffer up to the provided number of messages.  Once the
    ///     buffer is full, attempts to send new messages will wait until a message is
    ///     received from the channel. The provided buffer capacity must be at least 1.
    /// * `long_resident` - Whether the task should be "long-resident":
    ///   - `true`: Background task (e.g., heartbeat, periodic sync) that should
    ///     not be cancelled during normal cleanup. The task still follows its
    ///     retry schedule.
    ///   - `false`: Temporary task (e.g., request-related retry) that can be
    ///     cleaned up when the request ends.
    pub fn enqueue_on_new_thread(
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
        dam.enqueue(task)
    }

    /// Cancels all pending and running tasks on all execution threads.
    ///
    /// This includes tasks enqueued via [`enqueue`](Self::enqueue) on the default thread
    /// and all named threads.
    pub fn cancel_all(&self) -> BeaverResult<()> {
        if let Some(ref d) = *self.default.lock()? {
            let _ = d.cancel_all();
        }
        let mut named = self.named.lock()?;
        for e in named.values() {
            let _ = e.dam.cancel_all();
        }
        named.clear();
        Ok(())
    }

    /// Cancels all non-long-resident tasks.
    ///
    /// Long-resident tasks (e.g., heartbeat, periodic sync) are preserved.
    pub fn cancel_non_long_resident(&self) -> BeaverResult<()> {
        if let Some(ref d) = *self.default.lock()? {
            let _ = d.cancel_all();
        }
        let mut named = self.named.lock()?;
        named.retain(|_, e| {
            if e.long_resident {
                true
            } else {
                let _ = e.dam.cancel_all();
                false
            }
        });
        Ok(())
    }

    /// Releases a named execution thread and all its resources by name.
    ///
    /// The thread must have been created via [`enqueue_on_new_thread`](Self::enqueue_on_new_thread).
    pub fn release_thread_resource_by_name(&self, name: impl Into<String>) -> BeaverResult<()> {
        if let Some(e) = self.named.lock()?.remove(&name.into()) {
            let _ = e.dam.release();
        }
        Ok(())
    }

    /// Releases the default and all named execution threads, stopping task acceptance.
    pub fn uninit(&self) -> BeaverResult<()> {
        *self.default.lock()? = None;
        let mut named = self.named.lock()?;
        for e in named.drain().map(|(_, v)| v) {
            let _ = e.dam.release();
        }
        Ok(())
    }
}

struct NamedEntry {
    dam: Arc<Dam>,
    long_resident: bool,
}
