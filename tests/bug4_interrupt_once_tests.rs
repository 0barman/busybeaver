//! # Bug 4 regression tests
//!
//! `destroy` / `release_thread_resource_by_name` previously fired `on_interrupt`
//! **twice** for the currently-running task: once synchronously inside
//! `release()` (via `Task::interrupt`) and once again when the running loop
//! detected the `interrupted` flag. `cancel_all` only ever fired it once.
//!
//! After the fix all cancellation paths fire `on_interrupt` for the running task
//! **exactly once**.

use busybeaver::{listener, work, Beaver, BeaverResult, PeriodicBuilder, WorkResult};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `destroy()` must fire on_interrupt exactly once for the running task.
#[tokio::test]
async fn destroy_fires_on_interrupt_exactly_once() -> BeaverResult<()> {
    let beaver = Beaver::new("bug4-destroy", 16)?;
    let interrupts = Arc::new(AtomicU32::new(0));
    let ic = Arc::clone(&interrupts);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                ic.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(60)).await; // let it start running

    beaver.destroy().await?;
    tokio::time::sleep(Duration::from_millis(120)).await; // let the loop detect + fire

    assert_eq!(
        interrupts.load(Ordering::SeqCst),
        1,
        "destroy must fire on_interrupt exactly once"
    );
    Ok(())
}

/// `release_thread_resource_by_name` must fire on_interrupt exactly once.
#[tokio::test]
async fn release_thread_resource_fires_on_interrupt_exactly_once() -> BeaverResult<()> {
    let beaver = Beaver::new("bug4-release", 16)?;
    let interrupts = Arc::new(AtomicU32::new(0));
    let ic = Arc::clone(&interrupts);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                ic.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()?;
    beaver
        .enqueue_on_new_thread(task, "lane", 16, false)
        .await?;
    tokio::time::sleep(Duration::from_millis(60)).await;

    beaver.release_thread_resource_by_name("lane").await?;
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        interrupts.load(Ordering::SeqCst),
        1,
        "release_thread_resource_by_name must fire on_interrupt exactly once"
    );
    beaver.destroy().await
}

/// Consistency: `cancel_all` also fires on_interrupt exactly once (reference path).
#[tokio::test]
async fn cancel_all_fires_on_interrupt_exactly_once() -> BeaverResult<()> {
    let beaver = Beaver::new("bug4-cancel", 16)?;
    let interrupts = Arc::new(AtomicU32::new(0));
    let ic = Arc::clone(&interrupts);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                ic.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(60)).await;

    beaver.cancel_all().await?;
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        interrupts.load(Ordering::SeqCst),
        1,
        "cancel_all must fire on_interrupt exactly once"
    );
    beaver.destroy().await
}

/// Intentionally-wrong (OLD behavior): destroy fires on_interrupt twice.
/// Post-fix it fires once, so asserting `== 2` must panic.
#[tokio::test]
#[should_panic(expected = "OLD-double-interrupt-should-not-hold")]
async fn wrong_destroy_double_interrupt_must_fail() {
    let beaver = Beaver::new("bug4-wrong", 16).expect("valid test executor");
    let interrupts = Arc::new(AtomicU32::new(0));
    let ic = Arc::clone(&interrupts);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                ic.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .build()
        .unwrap();
    beaver.enqueue(task).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;

    beaver.destroy().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    assert_eq!(
        interrupts.load(Ordering::SeqCst),
        2,
        "OLD-double-interrupt-should-not-hold"
    );
}
