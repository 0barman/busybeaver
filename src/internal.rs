#[cfg(not(feature = "tracing"))]
use std::io::Write;
use std::sync::PoisonError;
use tokio::task::JoinError;

/// Records an invariant failure without risking a second panic while the
/// executor is already recovering from an internal fault.
pub(crate) fn log_internal_error(code: &'static str, message: &str) {
    #[cfg(feature = "tracing")]
    tracing::error!(
        error_code = code,
        error_message = message,
        "busybeaver internal error"
    );

    #[cfg(not(feature = "tracing"))]
    {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "busybeaver internal error [{code}]: {message}");
    }
}

/// Mutex poisoning is not recoverable through the public API from background
/// supervisors. Preserve liveness, but always make the degraded recovery
/// visible to operators.
pub(crate) fn recover_poison<T>(error: PoisonError<T>) -> T {
    log_internal_error(
        "BB-INTERNAL-LOCK-POISONED",
        "recovering state protected by a poisoned mutex",
    );
    error.into_inner()
}

/// Extracts a panic payload without relying on `JoinError::into_panic`, which
/// itself panics if a caller ever misclassifies the join error.
pub(crate) fn take_join_panic(
    error: JoinError,
    code: &'static str,
) -> Option<Box<dyn std::any::Any + Send + 'static>> {
    match error.try_into_panic() {
        Ok(payload) => Some(payload),
        Err(_) => {
            log_internal_error(code, "join error was not a panic as expected");
            None
        }
    }
}
