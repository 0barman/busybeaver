use crate::error::RuntimeError;
use std::sync::Arc;

/// Listener for task lifecycle events.
///
/// **Important**: listener callbacks run on the executor lane and **must not**
/// panic or block. Panics inside a listener callback are *not* isolated by
/// the framework and will tear down the executor lane.
pub trait WorkListener: Send + Sync {
    /// Called when the task **completes successfully** — i.e. the work returned
    /// [`WorkResult::Done`](crate::WorkResult::Done).
    ///
    /// This fires for **all** task types the moment the work returns `Done`:
    /// fixed-count, time-interval and range-interval tasks (including a `Done`
    /// on an intermediate attempt, before retries are exhausted) as well as
    /// tasks built by [`PeriodicBuilder`](crate::PeriodicBuilder).
    ///
    /// It does **not** fire when a bounded task exhausts all of its retry
    /// attempts without ever succeeding — that terminal state is reported via
    /// [`on_error`](Self::on_error) with
    /// [`RuntimeError::RetriesExhausted`](crate::RuntimeError::RetriesExhausted).
    fn on_complete(&self);

    /// Called when the task is cancelled or interrupted by
    /// [`Beaver::cancel_all`](crate::Beaver::cancel_all),
    /// [`Beaver::cancel_non_long_resident`](crate::Beaver::cancel_non_long_resident),
    /// [`Beaver::release_thread_resource_by_name`](crate::Beaver::release_thread_resource_by_name),
    /// or [`Beaver::destroy`](crate::Beaver::destroy).
    fn on_interrupt(&self);

    /// Called when a runtime error occurs during task execution.
    ///
    /// This includes:
    /// - panics inside [`work`](crate::work): if the closure or its async block
    ///   panics, the panic is caught and reported as
    ///   [`RuntimeError::TaskExecutionFailed`]; the worker continues so other
    ///   tasks can still run.
    /// - a bounded task (fixed-count / time-interval / range-interval) exhausting
    ///   every retry attempt without ever returning `Done`, reported as
    ///   [`RuntimeError::RetriesExhausted`].
    ///
    /// Default implementation does nothing; callers may optionally override.
    fn on_error(&self, _error: RuntimeError) {}
}

/// Progress callback for fixed-count retry tasks, called before each execution.
pub trait FixedCountProgress: Send + Sync {
    /// Called before the `current`-th execution (out of `total`), with the task's `tag`.
    fn on_progress(&self, current: u32, total: u32, tag: &str);
}

/// Constructs a [`WorkListener`] from closures.
pub struct WorkListenerClosure<C, I, E> {
    on_complete: C,
    on_interrupt: I,
    on_error: E,
}

/// Listener type returned by [`listener`] when no custom error callback is supplied.
pub type BasicWorkListener<C, I> = WorkListenerClosure<C, I, fn(RuntimeError)>;

impl<C, I, E> WorkListener for WorkListenerClosure<C, I, E>
where
    C: Fn() + Send + Sync,
    I: Fn() + Send + Sync,
    E: Fn(RuntimeError) + Send + Sync,
{
    fn on_complete(&self) {
        (self.on_complete)();
    }
    fn on_interrupt(&self) {
        (self.on_interrupt)();
    }
    fn on_error(&self, error: RuntimeError) {
        (self.on_error)(error);
    }
}

/// Creates a [`WorkListener`] from two closures (without error handling).
pub fn listener<C, I>(on_complete: C, on_interrupt: I) -> Arc<BasicWorkListener<C, I>>
where
    C: Fn() + Send + Sync,
    I: Fn() + Send + Sync,
{
    Arc::new(WorkListenerClosure {
        on_complete,
        on_interrupt,
        on_error: |_| {},
    })
}

/// Creates a [`WorkListener`] from three closures (with error handling).
pub fn listener_with_error<C, I, E>(
    on_complete: C,
    on_interrupt: I,
    on_error: E,
) -> Arc<WorkListenerClosure<C, I, E>>
where
    C: Fn() + Send + Sync,
    I: Fn() + Send + Sync,
    E: Fn(RuntimeError) + Send + Sync,
{
    Arc::new(WorkListenerClosure {
        on_complete,
        on_interrupt,
        on_error,
    })
}

impl<F> FixedCountProgress for F
where
    F: Fn(u32, u32, &str) + Send + Sync,
{
    fn on_progress(&self, current: u32, total: u32, tag: &str) {
        self(current, total, tag);
    }
}
