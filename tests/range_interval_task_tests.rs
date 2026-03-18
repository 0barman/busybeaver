//! # Range Interval Task Tests
//!
//! Comprehensive tests for RangeIntervalBuilder and range-interval task execution.
//! Range interval tasks run at most N times (total_retries), with configurable sleep
//! durations per attempt-index range between retries.

use busybeaver::{
    listener, work, Beaver, BeaverError, BeaverResult, RangeIntervalBuilder, WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================
// BASIC RANGE INTERVAL TASK TESTS
// =============================================================================

/// Test: Basic range interval task with one range.
/// First 3 attempts use 0ms delay, then run to total_retries.
#[tokio::test]
async fn test_basic_range_interval_task() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        5,
    )
    .add_range(0, 4, Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "Should execute exactly 5 times (total_retries)"
    );

    Ok(())
}

/// Test: Range interval task with multiple ranges (tiered backoff).
/// Attempts 0-2: 0ms; attempts 3-4: 100ms (so we see delay before 4th and 5th execution).
#[tokio::test]
async fn test_range_interval_multiple_ranges() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let timestamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ts_clone = Arc::clone(&timestamps);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let ts = Arc::clone(&ts_clone);
            async move {
                ts.lock().unwrap().push(Instant::now());
                WorkResult::NeedRetry
            }
        }),
        5,
    )
    .add_range(0, 2, Duration::ZERO)
    .add_range(3, 4, Duration::from_millis(200))
    .build()?;

    let _start = Instant::now();
    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(800)).await;

    let ts = timestamps.lock().unwrap();
    assert_eq!(ts.len(), 5, "Should have 5 executions");

    // First 3 should be quick (intervals[0]=intervals[1]=0)
    let first_three_span = ts[2] - ts[0];
    assert!(
        first_three_span < Duration::from_millis(150),
        "First 3 executions should be close together"
    );

    // intervals[2]=0, so delay before 4th execution is ~0
    let delay_before_fourth = ts[3] - ts[2];
    assert!(
        delay_before_fourth < Duration::from_millis(100),
        "Delay before 4th should be ~0 (range 0..2), got {:?}",
        delay_before_fourth
    );

    // intervals[3]=200ms, so delay before 5th execution is ~200ms
    let delay_before_fifth = ts[4] - ts[3];
    assert!(
        delay_before_fifth >= Duration::from_millis(180)
            && delay_before_fifth < Duration::from_millis(400),
        "Delay before 5th should be ~200ms, got {:?}",
        delay_before_fifth
    );

    Ok(())
}

/// Test: No ranges means all intervals are 0 (fast retries).
#[tokio::test]
async fn test_range_interval_no_ranges() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        5,
    )
    .build()?;

    let start = Instant::now();
    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 5);
    assert!(
        start.elapsed() < Duration::from_millis(300),
        "No delays should complete quickly"
    );

    Ok(())
}

/// Test: total_retries = 0 means no execution (boundary).
#[tokio::test]
async fn test_range_interval_zero_total_retries() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        0,
    )
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "total_retries=0 should never execute work"
    );

    Ok(())
}

/// Test: total_retries = 1 executes exactly once (no sleep).
#[tokio::test]
async fn test_range_interval_single_attempt() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        1,
    )
    .add_range(0, 0, Duration::from_secs(10)) // would sleep after 1st, but no 2nd run
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 1);

    Ok(())
}

/// Test: Task stops early when work returns Done.
#[tokio::test]
async fn test_range_interval_stops_on_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                let count = c.fetch_add(1, Ordering::SeqCst) + 1;
                if count == 3 {
                    WorkResult::Done(())
                } else {
                    WorkResult::NeedRetry
                }
            }
        }),
        10,
    )
    .add_range(0, 9, Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "Should stop after returning Done"
    );

    Ok(())
}

// =============================================================================
// RANGE SEMANTICS (OVERLAP, BEYOND TOTAL)
// =============================================================================

