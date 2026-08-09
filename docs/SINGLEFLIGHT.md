# Typed singleflight

`Singleflight<K, T, E>` is an optional high-level registry created from a
`Scheduler` or `TaskGroup`. Concurrent callers with the same key share one
Scheduler task and receive the same `Arc<TaskTerminal<T, E>>`.

```rust
use busybeaver::{Scheduler, TaskTerminal};

# #[tokio::main]
# async fn main() {
let scheduler = Scheduler::builder().build().unwrap();
let requests = scheduler.singleflight::<String, usize, String>();
let terminal = requests
    .run("key".to_owned(), |_| async { Ok(42) })
    .await;
assert!(matches!(&*terminal, TaskTerminal::Completed(42)));
# }
```

The registry provides concurrent coalescing only; it is not a result cache.
The key is compare-removed before terminal publication, so a later call starts
a fresh execution. Keys are never copied into Scheduler events or metrics.

`T` and `E` must be `Sync` in addition to the normal Scheduler `Send` bound,
because all waiters share the terminal through `Arc`. A follower's operation
closure is dropped without being invoked.

Dropping one or every waiter does not cancel the leader. This fixed policy
prevents one caller from surprising the others; cancellation remains owned by
the Scheduler or TaskGroup scope. Group shutdown therefore produces the same
`Cancelled { GroupShutdown }` terminal for every waiter. Leader panic likewise
produces one shared `Panicked` terminal.

If the scope closes before the leader can be submitted, the abstraction
returns `TaskTerminal::SubmissionFailed { reason }`; `Closed` identifies an
already-closed Scheduler or TaskGroup, while other Scheduler rejection reasons
remain distinct. Unlike the lower-level submit API, `run` consumes an operation
closure and cannot return a reusable `Job`.

`ExecutorStopped` is reserved for loss of the bound runtime or an internal
publication task before another terminal can be known.
