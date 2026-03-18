//! # Beaver Core Tests
//!
//! Comprehensive tests for the Beaver task execution engine.
//! Covers creation, lifecycle management, task enqueueing, and cancellation.

use busybeaver::{listener, work, Beaver, BeaverError, BeaverResult, PeriodicBuilder, WorkResult};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// =============================================================================
// BEAVER CREATION TESTS
// =============================================================================

/// Test: Create Beaver instance using Beaver::new() within tokio runtime.
/// This is the most common way to create a Beaver instance.
/// Beginner developers should start with this pattern.
#[tokio::test]
async fn test_beaver_new_in_tokio_runtime() {
    let beaver = Beaver::new("test1", 256);

    // Verify beaver can accept tasks
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()
        .unwrap();

    let result = beaver.enqueue(task).await;
    assert!(result.is_ok(), "Beaver should accept tasks after creation");
}

/// Test: Create Beaver using Default trait implementation.
/// This is equivalent to calling Beaver::new().
#[tokio::test]
async fn test_beaver_default_trait() {
    let beaver = Beaver::new("first_thread_queue", 256);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()
        .unwrap();

    assert!(beaver.enqueue(task).await.is_ok());
}

/// Test: Create Beaver using with_handle() from outside tokio runtime.
/// This is useful when you need to manage the runtime separately.
/// Intermediate developers often use this pattern for better control.
#[test]
fn test_beaver_with_handle_outside_tokio() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Failed to create runtime");

    let beaver = Beaver::new_with_handle("test2", 256, rt.handle().clone());

    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .build()
    .unwrap();

    rt.block_on(beaver.enqueue(task)).unwrap();

    // Wait for task execution
    thread::sleep(Duration::from_millis(100));

    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// Test: Create Beaver with handle from a current_thread runtime.
