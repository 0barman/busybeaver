use crate::work_result::WorkResult;
use async_trait::async_trait;

/// A unit of work that can be executed and retried.
///
/// If the implementation panics (e.g. inside async code produced by [`work`](crate::work)),
/// the runtime catches it and reports via the task's [`WorkListener::on_error`]; other tasks
/// on the same execution thread are not affected.
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
