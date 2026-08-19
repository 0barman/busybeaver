//! # busybeaver
//!
//! `busybeaver` is an asynchronous task executor with configurable retry
//! strategies, purpose-built for Rust async runtimes such as Tokio. It runs
//! your futures independently of your worker threads and supports execution
//! strategies based on counts, time intervals, range-based intervals, and
//! fixed-period polling.
//!
//! ## At a glance
//!
//! - [`FixedCountBuilder`] – retry up to a fixed number of attempts.
//! - [`TimeIntervalBuilder`] – retry with an explicit list of intervals.
//! - [`RangeIntervalBuilder`] – retry with different intervals per attempt range.
//! - [`PeriodicBuilder`] – run a task periodically until completion or interruption.
//!
//! All strategies share the same execution model: tasks are submitted to a
//! [`Beaver`] which dispatches them to one of its execution lanes (a default
//! lane plus optional named lanes). Each lane processes tasks **serially**;
//! different lanes run **in parallel**.
//!
//! ## Quick start
//!
//! ```ignore
//! use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let beaver = Beaver::new("default", 256);
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
//! **Listener and progress callbacks themselves should not panic** – they run
//! on the executor task and a panic inside them is *not* isolated by the framework.

mod beaver;
mod dam;
mod error;
mod execution;
mod fixed_count_task;
mod ids;
mod lane;
mod listener;
mod periodic_task;
mod range_interval_task;
mod retry;
mod shutdown;
mod task;
mod time_interval_task;
mod work;
mod work_fn;
mod work_result;

pub(crate) mod platform;

pub use beaver::Beaver;
pub use error::{BeaverError, BeaverResult, RuntimeError};
pub use execution::{
    BatchCancelRecord, BatchCancelReport, CancelOnDrop, CancelReason, CancelRequestOutcome,
    CancelWait, Cancelled, ChildHandle, ExecutorStopReason, JoinResultError, PanicSource,
    SpawnChildError, StopCauseSummary, TaskControlHandle, TaskExit, TaskExitSummary, TaskFailure,
    TaskHandle, TaskSelector, TaskSnapshot, TaskSpec, TaskState, WorkContext,
};
pub use fixed_count_task::FixedCountBuilder;
pub use ids::{AttemptId, ExecutionId, LaneId, ScopeId, TaskSpecId};
pub use lane::{Lane, LaneConfig, LaneLifetime, LaneStats, SpawnError};
pub use listener::{listener, listener_with_error, FixedCountProgress, WorkListener};
pub use periodic_task::PeriodicBuilder;
pub use range_interval_task::RangeIntervalBuilder;
pub use retry::{
    AttemptContext, Backoff, Jitter, RetryBuildError, RetryBuilder, RetryDecisionContext,
    RetryFailure, RetryPolicyStage, RetrySpec,
};
pub use shutdown::{
    CallbackFailure, CleanupOutcome, CleanupPhase, CleanupProgress, ShutdownError, ShutdownHandle,
    ShutdownMode, ShutdownOptions, ShutdownOutcome, ShutdownReport, ShutdownReportSnapshot,
    ShutdownTimeoutAction, ShutdownWaitError, TaskShutdownProgressRecord, TaskShutdownRecord,
    WorkerFailure,
};
pub use task::{Task, TaskId};
pub use time_interval_task::TimeIntervalBuilder;
pub use work::Work;
pub use work_fn::work;
pub use work_result::WorkResult;
