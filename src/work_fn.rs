use crate::work::Work;
use crate::work_result::WorkResult;
use async_trait::async_trait;
use std::future::Future;

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

/// Type-erased [`Work`] used as the storage type inside each task struct.
///
/// `Box<dyn Work>` is sufficient here: [`Work::execute`] only takes `&self`,
/// so it does not need `Pin` semantics. `+ Send + Sync` is required because
/// the enclosing task is shared across threads as `Arc<Task>`.
pub(crate) type BoxWork = Box<dyn Work + Send + Sync>;

#[async_trait]
impl Work for Box<dyn Work + Send + Sync> {
    async fn execute(&self) -> WorkResult<()> {
        (**self).execute().await
    }
}