/// Test: Later range overwrites earlier for overlapping indices.
#[tokio::test]
async fn test_range_interval_overlapping_ranges_later_wins() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let timestamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ts_clone = Arc::clone(&timestamps);

    // Index 1 is in both ranges; second add_range(1, 2, 200) should win for index 1
    let task = RangeIntervalBuilder::new(
        work(move || {
            let ts = Arc::clone(&ts_clone);
            async move {
                ts.lock().unwrap().push(Instant::now());
                WorkResult::NeedRetry
            }
        }),
        4,
    )
    .add_range(0, 1, Duration::from_millis(50))
    .add_range(1, 2, Duration::from_millis(200))
    .build()?;

    let _start = Instant::now();
    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(600)).await;

    let ts = timestamps.lock().unwrap();
    assert_eq!(ts.len(), 4);

    // After attempt 0: 50ms (first range for index 0)
    let d1 = ts[1] - ts[0];
    assert!(
        d1 >= Duration::from_millis(45) && d1 < Duration::from_millis(120),
        "Delay after 0 should be 50ms (first range)"
    );

    // After attempt 1: 200ms (second range overwrites index 1)
    let d2 = ts[2] - ts[1];
    assert!(
        d2 >= Duration::from_millis(180) && d2 < Duration::from_millis(350),
        "Delay after 1 should be 200ms (second range)"
    );

    // After attempt 2: 200ms
    let d3 = ts[3] - ts[2];
    assert!(
        d3 >= Duration::from_millis(180),
        "Delay after 2 should be 200ms"
    );

    Ok(())
}

/// Test: Range with end_inclusive beyond total_retries is clamped.
#[tokio::test]
async fn test_range_interval_range_beyond_total() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    // total_retries=3 (attempts 0,1,2). Range 0..=99 effectively 0..=2.
    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        3,
    )
    .add_range(0, 99, Duration::from_millis(100))
    .build()?;

    let start = Instant::now();
    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 3);
    // Should have waited ~100ms before 2nd and ~100ms before 3rd
    assert!(start.elapsed() >= Duration::from_millis(200));

    Ok(())
}

// =============================================================================
// LISTENER TESTS
// =============================================================================

/// Test: on_complete is called when all retries are exhausted.
#[tokio::test]
async fn test_range_interval_on_complete() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::NeedRetry }), 3)
        .add_range(0, 2, Duration::ZERO)
        .listener(listener(
            move || {
                completed_clone.store(true, Ordering::SeqCst);
            },
            || {},
        ))
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        completed.load(Ordering::SeqCst),
        "on_complete should be called after all retries exhausted"
    );

    Ok(())
}

/// Test: on_complete is NOT called when Done is returned early.
#[tokio::test]
async fn test_range_interval_no_on_complete_on_early_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 5)
        .add_range(0, 4, Duration::ZERO)
        .listener(listener(
            move || {
                completed_clone.store(true, Ordering::SeqCst);
            },
            || {},
        ))
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !completed.load(Ordering::SeqCst),
        "on_complete should NOT be called when Done is returned"
    );

    Ok(())
}

/// Test: on_interrupt is called when task is cancelled.
/// Interrupt is checked at start of each loop and after interval sleep; use a short
/// sleep so that after cancel the worker soon wakes and sees interrupted.
#[tokio::test]
async fn test_range_interval_on_interrupt() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let execution_count = Arc::new(AtomicU32::new(0));

    let interrupted_clone = Arc::clone(&interrupted);
    let ec_clone = Arc::clone(&execution_count);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let ec = Arc::clone(&ec_clone);
            async move {
                ec.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        10,
    )
    .add_range(0, 9, Duration::from_millis(300)) // sleep 300ms after first, then we check interrupted
    .listener(listener(
        || {},
        move || {
            interrupted_clone.store(true, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(execution_count.load(Ordering::SeqCst), 1);

    beaver.cancel_all().await?;
    // Wait for worker to wake from 300ms sleep and see interrupted, then call on_interrupt
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        interrupted.load(Ordering::SeqCst),
        "on_interrupt should be called after worker sees interrupt"
    );

    Ok(())
}

// =============================================================================
// TAG TESTS
// =============================================================================

/// Test: Range interval task with tag.
#[tokio::test]
async fn test_range_interval_with_tag() -> BeaverResult<()> {
    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 5)
        .add_range(0, 4, Duration::ZERO)
        .tag("my-range-task")
        .build()?;

    assert_eq!(task.tag(), "my-range-task");

    Ok(())
}

/// Test: Range interval task without tag.
#[tokio::test]
async fn test_range_interval_without_tag() -> BeaverResult<()> {
    let task = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 3).build()?;

    assert_eq!(task.tag(), "");

    Ok(())
}

