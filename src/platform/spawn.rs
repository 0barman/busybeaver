use std::future::Future;

#[cfg(not(target_arch = "wasm32"))]
use tokio::task::JoinHandle;

/// Spawns a future on the specified tokio runtime handle.
///
/// Can be called outside a tokio runtime context.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn_on<T>(handle: &tokio::runtime::Handle, future: T) -> JoinHandle<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    handle.spawn(future)
}
