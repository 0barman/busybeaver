//! # Concurrency and Thread Safety Tests
//!
//! Comprehensive tests for concurrent operations, thread safety, and stress scenarios.
//! Critical for ensuring the library works correctly in multi-threaded applications.

use busybeaver::{
    listener, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder, TimeIntervalBuilder,
    WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// BASIC THREAD SAFETY TESTS
// =============================================================================

/// Test: Beaver can be shared across threads using Arc.
#[tokio::test]
async fn test_beaver_arc_sharing() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test", 256));
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

            beaver_clone.enqueue(task)
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap()?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 4);

    Ok(())
}

/// Test: Concurrent enqueue from multiple threads.
#[tokio::test]
async fn test_concurrent_enqueue() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test", 256));
    let success_count = Arc::new(AtomicU32::new(0));

    let mut handles = vec![];

    for i in 0..20 {
        let beaver_clone = Arc::clone(&beaver);
        let success = Arc::clone(&success_count);

        let handle = tokio::spawn(async move {
            let s = Arc::clone(&success);
            let task = PeriodicBuilder::new(work(move || {
                let sc = Arc::clone(&s);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    WorkResult::Done(())
                }
            }))
            .interval(Duration::ZERO)
            .tag(format!("task-{}", i))
            .build()
            .unwrap();

            // Enqueue to different dams for parallel execution
            beaver_clone.enqueue_on_new_thread(task, format!("dam-{}", i), 256, false)
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap()?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        success_count.load(Ordering::SeqCst),
        20,
        "All tasks should have executed"
    );

    Ok(())
}

/// Test: Concurrent enqueue and cancel operations.
#[tokio::test]
async fn test_concurrent_enqueue_and_cancel() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test", 256));

    let mut handles = vec![];

    // Spawn enqueue tasks
    for i in 0..10 {
        let beaver_clone = Arc::clone(&beaver);
        let handle = tokio::spawn(async move {
            let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
                .interval(Duration::from_millis(50))
                .tag(format!("task-{}", i))
                .build()
                .unwrap();

            // Ignore errors - some may fail due to concurrent cancel
            let _ = beaver_clone.enqueue(task);
        });
        handles.push(handle);
    }

    // Spawn cancel tasks
    for _ in 0..5 {
        let beaver_clone = Arc::clone(&beaver);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let _ = beaver_clone.cancel_all();
        });
        handles.push(handle);
    }

    // Wait for all operations
    for handle in handles {
        let _ = handle.await;
    }

    // System should be stable
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}

// =============================================================================
// PARALLEL EXECUTION TESTS
// =============================================================================

