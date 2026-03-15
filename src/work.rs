use crate::work_result::WorkResult;
use async_trait::async_trait;

/// A unit of work that can be executed and retried.
#[async_trait]
pub trait Work: Send + Sync {
    /// Executes the work once.
    ///
    /// Returns [`WorkResult::NeedRetry`] to indicate retry is needed,
    /// or [`WorkResult::Done`] to indicate successful completion.
    async fn execute(&self) -> WorkResult<()>;
}
