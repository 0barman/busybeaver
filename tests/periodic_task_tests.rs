//! # Periodic Task Tests
//!
//! Comprehensive tests for PeriodicBuilder and periodic task execution.
//! Periodic tasks execute repeatedly at fixed intervals until stopped.

use busybeaver::{listener, work, Beaver, BeaverResult, PeriodicBuilder, WorkResult};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================
// BASIC PERIODIC TASK TESTS
// =============================================================================

/// Test: Basic periodic task execution.
/// The most common use case for periodic tasks.
#[tokio::test]
async fn test_basic_periodic_task() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval_ms(50)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let count = counter.load(Ordering::SeqCst);
    // Should execute approximately 5 times (0ms, 50ms, 100ms, 150ms, 200ms)
    assert!(
        count >= 4 && count <= 6,
        "Expected 4-6 executions, got {}",
        count
    );

    beaver.cancel_all()?;
    beaver.uninit()?;
    Ok(())
}

/// Test: Periodic task stops when work returns Done.
/// This is the normal completion path for periodic tasks.
#[tokio::test]
async fn test_periodic_stops_on_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let completed = Arc::new(AtomicBool::new(false));

    let counter_clone = Arc::clone(&counter);
    let completed_clone = Arc::clone(&completed);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            let count = c.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= 3 {
                WorkResult::Done(())
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .interval_ms(50)
    .listener(listener(
        move || {
            completed_clone.store(true, Ordering::SeqCst);
        },
        || {},
    ))
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "Should execute exactly 3 times"
    );
    assert!(
        completed.load(Ordering::SeqCst),
        "on_complete should be called"
    );

    Ok(())
}

/// Test: Periodic task with zero interval executes as fast as possible.
/// Useful for CPU-intensive batch processing.
/// Warning: Be careful with this in production!
#[tokio::test]
async fn test_periodic_zero_interval() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            let count = c.fetch_add(1, Ordering::SeqCst);
            if count >= 100 {
                WorkResult::Done(())
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .interval_ms(0)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // With zero interval, should execute many times very quickly
    assert!(
        counter.load(Ordering::SeqCst) >= 100,
        "Zero interval should execute many times quickly"
    );

    Ok(())
}

/// Test: Periodic task with very long interval.
/// Verifies the task actually waits for the interval.
#[tokio::test]
async fn test_periodic_long_interval() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval_ms(1000) // 1 second interval
    .build()?;

    beaver.enqueue(task)?;

    // Wait only 500ms
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Should only have executed once (at t=0)
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "Should only execute once within interval"
    );

    beaver.cancel_all()?;
    beaver.uninit()?;
    Ok(())
}

// =============================================================================
// INITIAL DELAY TESTS
// =============================================================================

/// Test: Periodic task with initial_delay(false) executes immediately.
/// This is the default behavior.
#[tokio::test]
async fn test_periodic_no_initial_delay() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let first_execution = Arc::new(std::sync::Mutex::new(None));
    let first_clone = Arc::clone(&first_execution);
    let start = Instant::now();

    let task = PeriodicBuilder::new(work(move || {
        let f = Arc::clone(&first_clone);
        async move {
            let mut guard = f.lock().unwrap();
            if guard.is_none() {
                *guard = Some(Instant::now());
            }
            WorkResult::Done(())
        }
    }))
    .interval_ms(1000)
    .initial_delay(false)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let first_time = first_execution.lock().unwrap().unwrap();
    let delay = first_time - start;

    assert!(
        delay < Duration::from_millis(50),
        "Should execute immediately without initial delay"
    );

    Ok(())
}

/// Test: Periodic task with initial_delay(true) waits before first execution.
/// Useful when you want to delay the first execution.
#[tokio::test]
async fn test_periodic_with_initial_delay() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let first_execution = Arc::new(std::sync::Mutex::new(None));
    let first_clone = Arc::clone(&first_execution);
    let start = Instant::now();

    let task = PeriodicBuilder::new(work(move || {
        let f = Arc::clone(&first_clone);
        async move {
            let mut guard = f.lock().unwrap();
            if guard.is_none() {
                *guard = Some(Instant::now());
            }
            WorkResult::Done(())
        }
    }))
    .interval_ms(200)
    .initial_delay(true)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let first_time = first_execution.lock().unwrap().unwrap();
    let delay = first_time - start;

    assert!(
        delay >= Duration::from_millis(180),
        "Should wait for initial delay before first execution"
    );

    Ok(())
}

