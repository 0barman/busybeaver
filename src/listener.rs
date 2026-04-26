use crate::error::RuntimeError;
use std::sync::Arc;

/// Listener for task lifecycle events.
///
/// **Important**: listener callbacks run on the executor lane and **must not**
/// panic or block. Panics inside a listener callback are *not* isolated by
/// the framework and will tear down the executor lane.
pub trait WorkListener: Send + Sync {
    /// Called when the task **exhausts all of its retry attempts** without the
    /// work ever returning [`WorkResult::Done`](crate::WorkResult::Done).
    ///
    /// Despite the name, this callback represents *retries-exhausted*, not
    /// *successful completion*. It fires only after the last permitted
    /// execution still returned [`WorkResult::NeedRetry`](crate::WorkResult::NeedRetry).
    /// For [`PeriodicTask`](crate::periodic_task::PeriodicTask) it also fires
    /// when the work eventually returns `Done` (since periodic tasks have no
    /// retry budget to exhaust).
    fn on_complete(&self);

    /// Called when the task is cancelled or interrupted by
    /// [`Beaver::cancel_all`](crate::Beaver::cancel_all),
    /// [`Beaver::cancel_non_long_resident`](crate::Beaver::cancel_non_long_resident),
    /// [`Beaver::release_thread_resource_by_name`](crate::Beaver::release_thread_resource_by_name),
    /// or [`Beaver::destroy`](crate::Beaver::destroy).
    fn on_interrupt(&self);

    /// Called when a runtime error occurs during task execution.
    ///
    /// This includes panics inside [`work`](crate::work): if the closure or its async block
    /// panics, the panic is caught and reported as [`RuntimeError::TaskExecutionFailed`];
    /// the worker continues so other tasks can still run.
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
pub fn listener<C, I>(
    on_complete: C,
    on_interrupt: I,
) -> Arc<WorkListenerClosure<C, I, fn(RuntimeError)>>
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
