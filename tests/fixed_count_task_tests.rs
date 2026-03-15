//! # Fixed Count Task Tests
//!
//! Comprehensive tests for FixedCountBuilder and fixed count task execution.
//! Fixed count tasks execute a specific number of times or until work returns Done.

use busybeaver::{
    listener, work, Beaver, BeaverResult, FixedCountBuilder, FixedCountProgress, WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// BASIC FIXED COUNT TASK TESTS
// =============================================================================

/// Test: Basic fixed count task executes the specified number of times.
/// This is the most common use case for fixed count tasks.
#[tokio::test]
async fn test_basic_fixed_count_task() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(5)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "Should execute exactly 5 times"
    );

    Ok(())
}

/// Test: Fixed count task with count = 1 executes exactly once.
/// Edge case for minimum count value.
#[tokio::test]
async fn test_fixed_count_one() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(1)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "count=1 should execute exactly once"
    );

    Ok(())
}

/// Test: Fixed count task with count = 0 is treated as count = 1.
/// Edge case: zero count is normalized to 1.
#[tokio::test]
async fn test_fixed_count_zero_normalized_to_one() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(0) // Should be treated as 1
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "count=0 should be normalized to 1"
    );

    Ok(())
}

/// Test: Fixed count task stops early when work returns Done.
/// This allows early termination on success.
#[tokio::test]
async fn test_fixed_count_stops_early_on_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            let count = c.fetch_add(1, Ordering::SeqCst) + 1;
            if count == 3 {
                WorkResult::Done(()) // Stop at 3rd execution
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .count(10) // Max 10 times, but will stop at 3
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "Should stop early when Done is returned"
    );

    Ok(())
}

/// Test: Default count is 3.
#[tokio::test]
async fn test_fixed_count_default_count() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    // Don't set count - use default (3)
    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "Default count should be 3"
    );

    Ok(())
}

/// Test: Large count value.
#[tokio::test]
async fn test_fixed_count_large_count() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(100)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        100,
        "Should execute 100 times"
    );

    Ok(())
}

// =============================================================================
// PROGRESS CALLBACK TESTS
// =============================================================================

/// Test: Progress callback is called before each execution.
/// This is unique to fixed count tasks.
#[tokio::test]
async fn test_fixed_count_progress_callback() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let progress_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress_clone = Arc::clone(&progress_log);

    // Create a progress callback
    let progress_fn: Arc<dyn FixedCountProgress> =
        Arc::new(move |current: u32, total: u32, tag: &str| {
            let mut log = progress_clone.lock().unwrap();
            log.push((current, total, tag.to_string()));
        });

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(5)
        .tag("progress-test")
        .progress(progress_fn)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = progress_log.lock().unwrap();
    assert_eq!(log.len(), 5, "Progress should be called 5 times");

    // Verify progress values
    assert_eq!(log[0], (1, 5, "progress-test".to_string()));
    assert_eq!(log[1], (2, 5, "progress-test".to_string()));
    assert_eq!(log[2], (3, 5, "progress-test".to_string()));
    assert_eq!(log[3], (4, 5, "progress-test".to_string()));
    assert_eq!(log[4], (5, 5, "progress-test".to_string()));

    Ok(())
}

/// Test: Progress callback receives correct tag.
#[tokio::test]
async fn test_fixed_count_progress_with_tag() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let received_tag = Arc::new(std::sync::Mutex::new(String::new()));
    let tag_clone = Arc::clone(&received_tag);

    let progress_fn: Arc<dyn FixedCountProgress> = Arc::new(move |_: u32, _: u32, tag: &str| {
        let mut t = tag_clone.lock().unwrap();
        *t = tag.to_string();
    });

    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .tag("my-special-tag")
        .progress(progress_fn)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        *received_tag.lock().unwrap(),
        "my-special-tag",
        "Progress should receive correct tag"
    );

    Ok(())
}