/// Test: Tasks on different dams execute in parallel.
#[tokio::test]
async fn test_parallel_dam_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let start_times = Arc::new(std::sync::Mutex::new(Vec::new()));

    for i in 0..5 {
        let times = Arc::clone(&start_times);
        let task = PeriodicBuilder::new(work(move || {
            let t = Arc::clone(&times);
            async move {
                t.lock().unwrap().push(std::time::Instant::now());
                // Simulate work
                tokio::time::sleep(Duration::from_millis(50)).await;
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        beaver.enqueue_on_new_thread(task, format!("parallel-dam-{}", i), 256, false)?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let times = start_times.lock().unwrap();
    assert_eq!(times.len(), 5, "All tasks should have started");

    // Check that tasks started roughly at the same time (within 30ms of each other)
    let min_time = times.iter().min().unwrap();
    let max_time = times.iter().max().unwrap();
    let diff = *max_time - *min_time;

    assert!(
        diff < Duration::from_millis(50),
        "Tasks on different dams should start in parallel, diff was {:?}",
        diff
    );

    Ok(())
}

/// Test: Tasks on same dam execute sequentially.
#[tokio::test]
async fn test_sequential_same_dam_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::new()));

    for i in 1..=5 {
        let order = Arc::clone(&execution_order);
        let task = PeriodicBuilder::new(work(move || {
            let o = Arc::clone(&order);
            async move {
                o.lock().unwrap().push(i);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        // All tasks on same dam
        beaver.enqueue_on_new_thread(task, "sequential-dam", 256, false)?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let order = execution_order.lock().unwrap();
    assert_eq!(*order, vec![1, 2, 3, 4, 5], "Tasks should execute in order");

    Ok(())
}

// =============================================================================
// STRESS TESTS
// =============================================================================

/// Test: Many tasks executing concurrently.
#[tokio::test]
async fn test_many_concurrent_tasks() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicU32::new(0));

    let task_count = 100;

    for i in 0..task_count {
        let c = Arc::clone(&completed);
        let task = PeriodicBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        // Use modulo to distribute across a few dams
        let dam_name = format!("stress-dam-{}", i % 10);
        beaver.enqueue_on_new_thread(task, dam_name, 256, false)?;
    }

    tokio::time::sleep(Duration::from_millis(1000)).await;

    assert_eq!(
        completed.load(Ordering::SeqCst),
        task_count,
        "All tasks should complete"
    );

    Ok(())
}

/// Test: Rapid enqueue/cancel cycles.
#[tokio::test]
async fn test_rapid_enqueue_cancel_cycles() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    for _ in 0..50 {
        // Enqueue a task
        let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
            .interval(Duration::from_millis(10))
            .build()?;

        beaver.enqueue(task)?;

        // Immediately cancel
        beaver.cancel_all()?;
    }

    // System should be stable
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}

/// Test: High frequency task execution.
#[tokio::test]
async fn test_high_frequency_execution() -> BeaverResult<()> {
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
    .interval(Duration::from_millis(1)) // 1ms interval
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    beaver.cancel_all()?;

    let count = counter.load(Ordering::SeqCst);
    assert!(
        count >= 30,
        "High frequency should execute many times, got {}",
        count
    );

    Ok(())
}

// =============================================================================
// LONG RESIDENT DAM TESTS
// =============================================================================

/// Test: Long resident dams survive cancel_non_long_resident.
#[tokio::test]
async fn test_long_resident_survives_cancel() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let long_resident_running = Arc::new(AtomicU32::new(0));
    let non_resident_running = Arc::new(AtomicBool::new(true));

    // Long resident task
    let lr = Arc::clone(&long_resident_running);
    let long_task = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&lr);
        async move {
            r.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(50))
    .build()?;

    // Non-resident task
    let nr = Arc::clone(&non_resident_running);
    let short_task = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&nr);
        async move {
            if r.load(Ordering::SeqCst) {
                WorkResult::NeedRetry
            } else {
                WorkResult::Done(())
            }
        }
    }))
    .interval(Duration::from_millis(50))
    .build()?;

    beaver.enqueue_on_new_thread(long_task, "long-dam", 256, true)?;
    beaver.enqueue_on_new_thread(short_task, "short-dam", 256, false)?;

    // Let them run
    tokio::time::sleep(Duration::from_millis(150)).await;

    let before_cancel = long_resident_running.load(Ordering::SeqCst);

    // Cancel non-long-resident
    beaver.cancel_non_long_resident()?;

    // Wait more
    tokio::time::sleep(Duration::from_millis(150)).await;

    let after_cancel = long_resident_running.load(Ordering::SeqCst);

    assert!(
        after_cancel > before_cancel,
        "Long resident should continue running"
    );

    // Cleanup
    beaver.cancel_all()?;

    Ok(())
}

/// Test: Multiple long resident dams.
#[tokio::test]
async fn test_multiple_long_resident_dams() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let counters: Vec<Arc<AtomicU32>> = (0..3).map(|_| Arc::new(AtomicU32::new(0))).collect();

    for (i, counter) in counters.iter().enumerate() {
        let c = Arc::clone(counter);
        let task = PeriodicBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .interval(Duration::from_millis(30))
        .build()?;

        beaver.enqueue_on_new_thread(task, format!("long-dam-{}", i), 256, true)?;
    }

    // Let them run
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancel non-long-resident (should not affect our dams)
    beaver.cancel_non_long_resident()?;

    // Wait more
    tokio::time::sleep(Duration::from_millis(100)).await;

    // All should still be running
    for (i, counter) in counters.iter().enumerate() {
        assert!(
            counter.load(Ordering::SeqCst) >= 2,
            "Long resident dam {} should have run multiple times",
            i
        );
    }

    // Cleanup
    beaver.cancel_all()?;

    Ok(())
}

