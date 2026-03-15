use crate::error::RuntimeError;
use std::sync::Arc;

/// Listener for task lifecycle events.
pub trait WorkListener: Send + Sync {
    /// Called when the task completes after exhausting all retry attempts
    /// (the last execution still returned retry-needed).
    fn on_complete(&self);

    /// Called when the task is cancelled or interrupted.
    fn on_interrupt(&self);

    /// Called when a runtime error occurs during task execution.
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
