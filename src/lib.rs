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
//! on the same lane. **Listener and progress callbacks themselves should not
//! panic** – they run on the executor task and a panic inside them is *not*
//! isolated by the framework.

mod beaver;
mod dam;
mod error;
mod fixed_count_task;
mod listener;
mod periodic_task;
mod range_interval_task;
mod task;
mod time_interval_task;
mod work;
mod work_fn;
mod work_result;

pub(crate) mod platform;

pub use beaver::Beaver;
pub use error::{BeaverError, BeaverResult, RuntimeError};
pub use fixed_count_task::FixedCountBuilder;
pub use listener::{listener, listener_with_error, FixedCountProgress, WorkListener};
pub use periodic_task::PeriodicBuilder;
pub use range_interval_task::RangeIntervalBuilder;
pub use task::{Task, TaskId};
pub use time_interval_task::TimeIntervalBuilder;
pub use work::Work;
pub use work_fn::work;
pub use work_result::WorkResult;