// =============================================================================
// INTERRUPTION TESTS
// =============================================================================

/// Test: Task can be interrupted during interval sleep.
#[tokio::test]
async fn test_range_interval_interrupt_during_sleep() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        5,
    )
    .add_range(0, 4, Duration::from_secs(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    beaver.cancel_all().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "Should only execute once before interrupt"
    );

    Ok(())
}

/// Test: Cancel during work stops further executions.
#[tokio::test]
async fn test_range_interval_cancel_during_work() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let execution_count = Arc::new(AtomicU32::new(0));
    let ec_clone = Arc::clone(&execution_count);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let ec = Arc::clone(&ec_clone);
            async move {
                ec.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(80)).await;
                WorkResult::NeedRetry
            }
        }),
        10,
    )
    .add_range(0, 9, Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    beaver.cancel_all().await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let count = execution_count.load(Ordering::SeqCst);
    assert!(
        count >= 1 && count < 10,
        "Should execute some but not all, got {}",
        count
    );

    Ok(())
}

// =============================================================================
// BUILDER ERROR TESTS
// =============================================================================

/// Test: Build fails when number of ranges exceeds total_retries.
#[tokio::test]
async fn test_range_interval_ranges_exceed_total_error() {
    let builder = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 5)
        .add_range(0, 0, Duration::ZERO)
        .add_range(1, 1, Duration::ZERO)
        .add_range(2, 2, Duration::ZERO)
        .add_range(3, 3, Duration::ZERO)
        .add_range(4, 4, Duration::ZERO)
        .add_range(0, 0, Duration::ZERO); // 6th range

    let result = builder.build();
    assert!(result.is_err());
    match result {
        Err(BeaverError::RangeIntervalRangesExceedTotal {
            total,
            ranges_count,
        }) => {
            assert_eq!(total, 5);
            assert_eq!(ranges_count, 6);
        }
        _ => panic!("expected RangeIntervalRangesExceedTotal error"),
    }
}

// =============================================================================
// BUILDER TESTS
// =============================================================================

/// Test: Builder method chaining in any order.
#[tokio::test]
async fn test_range_interval_builder_chain_order() -> BeaverResult<()> {
    let _task1 = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 5)
        .add_range(0, 4, Duration::from_millis(100))
        .tag("task1")
        .listener(listener(|| {}, || {}))
        .build()?;

    let _task2 = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 5)
        .listener(listener(|| {}, || {}))
        .tag("task2")
        .add_range(0, 4, Duration::from_millis(100))
        .build()?;

    Ok(())
}

/// Test: Each build creates unique task ID.
#[tokio::test]
async fn test_range_interval_unique_task_id() -> BeaverResult<()> {
    let task1 = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 3).build()?;

    let task2 = RangeIntervalBuilder::new(work(|| async { WorkResult::Done(()) }), 3).build()?;

    assert_ne!(task1.id(), task2.id());

    Ok(())
}

// =============================================================================
// REAL-WORLD SCENARIOS
// =============================================================================

/// Test: Tiered backoff - fast retries first, then slower.
#[tokio::test]
async fn test_range_interval_tiered_backoff() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let attempt = Arc::new(AtomicU32::new(0));
    let success = Arc::new(AtomicBool::new(false));

    let att = Arc::clone(&attempt);
    let suc = Arc::clone(&success);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let a = Arc::clone(&att);
            let s = Arc::clone(&suc);
            async move {
                let current = a.fetch_add(1, Ordering::SeqCst) + 1;
                if current >= 4 {
                    s.store(true, Ordering::SeqCst);
                    WorkResult::Done(())
                } else {
                    WorkResult::NeedRetry
                }
            }
        }),
        10,
    )
    .add_range(0, 2, Duration::from_millis(50))
    .add_range(3, 9, Duration::from_millis(200))
    .tag("tiered-backoff")
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(attempt.load(Ordering::SeqCst), 4);
    assert!(success.load(Ordering::SeqCst));

    Ok(())
}

/// Test: Large total_retries with all zero intervals.
#[tokio::test]
async fn test_range_interval_large_total_zero_intervals() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let c = Arc::clone(&counter_clone);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        50,
    )
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 50);

    Ok(())
}
