use crate::work_result::WorkResult;
use async_trait::async_trait;

/// A unit of work that can be executed and retried.
///
/// If the implementation panics (e.g. inside async code produced by [`work`](crate::work)),
/// the runtime catches it and reports via the task's [`WorkListener::on_error`]; other tasks
/// on the same execution thread are not affected.
///
/// # Performance note
///
/// This trait uses [`async_trait`] under the hood, which means **each call to
/// [`execute`](Self::execute) allocates one boxed future on the heap**. For
/// the vast majority of workloads (network retries, periodic heartbeats, etc.)
/// this is negligible. For ultra-high-frequency tasks (e.g. a periodic task
/// firing every few microseconds in a hot loop), be aware of this overhead;
/// the boxing is unavoidable on stable Rust today as long as the executor
/// stores tasks behind a trait object for type erasure.
#[async_trait]
pub trait Work: Send + Sync {
    /// Executes the work once.
    ///
    /// Returns [`WorkResult::NeedRetry`] to indicate retry is needed,
    /// or [`WorkResult::Done`] to indicate successful completion.
    ///
    /// Panics or other failures during execution are isolated: they are reported via the
    /// listener's `on_error` and do not affect other tasks.
    async fn execute(&self) -> WorkResult<()>;
}
