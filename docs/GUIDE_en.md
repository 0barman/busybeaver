# BusyBeaver complete developer guide

[中文完整文档](GUIDE_zh.md) · [README](../README.md) · [Changelog](../CHANGELOG.md) ·
[Rust API reference](https://docs.rs/busybeaver)

This is the single English guide for application developers, maintainers, and release engineers.
It describes every public execution model, the lifecycle contract, operational limits, diagnostics,
and the repository quality gate. Version-to-version changes belong in the changelog instead of a
separate migration document.

## Requirements and installation

BusyBeaver supports Rust 1.89 or newer, native Tokio runtimes, and `Send + 'static` futures. Rust
1.89 is the minimum supported Rust version (MSRV); repository development is pinned to Rust 1.89.0
by [`rust-toolchain.toml`](../rust-toolchain.toml). Rustup selects the pinned development toolchain
automatically when commands are run from this repository. Build the executor inside a runtime or
provide an explicit `tokio::runtime::Handle`. Enable Tokio time when using sleeps, deadlines,
retries, recurring schedules, admission timeouts, or shutdown grace periods.

```toml
[dependencies]
busybeaver = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
```

BusyBeaver has no default feature. The optional `tracing` feature emits bounded, redacted lifecycle
fields through the tracing ecosystem.

```toml
busybeaver = { version = "0.3", features = ["tracing"] }
```

```rust,no_run
use busybeaver::{Beaver, BeaverError, ResourceLimits};

fn build_executor() -> Result<Beaver, BeaverError> {
    Beaver::builder("default", 256)
        .resource_limits(ResourceLimits::default())
        .build()
}
```

`Beaver::try_new_with_handle` and `BeaverBuilder::runtime_handle` are the construction paths for a
caller that is currently outside Tokio. Construction is fallible: an invalid capacity, an invalid
resource limit, or a missing runtime is returned as `BeaverError` rather than panicking.

## Choose the execution model

| Requirement | Primary API |
| --- | --- |
| One typed asynchronous operation | `TaskSpec<T, E>` and `Beaver::spawn` |
| Bounded queue, concurrency, priority, and ordering | `Lane` and `LaneConfig` |
| Business retry with an owned last error | `RetryBuilder` |
| Fixed, stepped, or dynamic recurring work | `RecurringBuilder` and `Schedule` |
| Newest-wins replacement | `TaskSlot` |
| Session, page, or request generations | `Scope` |
| Readiness, health, restart, and shutdown hooks | `ServiceBuilder` |
| Compatibility count/time/range/periodic work | Legacy builders and `Beaver::enqueue` |
| Bounded events, snapshots, and history | `subscribe_events` and `snapshot` |
| Shared and reportable shutdown | `Beaver::shutdown` |

## Typed execution, identity, and results

`TaskSpecId` identifies a reusable definition. Every accepted execution receives a unique
`ExecutionId`, independent state, cancellation, result storage, and tracked-child storage.

```rust,no_run
use busybeaver::{Beaver, CancelReason, TaskExit, TaskSpec};
use std::io;
use std::time::Duration;

async fn typed_task(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let spec = TaskSpec::new(|context| async move {
        context.sleep(Duration::from_millis(10)).await?;
        Ok::<_, busybeaver::Cancelled>(42_u8)
    });
    let mut handle = beaver.spawn(spec)?;
    let control = handle.control();
    if control.state().is_terminal() {
        return Err(io::Error::other("new execution was already terminal").into());
    }
    control.cancel(CancelReason::Other("application stop".to_string()));
    match handle.join().await? {
        TaskExit::Cancelled { .. } | TaskExit::Completed(_) => Ok(()),
        _ => Err(io::Error::other("unexpected terminal outcome").into()),
    }
}
```

`TaskHandle::wait` is repeatable and returns a redacted `TaskExitSummary`. `TaskHandle::join` takes
the typed `TaskExit<T, E>` once. `TaskControlHandle` is cloneable and type-erased. Dropping a normal
handle detaches; `cancel_on_drop` creates a guard that requests cancellation. Dropping a pending
wait or join future does not consume the result.

An accepted execution reaches exactly one terminal outcome: `Completed`, `Failed`, `Cancelled`,
`Aborted`, `Panicked`, or `ExecutorStopped`. Cancel and deadline are first-wins. Forced cancellation
is available only to work admitted with `AbortPolicy::Allowed`, and drops only the tracked async
future. It cannot pre-empt CPU loops, blocking syscalls, `spawn_blocking`, OS/FFI threads, untracked
Tokio tasks, or remote side effects.

`TaskSelector` and `Beaver::cancel_snapshot` provide a linearized batch-cancellation snapshot.
`BatchCancelReport` records the result for every selected execution. Use `wait_checked` when one
tracked execution waits for another; direct self and ancestor joins return
`ExecutionWaitError::WouldJoin`.

### Structured children

`WorkContext::spawn_child` and `spawn_child_future` create tracked children. Parent terminal cleanup
first closes child admission, then cancels and joins remaining tracked children. Attempt children
are also cleaned before retry advances. Application-created Tokio tasks remain untracked and
caller-owned.

## Lanes, overload, priority, and ordering

Queue capacity counts queued entries; running concurrency is a separate limit. Moving an execution
from queued to running immediately restores queue capacity.

```rust,no_run
use busybeaver::{Beaver, LaneConfig, OrderingKey, Priority, SpawnOptions, TaskSpec};

async fn lane_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(
        LaneConfig::new("network").capacity(128).concurrency(8),
    )?;
    let options = SpawnOptions::new()
        .priority(Priority::new(6)?)
        .ordering_key(OrderingKey::try_from("customer:42")?);
    let handle = lane.try_spawn_with_options(
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        options,
    )?;
    handle.wait().await;
    Ok(())
}
```

- `try_spawn` returns overload immediately.
- `spawn` waits using FIFO producer tickets.
- `spawn_timeout` captures its admission deadline when the method is called.
- A non-waiting producer cannot barge ahead of queued producers.
- Priority is 0 through 7. Seven priority selections are followed by one oldest-ready selection.
- Equal ordering keys never overlap. A blocked entry is not considered ready for aging.
- Queued cancellation physically removes the entry and restores capacity and key accounting.
- Lane configuration is immutable. Reusing a name with a different configuration is an error.

`LaneStats` exposes queued/running/capacity/waiter/key-blocking counts. `close` stops new admission;
`close_and_cancel` also waits for accepted work to terminate. If the captured runtime disappears,
accepted handles terminalize and waiting producers return `ExecutorUnavailable`.

## Typed retry

Retry is explicit: configure `retry_all_errors` or a predicate. Attempt timeout does not authorize
retry unless `retry_timed_out_attempts` is selected, because an external side effect may already
have happened.

```rust,no_run
use busybeaver::{AttemptContext, Backoff, Beaver, LaneConfig, RetryBuilder, TaskExit};
use std::time::Duration;

async fn retry_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("retry"))?;
    let retry = RetryBuilder::new(|attempt: AttemptContext| async move {
        if attempt.number() < 3 { Err("temporary") } else { Ok(7_u8) }
    })
    .max_attempts(4)
    .retry_all_errors()
    .backoff(Backoff::fixed(Duration::from_millis(10)))
    .build()?;
    let mut handle = lane.spawn_retry(retry).await?;
    match handle.join().await? {
        TaskExit::Completed(7) => Ok(()),
        _ => Err(std::io::Error::other("retry did not complete").into()),
    }
}
```

Backoff supports none, fixed, explicit, and exponential delays. Seeded `Jitter` is deterministic.
An overall deadline, per-attempt timeout, retry predicate, and `RetryDecisionContext` can be
combined. `RetrySpec::clone` shares immutable configuration while every execution owns an
independent attempt and jitter iterator.

## Recurring execution and schedules

Recurring work is non-overlapping. `TickOutcome::Continue` schedules another tick;
`TickOutcome::Stop(T)` completes with a typed value.

```rust,no_run
use busybeaver::{Beaver, LaneConfig, RecurringBuilder, Schedule, TickOutcome};
use std::time::Duration;

async fn recurring_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("recurring"))?;
    let recurring = RecurringBuilder::new(|tick| async move {
        Ok::<_, ()>(if tick.number() >= 3 {
            TickOutcome::Stop(tick.number())
        } else {
            TickOutcome::Continue
        })
    })
    .schedule(Schedule::fixed_delay(Duration::from_millis(20)))
    .build()?;
    let mut handle = lane.spawn_recurring(recurring).await?;
    let _ = handle.join().await?;
    Ok(())
}
```

Schedules support initial delay, fixed delay, fixed rate, finite steps, repeat-last steps, dynamic
decisions, missed-tick policy, resume policy, and deterministic jitter. Tick failure and panic
restart are explicit and bounded through `TickFailurePolicy`, `PanicPolicy`, and `RestartPolicy`.
`RecurringBuilder::from_retry` runs a complete typed retry policy inside each tick.

## Slots and scopes

`TaskSlot` owns a revisioned newest-wins transaction. `StrictSingleInstance` waits for the old
execution to finish; `AvailabilityFirst` allows documented overlap. Stale revisions and conflicting
same-revision definitions are explicit outcomes. Dropping `ReplaceHandle` does not cancel an
accepted transaction.

`Scope` owns a current `ScopeGeneration`. Rotation closes admission for the old generation,
cancels or drains it according to `RotationPolicy`, joins child scopes, and publishes a new
generation. Dropping `RotationHandle` does not revoke the accepted rotation.

```rust,no_run
use busybeaver::{Beaver, LaneConfig, ReplacePolicy, RotationPolicy, SlotKey, TaskSpec};

async fn slot_and_scope(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("lifecycle"))?;
    let slot = beaver.create_task_slot(SlotKey::new("latest")?, lane.clone())?;
    let replacement = slot.replace(
        1,
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        ReplacePolicy::StrictSingleInstance,
    )?;
    let _ = replacement.await?;

    let scope = beaver.create_scope("session", lane)?;
    let rotation = scope.rotate(RotationPolicy::Strict)?;
    let _ = rotation.wait().await;
    Ok(())
}
```

Strict self-replace and self-rotation are rejected before changing the current transaction or
generation.

## Supervised services

Services use an independent supervisor rather than ordinary FIFO capacity. Readiness and health
are generation-scoped. Restart on failure or panic is explicit, windowed, bounded, and backed off.
Shutdown disables restart, cancels tracked children, and invokes the panic/timeout-isolated shutdown
hook exactly once.

```rust,no_run
use busybeaver::{Beaver, CancelReason, ServiceBuilder};

async fn service_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let service = ServiceBuilder::new(|context| async move {
        context.ready();
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .build()?;
    let mut handle = beaver.start_service(service)?;
    handle.wait_ready().await?;
    handle.control().cancel(CancelReason::UserRequested);
    let _ = handle.join().await?;
    Ok(())
}
```

`ServiceStatus`, `HealthStatus`, and `ServiceHandle::status` expose the current generation.
`ServiceContext::spawn_child` tracks service-owned children. Hook results are retained in checked
shutdown cleanup records.

## Legacy compatibility models

The legacy models remain supported public APIs:

| Builder | Execution behavior |
| --- | --- |
| `FixedCountBuilder` | At most N executions with optional progress callback |
| `TimeIntervalBuilder` | A supplied delay before every execution, including the first |
| `RangeIntervalBuilder` | Attempt-index ranges; first execution is immediate; later range wins |
| `PeriodicBuilder` | Resident periodic work with panic self-healing |

```rust,no_run
use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};

async fn legacy_example(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .build()?;
    beaver.enqueue(task).await?;
    Ok(())
}
```

`work`, `work_with_state`, `Work`, and `WorkResult` define legacy work. `WorkListener`, `listener`,
`listener_with_error`, and `FixedCountProgress` provide lifecycle and progress callbacks. Callback
panics are isolated, but synchronous callbacks must remain short because they can block a runtime
worker. Names containing “thread” refer to Tokio worker lanes, not dedicated OS threads.

## Events, snapshots, and privacy

`subscribe_events` returns a bounded broadcast `EventStream`. Slow subscribers receive
`EventRecvError::Lagged` and never apply backpressure to execution. For one execution, `Admitted`
precedes its state and terminal events, and `Terminal` is emitted exactly once.

```rust,no_run
use busybeaver::{Beaver, TaskEvent, TaskSpec};

async fn observe(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = beaver.subscribe_events()?;
    let handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) }))?;
    let execution_id = handle.execution_id();
    loop {
        match events.recv().await? {
            TaskEvent::Terminal { execution_id: observed, .. } if observed == execution_id => break,
            _ => {}
        }
    }
    let snapshot = beaver.snapshot();
    println!("active={}, history={}", snapshot.active.len(), snapshot.terminal_history.len());
    Ok(())
}
```

`ExecutorSnapshot` contains sorted active tasks, bounded/TTL terminal history, lane statistics, and
live scope/slot/subscriber counts. Events, reports, and default tracing exclude typed business
values, business errors, panic text, ordering-key bytes, and arbitrary metadata.

## Checked shutdown

The lifecycle is irreversible: `Running -> ShuttingDown(shared barrier) -> Stopped(shared report)`.
Concurrent callers with identical options receive the same `ShutdownHandle`; conflicting options
return `ShutdownError::ConfigConflict`.

```rust,no_run
use busybeaver::{Beaver, ShutdownMode, ShutdownOptions, ShutdownTimeoutAction};
use std::time::Duration;

async fn stop(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(5))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    let _ = shutdown.wait_grace_outcome().await?;
    let report = shutdown.wait_final().await?;
    println!("shutdown tasks={}", report.tasks.len());
    Ok(())
}
```

`CancelAll` cancels all work. `DrainFinite` drains finite executions and stops recurring work and
services. Timeout actions can keep work tracked, request abort only for opted-in futures, or wait
without a grace cutoff. Progress and final reports preserve task exits, forced-cancel requests,
cleanup phases, callback failures, and worker failures. `destroy` remains the compatibility helper;
checked shutdown is the complete reportable lifecycle.

## Errors, limits, and runtime boundaries

Match typed error variants for policy and record `code()` for telemetry. Do not parse `Display`
text. Public error and outcome enums marked `#[non_exhaustive]` require a fallback match arm.

| Error family | Stable code examples |
| --- | --- |
| Construction and resources | `BB-RUNTIME-UNAVAILABLE`, `BB-INVALID-LANE-CAPACITY`, `BB-RESOURCE-LIMIT-EXCEEDED` |
| Admission and lifecycle | `BB-QUEUE-FULL`, `BB-EXECUTOR-SHUTTING-DOWN`, `BB-SHUTDOWN-TIMED-OUT` |
| Legacy execution | `BB-RUNTIME-TASK-FAILED`, `BB-RUNTIME-RETRIES-EXHAUSTED` |
| Recurring | `BB-RECURRING-TICK-FAILED`, `BB-RECURRING-RESTART-LIMIT-EXCEEDED`, `BB-SCHEDULE-*` |
| Service | `BB-SERVICE-BODY-FAILED`, `BB-SERVICE-RESTART-LIMIT-EXCEEDED` |

`ResourceLimits` bounds active executions, lanes, scopes, slots, event subscribers, tracked
children, waiting producers, ordering keys, event capacity, tags, and terminal history. Limits are
immutable after construction, and failed admission rolls back without a ghost execution or event.

On a runtime without a time driver, timer-dependent work reports `TimerUnavailable`; non-timer work
remains usable. Panic isolation requires `panic = "unwind"`; `panic = "abort"` is process-fatal.
BusyBeaver does not provide distributed leases, remote idempotency, database transactions, or
rollback of external side effects.

## Future drop contract

| Future or handle | Drop behavior |
| --- | --- |
| lane spawn/retry/recurring before admission | No execution is published |
| accepted lane spawn/retry/recurring | Execution continues |
| wait/join future | Result remains available unless already atomically taken |
| `cancel_and_wait` future | The synchronously submitted cancellation remains active |
| slot replace or scope rotate handle | Accepted supervisor continues |
| shutdown wait | Shared shutdown supervisor continues |
| normal `TaskHandle` | Detaches; only cancel-on-drop requests cancellation |

No user future, callback, typed value, business error, metadata destructor, or hook is invoked while
an internal state lock is held.

## Contributor and release guide

Production code must not add `unsafe`, unchecked indexing/arithmetic, `unwrap`, `expect`, panic or
assert macros, `todo`, `unimplemented`, `unreachable`, or dynamic `RefCell` borrows. Recoverable
failure returns a typed `Result` or terminal outcome; private invariant failures log a stable
diagnostic without business data.

Use deterministic barriers or paused Tokio time for concurrency and timer tests. A stress test is
supplementary and never replaces a deterministic regression test. Public behavior changes require
an integration test; private state transitions require a colocated unit test; documentation Rust
examples must compile.

The repository's development, release-gate, and MSRV checks all use the Rust 1.89.0 toolchain pinned
in [`rust-toolchain.toml`](../rust-toolchain.toml). Install it with `rustup toolchain install 1.89.0`
if rustup has not already done so. Run the complete release gate from the repository root; the
explicit `rustup run 1.89.0` command keeps the MSRV compatibility check visible:

```text
cargo fmt --manifest-path busybeaver/Cargo.toml --all -- --check
cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- --include-ignored
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --no-default-features -- --include-ignored
cargo test --manifest-path busybeaver/Cargo.toml --release --all-targets --all-features
cargo test --manifest-path busybeaver/Cargo.toml --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path busybeaver/Cargo.toml --no-deps --all-features
RUSTFLAGS="-C panic=abort" cargo check --manifest-path busybeaver/Cargo.toml --lib --all-features
rustup run 1.89.0 cargo check --manifest-path busybeaver/Cargo.toml --all-targets --all-features
cargo package --manifest-path busybeaver/Cargo.toml --locked
```

Performance work additionally requires a public-API A/B benchmark on the same machine, toolchain,
features, and lockfile; median and tail latency, allocation count, peak RSS, and ordinary-path
regressions must be recorded. Cross-platform Linux, macOS, and Windows CI remains mandatory.

## Public feature coverage index

This guide covers all exported feature families: construction and limits; typed task identity,
control, result, cancellation, forced cancellation, selectors and tracked children; lanes,
priority, ordering and backpressure; retry; recurring schedules and policies; slots; scopes;
services; checked shutdown and cleanup reports; events, snapshots and retention; all four legacy
builders, work/listener/progress helpers; stable diagnostics; runtime, panic, privacy and Drop
contracts; and contributor/release verification. Exact fields and method signatures remain in the
[Rust API reference](https://docs.rs/busybeaver).