/// Test: Progress callback with empty tag.
#[tokio::test]
async fn test_fixed_count_progress_empty_tag() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let received_tag = Arc::new(std::sync::Mutex::new(String::from("placeholder")));
    let tag_clone = Arc::clone(&received_tag);

    let progress_fn: Arc<dyn FixedCountProgress> = Arc::new(move |_: u32, _: u32, tag: &str| {
        let mut t = tag_clone.lock().unwrap();
        *t = tag.to_string();
    });

    // No tag set
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .progress(progress_fn)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        *received_tag.lock().unwrap(),
        "",
        "Empty tag should be passed as empty string"
    );

    Ok(())
}

// =============================================================================
// LISTENER TESTS
// =============================================================================

/// Test: on_complete is called when all retries are exhausted.
#[tokio::test]
async fn test_fixed_count_on_complete_after_all_retries() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(3)
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
        "on_complete should be called after all retries"
    );

    Ok(())
}

/// Test: on_complete is NOT called when task returns Done early.
#[tokio::test]
async fn test_fixed_count_no_on_complete_on_early_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::Done(()) // Return Done immediately
        }
    }))
    .count(5)
    .listener(listener(
        move || {
            completed_clone.store(true, Ordering::SeqCst);
        },
        || {},
    ))
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Only executed once because Done was returned
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // on_complete should NOT be called (task completed successfully, not exhausted)
    assert!(
        !completed.load(Ordering::SeqCst),
        "on_complete should NOT be called when Done is returned early"
    );

    Ok(())
}

/// Test: on_interrupt is called when task is cancelled.
#[tokio::test]
async fn test_fixed_count_on_interrupt() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = FixedCountBuilder::new(work(|| async {
        // Simulate slow work
        tokio::time::sleep(Duration::from_millis(100)).await;
        WorkResult::NeedRetry
    }))
    .count(100)
    .listener(listener(
        || {},
        move || {
            interrupted_clone.store(true, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task)?;

    // Let it start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel
    beaver.cancel_all()?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        interrupted.load(Ordering::SeqCst),
        "on_interrupt should be called"
    );

    Ok(())
}

// =============================================================================
// TAG TESTS
// =============================================================================

/// Test: Fixed count task with tag.
#[tokio::test]
async fn test_fixed_count_with_tag() -> BeaverResult<()> {
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .tag("my-fixed-task")
        .build()?;

    assert_eq!(task.tag(), "my-fixed-task");

    Ok(())
}

/// Test: Fixed count task without tag.
#[tokio::test]
async fn test_fixed_count_without_tag() -> BeaverResult<()> {
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .build()?;

    assert_eq!(task.tag(), "");

    Ok(())
}

// =============================================================================
// INTERRUPTION TESTS
// =============================================================================

/// Test: Fixed count task can be interrupted mid-execution.
#[tokio::test]
async fn test_fixed_count_interrupt_mid_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            // Simulate slow work
            tokio::time::sleep(Duration::from_millis(100)).await;
            WorkResult::NeedRetry
        }
    }))
    .count(10)
    .build()?;

    beaver.enqueue(task)?;

    // Let it start first execution
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel
    beaver.cancel_all()?;

    // Wait to ensure it doesn't continue
    tokio::time::sleep(Duration::from_millis(500)).await;

    let count = counter.load(Ordering::SeqCst);
    assert!(
        count < 10,
        "Should be interrupted before completing all executions"
    );

    Ok(())
}

/// Test: Cancel stops task execution before all counts complete.
/// Verifies that long-running fixed count tasks can be interrupted.
#[tokio::test]
async fn test_fixed_count_cancel_stops_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("test_fixed_count_cancel_stops_execution", 256);
    let execution_count = Arc::new(AtomicU32::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));

    let ec = Arc::clone(&execution_count);
    let int = Arc::clone(&interrupted);

    let task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&ec);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            // Slow work
            tokio::time::sleep(Duration::from_millis(100)).await;
            WorkResult::NeedRetry
        }
    }))
    .count(100) // Would take 10 seconds to complete
    .listener(listener(
        || {},
        move || {
            int.store(true, Ordering::SeqCst);
        },
    ))
    .build()?;

    beaver.enqueue(task)?;

    // Let it run a bit
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Cancel
    beaver.cancel_all()?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let count = execution_count.load(Ordering::SeqCst);
    assert!(
        count < 100,
        "Task should not complete all 100 executions, got {}",
        count
    );
    assert!(
        interrupted.load(Ordering::SeqCst),
        "on_interrupt should be called"
    );

    Ok(())
}

