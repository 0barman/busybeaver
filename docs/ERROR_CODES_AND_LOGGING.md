# Error codes and SDK logging

BusyBeaver returns typed errors for recoverable failures and emits diagnostics
through the Rust `log` facade for internal executor failures. The crate never
installs a logger and never writes directly to stdout or stderr. An application
that wants BusyBeaver logs must install any `log`-compatible implementation.

## Handling typed errors

Public error types expose `code()`; scheduler invariant failures expose
`ExecutorErrorCode::as_str()`. The code is payload-free and stable within the
0.3 line, so it is suitable for alert labels, log searches, and support tickets.
Always retain the typed variant for program control flow.

```rust
use busybeaver::{Beaver, ValidationError};

fn create() -> Result<Beaver, ValidationError> {
    match Beaver::try_new("default", 256) {
        Ok(beaver) => Ok(beaver),
        Err(error) => {
            // Record `error.code()` in your own telemetry if desired.
            eprintln!("busybeaver setup failed code={}: {error}", error.code());
            Err(error)
        }
    }
}
```

Creating a named compatibility lane is also fallible. Invalid configuration is
returned as `BeaverError::InvalidConfiguration(ValidationError)`; no partially
created lane is retained.

```rust
# use busybeaver::{work, Beaver, BeaverError, PeriodicBuilder, WorkResult};
# async fn example(beaver: &Beaver) -> Result<(), BeaverError> {
let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) })).build()?;
if let Err(error) = beaver
    .enqueue_on_new_thread(task, "reports", 0, false)
    .await
{
    // `BB-VAL-008`; retrying with the same capacity cannot succeed.
    let code = error.code();
    eprintln!("lane rejected code={code}: {error}");
}
# Ok(())
# }
```

## Executor terminal errors

An internal Scheduler invariant does not panic the worker. The owning handle
receives `TaskTerminal::ExecutorError { code }`; observers see terminal state
`TaskState::ExecutorError`, and shutdown reports count it as a failed run.

```rust
# use busybeaver::{ExecutorErrorCode, TaskTerminal};
# fn inspect<T, E>(terminal: TaskTerminal<T, E>) {
match terminal {
    TaskTerminal::ExecutorError { code } => {
        eprintln!("executor failed code={}", code.as_str());
    }
    _ => {}
}
# }
```

The executor code never contains `T`, `E`, a task key, or panic text.

## Diagnostic record contract

BusyBeaver diagnostic records use log target `busybeaver` and include these
message fields:

```text
code=BB-EXEC-002 component=scheduler scheduled output has no cursor run_id=...
```

- `error` level is reserved for a broken internal invariant, a poisoned legacy
  worker lock, or the compatibility constructor immediately before preserving
  its documented panic contract.
- Expected validation, backpressure, cancellation, and closed-queue results are
  returned to the caller without being logged a second time.
- No generic task result/error value or closure capture is formatted by the SDK.
- If the application installs no logger, the `log` facade is a no-op; returned
  errors and terminals are unaffected.

## Code catalogue

| Family | Codes | Public access |
| --- | --- | --- |
| Legacy Beaver | `BB-LEGACY-001` through `BB-LEGACY-006` | `BeaverError::code()` |
| Strict validation | `BB-VAL-001` through `BB-VAL-010` | `ValidationError::code()` |
| Legacy runtime/listener | `BB-RUN-001` through `BB-RUN-003` | `RuntimeError::code()` |
| Retry | `BB-RETRY-001` through `BB-RETRY-009` | `RetryPolicyError::code()` |
| Dispatch config | `BB-DISPATCH-CONFIG-001` through `003` | `DispatchQueueConfigError::code()` |
| Dispatch submit | `BB-DISPATCH-001` through `004`, and invariant `006` | `DispatchSubmitError::code()` |
| Scheduler executor | `BB-EXEC-001` through `BB-EXEC-003` | `ExecutorErrorCode::as_str()` |

Callers should treat code strings as identifiers, not parse their numeric
suffixes. Match the typed Rust variants when behavior must differ.

