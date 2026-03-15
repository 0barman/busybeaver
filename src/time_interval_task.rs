use crate::error::{BeaverError, BeaverResult};
use crate::listener::WorkListener;
use crate::task::{Task, TaskId};
use crate::work::Work;
use crate::work_fn::BoxWork;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A task that retries with time intervals: waits `intervals[i]` seconds after each
/// failure before the next execution.
pub struct TimeIntervalTask {
    pub(crate) id: TaskId,
    pub(crate) work: BoxWork,
    /// Intervals in seconds. `Box<[u64]>` avoids extra Vec capacity overhead.
    pub(crate) intervals: Box<[u64]>,
    pub(crate) tag: Option<Box<String>>,
    pub(crate) listener: Option<Arc<dyn WorkListener>>,
    pub(crate) interrupted: AtomicBool,
}

/// Builder for time-interval retry tasks.
pub struct TimeIntervalBuilder {
    work: Option<BoxWork>,
    intervals: Vec<u64>,
    tag: Option<Box<String>>,
    listener: Option<Arc<dyn WorkListener>>,
}

impl TimeIntervalBuilder {
    /// Creates a new `TimeIntervalBuilder` with the given work.
    ///
    /// The work will be retried with configurable time intervals between each attempt.
    /// Use the builder methods to customize the retry behavior before calling [`build`](Self::build).
    ///
    /// # Arguments
    ///
    /// * `work` - The work to be executed. Must implement [`Work`] + `Send` + `'static`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use busybeaver::{listener, work, Beaver, TimeIntervalBuilder, WorkResult};
    /// let beaver = Beaver::new("first_thread_queue", 256);
    /// let task = TimeIntervalBuilder::new(work(move || async {
    ///     println!("-----execute");
    ///     WorkResult::NeedRetry
    /// }))
    /// .listener(listener(
    ///     move || println!("-----on_complete"),
    ///     || println!("-----on_interrupt"),
    /// ))
    /// .intervals(vec![1, 2, 3, 4])
    /// .build()
    /// .unwrap();
    /// let _ = beaver.enqueue(task);
    /// ```
    pub fn new<W>(work: W) -> Self
    where
        W: Work + Send + 'static,
    {
        TimeIntervalBuilder {
            work: Some(Box::pin(work)),
            intervals: vec![1],
            tag: None,
            listener: None,
        }
    }

    /// Sets the retry intervals in seconds. For example, `[1, 2, 4, 8, 16]`.
    pub fn intervals(mut self, secs: impl Into<Vec<u64>>) -> Self {
        self.intervals = secs.into();
        self
    }

    /// Sets the task tag for identification.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(Box::new(tag.into()));
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

        let intervals: Box<[u64]> = if self.intervals.is_empty() {
            [0].into()
        } else {
            self.intervals.into()
        };

        Ok(Arc::new(Task::TimeInterval(TimeIntervalTask {
            id: TaskId::new(),
            work,
            intervals,
            tag: self.tag,
            listener: self.listener,
            interrupted: AtomicBool::new(false),
        })))
    }
}
