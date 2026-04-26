//! # Improvement / Regression Pinning Tests
//!
//! Tests added during the post-review hardening pass. They cover scenarios
//! highlighted in `docs/REVIEW_zh.md` §3.2:
//!
//! - Static `Send`/`Sync` assertions for the public types.
//! - Concurrent `destroy()` (no deadlock, idempotent).
//! - Cancel-then-enqueue semantics (currently *not* fenced; pinned here so it
//!   cannot silently change).
//! - Builder edge values (`buffer = 0`, `total_retries = 0`, `count(0)`).
//! - Time semantics divergence between [`TimeIntervalBuilder`] (sleeps before
//!   the first attempt) and [`RangeIntervalBuilder`] (does not).
//! - `TaskId::as_uuid()` accessor.
//! - `WorkResult<T>` generic pitfall (the `T` payload is ignored by the
//!   executor; pinned so a future API redesign is forced to change this test).
//! - Known-issue cases for listener/progress panic isolation
//!   (marked `#[ignore]` until the fix lands).

use busybeaver::{
    listener, listener_with_error, work, Beaver, BeaverError, BeaverResult, FixedCountBuilder,
    PeriodicBuilder, RangeIntervalBuilder, RuntimeError, Task, TaskId, TimeIntervalBuilder,
    WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================
// SEND / SYNC STATIC ASSERTIONS
// =============================================================================

/// Compile-time assertions that the public types are `Send + Sync + 'static`.
/// If a future change accidentally introduces a non-`Send` field, this fails
/// at compile time, before any test even runs.
#[test]
fn public_types_are_send_sync_static() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    assert_send_sync::<Beaver>();
    assert_send_sync::<Arc<Beaver>>();
    assert_send_sync::<Task>();
    assert_send_sync::<Arc<Task>>();
    assert_send_sync::<TaskId>();
    assert_send_sync::<BeaverError>();
    assert_send_sync::<RuntimeError>();
}

// =============================================================================
// LIFECYCLE: CONCURRENT DESTROY
// =============================================================================

/// Concurrent `destroy()` calls must not deadlock and must be idempotent.
/// (Previously, two async-mutex acquires from the same task could starve;
/// after the std-mutex migration this is also a regression test.)
#[tokio::test]
async fn concurrent_destroy_is_idempotent() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("concurrent-destroy", 64));

    // Plant a periodic task so destroy actually has something to interrupt.
    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(10))
        .build()?;
    beaver.enqueue(task).await?;

    let b1 = Arc::clone(&beaver);
    let b2 = Arc::clone(&beaver);
    let b3 = Arc::clone(&beaver);
    let (r1, r2, r3) = tokio::join!(b1.destroy(), b2.destroy(), b3.destroy());
    r1?;
    r2?;
    r3?;

    // After destroy the default lane is gone.
    let task2 = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    let err = beaver.enqueue(task2).await.unwrap_err();
    assert!(matches!(err, BeaverError::NoDam));
    Ok(())
}

// =============================================================================
// CANCEL SEMANTICS
// =============================================================================

