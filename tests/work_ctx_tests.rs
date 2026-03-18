//! # Example tests: passing external `&Arc<Context>` into `work()`
//!
//! `work` requires `Fn() -> Fut`; the task may run many times. You cannot attach
//! `async { ctx_ref.task() }` directly to `move ||` (the `Future` would borrow
//! closure captures, which conflicts with `Fn`). Recommended pattern:
//!
//! ```ignore
//! let a = Arc::clone(ctx);
//! let b = Arc::clone(ctx2);
//! work(move || {
//!     let c1 = Arc::clone(&a);
//!     let c2 = Arc::clone(&b);
//!     async move {
//!         c1.task();
//!         c2.task();
//!         WorkResult::NeedRetry // or Done
//!     }
//! })
//! ```
//!
//! The cases below cover **Periodic**, **TimeInterval**, **FixedCount**, and **RangeInterval**.

use busybeaver::{
    listener, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder, RangeIntervalBuilder,
    TimeIntervalBuilder, WorkResult,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Stand-in for a typical app `Context`: `task()` increments a counter for assertions.
#[derive(Default)]
struct WorkCtx {
    task_calls: AtomicU32,
}

impl WorkCtx {
    fn task(&self) {
        self.task_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn task_calls(&self) -> u32 {
        self.task_calls.load(Ordering::SeqCst)
    }
}

// -----------------------------------------------------------------------------
// Periodic: take `&Arc` from an async entry point, then pass into `work`
// -----------------------------------------------------------------------------

async fn enqueue_periodic_with_ctx(ctx: &Arc<WorkCtx>, ctx2: &Arc<WorkCtx>) -> BeaverResult<()> {
    let beaver = Beaver::new("work_ctx_periodic", 256);
    let a = Arc::clone(ctx);
    let b = Arc::clone(ctx2);

    let task = PeriodicBuilder::new(work(move || {
        let c1 = Arc::clone(&a);
        let c2 = Arc::clone(&b);
        async move {
            c1.task();
            c2.task();
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn periodic_work_passes_external_arc_ctx() -> BeaverResult<()> {
    let ctx = Arc::new(WorkCtx::default());
    let ctx2 = Arc::new(WorkCtx::default());
    enqueue_periodic_with_ctx(&ctx, &ctx2).await?;
    assert!(ctx.task_calls() >= 1);
    assert!(ctx2.task_calls() >= 1);
    Ok(())
}

// -----------------------------------------------------------------------------
// TimeInterval
// -----------------------------------------------------------------------------

async fn enqueue_time_interval_with_ctx(
    ctx: &Arc<WorkCtx>,
    ctx2: &Arc<WorkCtx>,
) -> BeaverResult<()> {
    let beaver = Beaver::new("work_ctx_time_interval", 256);
    let a = Arc::clone(ctx);
    let b = Arc::clone(ctx2);

    let task = TimeIntervalBuilder::new(work(move || {
        let c1 = Arc::clone(&a);
        let c2 = Arc::clone(&b);
        async move {
            c1.task();
            c2.task();
            WorkResult::NeedRetry
        }
    }))
    .intervals_millis([0, 0])
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn time_interval_work_passes_external_arc_ctx() -> BeaverResult<()> {
    let ctx = Arc::new(WorkCtx::default());
    let ctx2 = Arc::new(WorkCtx::default());
    enqueue_time_interval_with_ctx(&ctx, &ctx2).await?;
    // Two intervals => two runs, one `task()` each per run
    assert_eq!(ctx.task_calls(), 2);
    assert_eq!(ctx2.task_calls(), 2);
    Ok(())
}

// -----------------------------------------------------------------------------
// FixedCount
// -----------------------------------------------------------------------------

async fn enqueue_fixed_count_with_ctx(ctx: &Arc<WorkCtx>, ctx2: &Arc<WorkCtx>) -> BeaverResult<()> {
    let beaver = Beaver::new("work_ctx_fixed_count", 256);
    let a = Arc::clone(ctx);
    let b = Arc::clone(ctx2);

    let task = FixedCountBuilder::new(work(move || {
        let c1 = Arc::clone(&a);
        let c2 = Arc::clone(&b);
        async move {
            c1.task();
            c2.task();
            WorkResult::NeedRetry
        }
    }))
    .count(4)
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn fixed_count_work_passes_external_arc_ctx() -> BeaverResult<()> {
    let ctx = Arc::new(WorkCtx::default());
    let ctx2 = Arc::new(WorkCtx::default());
    enqueue_fixed_count_with_ctx(&ctx, &ctx2).await?;
    assert_eq!(ctx.task_calls(), 4);
    assert_eq!(ctx2.task_calls(), 4);
    Ok(())
}

// -----------------------------------------------------------------------------
// RangeInterval with `listener` (closer to real usage)
// -----------------------------------------------------------------------------

async fn enqueue_range_interval_with_ctx(
    ctx: &Arc<WorkCtx>,
    ctx2: &Arc<WorkCtx>,
) -> BeaverResult<()> {
    let beaver = Beaver::new("work_ctx_range_interval", 256);
    let a = Arc::clone(ctx);
    let b = Arc::clone(ctx2);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c1 = Arc::clone(&a);
            let c2 = Arc::clone(&b);
            async move {
                c1.task();
                c2.task();
                WorkResult::NeedRetry
            }
        }),
        3,
    )
    .add_range(0, 2, Duration::ZERO)
    .listener(listener(|| {}, || {}))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn range_interval_work_passes_external_arc_ctx() -> BeaverResult<()> {
    let ctx = Arc::new(WorkCtx::default());
    let ctx2 = Arc::new(WorkCtx::default());
    enqueue_range_interval_with_ctx(&ctx, &ctx2).await?;
    assert_eq!(ctx.task_calls(), 3);
    assert_eq!(ctx2.task_calls(), 3);
    Ok(())
}

// -----------------------------------------------------------------------------
// Same `Arc` passed as two parameters (like `test5(&ctx, &ctx)` in main)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn fixed_count_same_arc_twice_as_external_ctx() -> BeaverResult<()> {
    let beaver = Beaver::new("work_ctx_same_arc", 256);
    let ctx = Arc::new(WorkCtx::default());
    let a = Arc::clone(&ctx);
    let b = Arc::clone(&ctx);

    let task = FixedCountBuilder::new(work(move || {
        let c1 = Arc::clone(&a);
        let c2 = Arc::clone(&b);
        async move {
            c1.task();
            c2.task();
            WorkResult::NeedRetry
        }
    }))
    .count(2)
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    beaver.cancel_all().await?;
    beaver.destroy().await?;

    // Each run increments both c1 and c2; same Arc => +2 per run
    assert_eq!(ctx.task_calls(), 4);
    Ok(())
}
