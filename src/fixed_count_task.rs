use crate::error::{BeaverError, BeaverResult};
use crate::listener::{FixedCountProgress, WorkListener};
use crate::task::{Task, TaskId};
use crate::work::Work;
use crate::work_fn::BoxWork;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A task that retries a fixed number of times: executes at most `count` times
/// (1 initial attempt + count-1 retries).
pub struct FixedCountTask {
    pub(crate) id: TaskId,
    pub(crate) work: BoxWork,
    pub(crate) count: u32,
    pub(crate) tag: Option<Box<String>>,
    pub(crate) progress: Option<Arc<dyn FixedCountProgress>>,
    pub(crate) listener: Option<Arc<dyn WorkListener>>,
    pub(crate) interrupted: AtomicBool,
}

/// Builder for fixed-count retry tasks.
pub struct FixedCountBuilder {
    work: Option<BoxWork>,
    count: u32,
    tag: Option<Box<String>>,
    progress: Option<Arc<dyn FixedCountProgress>>,
    listener: Option<Arc<dyn WorkListener>>,
}

impl FixedCountBuilder {
    /// Creates a new `FixedCountBuilder` with the given work.
    ///
    /// The work will be executed up to a fixed number of times (default is 3).
    /// Use the builder methods to customize the retry behavior before calling [`build`](Self::build).
    ///
    /// # Arguments
    ///
    /// * `work` - The work to be executed. Must implement [`Work`] + `Send` + `'static`.
    ///   If the work's async code panics or crashes, it is reported via the listener's `on_error`
    ///   and does not affect other tasks.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use busybeaver::{listener, work, Beaver, FixedCountBuilder, WorkResult};
    /// let beaver = Beaver::new("first_thread_queue", 256);
    /// let task = FixedCountBuilder::new(work(move || async {
    ///     println!("-----execute");
    ///     WorkResult::NeedRetry
    /// }))
    /// .count(5)
    /// .listener(listener(
    ///     move || println!("-----on_complete"),
    ///     || println!("-----on_interrupt"),
    /// ))
    /// .build()
    /// .unwrap();
    /// let _ = beaver.enqueue(task).await;
    /// ```
    pub fn new<W>(work: W) -> Self
    where
        W: Work + Send + 'static,
    {
        FixedCountBuilder {
            work: Some(Box::pin(work)),
            count: 3,
            tag: None,
            progress: None,
            listener: None,
        }
    }

    /// Sets the maximum execution count (minimum 1).
    pub fn count(mut self, n: u32) -> Self {
        self.count = n.max(1);
        self
    }

    /// Sets the task tag for identification.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Box::new(tag.into()));
        self
    }

    /// Sets the progress callback, called before each execution.
    pub fn progress(mut self, p: Arc<dyn FixedCountProgress>) -> Self {
        self.progress = Some(p);
        self
    }

    /// Sets the lifecycle event listener.
    pub fn listener(mut self, listener: Arc<dyn WorkListener>) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Builds the task. Returns an error if required fields are missing.
    pub fn build(self) -> BeaverResult<Arc<Task>> {
        let work = self.work.ok_or(BeaverError::BuilderMissingField("work"))?;

        Ok(Arc::new(Task::FixedCount(FixedCountTask {
            id: TaskId::new(),
            work,
            count: self.count,
            tag: self.tag,
            progress: self.progress,
            listener: self.listener,
            interrupted: AtomicBool::new(false),
        })))
    }
}