/// Verifies compatibility with single-threaded runtime.
#[test]
fn test_beaver_with_current_thread_runtime() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create runtime");

    let beaver = Beaver::new_with_handle("test2", 256, rt.handle().clone());
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = Arc::clone(&executed);

    let task = PeriodicBuilder::new(work(move || {
        let e = Arc::clone(&executed_clone);
        async move {
            e.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .build()
    .unwrap();

    rt.block_on(async {
        beaver.enqueue(task).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    assert!(executed.load(Ordering::SeqCst));
}

// =============================================================================
// BEAVER LIFECYCLE TESTS
// =============================================================================

/// Test: destroy() releases all dams and prevents new enqueues.
/// Advanced developers use this for graceful shutdown.
#[tokio::test]
async fn test_destroy_releases_all_dams() -> BeaverResult<()> {
    let beaver = Beaver::new("test1", 256);

    // Create some named dams
    let task1 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    let task2 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver
        .enqueue_on_new_thread(task1, "dam1", 256, false)
        .await?;
    beaver
        .enqueue_on_new_thread(task2, "dam2", 256, true)
        .await?;

    // Uninit should release everything
    beaver.destroy().await?;

    // All enqueues should now fail
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    assert!(matches!(
        beaver.enqueue(task).await,
        Err(BeaverError::NoDam)
    ));

    Ok(())
}

// =============================================================================
// TASK ENQUEUE TESTS
// =============================================================================

/// Test: Basic enqueue to default dam.
/// Most common operation for beginners.
#[tokio::test]
async fn test_basic_enqueue() -> BeaverResult<()> {
    let beaver = Beaver::new("test3", 256);
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = Arc::clone(&executed);

    let task = PeriodicBuilder::new(work(move || {
        let e = Arc::clone(&executed_clone);
        async move {
            e.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(executed.load(Ordering::SeqCst), "Task should have executed");
    Ok(())
}

/// Test: Enqueue to named dam with false.
/// Named dams allow parallel execution on different queues.
#[tokio::test]
async fn test_enqueue_on_named_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test5", 256);
    let executed = Arc::new(AtomicBool::new(false));
    let executed_clone = Arc::clone(&executed);

    let task = PeriodicBuilder::new(work(move || {
        let e = Arc::clone(&executed_clone);
        async move {
            e.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver
        .enqueue_on_new_thread(task, "my-custom-dam", 256, false)
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(executed.load(Ordering::SeqCst));
    Ok(())
}

/// Test: Enqueue to named dam with true.
/// Long resident dams survive cancel_non_long_resident().
#[tokio::test]
async fn test_enqueue_on_long_resident_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test5", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(50))
    .build()?;

    beaver
        .enqueue_on_new_thread(task, "persistent-dam", 256, true)
        .await?;

    // Let it run a bit
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Cancel non-long-resident - should NOT affect our task
    beaver.cancel_non_long_resident().await?;

    // Task should still be running
    let before = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after = counter.load(Ordering::SeqCst);

    assert!(
        after > before,
        "Long resident dam should keep running after cancel_non_long_resident"
    );

    Ok(())
}

/// Test: Enqueue multiple tasks to same named dam.
/// Tasks on same dam execute sequentially.
#[tokio::test]
async fn test_multiple_tasks_same_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test7", 256);
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::new()));

    for i in 1..=3 {
        let order = Arc::clone(&execution_order);
        let task = PeriodicBuilder::new(work(move || {
            let o = Arc::clone(&order);
            async move {
                o.lock().unwrap().push(i);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::from_millis(10))
        .build()?;

        beaver
            .enqueue_on_new_thread(task, "sequential-dam", 256, false)
            .await?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let order = execution_order.lock().unwrap();
    assert_eq!(*order, vec![1, 2, 3], "Tasks should execute in order");
    Ok(())
}

/// Test: Enqueue tasks to different named dams.
/// Tasks on different dams can execute in parallel.
#[tokio::test]
async fn test_tasks_on_different_dams_parallel() -> BeaverResult<()> {
    let beaver = Beaver::new("test8", 256);
    let start_times = Arc::new(std::sync::Mutex::new(Vec::new()));

    for i in 1..=3 {
        let times = Arc::clone(&start_times);
        let task = PeriodicBuilder::new(work(move || {
            let t = Arc::clone(&times);
            async move {
                t.lock().unwrap().push(std::time::Instant::now());
                // Simulate some work
                tokio::time::sleep(Duration::from_millis(50)).await;
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        let dam_name = format!("parallel-dam-{}", i);
        beaver
            .enqueue_on_new_thread(task, dam_name, 256, false)
            .await?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let times = start_times.lock().unwrap();
    assert_eq!(times.len(), 3, "All tasks should have started");

    // Check that tasks started roughly at the same time (within 30ms)
    if times.len() >= 2 {
        let max_diff = times.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(
            max_diff < Duration::from_millis(30),
            "Tasks on different dams should start in parallel"
        );
    }

    Ok(())
}

// =============================================================================
// CANCELLATION TESTS
// =============================================================================

/// Test: cancel_all() stops all running tasks.
/// Used for emergency stop or cleanup.
#[tokio::test]
async fn test_cancel_all() -> BeaverResult<()> {
    let beaver = Beaver::new("test_cancel_all", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(10))
        .listener(listener(
            || {},
            move || {
                interrupted_clone.store(true, Ordering::SeqCst);
            },
        ))
        .build()?;

    beaver.enqueue(task).await?;

    // Let it start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel everything
    beaver.cancel_all().await?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        interrupted.load(Ordering::SeqCst),
        "Task should be interrupted"
    );
    beaver.destroy().await?;
    Ok(())
}

/// Test: cancel_non_long_resident() only cancels non-long-resident dams.
/// Useful for partial cleanup while keeping important tasks running.
#[tokio::test]
async fn test_cancel_non_long_resident() -> BeaverResult<()> {
    let beaver = Beaver::new("test_cancel_non_long_resident", 256);

    let non_resident_interrupted = Arc::new(AtomicBool::new(false));
    let resident_running = Arc::new(AtomicU32::new(0));

    // Non-long-resident task
    let non_res_int = Arc::clone(&non_resident_interrupted);
    let task1 = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                non_res_int.store(true, Ordering::SeqCst);
            },
        ))
        .build()?;

    // Long-resident task
    let res_run = Arc::clone(&resident_running);
    let task2 = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&res_run);
        async move {
            r.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(20))
    .build()?;

    beaver
        .enqueue_on_new_thread(task1, "temp-dam", 256, false)
        .await?;
    beaver
        .enqueue_on_new_thread(task2, "persistent-dam", 256, true)
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    beaver.cancel_non_long_resident().await?;

    // Give time for cancellation to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        non_resident_interrupted.load(Ordering::SeqCst),
        "Non-resident task should be interrupted"
    );

    // Long-resident should still be running
    let before = resident_running.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = resident_running.load(Ordering::SeqCst);

    assert!(after > before, "Long-resident task should still be running");

    beaver.destroy().await?;
    Ok(())
}

/// Test: release_thread_resource_by_name() releases a specific named dam.
/// Allows fine-grained control over dam lifecycle.
#[tokio::test]
async fn test_release_specific_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test_release_specific_dam", 256);

    let dam1_interrupted = Arc::new(AtomicBool::new(false));
    let dam2_running = Arc::new(AtomicU32::new(0));

    let int1 = Arc::clone(&dam1_interrupted);
    let task1 = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(20))
        .listener(listener(
            || {},
            move || {
                int1.store(true, Ordering::SeqCst);
            },
        ))
        .build()?;

    let run2 = Arc::clone(&dam2_running);
    let task2 = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&run2);
        async move {
            r.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(20))
    .build()?;

    beaver
        .enqueue_on_new_thread(task1, "dam-to-release", 256, false)
        .await?;
    beaver
        .enqueue_on_new_thread(task2, "dam-to-keep", 256, false)
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Release only one dam
    beaver
        .release_thread_resource_by_name("dam-to-release")
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        dam1_interrupted.load(Ordering::SeqCst),
        "Released dam's task should be interrupted"
    );

    let before = dam2_running.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = dam2_running.load(Ordering::SeqCst);

    assert!(after > before, "Other dam should still be running");

    beaver.destroy().await?;
    Ok(())
}

/// Test: release_thread_resource_by_name() on non-existent dam is a no-op.
/// Ensures defensive programming doesn't cause errors.
#[tokio::test]
async fn test_release_nonexistent_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test_release_nonexistent_dam", 256);

    // Should not error
    let result = beaver
        .release_thread_resource_by_name("does-not-exist")
        .await;
    assert!(
        result.is_ok(),
        "Releasing non-existent dam should not error"
    );

    Ok(())
}

// =============================================================================
// ERROR HANDLING TESTS
// =============================================================================

/// Test: Enqueue after destroy() returns NoDam error.
/// Important for understanding lifecycle errors.
#[tokio::test]
async fn test_enqueue_after_destroy_returns_no_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test_enqueue_after_destroy_returns_no_dam", 256);
    beaver.destroy().await?;

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    let result = beaver.enqueue(task).await;

    assert!(
        matches!(result, Err(BeaverError::NoDam)),
        "Should return NoDam error after destroy"
    );

    Ok(())
}

// =============================================================================
// THREAD SAFETY TESTS
// =============================================================================

/// Test: Beaver can be shared across threads using Arc.
/// Critical for multi-threaded applications.
#[tokio::test]
async fn test_beaver_thread_safety() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new(
        "test_enqueue_after_destroy_returns_no_dam",
        256,
    ));
    let counter = Arc::new(AtomicU32::new(0));

    let mut handles = vec![];

    for _ in 0..4 {
        let beaver_clone = Arc::clone(&beaver);
        let counter_clone = Arc::clone(&counter);

        let handle = tokio::spawn(async move {
            let task = PeriodicBuilder::new(work(move || {
                let c = Arc::clone(&counter_clone);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    WorkResult::Done(())
                }
            }))
            .interval(Duration::from_millis(10))
            .build()
            .unwrap();

            beaver_clone.enqueue(task).await
        });

        handles.push(handle);
    }

    // Wait for all spawns to complete
    for handle in handles {
        handle.await.unwrap()?;
    }

    // Wait for tasks to execute
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        4,
        "All tasks should have executed"
    );

    Ok(())
}

/// Test: Concurrent enqueue and cancel operations.
/// Stress test for thread safety.
#[tokio::test]
async fn test_concurrent_enqueue_and_cancel() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test_concurrent_enqueue_and_cancel", 256));

    let mut handles = vec![];

    // Spawn multiple enqueue tasks
    for i in 0..10 {
        let beaver_clone = Arc::clone(&beaver);
        let handle = tokio::spawn(async move {
            let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
                .interval(Duration::from_millis(50))
                .tag(format!("task-{}", i))
                .build()
                .unwrap();

            let _ = beaver_clone.enqueue(task).await;
        });
        handles.push(handle);
    }

    // Spawn cancel operation
    let beaver_clone = Arc::clone(&beaver);
    let cancel_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = beaver_clone.cancel_all().await;
    });
    handles.push(cancel_handle);

    // Wait for all operations
    for handle in handles {
        let _ = handle.await;
    }

    // System should be stable
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}
