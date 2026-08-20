//! # Bug 7 regression tests
//!
//! `destroy` used to only *signal* lanes to stop and return immediately, without
//! waiting for the background workers to actually terminate. It now stores each
//! worker's join handle and awaits termination (bounded by an internal timeout),
//! so when `destroy().await` returns the workers have really exited.
//!
//! Observable: with a lane whose current `work.execute()` is a long (200ms)
//! in-flight iteration, `destroy` blocks until that iteration finishes and the
//! worker exits — so it takes noticeably longer than the old signal-and-return.

use busybeaver::{work, Beaver, BeaverResult, PeriodicBuilder, WorkResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Builds a periodic task (interval 0) whose every iteration takes ~200ms, and a
/// flag that is set as soon as an iteration starts.
fn slow_periodic(started: Arc<AtomicBool>) -> BeaverResult<Arc<busybeaver::Task>> {
    PeriodicBuilder::new(work(move || {
        let s = Arc::clone(&started);
        async move {
            s.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::ZERO)
    .build()
}

async fn wait_until(flag: &Arc<AtomicBool>) {
    for _ in 0..100 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("work iteration never started");
}

/// `destroy` awaits the worker: with a 200ms in-flight iteration it blocks until
/// the iteration finishes and the worker exits.
#[tokio::test]
async fn destroy_waits_for_worker_to_exit() -> BeaverResult<()> {
    let beaver = Beaver::new("bug7-wait", 16)?;
    let started = Arc::new(AtomicBool::new(false));
    beaver.enqueue(slow_periodic(Arc::clone(&started))?).await?;
    wait_until(&started).await;

    let t0 = Instant::now();
    beaver.destroy().await?;
    let elapsed = t0.elapsed();

    assert!(
        elapsed >= Duration::from_millis(100),
        "destroy should wait for the in-flight work + worker exit (elapsed {:?})",
        elapsed
    );
    Ok(())
}

/// `destroy` must still return well within the timeout for a fast lane (no hang).
#[tokio::test]
async fn destroy_completes_quickly_for_fast_lane() -> BeaverResult<()> {
    let beaver = Beaver::new("bug7-quick", 16)?;
    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(10))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(40)).await;

    let t0 = Instant::now();
    beaver.destroy().await?;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "destroy must not hang for a fast lane (elapsed {:?})",
        t0.elapsed()
    );
    Ok(())
}

/// Intentionally-wrong (OLD behavior): destroy returns immediately, before the
/// worker has stopped. Post-fix it waits ~200ms, so asserting it returned in
/// under 50ms must panic.
#[tokio::test]
#[should_panic(expected = "OLD-destroy-returns-before-worker-stops")]
async fn wrong_destroy_returns_immediately_must_fail() {
    let beaver = Beaver::new("bug7-wrong", 16).expect("valid test executor");
    let started = Arc::new(AtomicBool::new(false));
    beaver
        .enqueue(slow_periodic(Arc::clone(&started)).unwrap())
        .await
        .unwrap();
    wait_until(&started).await;

    let t0 = Instant::now();
    beaver.destroy().await.unwrap();
    let elapsed = t0.elapsed();

    assert!(
        elapsed < Duration::from_millis(50),
        "OLD-destroy-returns-before-worker-stops (elapsed {:?})",
        elapsed
    );
}