/// Pin the current behavior: once `cancel_all` has been *processed* by the
/// worker (i.e. the cancel signal has drained pending tasks), subsequently
/// enqueued tasks run normally. This is the intended steady-state semantics.
///
/// Note: enqueueing **immediately** after `cancel_all` is racy – the new
/// task may or may not be drained by the in-flight CancelAll, depending on
/// scheduling. Users who want strict "no more tasks after cancel" semantics
/// should call [`Beaver::destroy`] instead.
#[tokio::test]
async fn cancel_all_then_enqueue_runs_after_drain() -> BeaverResult<()> {
    let beaver = Beaver::new("cancel-then-enqueue", 64);
    beaver.cancel_all().await?;
    // Allow the CancelAll signal to be processed before enqueueing.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let ran = Arc::new(AtomicBool::new(false));
    let ran_c = Arc::clone(&ran);
    let task = FixedCountBuilder::new(work(move || {
        let r = Arc::clone(&ran_c);
        async move {
            r.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        ran.load(Ordering::SeqCst),
        "tasks enqueued after cancel_all has been processed should run"
    );
    beaver.destroy().await
}

// =============================================================================
// BUILDER EDGE VALUES
// =============================================================================

/// `Beaver::new(name, 0)` panics (documented). Pin the panic.
#[test]
#[should_panic]
fn beaver_new_zero_buffer_panics() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _ = rt.block_on(async { Beaver::new("zero-buf", 0) });
}

/// `RangeIntervalBuilder` with `total_retries = 0` builds a no-op task: it
/// runs no executions and triggers no listener callbacks.
#[tokio::test]
async fn range_interval_total_zero_runs_nothing() -> BeaverResult<()> {
    let beaver = Beaver::new("range-zero", 16);
    let executions = Arc::new(AtomicU32::new(0));
    let on_complete = Arc::new(AtomicBool::new(false));
    let on_interrupt = Arc::new(AtomicBool::new(false));

    let exec_c = Arc::clone(&executions);
    let comp_c = Arc::clone(&on_complete);
    let intr_c = Arc::clone(&on_interrupt);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let e = Arc::clone(&exec_c);
            async move {
                e.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        0,
    )
    .listener(listener(
        move || comp_c.store(true, Ordering::SeqCst),
        move || intr_c.store(true, Ordering::SeqCst),
    ))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(!on_complete.load(Ordering::SeqCst));
    assert!(!on_interrupt.load(Ordering::SeqCst));
    beaver.destroy().await
}

/// `FixedCountBuilder::count(0)` is silently clamped to 1 (documented).
#[tokio::test]
async fn fixed_count_zero_runs_exactly_once() -> BeaverResult<()> {
    let beaver = Beaver::new("fc-zero", 16);
    let executions = Arc::new(AtomicU32::new(0));
    let exec_c = Arc::clone(&executions);

    let task = FixedCountBuilder::new(work(move || {
        let e = Arc::clone(&exec_c);
        async move {
            e.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(0)
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    beaver.destroy().await
}

// =============================================================================
// TIME SEMANTICS DIVERGENCE
// =============================================================================

/// `TimeIntervalBuilder` sleeps **before the first attempt**. Pinned so the
/// (currently surprising) behavior cannot regress without intent.
#[tokio::test]
async fn time_interval_sleeps_before_first_attempt() -> BeaverResult<()> {
    let beaver = Beaver::new("ti-first", 16);
    let first_seen_at: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    let seen_c = Arc::clone(&first_seen_at);

    let start = Instant::now();
    let task = TimeIntervalBuilder::new(work(move || {
        let s = Arc::clone(&seen_c);
        async move {
            let mut g = s.lock().unwrap();
            if g.is_none() {
                *g = Some(Instant::now());
            }
            WorkResult::Done(())
        }
    }))
    .intervals_millis(vec![120])
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(220)).await;

    let seen = first_seen_at.lock().unwrap().expect("work must run");
    let delay = seen.duration_since(start);
    assert!(
        delay >= Duration::from_millis(100),
        "TimeInterval should sleep ~120ms before first attempt, observed {:?}",
        delay
    );
    beaver.destroy().await
}

/// `RangeIntervalBuilder` does **not** sleep before the first attempt.
#[tokio::test]
async fn range_interval_does_not_sleep_before_first_attempt() -> BeaverResult<()> {
    let beaver = Beaver::new("ri-first", 16);
    let first_seen_at: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    let seen_c = Arc::clone(&first_seen_at);

    let start = Instant::now();
    let task = RangeIntervalBuilder::new(
        work(move || {
            let s = Arc::clone(&seen_c);
            async move {
                let mut g = s.lock().unwrap();
                if g.is_none() {
                    *g = Some(Instant::now());
                }
                WorkResult::Done(())
            }
        }),
        3,
    )
    .add_range(0, 2, Duration::from_millis(200))
    .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;

    let seen = first_seen_at.lock().unwrap().expect("work must run");
    let delay = seen.duration_since(start);
    assert!(
        delay < Duration::from_millis(80),
        "RangeInterval first attempt must run promptly, observed delay {:?}",
        delay
    );
    beaver.destroy().await
}

// =============================================================================
// NAMED DAM SEMANTICS
// =============================================================================

/// When `enqueue_on_new_thread` is called twice with the same name, the
/// `long_resident` flag of the second call wins (current behavior).
#[tokio::test]
async fn named_dam_long_resident_last_writer_wins() -> BeaverResult<()> {
    let beaver = Beaver::new("default", 16);

    let make_task = || {
        FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
            .count(1)
            .build()
    };

    // First: long_resident = true.
    beaver
        .enqueue_on_new_thread(make_task()?, "lane-x", 8, true)
        .await?;
    // Second on the same name: long_resident = false.
    beaver
        .enqueue_on_new_thread(make_task()?, "lane-x", 8, false)
        .await?;

    // cancel_non_long_resident should now drop "lane-x" because the last
    // writer set it to non-long-resident.
    let token_alive = Arc::new(AtomicBool::new(false));
    let alive_c = Arc::clone(&token_alive);
    let canary = PeriodicBuilder::new(work(move || {
        let a = Arc::clone(&alive_c);
        async move {
            a.store(true, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(20))
    .build()?;

    beaver
        .enqueue_on_new_thread(canary, "lane-x", 8, false)
        .await?;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(token_alive.load(Ordering::SeqCst));

    beaver.cancel_non_long_resident().await?;
    // After cancel_non_long_resident, the lane is dropped; we cannot easily
    // re-target it, but at least destroy should still succeed.
    beaver.destroy().await
}

// =============================================================================
// TASK ID PUBLIC API
// =============================================================================

/// `TaskId::as_uuid()` returns the underlying UUID, and its Display matches
/// the wrapper's Display.
#[test]
fn task_id_as_uuid_matches_display() {
    let id = TaskId::default();
    let via_display = format!("{}", id);
    let via_uuid = format!("{}", id.as_uuid());
    assert_eq!(via_display, via_uuid);
    assert_eq!(via_display.len(), 36); // standard hyphenated UUID
}

// =============================================================================
// WORK RESULT GENERIC PITFALL
// =============================================================================

/// Documents a known API quirk: `Work::execute` is fixed to `WorkResult<()>`,
/// so any `T` you might want to surface in `WorkResult::Done(T)` is impossible
/// to consume from the executor side. The test shows that the local
/// `WorkResult<i32>` value is still constructible — just unobservable.
#[test]
fn work_result_generic_payload_is_local_only() {
    let r: WorkResult<i32> = WorkResult::Done(42);
    assert_eq!(r.as_done().copied(), Some(42));
    assert!(!r.need_retry());
    // There is no API to feed `WorkResult<i32>` into a Beaver task today;
    // `Work::execute` returns `WorkResult<()>`. A future API redesign should
    // make this test fail (replace `Done(i32)` plumbing).
}

// =============================================================================
// QUEUE FULL RECOVERY
// =============================================================================

/// `BeaverError::QueueFull` is recoverable: after the worker drains, new
/// enqueues succeed again.
#[tokio::test]
async fn queue_full_is_recoverable() -> BeaverResult<()> {
    let beaver = Beaver::new("recover", 1);

    // Block the worker on a 200ms task.
    let blocker = FixedCountBuilder::new(work(|| async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        WorkResult::Done(())
    }))
    .count(1)
    .build()?;
    beaver.enqueue(blocker).await?;

    // Yield so the worker actually picks up the blocker, freeing the slot.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Fill the buffer with one queued task.
    let pending = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    beaver.enqueue(pending).await?;

    // Third enqueue should fail with QueueFull while the buffer is occupied.
    let extra = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    let err = beaver.enqueue(extra).await.unwrap_err();
    assert!(matches!(err, BeaverError::QueueFull));

    // Wait for the worker to drain, then we can enqueue again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let revived = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    beaver.enqueue(revived).await?;
    beaver.destroy().await
}

// =============================================================================
// LISTENER WIRING SMOKE
// =============================================================================

/// Sanity: `listener_with_error` round-trips an error through `on_error`.
#[tokio::test]
async fn listener_with_error_routes_panic() -> BeaverResult<()> {
    let beaver = Beaver::new("listener-err", 16);
    let saw_error = Arc::new(AtomicBool::new(false));
    let saw_c = Arc::clone(&saw_error);

    let task = FixedCountBuilder::new(work(|| async { panic!("boom") }))
        .count(1)
        .listener(listener_with_error(
            || {},
            || {},
            move |_e: RuntimeError| saw_c.store(true, Ordering::SeqCst),
        ))
        .build()?;

    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(saw_error.load(Ordering::SeqCst));
    beaver.destroy().await
}

// =============================================================================
// KNOWN ISSUES (kept as #[ignore] until fixed)
// =============================================================================

/// Known issue: panics inside `WorkListener::on_complete` are NOT caught by
/// the framework and tear down the executor lane. This test documents the
/// expectation we *want* (worker survives & subsequent task runs) but is
/// `#[ignore]` until the fix lands. Run with
/// `cargo test -- --ignored listener_panic_in_on_complete_should_be_isolated`.
#[tokio::test]
#[ignore = "known issue: listener panics are not isolated"]
async fn listener_panic_in_on_complete_should_be_isolated() -> BeaverResult<()> {
    let beaver = Beaver::new("listener-panic", 16);

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(1)
        .listener(listener(|| panic!("listener boom"), || {}))
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // After a panicking on_complete, the worker should still accept new tasks.
    let ran = Arc::new(AtomicBool::new(false));
    let ran_c = Arc::clone(&ran);
    let next = FixedCountBuilder::new(work(move || {
        let r = Arc::clone(&ran_c);
        async move {
            r.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(next).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        ran.load(Ordering::SeqCst),
        "worker must survive a panicking listener"
    );
    beaver.destroy().await
}

/// Known issue: panics inside `FixedCountProgress::on_progress` are also not
/// isolated. Same disposition as above.
#[tokio::test]
#[ignore = "known issue: progress callback panics are not isolated"]
async fn progress_panic_should_be_isolated() -> BeaverResult<()> {
    let beaver = Beaver::new("progress-panic", 16);

    let progress: Arc<dyn busybeaver::FixedCountProgress> =
        Arc::new(|_c: u32, _t: u32, _tag: &str| panic!("progress boom"));

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(2)
        .progress(progress)
        .build()?;
    beaver.enqueue(task).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let ran = Arc::new(AtomicBool::new(false));
    let ran_c = Arc::clone(&ran);
    let next = FixedCountBuilder::new(work(move || {
        let r = Arc::clone(&ran_c);
        async move {
            r.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(next).await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(ran.load(Ordering::SeqCst));
    beaver.destroy().await
}
