//! # Bug 3 regression tests
//!
//! Contract after the fix:
//! - `on_complete` fires when the work returns [`WorkResult::Done`] — for **all**
//!   task types, including a `Done` on an intermediate attempt of a bounded task.
//! - A bounded task (fixed-count / time-interval / range-interval) that runs every
//!   permitted attempt and never succeeds reports
//!   `on_error(RuntimeError::RetriesExhausted)` — **not** `on_complete`.
//! - Periodic tasks: `Done` -> `on_complete` (unchanged); they never exhaust.
//!
//! Includes deliberately-wrong expectations (pinned with `#[should_panic]`) that
//! encode the OLD semantics and must now fail, proving the fix took effect.

use busybeaver::{
    listener_with_error, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder,
    RangeIntervalBuilder, RuntimeError, TimeIntervalBuilder, WorkListener, WorkResult,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Counts {
    complete: AtomicU32,
    interrupt: AtomicU32,
    exhausted: AtomicU32,
    other_error: AtomicU32,
}

impl Counts {
    fn complete(&self) -> u32 {
        self.complete.load(Ordering::SeqCst)
    }
    fn interrupt(&self) -> u32 {
        self.interrupt.load(Ordering::SeqCst)
    }
    fn exhausted(&self) -> u32 {
        self.exhausted.load(Ordering::SeqCst)
    }
    fn other_error(&self) -> u32 {
        self.other_error.load(Ordering::SeqCst)
    }
}

/// Builds a listener that records on_complete / on_interrupt and splits on_error
/// into RetriesExhausted vs. anything else.
fn tracking_listener(c: Arc<Counts>) -> Arc<dyn WorkListener> {
    let c1 = Arc::clone(&c);
    let c2 = Arc::clone(&c);
    let c3 = Arc::clone(&c);
    let l: Arc<dyn WorkListener> = listener_with_error(
        move || {
            c1.complete.fetch_add(1, Ordering::SeqCst);
        },
        move || {
            c2.interrupt.fetch_add(1, Ordering::SeqCst);
        },
        move |e: RuntimeError| match e {
            RuntimeError::RetriesExhausted => {
                c3.exhausted.fetch_add(1, Ordering::SeqCst);
            }
            _ => {
                c3.other_error.fetch_add(1, Ordering::SeqCst);
            }
        },
    );
    l
}

// =============================================================================
// SUCCESS (Done) -> on_complete, for every task type
// =============================================================================

#[tokio::test]
async fn fixed_count_done_midway_fires_on_complete_only() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-fc-done", 16);
    let c = Arc::new(Counts::default());

    // count=5 but Done on the 2nd attempt: success before retries are exhausted.
    let calls = Arc::new(AtomicU32::new(0));
    let task = FixedCountBuilder::new(work(move || {
        let calls = Arc::clone(&calls);
        async move {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= 2 {
                WorkResult::Done(())
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .count(5)
    .listener(tracking_listener(Arc::clone(&c)))
    .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(c.complete(), 1, "Done must fire on_complete exactly once");
    assert_eq!(
        c.exhausted(),
        0,
        "successful task must not report RetriesExhausted"
    );
    assert_eq!(c.interrupt(), 0);
    assert_eq!(c.other_error(), 0);
    beaver.destroy().await
}

#[tokio::test]
async fn time_interval_done_fires_on_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-ti-done", 16);
    let c = Arc::new(Counts::default());

    let task = TimeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }))
        .intervals_millis([0, 0, 0])
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(c.complete(), 1);
    assert_eq!(c.exhausted(), 0);
    beaver.destroy().await
}

#[tokio::test]
async fn range_interval_done_fires_on_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-ri-done", 16);
    let c = Arc::new(Counts::default());

    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 3)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(c.complete(), 1);
    assert_eq!(c.exhausted(), 0);
    beaver.destroy().await
}

#[tokio::test]
async fn periodic_done_fires_on_complete_regression() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-periodic-done", 16);
    let c = Arc::new(Counts::default());

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(10))
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(c.complete(), 1, "periodic Done still fires on_complete");
    assert_eq!(c.exhausted(), 0, "periodic never exhausts");
    beaver.destroy().await
}

// =============================================================================
// EXHAUSTION -> on_error(RetriesExhausted), NOT on_complete
// =============================================================================

#[tokio::test]
async fn fixed_count_exhausted_fires_on_error_not_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-fc-exhaust", 16);
    let c = Arc::new(Counts::default());

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(3)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        c.exhausted(),
        1,
        "exhaustion must report RetriesExhausted once"
    );
    assert_eq!(c.complete(), 0, "exhaustion must NOT fire on_complete");
    assert_eq!(c.interrupt(), 0);
    beaver.destroy().await
}

#[tokio::test]
async fn time_interval_exhausted_fires_on_error_not_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-ti-exhaust", 16);
    let c = Arc::new(Counts::default());

    let task = TimeIntervalBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .intervals_millis([0, 0])
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(c.exhausted(), 1);
    assert_eq!(c.complete(), 0);
    beaver.destroy().await
}

#[tokio::test]
async fn range_interval_exhausted_fires_on_error_not_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-ri-exhaust", 16);
    let c = Arc::new(Counts::default());

    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::NeedRetry }), 3)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(c.exhausted(), 1);
    assert_eq!(c.complete(), 0);
    beaver.destroy().await
}

/// `total_retries = 0` runs no attempts: neither success nor exhaustion.
#[tokio::test]
async fn range_interval_total_zero_fires_nothing() -> BeaverResult<()> {
    let beaver = Beaver::new("bug3-ri-zero", 16);
    let c = Arc::new(Counts::default());

    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::NeedRetry }), 0)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(c.complete(), 0);
    assert_eq!(c.exhausted(), 0);
    assert_eq!(c.interrupt(), 0);
    beaver.destroy().await
}

// =============================================================================
// INTENTIONALLY-WRONG expectations (OLD semantics) — must now fail.
// =============================================================================

/// OLD (wrong) belief: exhaustion fires on_complete. Post-fix it does not, so
/// asserting `complete >= 1` here must panic.
#[tokio::test]
#[should_panic(expected = "OLD-on_complete-on-exhaustion-should-not-hold")]
async fn wrong_exhaustion_fires_on_complete_must_fail() {
    let beaver = Beaver::new("bug3-wrong-1", 16);
    let c = Arc::new(Counts::default());

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(3)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()
        .unwrap();
    beaver.enqueue(task).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = beaver.destroy().await;

    assert!(
        c.complete() >= 1,
        "OLD-on_complete-on-exhaustion-should-not-hold (complete={}, exhausted={})",
        c.complete(),
        c.exhausted()
    );
}

/// OLD (wrong) belief: a successful (Done) bounded task does NOT fire on_complete.
/// Post-fix it does, so asserting `complete == 0` here must panic.
#[tokio::test]
#[should_panic(expected = "OLD-no-on_complete-on-done-should-not-hold")]
async fn wrong_done_does_not_fire_on_complete_must_fail() {
    let beaver = Beaver::new("bug3-wrong-2", 16);
    let c = Arc::new(Counts::default());

    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(10)
        .listener(tracking_listener(Arc::clone(&c)))
        .build()
        .unwrap();
    beaver.enqueue(task).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = beaver.destroy().await;

    assert_eq!(
        c.complete(),
        0,
        "OLD-no-on_complete-on-done-should-not-hold"
    );
}
