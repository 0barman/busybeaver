# Migrating from BusyBeaver 0.2 to 0.3

Version 0.3 adds a typed execution API alongside the existing `Beaver` API.
Existing Builder-based code does not need an immediate rewrite.

## Dependency

```toml
[dependencies]
busybeaver = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
```

The MSRV remains Rust 1.88. Version 0.3 supports native `std` targets on
Windows, Linux, and macOS. WebAssembly is not supported and emits an explicit
compile error.

## Compatibility API

`Beaver`, `Work`, `WorkResult`, `WorkListener`, `FixedCountBuilder`,
`TimeIntervalBuilder`, `RangeIntervalBuilder`, and `PeriodicBuilder` remain
available. The following behaviors intentionally remain unchanged:

- `Beaver::enqueue` is an immediate queue attempt and can return `QueueFull`;
- fixed count zero is normalized to one execution;
- an empty legacy time-interval sequence becomes one immediate execution;
- range total zero executes nothing;
- periodic zero interval remains allowed;
- time-interval and range-interval first-run timing remains different;
- periodic work panic reports an error and self-heals; bounded work stops;
- `cancel_all` affects work already submitted, while later work can run;
- named-lane capacity and resident compatibility rules remain unchanged.

Use `try_new`, `try_new_with_handle`, and each Builder's strict build method
when invalid input must be returned as a typed error instead of normalized or
panicking. `destroy_with_report` exposes accurate legacy worker progress;
`destroy` retains its old signature and maps timeout to the existing error set.
Creating a new named lane through `enqueue_on_new_thread` now returns
`BeaverError::InvalidConfiguration` for invalid capacity instead of panicking;
an existing named lane retains its historical compatibility behavior.

## Typed Scheduler

Use Scheduler for typed results, per-run control, explicit shutdown and new
features:

```rust
use busybeaver::{CancelReason, Job, Scheduler, TaskTerminal};

# #[tokio::main]
# async fn main() {
let scheduler = Scheduler::builder().build().unwrap();
let handle = scheduler
    .submit(Job::once(|context| async move {
        tokio::select! {
            _ = context.cancelled() => Err("cancelled"),
            _ = tokio::task::yield_now() => Ok(42),
        }
    }))
    .await
    .unwrap();
let controller = handle.controller();
let _ = controller.cancel(CancelReason::User);
let terminal = handle.join().await;
assert!(matches!(
    terminal,
    TaskTerminal::Completed(42) | TaskTerminal::Failed("cancelled")
        | TaskTerminal::Cancelled { .. }
));
# }
```

`TaskHandle::join(self)` is the only owner of `T/E`; clone an observer or
controller for status and control without cloning business values. Dropping a
normal handle detaches. Use `cancel_on_drop` when drop must request cancellation.
If Scheduler detects an internal ownership or schedule invariant, the handle
receives `TaskTerminal::ExecutorError { code }` rather than unwinding a runtime
worker. Use `ExecutorErrorCode::as_str()` for the stable diagnostic code.

## Submission and shutdown

- `try_submit` never waits and returns the original Job on `Full`.
- `submit` waits for capacity when the lane uses waiting backpressure and is
  cancellation-safe before its commit point.
- `ShuttingDown` means the Scheduler is currently closing; `Closed` means it is
  terminated or the selected lane is closed; `LaneNotFound` is separate.
- `shutdown` is shared and idempotent. `shutdown_with_timeout` only bounds one
  caller's observation and does not abandon the coordinator.
- cooperative cancel does not prove a user future stopped. `Aborted` is emitted
  only after Tokio confirms abort completion.

Generic submission, keyed-submission, replacement, validation, lane, group,
retry, and schedule errors are non-exhaustive where future variants are
expected. `SubmissionFailure` is also non-exhaustive. Downstream matches on
these types should include a wildcard arm. `TaskTerminal` remains exhaustive
so terminal handling can be explicit.

## Scheduling and advanced modules

- Retry and schedule wait states do not hold execution permits.
- Pause is cooperative and does not freeze total retry budgets or monotonic
  schedule time. Run-now never bypasses retry backoff.
- Keyed replacement distinguishes no-overlap and explicitly allowed overlap.
- Singleflight shares `Arc<TaskTerminal<T,E>>` only for concurrent calls; it is
  not a cache. A scope rejection after its high-level operation is accepted is
  `SubmissionFailed`, not `ExecutorStopped`.
- `DispatchQueue` applies priority, aging, per-key limits and eviction only to
  pending homogeneous jobs. A failed Scheduler handoff is
  `SubmissionFailed { reason }`; dispatched jobs follow normal Scheduler
  terminals.

See `SCHEDULER_CONTROL.md`, `SCHEDULER_OBSERVABILITY.md`, `SINGLEFLIGHT.md`, and
`DISPATCH_QUEUE.md` for detailed contracts.

## Panic and data boundaries

Panic isolation requires `panic=unwind`. With `panic=abort`, the crate compiles
but Rust terminates the process before any library can catch the panic.
Scheduler events, snapshots and metrics omit job outputs, errors, panic text,
and arbitrary keys by default.

## Error codes and logging

`BeaverError`, `ValidationError`, `RuntimeError`, `RetryPolicyError`, and the
Dispatch error types expose stable `code()` methods. BusyBeaver emits internal
diagnostics through the `log` facade with target `busybeaver`; it does not
select or install a logger. Expected typed validation and backpressure results
are not logged twice. See [Error codes and SDK logging](ERROR_CODES_AND_LOGGING.md).
