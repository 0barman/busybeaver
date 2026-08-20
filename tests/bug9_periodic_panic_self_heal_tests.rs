//! # Bug 9 regression tests
//!
//! A panic inside a periodic task's `work` used to unwind the whole loop and
//! kill the task permanently (only one `on_error`, no restart). Now a periodic
//! task self-heals: the panic is reported via `on_error` and the loop resumes on
//! the next period. Bounded tasks (fixed-count etc.) are unchanged: a panic fires
//! `on_error` once and stops.
//!
//! Note: `work` panics happen in isolated spawned tasks and are caught by the
//! framework, so they never fail these tests; only the explicit assertions do.

use busybeaver::{
    listener_with_error, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder,
    RuntimeError, WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Periodic task panics 3 times, then succeeds: it must keep running across the
/// panics (each reported via on_error) and eventually fire on_complete.
#[tokio::test]
async fn periodic_self_heals_after_panics_then_completes() -> BeaverResult<()> {
    let beaver = Beaver::new("bug9-heal", 16)?;
    let attempts = Arc::new(AtomicU32::new(0));
    let errors = Arc::new(AtomicU32::new(0));
    let completed = Arc::new(AtomicBool::new(false));

    let a = Arc::clone(&attempts);
    let e = Arc::clone(&errors);
    let comp = Arc::clone(&completed);

    let task = PeriodicBuilder::new(work(move || {
        let a = Arc::clone(&a);
        async move {
            let n = a.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 3 {
                panic!("boom #{}", n);
            }
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .listener(listener_with_error(
        move || comp.store(true, Ordering::SeqCst),
        || {},
        move |_e: RuntimeError| {
            e.fetch_add(1, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        attempts.load(Ordering::SeqCst) >= 4,
        "task should resume past the panics (attempts={})",
        attempts.load(Ordering::SeqCst)
    );
    assert!(
        errors.load(Ordering::SeqCst) >= 3,
        "each panic should fire on_error (errors={})",
        errors.load(Ordering::SeqCst)
    );
    assert!(
        completed.load(Ordering::SeqCst),
        "the task should eventually complete (on_complete)"
    );
    beaver.destroy().await
}

/// A periodic task that always panics keeps self-healing: on_error fires
/// repeatedly (not just once).
#[tokio::test]
async fn periodic_keeps_self_healing_on_repeated_panics() -> BeaverResult<()> {
    let beaver = Beaver::new("bug9-repeat", 16)?;
    let errors = Arc::new(AtomicU32::new(0));
    let e = Arc::clone(&errors);

    let task = PeriodicBuilder::new(work(|| async {
        panic!("always boom");
    }))
    .interval(Duration::from_millis(20))
    .listener(listener_with_error(
        || {},
        || {},
        move |_e: RuntimeError| {
            e.fetch_add(1, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let count = errors.load(Ordering::SeqCst);
    assert!(
        count > 1,
        "repeated panics should keep firing on_error (self-heal), got {}",
        count
    );
    beaver.destroy().await
}

/// Regression: a bounded (fixed-count) task does NOT self-heal — a panic fires
/// on_error exactly once and the task stops (it does not retry the panic).
#[tokio::test]
async fn fixed_count_panic_does_not_self_heal() -> BeaverResult<()> {
    let beaver = Beaver::new("bug9-bounded", 16)?;
    let attempts = Arc::new(AtomicU32::new(0));
    let errors = Arc::new(AtomicU32::new(0));
    let a = Arc::clone(&attempts);
    let e = Arc::clone(&errors);

    let task = FixedCountBuilder::new(work(move || {
        let a = Arc::clone(&a);
        async move {
            a.fetch_add(1, Ordering::SeqCst);
            panic!("bounded boom");
        }
    }))
    .count(5)
    .listener(listener_with_error(
        || {},
        || {},
        move |_e: RuntimeError| {
            e.fetch_add(1, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "bounded task panic must not retry/self-heal"
    );
    assert_eq!(
        errors.load(Ordering::SeqCst),
        1,
        "bounded task panic fires on_error exactly once"
    );
    beaver.destroy().await
}

/// Intentionally-wrong (OLD behavior): a periodic task dies after its first
/// panic, so on_error fires exactly once. Post-fix it self-heals and fires
/// on_error many times, so asserting `== 1` must panic.
#[tokio::test]
#[should_panic(expected = "OLD-periodic-dies-on-first-panic")]
async fn wrong_periodic_dies_on_first_panic_must_fail() {
    let beaver = Beaver::new("bug9-wrong", 16).expect("valid test executor");
    let errors = Arc::new(AtomicU32::new(0));
    let e = Arc::clone(&errors);

    let task = PeriodicBuilder::new(work(|| async {
        panic!("boom");
    }))
    .interval(Duration::from_millis(20))
    .listener(listener_with_error(
        || {},
        || {},
        move |_e: RuntimeError| {
            e.fetch_add(1, Ordering::SeqCst);
        },
    ))
    .build()
    .unwrap();

    beaver.enqueue(task).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let count = errors.load(Ordering::SeqCst);
    let _ = beaver.destroy().await;

    assert_eq!(
        count, 1,
        "OLD-periodic-dies-on-first-panic (count={})",
        count
    );
}
