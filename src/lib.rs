//! # busybeaver
//!
//! `busybeaver` is a Tokio-native execution SDK for finite tasks, typed retry
//! jobs, recurring jobs, and supervised services. Every submission receives an
//! independent execution identity, cancellation state, terminal result, and
//! observable lifecycle.
//!
//! ## At a glance
//!
//! - [`TaskSpec`] and [`TaskHandle`] – typed execution, cancellation, state, and result.
//! - [`RetryBuilder`] – typed retry with backoff, jitter, deadlines, and classifiers.
//! - [`RecurringBuilder`] – fixed or dynamic schedules with explicit tick semantics.
//! - [`ServiceBuilder`] – readiness, health, restart, child tracking, and shutdown hooks.
//! - [`Lane`], [`Scope`], and [`TaskSlot`] – bounded QoS, hierarchical lifetime, and newest-wins replacement.
//!
//! Public lanes have immutable capacity and concurrency configuration. Within
//! a lane, priority scheduling and optional ordering keys determine which ready
//! work can run; different lanes can make progress independently.
//!
//! ## Quick start
//!
//! ```no_run
//! use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let beaver = Beaver::new("default", 256)?;
//!
//!     let task = FixedCountBuilder::new(work(|| async {
//!         // do work, return WorkResult::Done(()) when finished
//!         WorkResult::NeedRetry
//!     }))
//!     .count(5)
//!     .build()?;
//!
//!     beaver.enqueue(task).await?;
//!
//!     // ... do other work ...
//!
//!     // Always destroy the beaver before letting it go out of scope so that
//!     // background workers and resident tasks are cleaned up.
//!     beaver.destroy().await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Lifecycle & cleanup
//!
//! - Always call [`Beaver::destroy`] before dropping a `Beaver`, especially if
//!   you have enqueued a [`PeriodicBuilder`] task (which would otherwise
//!   continue running on the runtime).
//! - [`Beaver::cancel_all`] cancels every queued and running task across all
//!   lanes; tasks enqueued **after** `cancel_all` are still executed.
//! - [`Beaver::cancel_non_long_resident`] preserves lanes created with
//!   `long_resident = true`.
//!
//! ## Panic safety
//!
//! If the closure or async block inside [`work`] panics (e.g. `panic!`,
//! `unwrap` on `None`, `todo!`, indexing out of bounds), the executor
//! catches the panic, reports it through [`WorkListener::on_error`] as
//! [`RuntimeError::TaskExecutionFailed`], and continues running other tasks
//! on the same lane. A [`PeriodicBuilder`] task additionally **self-heals**:
//! after the panic is reported it resumes on the next period (throttled by the
//! configured interval) instead of dying permanently. Bounded tasks
//! (fixed-count / time-interval / range-interval) stop after a panic.
//! Listener and progress callback panics are isolated from the lane. A synchronous
//! callback can still block a runtime worker, so callbacks should remain short.

#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]
// `map_or_else(recover_poison, identity)` is intentionally preferred over
// `unwrap_or_else`: the former makes the no-unwrap production policy
// mechanically auditable while preserving the same logged poison recovery.
#![allow(clippy::unnecessary_result_map_or_else)]

mod beaver;
mod dam;
mod error;
mod execution;
mod fixed_count_task;
#[cfg(doctest)]
mod github_docs;
mod ids;
mod internal;
mod lane;
mod listener;
mod observation;
mod periodic_task;
mod range_interval_task;
mod recurring;
mod retry;
mod scope;
mod service;
mod shutdown;
mod slot;
mod task;
mod time_interval_task;
mod work;
mod work_fn;
mod work_result;

pub(crate) mod platform;

pub use beaver::{Beaver, BeaverBuilder};
pub use error::{BeaverError, BeaverResult, RuntimeError};
pub use execution::{
    AbortPolicy, BatchCancelRecord, BatchCancelReport, CancelOnDrop, CancelReason,
    CancelRequestOutcome, CancelWait, Cancelled, ChildHandle, ExecutionKind, ExecutionWaitError,
    ExecutorStopReason, ForcedCancellationError, ForcedCancellationOutcome, JoinResultError,
    PanicSource, SpawnChildError, StopCauseSummary, TaskControlHandle, TaskExit, TaskExitSummary,
    TaskFailure, TaskHandle, TaskSelector, TaskSnapshot, TaskSpec, TaskState, WorkContext,
};
pub use fixed_count_task::FixedCountBuilder;
pub use ids::{AttemptId, ExecutionId, LaneId, ScopeId, TaskSpecId};
pub use lane::{
    InvalidOrderingKey, InvalidPriority, Lane, LaneConfig, LaneLifetime, LaneStats, OrderingKey,
    Priority, SpawnError, SpawnOptions,
};
pub use listener::{listener, listener_with_error, FixedCountProgress, WorkListener};
pub use observation::{
    EventRecvError, EventStream, EventSubscribeError, ExecutorSnapshot, InvalidResourceLimits,
    LaneSnapshot, ResourceLimits, TaskEvent, TerminalRecord,
};
pub use periodic_task::PeriodicBuilder;
pub use range_interval_task::RangeIntervalBuilder;
pub use recurring::{
    MissedTickPolicy, PanicPolicy, RecurringBuildError, RecurringBuilder, RecurringFailure,
    RecurringSpec, RestartPolicy, ResumePolicy, Schedule, ScheduleDecisionContext, ScheduleMode,
    TickContext, TickFailurePolicy, TickOutcome,
};
pub use retry::{
    AttemptContext, Backoff, Jitter, RetryBuildError, RetryBuilder, RetryDecisionContext,
    RetryFailure, RetryPolicyStage, RetrySpec,
};
pub use scope::{
    RotationHandle, RotationOutcome, RotationPolicy, Scope, ScopeError, ScopeGeneration,
    ScopeSpawnError,
};
pub use service::{
    HealthStatus, HookOutcome, RestartTrigger, ServiceBuildError, ServiceBuilder, ServiceContext,
    ServiceFailure, ServiceHandle, ServiceSpec, ServiceStatus, ServiceWaitError,
};
pub use shutdown::{
    CallbackFailure, CleanupOutcome, CleanupPhase, CleanupProgress, ShutdownError, ShutdownHandle,
    ShutdownMode, ShutdownOptions, ShutdownOutcome, ShutdownReport, ShutdownReportSnapshot,
    ShutdownTimeoutAction, ShutdownWaitError, TaskShutdownProgressRecord, TaskShutdownRecord,
    WorkerFailure,
};
pub use slot::{
    InvalidSlotKey, ReplaceError, ReplaceHandle, ReplaceOutcome, ReplacePolicy, ReplaceWaitError,
    SlotKey, TaskSlot, TaskSlotSnapshot,
};
pub use task::{Task, TaskId};
pub use time_interval_task::TimeIntervalBuilder;
pub use work::Work;
pub use work_fn::{work, work_with_state, StatefulWorkFn};
pub use work_result::WorkResult;
