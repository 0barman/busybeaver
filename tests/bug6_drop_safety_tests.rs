//! # Bug 6 regression tests
//!
//! `Beaver`/`Dam` had no `Drop`. Dropping a `Beaver` that still had a running
//! periodic task (without calling `destroy()`) leaked it: the worker stayed
//! parked on `join.await` of the forever-looping task. `Drop for Dam` is now a
//! safety net that signals such tasks to stop.
//!
//! Each periodic task here uses a 10ms interval (an `await` point), so it yields
//! cooperatively and does not starve the single-threaded test runtime.

use busybeaver::{work, Beaver, BeaverResult, PeriodicBuilder, WorkResult};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Dropping the Beaver (default lane) without destroy() must stop the periodic task.
#[tokio::test]
async fn drop_without_destroy_stops_periodic_task() -> BeaverResult<()> {
    let counter = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&counter);

    let beaver = Beaver::new("bug6-drop", 16)?;
    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&c);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(50)).await; // let it run a few times

    drop(beaver); // forgot destroy(): the Drop safety net must stop the task

    tokio::time::sleep(Duration::from_millis(40)).await; // let the in-flight iteration settle
    let c1 = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await; // ~15 more runs if it had leaked
    let c2 = counter.load(Ordering::SeqCst);

    assert!(
        c2 - c1 <= 1,
        "Beaver Drop must stop the periodic task (after drop c1={}, later c2={})",
        c1,
        c2
    );
    Ok(())
}

/// Drop must also stop tasks on a named lane (here a long-resident one).
#[tokio::test]
async fn drop_stops_named_long_resident_lane() -> BeaverResult<()> {
    let counter = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&counter);

    let beaver = Beaver::new("bug6-named", 16)?;
    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&c);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;
    beaver.enqueue_on_new_thread(task, "lane", 16, true).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(beaver);

    tokio::time::sleep(Duration::from_millis(40)).await;
    let c1 = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let c2 = counter.load(Ordering::SeqCst);

    assert!(
        c2 - c1 <= 1,
        "Drop must stop named-lane tasks too (c1={}, c2={})",
        c1,
        c2
    );
    Ok(())
}

/// Intentionally-wrong (OLD behavior): drop without destroy leaks the task and it
/// keeps running. Post-fix the task stops, so asserting it kept running must panic.
#[tokio::test]
#[should_panic(expected = "OLD-leak-should-not-hold")]
async fn wrong_drop_leaks_periodic_task_must_fail() {
    let counter = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&counter);

    let beaver = Beaver::new("bug6-wrong", 16).expect("valid test executor");
    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&c);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(10))
    .build()
    .unwrap();
    beaver.enqueue(task).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(beaver);

    tokio::time::sleep(Duration::from_millis(40)).await;
    let c1 = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let c2 = counter.load(Ordering::SeqCst);

    assert!(
        c2 > c1 + 5,
        "OLD-leak-should-not-hold (c1={}, c2={})",
        c1,
        c2
    );
}
