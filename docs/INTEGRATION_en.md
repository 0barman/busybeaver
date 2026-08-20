# BusyBeaver 0.3 integration guide

This guide is the shortest path from dependency setup to production lifecycle handling. For exact
behavioral guarantees, see the [API contract](API_CONTRACT_0_3.md). For machine-readable failures,
see the [error code reference](ERROR_CODES.md).

## Runtime and construction

BusyBeaver captures one Tokio runtime at construction. Later calls and lanes never silently rebind
to another runtime.

```rust
use busybeaver::{Beaver, BeaverError, ResourceLimits};

fn build_executor() -> Result<Beaver, BeaverError> {
    Beaver::builder("legacy-default", 256)
        .resource_limits(ResourceLimits::default())
        .build()
}
```

Use `.runtime_handle(handle)` outside a runtime. `new`/`new_with_handle` and `builder`/`try_new`
all return `Result`; invalid capacity and missing-runtime failures are reported through stable
`BeaverError` variants instead of panicking.
Use `BeaverError::code()` for a stable machine-readable `BB-*` code. Legacy listener
`RuntimeError` values and recurring/service terminal failures also expose `code()`.
The supplied runtime must normally enable time. If it does not, timer use is reported as
`TimerUnavailable` while non-timer work and the lane remain usable.

## Choose the execution model

| Need | API |
|---|---|
| one reusable typed operation | `TaskSpec` + `Beaver::spawn` |
| bounded queue/concurrency/isolation | `Lane` |
| business retry and last error | `RetryBuilder` |
| dynamic or infinite cadence | `RecurringBuilder` |
| newest-wins replacement | `TaskSlot` |
| session/page/request generation | `Scope` |
| readiness/restart/shutdown hook | `ServiceBuilder` |

## Typed task and cancellation

```rust
use busybeaver::{CancelReason, TaskExit, TaskSpec};

async fn run(
    beaver: &busybeaver::Beaver,
    should_stop: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut handle = beaver.spawn(TaskSpec::new(|context| async move {
        context.sleep(std::time::Duration::from_secs(1)).await?;
        Ok::<_, busybeaver::Cancelled>(42)
    }))?;

    let control = handle.control();
    if should_stop {
        control.cancel(CancelReason::UserRequested);
    }
    match handle.join().await? {
        TaskExit::Completed(value) => println!("completed with {value}"),
        TaskExit::Cancelled { reason } => println!("cancelled: {reason:?}"),
        _ => println!("execution reached another terminal outcome"),
    }
    Ok(())
}
```

Cancellation is cooperative. Use tracked children and `WorkContext::sleep` to make cleanup
structured. `AbortPolicy::Allowed` permits dropping only the tracked async body; it is not an OS
thread or process kill. Within an execution, use `wait_checked` so direct self/ancestor waits return
`ExecutionWaitError::WouldJoin` instead of deadlocking.

## Lane and overload

```rust
use busybeaver::{LaneConfig, OrderingKey, Priority, SpawnOptions, TaskSpec};

async fn submit(beaver: &busybeaver::Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(
        LaneConfig::new("http").capacity(128).concurrency(16),
    )?;
    let options = SpawnOptions::new()
        .priority(Priority::new(6)?)
        .ordering_key(OrderingKey::try_from("tenant:17")?);
    let handle = lane.try_spawn_with_options(
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
        options,
    )?;
    handle.wait().await;
    Ok(())
}
```

Use `try_spawn` for immediate overload, `spawn` for backpressure, and `spawn_timeout` for bounded
admission. Equal ordering keys never overlap. Priority has bounded aging.
Waiting producers use FIFO tickets, and immediate producers cannot barge ahead of them.

## Retry, recurring, scope/slot, and service

- Retry requires explicit retry authorization and keeps the owned last business error.
- Recurring returns `Continue` or `Stop(T)` and supports fixed delay/rate, steps, dynamic functions,
  seeded jitter, missed ticks, explicit resume notifications, and typed retry composition.
- Slot revisions make stale replace explicit. Strict replace never overlaps old/new executions.
- Scoped spawn is linearized with generation rotation; old generations cannot admit work.
- Services run outside ordinary FIFO capacity and provide generation readiness, health, bounded
  restart, tracked children, and an exactly-once shutdown hook.

See [the API contract](API_CONTRACT_0_3.md) and [migration guide](MIGRATION_0_2_TO_0_3.md).

## Shutdown

```rust
use busybeaver::{ShutdownMode, ShutdownOptions, ShutdownTimeoutAction};
use std::time::Duration;

async fn stop(beaver: &busybeaver::Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let handle = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(5))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    let grace = handle.wait_grace_outcome().await?;
    let final_report = handle.wait_final().await?;
    println!("grace: {grace:?}; final: {final_report:?}");
    Ok(())
}
```

Shutdown is irreversible and shared by concurrent callers. Drain mode drains finite work and stops
recurring/services. Timeout keeps controls and a reusable final wait handle.

## Observation and boundaries

Events and optional tracing are bounded and redacted; snapshots retain only bounded/TTL terminal
summaries. BusyBeaver does not own remote idempotency, business consistency, blocking/FFI/OS-thread
shutdown, untracked child tasks, or remote side-effect rollback.

## Next steps

- Review the complete [API contract](API_CONTRACT_0_3.md).
- Use the [error code reference](ERROR_CODES.md) for telemetry and support diagnostics.
- Follow the [0.2 → 0.3 migration guide](MIGRATION_0_2_TO_0_3.md) when upgrading legacy builders.
- Contributors should read the [development guide](DEVELOPMENT.md).
