# Pause, resume, and run-now

`TaskController` provides cooperative execution-boundary controls. These
commands never preempt or drop a user future that is already running.

## Pause and resume

`pause()` stops a run before its next permit acquisition, retry attempt, or
schedule trigger. A currently executing attempt may still finish. Paused runs
hold no global or lane execution permit.

Retry `max_elapsed` and schedule time continue to advance while paused. When a
run resumes after its schedule deadline, it proceeds immediately and then
applies the configured missed-tick behavior. Cancellation and shutdown always
wake a paused run.

`TaskGroup::pause()` applies to existing members and to jobs submitted to the
group afterward. A member cannot resume itself while its group remains paused;
the command returns `TaskCommandError::ScopePaused`. Group resume releases all
current members.

## Manual schedule triggers

`run_now()` is valid only for `ScheduledJob`. It advances the next schedule
trigger and never bypasses a retry backoff. If the job returns
`TaskControl::Complete`, pending manual triggers do not override that explicit
terminal decision.

Configure trigger shaping on the reusable definition:

```rust
use busybeaver::{FirstRun, Schedule, ScheduledJob, TaskControl, TriggerPolicy};
use std::time::Duration;

let schedule = Schedule::fixed_delay(Duration::from_secs(60), FirstRun::Immediate)?;
let job = ScheduledJob::new(schedule, |_| async {
    Ok::<_, ()>(TaskControl::<()>::Continue)
})
.trigger_policy(TriggerPolicy::CoalesceOne);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Policies are bounded:

- `Drop`: accepts a trigger only while waiting for the next schedule and when
  no trigger is pending.
- `CoalesceOne` (default): retains at most one pending trigger.
- `QueueAll { capacity }`: retains at most the non-zero configured capacity.
- `Replace`: replaces the pending manual trigger with one newer trigger; it
  does not abort a running attempt.

While paused, `run_now()` returns `TriggerOutcome::Paused` and does not queue a
trigger. A manual trigger racing the natural deadline is folded into that same
run.
