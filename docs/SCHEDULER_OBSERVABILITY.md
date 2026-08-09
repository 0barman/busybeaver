# Scheduler observability

`Scheduler` exposes two optional observation paths. Neither path owns a task
result or participates in deciding its terminal state.

## Event stream

Subscribe before submitting work:

```rust
use busybeaver::{Job, Scheduler, TaskEventKind, TaskTerminal};

# #[tokio::main]
# async fn main() {
let scheduler = Scheduler::builder()
    .event_capacity(256)
    .build()
    .unwrap();
let mut events = scheduler.subscribe_events();
let handle = scheduler
    .submit(Job::once(|_| async { Ok::<_, ()>(42) }))
    .await
    .unwrap();

while let Ok(event) = events.recv().await {
    if event.kind() == TaskEventKind::Terminal {
        break;
    }
}
assert!(matches!(handle.join().await, TaskTerminal::Completed(42)));
# }
```

The stream is bounded and best effort. A slow receiver skips overwritten
events; each skipped delivery increments
`SchedulerSnapshot::dropped_event_deliveries()`. This can include a terminal
event when unrelated later traffic overwrites it. Use `TaskHandle::join` or
`TaskObserver::wait_terminal` for authoritative completion.

Events contain run/job identity, lane and group generations, state, cancel
reason, observation time, and an optional next wake-up time. They never contain
the task key, output, error value, panic text, or closure captures.
`TaskState::ExecutorError` identifies an internal invariant terminal without
embedding its code in the best-effort event. The authoritative result owner can
read that code from `TaskTerminal::ExecutorError { code }`.

## Metrics hook

`SchedulerBuilder::metrics_hook` accepts a `MetricsHook`. Calls run through a
bounded serial dispatcher. A blocking hook does not block task execution, but
it delays all later hook invocations; once the bounded queue fills, new hook
deliveries are dropped. Hook panics are caught when unwinding is enabled and
increment `observation_failures()`; a full hook queue increments the
dropped-delivery count.

The hook receives the same low-cardinality `TaskEvent` as event subscribers.
Map user keys or error text to metrics only in application code after applying
its own cardinality and redaction policy.

## Snapshots and shutdown

`Scheduler::snapshot()` reports active, queued, running, waiting, and
cancel-requested task counts plus configured lane/group counts and observation
failure counters. It is eventually consistent: fields are sampled from
separate registries. `snapshot_version()` is a monotonic observation sequence,
not a transactional database version.

`TaskSnapshot::next_wake_at()` is expressed as monotonic elapsed time since the
Scheduler observation subsystem was created. It is present only during retry
or schedule waits.

Shutdown first applies its task grace/abort policy. It then gives the internal
metrics hook an independent bounded drain window, configured with
`ShutdownPolicy::with_event_drain`. External event receivers are never drained
or awaited by shutdown.

SDK diagnostic logs are separate from the event stream. They use the `log`
target `busybeaver`, include a stable `code=...`, and never format `T` or `E`.
See [Error codes and SDK logging](ERROR_CODES_AND_LOGGING.md).