/// Test: initial_delay(true) with zero interval should NOT delay.
/// Edge case: zero interval means no delay even with initial_delay(true).
#[tokio::test]
async fn test_initial_delay_with_zero_interval() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = Arc::clone(&executed);
    let start = Instant::now();

    let task = PeriodicBuilder::new(work(move || {
        let e = Arc::clone(&executed_clone);
        async move {
            e.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval_ms(0)
    .initial_delay(true) // Should be ignored for zero interval
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        executed.load(Ordering::SeqCst),
        "Should execute without delay"
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "Zero interval should not cause delay"
    );

    Ok(())
}

// =============================================================================
// TAG TESTS
// =============================================================================

/// Test: Periodic task with tag.
/// Tags are useful for debugging and logging.
#[tokio::test]
async fn test_periodic_with_tag() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .tag("my-periodic-task")
        .build()?;

    // Verify task was created with tag (we can check via Task::tag())
    assert_eq!(task.tag(), "my-periodic-task");

    beaver.enqueue(task)?;

    Ok(())
}

/// Test: Periodic task without tag has empty string tag.
#[tokio::test]
async fn test_periodic_without_tag() -> BeaverResult<()> {
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .build()?;

    assert_eq!(task.tag(), "");

    Ok(())
}

// =============================================================================
// LISTENER TESTS
// =============================================================================

/// Test: Periodic task listener receives on_complete when Done is returned.
#[tokio::test]
async fn test_periodic_on_complete_callback() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .listener(listener(
            move || {
                completed_clone.store(true, Ordering::SeqCst);
            },
            || {},
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        completed.load(Ordering::SeqCst),
        "on_complete should be called"
    );

    Ok(())
}

/// Test: Periodic task listener receives on_interrupt when cancelled.
#[tokio::test]
async fn test_periodic_on_interrupt_callback() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval_ms(50)
        .listener(listener(
            || {},
            move || {
                interrupted_clone.store(true, Ordering::SeqCst);
            },
        ))
        .build()?;

    beaver.enqueue(task)?;

    // Let it start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancel
    beaver.cancel_all()?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        interrupted.load(Ordering::SeqCst),
        "on_interrupt should be called"
    );

    Ok(())
}

// =============================================================================
// INTERRUPTION TESTS
// =============================================================================

/// Test: Periodic task can be interrupted mid-execution.
#[tokio::test]
async fn test_periodic_interrupt_during_sleep() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval_ms(100)
    .build()?;

    beaver.enqueue(task)?;

    // Let it run once
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel during interval sleep
    beaver.cancel_all()?;

    // Wait to ensure it doesn't continue
    tokio::time::sleep(Duration::from_millis(200)).await;

    let count = counter.load(Ordering::SeqCst);
    assert!(count <= 2, "Should not continue after interruption");

    Ok(())
}

/// Test: Task id is unique for each build.
#[tokio::test]
async fn test_periodic_unique_task_id() -> BeaverResult<()> {
    let task1 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .build()?;

    let task2 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .build()?;

    assert_ne!(task1.id(), task2.id(), "Each task should have a unique ID");

    Ok(())
}

// =============================================================================
// EDGE CASES AND ERROR HANDLING
// =============================================================================

/// Test: Periodic task with work that panics.
/// Note: Panics in async tasks are handled by tokio, not by our library.
#[tokio::test]
async fn test_periodic_work_async_operation() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    // Work that does async I/O simulation
    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            // Simulate async operation
            tokio::time::sleep(Duration::from_millis(10)).await;
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval_ms(50)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        counter.load(Ordering::SeqCst) >= 2,
        "Async work should execute"
    );

    beaver.cancel_all()?;
    beaver.uninit()?;
    Ok(())
}

/// Test: Builder chain is flexible - methods can be called in any order.
#[tokio::test]
async fn test_builder_method_order_flexible() -> BeaverResult<()> {
    // Order 1: interval -> tag -> listener
    let _task1 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(100)
        .tag("task1")
        .listener(listener(|| {}, || {}))
        .build()?;

    // Order 2: tag -> listener -> interval
    let _task2 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .tag("task2")
        .listener(listener(|| {}, || {}))
        .interval_ms(100)
        .build()?;

    // Order 3: listener -> interval -> tag
    let _task3 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .listener(listener(|| {}, || {}))
        .interval_ms(100)
        .tag("task3")
        .build()?;

    Ok(())
}

/// Test: Default interval_ms is 1000ms (1 second).
#[tokio::test]
async fn test_periodic_default_interval() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    // Don't set interval_ms - use default (1000ms)
    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .build()?;

    beaver.enqueue(task)?;

    // Wait less than default interval (1000ms)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Should only have executed once
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "Default interval should be 1000ms"
    );

    beaver.cancel_all()?;
    beaver.uninit()?;
    Ok(())
}

// =============================================================================
// STRESS TESTS
// =============================================================================

/// Test: Multiple periodic tasks running concurrently.
#[tokio::test]
async fn test_multiple_periodic_tasks_concurrent() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let total_counter = Arc::new(AtomicU32::new(0));

    for i in 0..5 {
        let counter = Arc::clone(&total_counter);
        let task = PeriodicBuilder::new(work(move || {
            let c = Arc::clone(&counter);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .interval_ms(50)
        .tag(format!("task-{}", i))
        .build()?;

        beaver.enqueue_on_new_thread(task, format!("dam-{}", i), 256, false)?;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Each task should execute ~5-6 times, total ~25-30
    let count = total_counter.load(Ordering::SeqCst);
    assert!(
        count >= 20,
        "Multiple tasks should execute concurrently, got {}",
        count
    );

    beaver.cancel_all()?;
    beaver.uninit()?;
    Ok(())
}

/// Test: High frequency periodic task.
#[tokio::test]
async fn test_high_frequency_periodic() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval_ms(1) // 1ms interval
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    beaver.cancel_all()?;

    // Should execute many times at high frequency
    // Note: actual count depends on system load, be lenient
    let count = counter.load(Ordering::SeqCst);
    assert!(
        count >= 20,
        "High frequency task should execute many times, got {}",
        count
    );

    beaver.uninit()?;
    Ok(())
}
