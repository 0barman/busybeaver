# BusyBeaver 0.3 error code reference

BusyBeaver uses typed Rust errors for control flow and stable `BB-*` strings for telemetry,
support, and log correlation. Do not parse `Display` text; match the typed enum when behavior must
change, and record `code()` when a machine-readable value is needed.

The codes in this document are part of the 0.3 compatibility contract. A code will not be reused
for a different meaning within the 0.3 series. New variants and codes may be added because the
public error enums are `#[non_exhaustive]`.

## Recommended handling pattern

```rust
use busybeaver::{Beaver, BeaverError};

fn build_executor() -> Result<Beaver, BeaverError> {
    Beaver::try_new("default", 256).map_err(|error| {
        eprintln!("BusyBeaver construction failed [{}]: {error}", error.code());
        error
    })
}
```

Use the enum variant for application policy and the code for metrics or diagnostics:

```rust
use busybeaver::BeaverError;

fn is_retryable_construction_error(error: &BeaverError) -> bool {
    match error {
        BeaverError::QueueFull => true,
        BeaverError::RuntimeUnavailable | BeaverError::ExecutorShuttingDown => false,
        _ => false,
    }
}
```

## `BeaverError`

Call [`BeaverError::code()`](https://docs.rs/busybeaver/latest/busybeaver/enum.BeaverError.html#method.code).

| Variant | Code | Typical action |
| --- | --- | --- |
| `BuilderMissingField` | `BB-BUILDER-MISSING-FIELD` | Fix the builder configuration. |
| `QueueFull` | `BB-QUEUE-FULL` | Apply backpressure, retry later, or use a bounded admission wait. |
| `DamReleased` | `BB-DAM-RELEASED` | Stop using the released legacy lane. |
| `LockPoisoned` | `BB-LOCK-POISONED` | Treat the operation as failed and preserve diagnostics. |
| `NoDam` | `BB-NO-DAM` | Create or retain the required legacy lane. |
| `ExecutorShuttingDown` | `BB-EXECUTOR-SHUTTING-DOWN` | Reject new work; shutdown is irreversible. |
| `ShutdownTimedOut` | `BB-SHUTDOWN-TIMED-OUT` | Inspect the shutdown report and remaining controls. |
| `WorkerFailed` | `BB-WORKER-FAILED` | Inspect the worker diagnostic and runtime state. |
| `InvalidLaneCapacity` | `BB-INVALID-LANE-CAPACITY` | Configure a capacity of at least one. |
| `InvalidLaneConcurrency` | `BB-INVALID-LANE-CONCURRENCY` | Configure concurrency of at least one. |
| `LaneConfigConflict` | `BB-LANE-CONFIG-CONFLICT` | Reuse the immutable configuration or choose another name. |
| `ScopeConfigConflict` | `BB-SCOPE-CONFIG-CONFLICT` | Reuse the existing lane generation or choose another scope name. |
| `SlotConfigConflict` | `BB-SLOT-CONFIG-CONFLICT` | Reuse the existing lane generation or choose another slot key. |
| `ResourceLimitExceeded` | `BB-RESOURCE-LIMIT-EXCEEDED` | Reduce active resources or raise the configured bounded limit. |
| `InvalidResourceLimit` | `BB-INVALID-RESOURCE-LIMIT` | Replace the zero resource limit with a positive value. |
| `RuntimeUnavailable` | `BB-RUNTIME-UNAVAILABLE` | Construct inside Tokio or supply a live runtime handle. |
| `RangeIntervalRangesExceedTotal` | `BB-RANGE-COUNT-EXCEEDS-TOTAL` | Reduce ranges or increase the legacy retry count. |

## `RuntimeError`

Legacy listeners receive `RuntimeError`. New integrations should prefer typed `TaskExit<T, E>`
values, but listener diagnostics can call `RuntimeError::code()`.

| Variant | Code | Meaning |
| --- | --- | --- |
| `LockPoisoned` | `BB-RUNTIME-LOCK-POISONED` | A legacy execution lock was poisoned. |
| `TaskExecutionFailed` | `BB-RUNTIME-TASK-FAILED` | Legacy work panicked or otherwise failed during execution. |
| `RetriesExhausted` | `BB-RUNTIME-RETRIES-EXHAUSTED` | A bounded legacy task used every permitted attempt. |
| `InternalInvariantViolation` | The code stored by the variant | A private executor invariant failed. |

## `RecurringFailure<E>`

Call `RecurringFailure::code()` after matching `TaskFailure::Recurring`.

| Variant | Code |
| --- | --- |
| `TickFailed` | `BB-RECURRING-TICK-FAILED` |
| `ConsecutiveFailuresExceeded` | `BB-RECURRING-CONSECUTIVE-FAILURES-EXCEEDED` |
| `ScheduleEnded` | `BB-RECURRING-SCHEDULE-ENDED` |
| `CounterOverflow` | `BB-RECURRING-COUNTER-OVERFLOW` |
| `RestartLimitExceeded` | `BB-RECURRING-RESTART-LIMIT-EXCEEDED` |
| `ScheduleOverflow` | The specific `BB-SCHEDULE-*` code stored by the variant |

Schedule overflow codes identify the failed arithmetic boundary:

- `BB-SCHEDULE-DYNAMIC-INSTANT-OVERFLOW`
- `BB-SCHEDULE-FIXED-DELAY-OVERFLOW`
- `BB-SCHEDULE-FIXED-RATE-DURATION-OVERFLOW`
- `BB-SCHEDULE-FIXED-RATE-INSTANT-OVERFLOW`
- `BB-SCHEDULE-FIXED-RATE-MULTIPLY-OVERFLOW`
- `BB-SCHEDULE-MISSED-ADVANCE-OVERFLOW`
- `BB-SCHEDULE-MISSED-COUNT-OVERFLOW`
- `BB-SCHEDULE-MISSED-DELAY-OVERFLOW`
- `BB-SCHEDULE-MISSED-DURATION-OVERFLOW`
- `BB-SCHEDULE-MISSED-INSTANT-OVERFLOW`
- `BB-SCHEDULE-STEP-COUNTER-UNDERFLOW`
- `BB-SCHEDULE-STEP-INDEX-OVERFLOW`
- `BB-SCHEDULE-STEP-INSTANT-OVERFLOW`

## `ServiceFailure<E>`

Call `ServiceFailure::code()` after matching `TaskFailure::Service`.

| Variant | Code |
| --- | --- |
| `BodyFailed` | `BB-SERVICE-BODY-FAILED` |
| `RestartLimitExceeded` | `BB-SERVICE-RESTART-LIMIT-EXCEEDED` |
| `GenerationOverflow` | `BB-SERVICE-GENERATION-OVERFLOW` |
| `CounterOverflow` | The code stored by the variant; currently `BB-SERVICE-RESTART-COUNT-OVERFLOW` |

## Internal diagnostic codes

Internal-only failures cannot always be returned through an existing synchronous API. BusyBeaver
logs them through `tracing::error!` when the `tracing` feature is enabled and otherwise writes a
minimal diagnostic to standard error. These paths do not include typed business values, business
errors, or metadata contents.

An internal code normally indicates a violated executor invariant, exhausted internal sequence, or
runtime task classification mismatch. Capture the complete line, BusyBeaver version, Rust version,
Tokio version, enabled features, and a minimal reproduction when reporting it.

| Area | Codes |
| --- | --- |
| Lock and terminal state | `BB-INTERNAL-LOCK-POISONED`, `BB-RESULT-DUPLICATE-STORE`, `BB-TERMINAL-EXIT-MISSING`, `BB-TERMINAL-DEADLINE-EXIT-MISSING`, `BB-TERMINAL-FORCED-EXIT-MISSING` |
| Resume and slot sequences | `BB-RESUME-EPOCH-OVERFLOW`, `BB-SLOT-CLOSE-SEQUENCE-OVERFLOW` |
| Lane state | `BB-LANE-WAITER-TICKET-MISSING`, `BB-LANE-WAITER-COUNT-UNDERFLOW`, `BB-LANE-RUNNING-OVERFLOW`, `BB-LANE-RUNNING-UNDERFLOW`, `BB-LANE-DISPATCH-COUNT-OVERFLOW`, `BB-LANE-CAPACITY-INVARIANT`, `BB-LANE-PRIORITY-OUT-OF-RANGE`, `BB-LANE-BLOCKED-STATS-OVERFLOW`, `BB-LANE-READY-STATS-OVERFLOW` |
| Retry and legacy range | `BB-RETRY-DELAY-MISSING`, `BB-RETRY-PREDICATE-MISSING`, `BB-RETRY-LOOP-FELL-THROUGH`, `BB-RANGE-INTERVAL-MISSING`, `BB-RANGE-END-OVERFLOW` |
| Join classification | `BB-WORK-JOIN-PANIC-MISCLASSIFIED`, `BB-FUTURE-JOIN-PANIC-MISCLASSIFIED`, `BB-EXIT-JOIN-PANIC-MISCLASSIFIED`, `BB-LEGACY-JOIN-PANIC-MISCLASSIFIED`, `BB-RETRY-JOIN-PANIC-MISCLASSIFIED`, `BB-RECURRING-JOIN-PANIC-MISCLASSIFIED`, `BB-SERVICE-JOIN-PANIC-MISCLASSIFIED`, `BB-SERVICE-CANCEL-JOIN-PANIC-MISCLASSIFIED` |

## Reporting checklist

Include the following in a bug report:

1. BusyBeaver, Rust, and Tokio versions.
2. Target OS and Tokio runtime flavor.
3. Enabled BusyBeaver features.
4. The typed error variant and `BB-*` code.
5. Whether the crate was built with `panic = "unwind"` or `panic = "abort"`.
6. A minimal reproduction that does not contain credentials or business data.

See the [development guide](DEVELOPMENT.md) for validation commands and pull-request requirements.
