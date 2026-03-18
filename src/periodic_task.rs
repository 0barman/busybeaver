use crate::error::{BeaverError, BeaverResult};
use crate::listener::WorkListener;
use crate::task::{Task, TaskId};
use crate::work::Work;
use crate::work_fn::BoxWork;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// A periodic task that executes at fixed time intervals.
///
/// Unlike [`TimeIntervalTask`](crate::time_interval_task::TimeIntervalTask),
/// it loops indefinitely until interrupted or work returns Done.
pub struct PeriodicTask {
    pub(crate) id: TaskId,
    pub(crate) work: BoxWork,
    /// Execution interval between runs.
    pub(crate) interval: Duration,
    /// Whether to wait one interval before the first execution.
    pub(crate) initial_delay: bool,
    pub(crate) tag: Option<Box<String>>,
    pub(crate) listener: Option<Arc<dyn WorkListener>>,
    pub(crate) interrupted: AtomicBool,
}

/// Builder for periodic tasks.
pub struct PeriodicBuilder {
    work: Option<BoxWork>,
    interval: Duration,
    initial_delay: bool,
    tag: Option<Box<String>>,
    listener: Option<Arc<dyn WorkListener>>,
}

impl PeriodicBuilder {
    /// Creates a new `PeriodicBuilder` with the given work.
    ///
    /// The work will be executed periodically based on the configured interval.
    /// If [`interval`](Self::interval) is not called, the default is [`Duration::ZERO`] (no delay between runs).
    /// Use the builder methods to customize the task behavior before calling [`build`](Self::build).
    ///
    /// # Arguments
    ///
    /// * `work` - The work to be executed periodically. Must implement [`Work`] + `Send` + `'static`.
    ///   If the work's async code panics or crashes, it is reported via the listener's `on_error`
    ///   and does not affect other tasks.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use busybeaver::{listener, work, Beaver, PeriodicBuilder, WorkResult};
    /// use std::time::Duration;
    /// let beaver = Beaver::new("first_thread_queue", 256);
    /// let task = PeriodicBuilder::new(work(move || async {
    ///     println!("-----execute");
    ///     WorkResult::NeedRetry
    /// }))
    /// .interval(Duration::ZERO)
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
        PeriodicBuilder {
            work: Some(Box::pin(work)),
            interval: Duration::ZERO, // default: no interval if caller does not call interval()
            initial_delay: false,
            tag: None,
            listener: None,
        }
    }

    /// Sets the execution interval. If the duration is zero, there will be no interval and the task will be executed continuously.
    pub fn interval(mut self, duration: Duration) -> Self {
        self.interval = duration;
        self
    }

    /// Sets whether to wait one interval before the first execution.
    ///
    /// Defaults to `false` (execute immediately on start).
    pub fn initial_delay(mut self, delay: bool) -> Self {
        self.initial_delay = delay;
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

        Ok(Arc::new(Task::Periodic(PeriodicTask {
            id: TaskId::new(),
            work,
            interval: self.interval,
            initial_delay: self.initial_delay,
            tag: self.tag,
            listener: self.listener,
            interrupted: AtomicBool::new(false),
        })))
    }
}
