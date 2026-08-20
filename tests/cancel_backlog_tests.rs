//! # Backlog Cancellation Tests
//!
//! Regression tests for the fix to: *`cancel_all` / `destroy` cannot cancel
//! tasks that are already sitting in the queue behind a running task.*
//!
//! The contract being verified:
//! - `cancel_all`, `destroy`, `cancel_non_long_resident` and
//!   `release_thread_resource_by_name` cancel **every task already enqueued**
//!   at the moment of the call, including tasks that are queued behind a
//!   currently-running ("blocker") task. Those queued tasks must **never run**
//!   and must receive exactly one `on_interrupt`.
//! - Tasks enqueued **after** `cancel_all` (the documented "still executed"
//!   case) are unaffected and run normally.

use busybeaver::{
    listener, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder, WorkResult,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Enqueues a "blocker" that occupies the lane for `hold` once the worker picks
/// it up, so that subsequently enqueued tasks are forced to wait in the queue.
/// Returns once the blocker is confirmed to be running.
async fn enqueue_running_blocker(beaver: &Beaver, hold: Duration) -> BeaverResult<()> {
    let started = Arc::new(AtomicU32::new(0));
    let s = Arc::clone(&started);
    let blocker = FixedCountBuilder::new(work(move || {
        let s = Arc::clone(&s);
        async move {
            s.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(hold).await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(blocker).await?;

    // Wait until the worker has actually started the blocker, so the next
    // enqueue lands in the queue rather than being picked up immediately.
    for _ in 0..50 {
        if started.load(Ordering::SeqCst) > 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("blocker never started running");
}

/// A periodic victim task that records how many times its work actually ran and
/// how many times it was interrupted.
fn victim(
    run_count: Arc<AtomicU32>,
    interrupt_count: Arc<AtomicU32>,
) -> BeaverResult<Arc<busybeaver::Task>> {
    let rc = Arc::clone(&run_count);
    let ic = Arc::clone(&interrupt_count);
    PeriodicBuilder::new(work(move || {
        let rc = Arc::clone(&rc);
        async move {
            rc.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(10))
    .listener(listener(
        || {},
        move || {
            ic.fetch_add(1, Ordering::SeqCst);
        },
    ))
    .build()
}

/// Core bug: a task queued behind a running blocker must be cancelled by
/// `cancel_all` and must never execute.
#[tokio::test]
async fn cancel_all_cancels_task_queued_behind_blocker() -> BeaverResult<()> {
    let beaver = Beaver::new("backlog-cancel-all", 64)?;

    enqueue_running_blocker(&beaver, Duration::from_millis(200)).await?;

    let run_count = Arc::new(AtomicU32::new(0));
    let interrupt_count = Arc::new(AtomicU32::new(0));
    let v = victim(Arc::clone(&run_count), Arc::clone(&interrupt_count))?;

    // v is now queued behind the still-running blocker.
    beaver.enqueue(Arc::clone(&v)).await?;

    beaver.cancel_all().await?;

    // Give the blocker time to finish and the worker time to drain the queue.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        run_count.load(Ordering::SeqCst),
        0,
        "queued task must NOT run after cancel_all"
    );
    assert!(v.interrupted(), "queued task must be marked interrupted");
    assert_eq!(
        interrupt_count.load(Ordering::SeqCst),
        1,
        "queued task must receive exactly one on_interrupt"
    );

    beaver.destroy().await
}

/// `destroy` must likewise cancel a task queued behind a running blocker.
#[tokio::test]
async fn destroy_cancels_task_queued_behind_blocker() -> BeaverResult<()> {
    let beaver = Beaver::new("backlog-destroy", 64)?;

    enqueue_running_blocker(&beaver, Duration::from_millis(200)).await?;

    let run_count = Arc::new(AtomicU32::new(0));
    let interrupt_count = Arc::new(AtomicU32::new(0));
    let v = victim(Arc::clone(&run_count), Arc::clone(&interrupt_count))?;

    beaver.enqueue(Arc::clone(&v)).await?;

    beaver.destroy().await?;

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        run_count.load(Ordering::SeqCst),
        0,
        "queued task must NOT run after destroy"
    );
    assert!(v.interrupted(), "queued task must be marked interrupted");

    Ok(())
}

/// Multiple queued tasks behind a blocker must all be cancelled, none may run,
/// and each must receive exactly one `on_interrupt`.
#[tokio::test]
async fn cancel_all_interrupts_all_queued_tasks_exactly_once() -> BeaverResult<()> {
    let beaver = Beaver::new("backlog-many", 64)?;

    enqueue_running_blocker(&beaver, Duration::from_millis(200)).await?;

    let run_count = Arc::new(AtomicU32::new(0));
    let interrupt_count = Arc::new(AtomicU32::new(0));

    for _ in 0..5 {
        let v = victim(Arc::clone(&run_count), Arc::clone(&interrupt_count))?;
        beaver.enqueue(v).await?;
    }

    beaver.cancel_all().await?;

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        run_count.load(Ordering::SeqCst),
        0,
        "no queued task may run after cancel_all"
    );
    assert_eq!(
        interrupt_count.load(Ordering::SeqCst),
        5,
        "every queued task must receive exactly one on_interrupt"
    );

    beaver.destroy().await
}

/// `cancel_non_long_resident` must cancel backlog on a non-resident named lane.
#[tokio::test]
async fn cancel_non_long_resident_cancels_backlog_on_named_lane() -> BeaverResult<()> {
    let beaver = Beaver::new("backlog-non-resident", 64)?;

    // Put a blocker on a named, non-resident lane.
    let started = Arc::new(AtomicU32::new(0));
    let s = Arc::clone(&started);
    let blocker = FixedCountBuilder::new(work(move || {
        let s = Arc::clone(&s);
        async move {
            s.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver
        .enqueue_on_new_thread(blocker, "lane-a", 64, false)
        .await?;
    for _ in 0..50 {
        if started.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        started.load(Ordering::SeqCst) > 0,
        "blocker must be running"
    );

    let run_count = Arc::new(AtomicU32::new(0));
    let interrupt_count = Arc::new(AtomicU32::new(0));
    let v = victim(Arc::clone(&run_count), Arc::clone(&interrupt_count))?;
    beaver
        .enqueue_on_new_thread(Arc::clone(&v), "lane-a", 64, false)
        .await?;

    beaver.cancel_non_long_resident().await?;

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        run_count.load(Ordering::SeqCst),
        0,
        "queued task on non-resident lane must NOT run after cancel_non_long_resident"
    );
    assert!(v.interrupted(), "queued task must be marked interrupted");

    beaver.destroy().await
}

/// `release_thread_resource_by_name` must cancel backlog on the released lane.
#[tokio::test]
async fn release_thread_resource_cancels_backlog() -> BeaverResult<()> {
    let beaver = Beaver::new("backlog-release", 64)?;

    let started = Arc::new(AtomicU32::new(0));
    let s = Arc::clone(&started);
    let blocker = FixedCountBuilder::new(work(move || {
        let s = Arc::clone(&s);
        async move {
            s.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver
        .enqueue_on_new_thread(blocker, "lane-b", 64, false)
        .await?;
    for _ in 0..50 {
        if started.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        started.load(Ordering::SeqCst) > 0,
        "blocker must be running"
    );

    let run_count = Arc::new(AtomicU32::new(0));
    let interrupt_count = Arc::new(AtomicU32::new(0));
    let v = victim(Arc::clone(&run_count), Arc::clone(&interrupt_count))?;
    beaver
        .enqueue_on_new_thread(Arc::clone(&v), "lane-b", 64, false)
        .await?;

    beaver.release_thread_resource_by_name("lane-b").await?;

    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        run_count.load(Ordering::SeqCst),
        0,
        "queued task on released lane must NOT run"
    );
    assert!(v.interrupted(), "queued task must be marked interrupted");

    beaver.destroy().await
}

/// The documented contract must be preserved: a task enqueued *after*
/// `cancel_all` returns is still executed.
#[tokio::test]
async fn enqueue_after_cancel_all_still_runs() -> BeaverResult<()> {
    let beaver = Beaver::new("after-cancel-runs", 64)?;

    // Backlog that should be cancelled.
    enqueue_running_blocker(&beaver, Duration::from_millis(100)).await?;
    let dropped_run = Arc::new(AtomicU32::new(0));
    let dropped_intr = Arc::new(AtomicU32::new(0));
    let dropped = victim(Arc::clone(&dropped_run), Arc::clone(&dropped_intr))?;
    beaver.enqueue(dropped).await?;

    beaver.cancel_all().await?;

    // Now enqueue a fresh task after cancel_all; it must run.
    let fresh_run = Arc::new(AtomicU32::new(0));
    let fresh_intr = Arc::new(AtomicU32::new(0));
    let fresh = victim(Arc::clone(&fresh_run), Arc::clone(&fresh_intr))?;
    beaver.enqueue(fresh).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        fresh_run.load(Ordering::SeqCst) > 0,
        "task enqueued after cancel_all must still run"
    );
    assert_eq!(
        dropped_run.load(Ordering::SeqCst),
        0,
        "backlog task present at cancel_all time must not run"
    );

    beaver.destroy().await
}
