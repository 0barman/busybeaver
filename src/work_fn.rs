use crate::work::Work;
use crate::work_result::WorkResult;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;

/// Wraps an async closure as [`Work`].
///
/// If the closure or its async block panics, the panic is caught by the executor and
/// reported via [`WorkListener::on_error`]; other tasks continue to run.
pub struct WorkFn<F> {
    f: F,
}

#[async_trait]
impl<F, Fut> Work for WorkFn<F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = WorkResult<()>> + Send,
{
    async fn execute(&self) -> WorkResult<()> {
        (self.f)().await
    }
}

/// Creates a [`Work`] from an async closure.
///
/// Crashes and panics inside the closure or the async block (e.g. `panic!`, `unwrap` on `None`,
/// `todo!`, `unimplemented!`, `unreachable!`) are isolated: they are reported via the task's
/// [`WorkListener::on_error`] and do not affect other tasks on the same execution thread.
pub fn work<F, Fut>(f: F) -> WorkFn<F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = WorkResult<()>> + Send,
{
    WorkFn { f }
}

/// Type-erased Work for storing any Work in a queue.
pub(crate) type BoxWork = Pin<Box<dyn Work + Send>>;

#[async_trait]
impl Work for Pin<Box<dyn Work + Send>> {
    async fn execute(&self) -> WorkResult<()> {
        self.as_ref().execute().await
    }
}

/// Converts a [`Work`] into a [`BoxWork`].
pub(crate) fn box_work<W: Work + Send + 'static>(w: W) -> BoxWork {
    Box::pin(w)
}
