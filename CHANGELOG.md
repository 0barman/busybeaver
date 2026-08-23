# Changelog

## Unreleased

### Performance

- Lane admission now tracks ordering-key lifetimes with private reference counts instead of
  rebuilding the full key set for every queued execution.
- Lane dispatch no longer allocates a temporary ready list or repeatedly retains the whole queue,
  and bounded producers use targeted FIFO notifications instead of a wake-all retry storm.
- Retry and range-interval schedules use compact private representations while preserving the
  existing delay sequence, validation order, seeded jitter, and later-range-wins behavior.
- Event emission avoids building snapshots when there is no subscriber and tracing is disabled;
  ordinary snapshots also release registry locks before visiting individual executions.
- Shutdown reuses collected terminal summaries and creates only the winning shared shutdown
  process for repeated callers.

### Fixed

- Task-slot capacity waiting and the final formal lane waiter now use a lossless notification
  handoff, preventing an accepted replacement from sleeping forever after capacity becomes
  available.

### Documentation

- The former version-specific contract, migration, error, integration, coverage, and development
  pages are consolidated into complete English and Chinese developer guides.

## [0.3.0] - 2026-08-19

Rust 1.89 is the minimum supported version. The supported runtime target is native Tokio with
`Send` futures.

### Added

- Unique typed execution handles, reusable task specs, independent execution IDs, typed results,
  cancel-aware `WorkContext`, tracked children, and opt-in forced future cancellation.
- Immutable bounded lanes with removable queues, finite concurrency, priority aging, per-key
  single-flight ordering, FIFO waiting producers without try-spawn barging, admission timeout, and
  detailed overload errors.
- Typed retry with predicates, owned last errors, attempt/overall deadlines, fixed/explicit/
  exponential backoff, seeded jitter, and structured attempt-child cleanup.
- Recurring fixed-delay/fixed-rate/step/dynamic schedules, missed-tick and explicit-resume policy,
  seeded jitter, typed retry composition, tick failure policy, and bounded panic restart.
- Newest-wins task slots, checked revisions, strict/availability-first replace, generation scopes,
  hierarchy, and supervised rotation.
- Independent services with generation readiness/health, bounded restart, tracked children, and an
  exactly-once panic/timeout-isolated shutdown hook.
- Shared checked shutdown reports, finite draining, timeout snapshots, forced-cancellation
  escalation, bounded events, executor/lane snapshots, TTL terminal history, optional tracing, and
  configurable resource limits.
- Non-panicking `Beaver::new`, `new_with_handle`, `builder`, `try_new`, `try_new_with_handle`, and
  `work_with_state`; all construction failures are returned as stable `BeaverError` variants.
- Checked self/ancestor waits plus pre-acceptance self-replace and self-rotate cycle rejection.
- GitHub-facing developer documentation covering API selection, runtime requirements, typed
  outcomes, cancellation boundaries, stable error codes, contribution rules, and release gates.

### Fixed

- SDK-owned sleeps now wake on cancellation and re-check before the next side effect.
- Queue-to-running cancellation handoff and repeated legacy enqueue identity are deterministic.
- Missing Tokio time drivers produce typed execution/admission/shutdown timer errors without
  killing the lane or rolling back an accepted shutdown.
- Callback panic is isolated and cannot restart successful work or kill a lane.
- Legacy callback panic diagnostics are retained in checked shutdown reports.
- Tracked-child and service-hook cleanup phases/outcomes now populate per-execution shutdown records.
- Concurrent shutdown callers share one irreversible barrier; timeout/runtime/worker failures are
  no longer reported as unconditional success.
- Named admission cannot resurrect an executor after shutdown; lane generations remain controlled
  until their workers exit.
- Public construction no longer contains panic fallbacks. Recoverable construction and terminal
  failures expose stable `BB-*` codes, while internal-only invariant failures are logged with a
  stable code.
- Production code now has compile-time and source-audit gates against direct unwrap/expect,
  panic-like macros, unchecked indexing/arithmetic, `RefCell` mutable borrows, and unsafe blocks.

### Compatibility changes

- `destroy` now exposes timeout and worker failure.
- Admission after shutdown returns `ExecutorShuttingDown`.
- Typed zero attempt/capacity/concurrency/resource configurations are rejected.
- Legacy builders and enqueue methods remain available for the 0.3 compatibility window; new code
  should use typed handles, lanes, retry/recurring/service builders, slots, and scopes.
