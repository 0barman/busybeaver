#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
//! # busybeaver
//!
//! `busybeaver` is an asynchronous task executor with configurable retry
//! strategies, purpose-built for Tokio. It runs your futures in Tokio tasks
//! independently of the submitting future and supports execution
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
//! ```no_run
//! use busybeaver::{work, Beaver, FixedCountBuilder, WorkResult};
//!
//! #[tokio::main]
//! async fn main() {
//!     let beaver = Beaver::new("default", 256);
//!
//!     let task = FixedCountBuilder::new(work(|| async {
//!         // do work, return WorkResult::Done(()) when finished
//!         WorkResult::NeedRetry
//!     }))
//!     .count(5)
//!     .build()
//!     .unwrap();
//!
//!     beaver.enqueue(task).await.unwrap();
//!
//!     // ... do other work ...
//!
//!     // Always destroy the beaver before letting it go out of scope so that
//!     // background workers and resident tasks are cleaned up.
//!     beaver.destroy().await.unwrap();
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
//! Listener and progress callback panics are isolated from both the lane and
//! the work error path; they do not cause a retry or change work completion.

#[cfg(target_family = "wasm")]
compile_error!(
    "busybeaver 0.3 does not support WebAssembly targets; use a native std target with Tokio"
);

mod beaver;
mod dam;
mod diagnostic;
mod dispatch;
mod error;
mod fixed_count_task;
mod listener;
mod observe;
mod periodic_task;
mod range_interval_task;
mod retry;
mod run_control;
mod schedule;
mod scheduler;
mod singleflight;
mod task;
#[cfg(test)]
mod test_log;
mod time_interval_task;
mod work;
mod work_fn;
mod work_result;

pub(crate) mod platform;

pub use beaver::{Beaver, ShutdownReport};
pub use dispatch::{
    DispatchOptions, DispatchQueue, DispatchQueueConfig, DispatchQueueConfigError,
    DispatchSubmitError, DispatchTaskHandle, QueueOverflowPolicy,
};
pub use error::{BeaverError, BeaverResult, RuntimeError, ValidationError};
pub use fixed_count_task::FixedCountBuilder;
pub use listener::{listener, listener_with_error, FixedCountProgress, WorkListener};
pub use observe::{
    EventReceiver, EventRecvError, MetricsHook, SchedulerSnapshot, TaskEvent, TaskEventKind,
};
pub use periodic_task::PeriodicBuilder;
pub use range_interval_task::RangeIntervalBuilder;
pub use retry::{
    Backoff, BackoffRange, JitterSource, RetryPolicy, RetryPolicyBuilder, RetryPolicyError,
};
pub use schedule::{
    FirstRun, MissedTickBehavior, RetryExhaustedAction, Schedule, ScheduleError, ScheduleTime,
    TaskControl,
};
pub use scheduler::{
    Backpressure, CancelOnDrop, CancelReason, Cancelled, ContextClosedError, EvictionPolicy,
    ExecutorErrorCode, GroupError, GroupState, Job, JobId, KeyStatus, KeyedSubmitError, LaneConfig,
    LaneError, PanicInfo, ReplaceError, ReplaceMode, ReplacePolicy, ReplaceTimeoutAction,
    ReusableJob, ScheduledJob, Scheduler, SchedulerBuildError, SchedulerBuilder,
    SchedulerShutdownReport, ShutdownPolicy, ShutdownWaitError, SubmissionFailure, SubmitError,
    TaskCommandError, TaskContext, TaskController, TaskGroup, TaskHandle, TaskObserver, TaskRunId,
    TaskSnapshot, TaskState, TaskTerminal, TimeoutScope, TriggerOutcome, TriggerPolicy,
    TriggerPolicyError, TrySubmitError,
};
pub use singleflight::Singleflight;
pub use task::{Task, TaskId};
pub use time_interval_task::TimeIntervalBuilder;
pub use work::Work;
pub use work_fn::work;
pub use work_result::WorkResult;