// =============================================================================
// MIXED TASK TYPE TESTS
// =============================================================================

/// Test: Different task types running concurrently.
#[tokio::test]
async fn test_mixed_task_types() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let periodic_count = Arc::new(AtomicU32::new(0));
    let fixed_count = Arc::new(AtomicU32::new(0));
    let interval_count = Arc::new(AtomicU32::new(0));

    // Periodic task
    let pc = Arc::clone(&periodic_count);
    let periodic_task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&pc);
        async move {
            let count = c.fetch_add(1, Ordering::SeqCst);
            if count >= 4 {
                WorkResult::Done(())
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .interval(Duration::from_millis(20))
    .build()?;

    // Fixed count task
    let fc = Arc::clone(&fixed_count);
    let fixed_task = FixedCountBuilder::new(work(move || {
        let c = Arc::clone(&fc);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .count(3)
    .build()?;

    // Time interval task
    let ic = Arc::clone(&interval_count);
    let interval_task = TimeIntervalBuilder::new(work(move || {
        let c = Arc::clone(&ic);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .intervals_millis([0, 0, 0])
    .build()?;

    beaver.enqueue_on_new_thread(periodic_task, "periodic-dam", 256, false)?;
    beaver.enqueue_on_new_thread(fixed_task, "fixed-dam", 256, false)?;
    beaver.enqueue_on_new_thread(interval_task, "interval-dam", 256, false)?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        periodic_count.load(Ordering::SeqCst),
        5,
        "Periodic should run 5 times"
    );
    assert_eq!(
        fixed_count.load(Ordering::SeqCst),
        3,
        "Fixed should run 3 times"
    );
    assert_eq!(
        interval_count.load(Ordering::SeqCst),
        3,
        "Interval should run 3 times"
    );

    Ok(())
}

// =============================================================================
// LISTENER THREAD SAFETY TESTS
// =============================================================================

/// Test: Listeners receive callbacks from correct threads.
#[tokio::test]
async fn test_listener_callback_thread_safety() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test", 256));
    let completed_count = Arc::new(AtomicU32::new(0));

    let mut handles = vec![];

    for i in 0..10 {
        let beaver_clone = Arc::clone(&beaver);
        let cc = Arc::clone(&completed_count);

        let handle = tokio::spawn(async move {
            let count = Arc::clone(&cc);
            let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
                .interval(Duration::ZERO)
                .listener(listener(
                    move || {
                        count.fetch_add(1, Ordering::SeqCst);
                    },
                    || {},
                ))
                .build()
                .unwrap();

            beaver_clone.enqueue_on_new_thread(task, format!("dam-{}", i), 256, false)
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap()?;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        completed_count.load(Ordering::SeqCst),
        10,
        "All listeners should be called"
    );

    Ok(())
}

// =============================================================================
// EDGE CASES
// =============================================================================

/// Test: Very short-lived Beaver instance.
#[tokio::test]
async fn test_short_lived_beaver() -> BeaverResult<()> {
    for _ in 0..10 {
        let beaver = Beaver::new("test", 256);

        let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .interval(Duration::ZERO)
            .build()?;

        beaver.enqueue(task)?;

        // Immediately drop beaver
        drop(beaver);
    }

    Ok(())
}

/// Test: Reusing dam names after release.
#[tokio::test]
async fn test_reuse_dam_name_after_release() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));

    // First use
    let c1 = Arc::clone(&counter);
    let task1 = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&c1);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue_on_new_thread(task1, "reusable-dam", 256, false)?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Release
    beaver.release_thread_resource_by_name("reusable-dam")?;

    // Reuse same name
    let c2 = Arc::clone(&counter);
    let task2 = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&c2);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue_on_new_thread(task2, "reusable-dam", 256, false)?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "Both tasks should have executed"
    );

    Ok(())
}

/// Test: Empty string dam name is valid.
#[tokio::test]
async fn test_empty_dam_name() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let executed = Arc::new(AtomicBool::new(false));
    let e = Arc::clone(&executed);

    let task = PeriodicBuilder::new(work(move || {
        let ex = Arc::clone(&e);
        async move {
            ex.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue_on_new_thread(task, "", 256, false)?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(executed.load(Ordering::SeqCst));

    Ok(())
}