// =============================================================================
// BUILDER TESTS
// =============================================================================

/// Test: Builder method chaining in any order.
#[tokio::test]
async fn test_fixed_count_builder_chain_order() -> BeaverResult<()> {
    // Create a simple progress callback for testing
    let progress_fn: Arc<dyn FixedCountProgress> = Arc::new(|_: u32, _: u32, _: &str| {});

    // Order 1
    let _task1 = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(5)
        .tag("task1")
        .progress(Arc::clone(&progress_fn))
        .listener(listener(|| {}, || {}))
        .build()?;

    // Order 2
    let _task2 = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .listener(listener(|| {}, || {}))
        .progress(Arc::clone(&progress_fn))
        .tag("task2")
        .count(5)
        .build()?;

    Ok(())
}

/// Test: Each build creates a unique task ID.
#[tokio::test]
async fn test_fixed_count_unique_task_id() -> BeaverResult<()> {
    let task1 = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .build()?;

    let task2 = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(3)
        .build()?;

    assert_ne!(task1.id(), task2.id());

    Ok(())
}

// =============================================================================
// COMPLEX SCENARIOS
// =============================================================================

/// Test: Fixed count task with async work.
#[tokio::test]
async fn test_fixed_count_async_work() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let results_clone = Arc::clone(&results);

    let task = FixedCountBuilder::new(work(move || {
        let r = Arc::clone(&results_clone);
        async move {
            // Simulate async I/O
            tokio::time::sleep(Duration::from_millis(10)).await;
            r.lock().unwrap().push(42);
            WorkResult::NeedRetry
        }
    }))
    .count(3)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(results.lock().unwrap().len(), 3);

    Ok(())
}

/// Test: Multiple fixed count tasks with different counts.
#[tokio::test]
async fn test_multiple_fixed_count_tasks() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let total = Arc::new(AtomicU32::new(0));

    for i in 1..=3 {
        let t = Arc::clone(&total);
        let task = FixedCountBuilder::new(work(move || {
            let total = Arc::clone(&t);
            async move {
                total.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .count(i as u32) // 1, 2, 3
        .build()?;

        beaver.enqueue_on_new_thread(task, format!("dam-{}", i), 256, false)?;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Total should be 1 + 2 + 3 = 6
    assert_eq!(total.load(Ordering::SeqCst), 6);

    Ok(())
}

/// Test: Retry simulation with conditional success.
/// Simulates a real-world retry pattern where success comes after N attempts.
#[tokio::test]
async fn test_retry_simulation() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let attempt = Arc::new(AtomicU32::new(0));
    let success = Arc::new(AtomicBool::new(false));

    let att = Arc::clone(&attempt);
    let suc = Arc::clone(&success);

    // Simulate: fails first 2 times, succeeds on 3rd
    let task = FixedCountBuilder::new(work(move || {
        let a = Arc::clone(&att);
        let s = Arc::clone(&suc);
        async move {
            let current = a.fetch_add(1, Ordering::SeqCst) + 1;
            if current >= 3 {
                // Success on 3rd attempt
                s.store(true, Ordering::SeqCst);
                WorkResult::Done(())
            } else {
                // Fail and retry
                WorkResult::NeedRetry
            }
        }
    }))
    .count(5) // Max 5 retries
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(attempt.load(Ordering::SeqCst), 3);
    assert!(success.load(Ordering::SeqCst));

    Ok(())
}
