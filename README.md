# BusyBeaver

[![CI](https://github.com/0barman/busybeaver/actions/workflows/ci.yml/badge.svg)](https://github.com/0barman/busybeaver/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/busybeaver.svg)](https://crates.io/crates/busybeaver)
[![docs.rs](https://docs.rs/busybeaver/badge.svg)](https://docs.rs/busybeaver)
[![MSRV](https://img.shields.io/badge/MSRV-1.89-blue.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/crates/l/busybeaver.svg)](#license)

BusyBeaver is a Tokio-native Rust SDK for typed asynchronous execution, bounded scheduling,
backpressure, retry, recurring work, lifecycle scopes, newest-wins replacement, supervised
services, checked shutdown, and bounded observability.

It is designed for applications that need explicit task identity, typed terminal outcomes,
cooperative cancellation, overload control, and deterministic lifecycle behavior without building
those mechanisms around every future.

> Rust 1.89 is the minimum supported Rust version (MSRV). The supported runtime target is native
> Tokio with `Send + 'static` futures.

## Documentation

| Audience | Document |
| --- | --- |
| Rust API reference | [docs.rs](https://docs.rs/busybeaver) |
| Complete English documentation | [Developer guide](docs/GUIDE_en.md) |
| 完整中文文档 | [开发者文档](docs/GUIDE_zh.md) |
| Release history | [Changelog](CHANGELOG.md) |

## Installation

Add BusyBeaver and a Tokio runtime to your application:

```toml
[dependencies]
busybeaver = "0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
```

BusyBeaver has no default crate features. Enable `tracing` only when lifecycle events should also be
emitted through the `tracing` ecosystem:

```toml
busybeaver = { version = "0.3", features = ["tracing"] }
```

## Quick start

```rust
use busybeaver::{Beaver, TaskExit, TaskSpec};
use std::io;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::try_new("default", 256)?;

    let spec = TaskSpec::new(|context| async move {
        context.sleep(Duration::from_millis(10)).await?;
        Ok::<_, busybeaver::Cancelled>(42)
    });

    let mut handle = beaver.spawn(spec)?;
    let value = match handle.join().await? {
        TaskExit::Completed(value) => value,
        _ => return Err(io::Error::other("task did not complete successfully").into()),
    };

    println!("result: {value}");
    beaver.destroy().await?;
    Ok(())
}
```

`TaskSpecId` identifies the reusable operation definition. Every accepted spawn receives a unique
`ExecutionId`, cancellation state, result cell, and tracked-child set. `TaskHandle::wait` is
repeatable and returns a redacted summary; `TaskHandle::join` takes the typed result once.

## Choose an execution model

| Requirement | Primary API |
| --- | --- |
| One reusable typed operation | `TaskSpec<T, E>` + `Beaver::spawn` |
| Bounded queue and concurrency | `Lane` + `LaneConfig` |
| Immediate overload response | `Lane::try_spawn` |
| Fair, cancellable admission wait | `Lane::spawn` or `Lane::spawn_timeout` |
| Business retry with the owned last error | `RetryBuilder` |
| Fixed, stepped, or dynamic cadence | `RecurringBuilder` + `Schedule` |
| Newest-wins replacement | `TaskSlot` |
| Session, page, or request generations | `Scope` |
| Readiness, health, restart, and shutdown hook | `ServiceBuilder` |
| Shared, reportable shutdown | `Beaver::shutdown` |
| Bounded lifecycle events and snapshots | `subscribe_events` + `snapshot` |

The legacy builders and enqueue methods remain supported compatibility APIs. New code should
prefer typed handles and the model-specific APIs above.

## Bounded lanes, priority, and ordering

Queue capacity and running concurrency are independent limits. A lane also supports priorities
from 0 through 7 and an optional bounded ordering key. Executions with the same ordering key never
run concurrently.

```rust
use busybeaver::{
    Beaver, LaneConfig, OrderingKey, Priority, SpawnOptions, TaskSpec,
};

async fn submit(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
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

Use `try_spawn` when overload must be returned immediately, `spawn` when the producer may wait for
capacity, and `spawn_timeout` when admission requires a deadline. Waiting producers use FIFO
tickets; immediate producers cannot barge ahead of an eligible waiter. Deterministic priority
aging prevents indefinite low-priority starvation.

## Retry and recurring work

`RetryBuilder` requires explicit retry authorization. It supports bounded attempts, fixed or
exponential backoff, explicit delay sequences, deterministic jitter, per-attempt timeouts, and an
overall deadline. A timed-out attempt is not retried unless the application explicitly opts in,
because the remote side effect may already have happened.

`RecurringBuilder` separates schedule continuation from business failure with
`TickOutcome::Continue` and `TickOutcome::Stop(T)`. Schedules support fixed delay, fixed rate,
steps, dynamic decisions, missed-tick policies, deterministic jitter, and explicit resume
notifications. `RecurringBuilder::from_retry` composes a complete typed retry policy inside each
non-overlapping tick.

## Typed terminal outcomes

An accepted execution reaches exactly one `TaskExit<T, E>` variant:

| Variant | Meaning |
| --- | --- |
| `Completed(T)` | The operation produced its success value. |
| `Failed(TaskFailure<E>)` | The operation or a typed policy failed. |
| `Cancelled` | Cooperative cancellation won the terminal race. |
| `Aborted` | Forced cancellation dropped an opted-in tracked future. |
| `Panicked` | A panic was isolated while using `panic = "unwind"`. |
| `ExecutorStopped` | The captured runtime or internal runner became unavailable. |

Public error enums are `#[non_exhaustive]`; downstream `match` expressions must include a fallback
arm. Construction and legacy runtime errors expose stable machine-readable values through
`BeaverError::code()` and `RuntimeError::code()`. Service and recurring failures also expose
`code()`. See the diagnostics section in the [English developer guide](docs/GUIDE_en.md).

## Cancellation and structured work

Cancellation is cooperative by default. SDK-owned sleeps and admission waits observe the execution
token, and tracked children are closed, cancelled, and joined before their parent reaches terminal
cleanup.

`AbortPolicy::Allowed` permits forced cancellation by dropping only the tracked async future. It
cannot pre-empt a CPU loop, blocking syscall, `spawn_blocking` operation, OS/FFI thread, untracked
Tokio task, or remote side effect. Application-level idempotency and compensation remain the
caller's responsibility.

When one execution waits for another tracked execution, use `wait_checked`. Direct self-waits and
ancestor waits return `ExecutionWaitError::WouldJoin` instead of deadlocking.

## Checked shutdown

`Beaver::shutdown` starts an irreversible, shared shutdown supervisor. Concurrent callers using the
same options receive the same barrier.

```rust
use busybeaver::{
    Beaver, ShutdownMode, ShutdownOptions, ShutdownTimeoutAction,
};
use std::time::Duration;

async fn stop(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(5))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;

    let grace_outcome = shutdown.wait_grace_outcome().await?;
    let final_report = shutdown.wait_final().await?;
    println!("grace: {grace_outcome:?}; final: {final_report:?}");
    Ok(())
}
```

`DrainFinite` drains finite work while stopping recurring executions and services. Timeout reports
retain controls and a reusable final wait handle. `destroy` remains available for compatibility,
but checked shutdown provides the complete lifecycle report.

## Observation and privacy

`subscribe_events` returns a bounded broadcast stream. Slow subscribers receive an explicit lag
error and never block execution. `snapshot` exposes active redacted executions, bounded/TTL terminal
history, lane statistics, and live scope/slot/subscriber counts.

Typed business values, business errors, panic text, and metadata contents are not emitted by the
default event or tracing paths. The optional `tracing` feature emits the same bounded, redacted
lifecycle fields. The complete public diagnostic table and handling guidance are in the
[English developer guide](docs/GUIDE_en.md).

## Runtime requirements and limitations

- Construct the executor inside a Tokio runtime, or supply an explicit `tokio::runtime::Handle`.
- Enable Tokio's time driver when using timers, admission timeouts, retries, recurring schedules, or
  finite shutdown grace periods.
- BusyBeaver supports native Tokio runtimes and `Send + 'static` futures. `LocalSet`, WASM, and
  `no_std` are not supported targets.
- Panic isolation requires `panic = "unwind"`; `panic = "abort"` remains process-fatal.
- BusyBeaver does not provide distributed leases, remote idempotency, database transactions, or
  rollback of external side effects.

## Contributing

The full build, test, documentation, safety, and pull-request rules are documented in the
[English developer guide](docs/GUIDE_en.md). Start with:

```text
cargo fmt --manifest-path busybeaver/Cargo.toml --all -- --check
cargo clippy --manifest-path busybeaver/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path busybeaver/Cargo.toml --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path busybeaver/Cargo.toml --no-deps --all-features
```

Bug reports should include the BusyBeaver version, Rust version, Tokio runtime configuration,
enabled crate features, the relevant `BB-*` error or diagnostic code, and a minimal reproduction.

## License

Licensed under either of the following, at your option:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)
