# Bounded priority dispatch

`DispatchQueue<T, E>` is an opt-in, homogeneous queue in front of a
`Scheduler`. It adds bounded pending capacity, priority ordering, optional
per-key concurrency and explicit overflow policies without changing the
Scheduler's FIFO lane contract.

```rust
use busybeaver::{
    DispatchOptions, DispatchQueueConfig, Job, QueueOverflowPolicy, Scheduler,
    TaskTerminal,
};

# #[tokio::main]
# async fn main() {
let scheduler = Scheduler::builder().build().unwrap();
let config = DispatchQueueConfig::new(64, 4)
    .unwrap()
    .overflow(QueueOverflowPolicy::EvictLowestPriority);
let queue = scheduler.dispatch_queue(config);

let handle = queue
    .submit(
        Job::once(|_| async { Ok::<_, String>(42) }),
        DispatchOptions::default().priority(10).with_key("partition-a"),
    )
    .unwrap();
assert!(matches!(handle.join().await, TaskTerminal::Completed(42)));
# }
```

The capacity is the number of pending jobs. Dispatched jobs are counted by
the separate queue concurrency limit and then follow every normal Scheduler
limit. A per-key limit applies only when a job has a key; unkeyed jobs are
limited by total queue concurrency.

Pending capacity must be in
`1..=DispatchQueueConfig::MAX_PENDING_CAPACITY` (currently 1,048,576).
Larger values return
`DispatchQueueConfigError::PendingCapacityTooLarge { capacity, maximum }`
before a queue or backing allocation is created. Queue construction reserves
space for at most 1,024 pending entries and grows with actual submissions, so
selecting the maximum logical capacity does not eagerly allocate one million
entries. Concurrency must also be greater than zero.

## Ordering and aging

The dispatcher selects the largest effective priority:

```text
effective priority = priority + dispatches_waited / aging_interval
```

Priority is an unsigned value from 0 through 255. FIFO breaks equal effective
priority ties. Aging uses saturating arithmetic, so old low-priority work
eventually outranks a continuing stream of newer high-priority work without
integer wraparound. A job blocked by its key limit remains pending and does
not prevent an eligible job with another key from running.

## Overflow policies

- `RejectNewest` returns `DispatchSubmitError::Full` with the unsubmitted Job.
- `EvictOldest` replaces the oldest pending job.
- `EvictLowestPriority` replaces the oldest lowest-priority pending job only
  when the new job has a strictly higher base priority; otherwise it returns
  `Full` with the new Job.
- `CoalesceByKey` replaces a pending job with the same key. It requires a key,
  never replaces a running job, and does not evict an unrelated key when full.

If an internally inconsistent queue claims to be full but has no eviction
candidate, `submit` returns `DispatchSubmitError::InvariantViolation` with the
original Job and logs `BB-DISPATCH-006`. This is distinct from `Full`: callers
must not retry an SDK invariant failure as ordinary backpressure.

Every displaced handle resolves exactly once as `TaskTerminal::Evicted` with
the selected policy. Eviction only applies before dispatch. Once a job is
owned by Scheduler, it can stop only through Scheduler's normal terminal
protocol.

Queue acceptance and Scheduler acceptance are separate commit points. If the
queue has already returned a handle but Scheduler then rejects the handoff,
the handle resolves as `TaskTerminal::SubmissionFailed { reason }`. The reason
preserves `Full`, `ShuttingDown`, `Closed`, or `LaneNotFound`; it is not
collapsed into `ExecutorStopped`.

`close()` and dropping the last queue handle reject new work and resolve all
pending handles as `ExecutorStopped`; already-dispatched Scheduler tasks keep
running. Once Scheduler shutdown has started, `submit` synchronously returns
`DispatchSubmitError::Closed` and closes this queue. A submission accepted just
before that shutdown boundary still owns a handle and settles through either
the normal Scheduler protocol or `SubmissionFailed`.

Keys are used only by this in-memory queue and are not copied into Scheduler
events, snapshots or metrics.

The queue is intentionally homogeneous in `T` and `E`. Create separate queues
for different result types or submit directly to Scheduler when priority and
eviction are not required.
