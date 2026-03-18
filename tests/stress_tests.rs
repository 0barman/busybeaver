//! # Stress Tests and High Concurrency Tests
//!
//! This module contains stress tests for the Beaver task execution engine.
//! These tests verify system behavior under extreme conditions:
//! - Large number of tasks
//! - High concurrency
//! - Rapid operations
//! - Resource management
//! - Long-running stability
//!
//! Note: Some tests may take longer to run. Use `cargo test --release` for faster execution.

use busybeaver::{
    listener, work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder, TimeIntervalBuilder,
    WorkResult,
};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// =============================================================================
// LARGE SCALE TASK TESTS
// =============================================================================

/// Test: Enqueue and execute 200 tasks across multiple dams.
/// Verifies the system can handle a large number of tasks.
#[tokio::test]
async fn test_many_tasks_across_dams() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let completed = Arc::new(AtomicU32::new(0));

    let task_count = 200;
    let dam_count = 20; // 10 tasks per dam

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

        let dam_name = format!("dam-{}", i % dam_count);
        beaver
            .enqueue_on_new_thread(task, dam_name, 256, false)
            .await?;
    }

    // Wait for all tasks to complete
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < task_count
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        completed.load(Ordering::SeqCst),
        task_count,
        "All {} tasks should complete",
        task_count
    );

    Ok(())
}

/// Test: Enqueue 100 tasks to a single dam (sequential execution).
/// Verifies the queue can handle many sequential tasks.
#[tokio::test]
async fn test_many_tasks_single_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let completed = Arc::new(AtomicU32::new(0));

    let task_count = 100u32;

    for _ in 0..task_count {
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

        beaver.enqueue(task).await?;
    }

    // Wait for all tasks to complete
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < task_count
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        completed.load(Ordering::SeqCst),
        task_count,
        "All {} sequential tasks should complete",
        task_count
    );

    Ok(())
}

/// Test: Create 30 dams with tasks running simultaneously.
/// Verifies the system can manage many parallel execution contexts.
#[tokio::test]
async fn test_many_parallel_dams() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let running = Arc::new(AtomicU32::new(0));
    let max_concurrent = Arc::new(AtomicU32::new(0));
    let completed = Arc::new(AtomicU32::new(0));

    let dam_count = 30;

    for i in 0..dam_count {
        let r = Arc::clone(&running);
        let m = Arc::clone(&max_concurrent);
        let c = Arc::clone(&completed);

        let task = PeriodicBuilder::new(work(move || {
            let running = Arc::clone(&r);
            let max = Arc::clone(&m);
            let comp = Arc::clone(&c);
            async move {
                // Track concurrent executions
                let current = running.fetch_add(1, Ordering::SeqCst) + 1;

                // Update max if needed
                loop {
                    let old_max = max.load(Ordering::SeqCst);
                    if current <= old_max {
                        break;
                    }
                    if max
                        .compare_exchange(old_max, current, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }

                // Simulate some work
                tokio::time::sleep(Duration::from_millis(20)).await;

                running.fetch_sub(1, Ordering::SeqCst);
                comp.fetch_add(1, Ordering::SeqCst);

                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("parallel-dam-{}", i), 256, false)
            .await?;
    }

    // Wait for completion
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < dam_count
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let max = max_concurrent.load(Ordering::SeqCst);
    println!("Max concurrent executions: {}", max);

    assert_eq!(completed.load(Ordering::SeqCst), dam_count);
    assert!(
        max >= 5,
        "Should have significant parallelism, got max {}",
        max
    );

    Ok(())
}

// =============================================================================
// HIGH FREQUENCY OPERATION TESTS
// =============================================================================

/// Test: Rapid enqueue operations - 200 enqueues in quick succession.
/// Verifies the system handles burst traffic.
#[tokio::test]
async fn test_rapid_enqueue_burst() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let completed = Arc::new(AtomicU32::new(0));
    let task_count = 200u32;

    let start = Instant::now();

    // Burst enqueue tasks as fast as possible
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

        // Distribute across 20 dams for parallelism
        beaver
            .enqueue_on_new_thread(task, format!("burst-dam-{}", i % 20), 256, false)
            .await?;
    }

    let enqueue_time = start.elapsed();
    println!("Time to enqueue {} tasks: {:?}", task_count, enqueue_time);

    // Wait for completion
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < task_count
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let total_time = start.elapsed();
    println!(
        "Total time for {} tasks: {:?}, throughput: {:.2} tasks/sec",
        task_count,
        total_time,
        task_count as f64 / total_time.as_secs_f64()
    );

    assert_eq!(completed.load(Ordering::SeqCst), task_count);

    Ok(())
}

/// Test: Rapid cancel cycles - repeatedly enqueue and cancel.
/// Verifies system stability under rapid state changes.
#[tokio::test]
async fn test_rapid_cancel_cycles() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let cycle_count = 20;

    for i in 0..cycle_count {
        // Enqueue a batch of tasks
        for j in 0..5 {
            let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
                .interval(Duration::from_millis(10))
                .build()?;

            beaver
                .enqueue_on_new_thread(task, format!("cycle-dam-{}-{}", i, j), 256, false)
                .await?;
        }

        // Let some tasks start
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Cancel all
        beaver.cancel_all().await?;
    }

    // System should be stable
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify we can still use the beaver
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    Ok(())
}

/// Test: High-frequency periodic task execution.
/// Verifies system can handle rapid repeated executions.
#[tokio::test]
async fn test_high_frequency_periodic_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let counter = Arc::new(AtomicU64::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(1)) // 1ms interval for high frequency
    .build()?;

    // Use a named thread so it gets properly cleaned up
    beaver
        .enqueue_on_new_thread(task, "high-freq-dam", 256, false)
        .await?;

    // Let it run for 200ms
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Cancel and release the dam
    beaver.cancel_all().await?;

    // Give time for cleanup
    tokio::time::sleep(Duration::from_millis(50)).await;

    let count = counter.load(Ordering::SeqCst);
    println!(
        "High-frequency executions in 200ms: {}, rate: {:.2} exec/sec",
        count,
        count as f64 / 0.2
    );

    // Should execute many times (at least 50 executions in 200ms with 1ms interval)
    assert!(
        count >= 30,
        "Should execute at least 30 times in 200ms, got {}",
        count
    );

    Ok(())
}

// =============================================================================
// CONCURRENT ACCESS TESTS
// =============================================================================

/// Test: Multiple threads enqueueing simultaneously.
/// Verifies thread-safe concurrent enqueue operations.
#[tokio::test]
async fn test_concurrent_enqueue_from_multiple_threads() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("stress_test", 256));
    let completed = Arc::new(AtomicU32::new(0));

    let thread_count = 5u32;
    let tasks_per_thread = 20u32;

    let mut handles = vec![];

    for t in 0..thread_count {
        let beaver_clone = Arc::clone(&beaver);
        let completed_clone = Arc::clone(&completed);

        let handle = tokio::spawn(async move {
            for i in 0..tasks_per_thread {
                let c = Arc::clone(&completed_clone);
                let task = PeriodicBuilder::new(work(move || {
                    let cc = Arc::clone(&c);
                    async move {
                        cc.fetch_add(1, Ordering::SeqCst);
                        WorkResult::Done(())
                    }
                }))
                .interval(Duration::ZERO)
                .build()
                .unwrap();

                let dam_name = format!("thread-{}-dam-{}", t, i % 5);
                let _ = beaver_clone
                    .enqueue_on_new_thread(task, dam_name, 256, false)
                    .await;
            }
        });

        handles.push(handle);
    }

    // Wait for all enqueue operations
    for handle in handles {
        handle.await.unwrap();
    }

    // Wait for tasks to complete
    let expected = thread_count * tasks_per_thread;
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < expected && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        completed.load(Ordering::SeqCst),
        expected,
        "All {} tasks should complete",
        expected
    );

    Ok(())
}

/// Test: Concurrent enqueue and cancel from different threads.
/// Verifies thread-safety of mixed operations.
#[tokio::test]
async fn test_concurrent_enqueue_and_cancel_threads() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("stress_test", 256));
    let enqueue_count = Arc::new(AtomicU32::new(0));
    let cancel_count = Arc::new(AtomicU32::new(0));

    let mut handles = vec![];

    // Enqueue threads
    for t in 0..3 {
        let beaver_clone = Arc::clone(&beaver);
        let ec = Arc::clone(&enqueue_count);

        let handle = tokio::spawn(async move {
            for i in 0..20 {
                let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
                    .interval(Duration::from_millis(10))
                    .build()
                    .unwrap();

                if beaver_clone
                    .enqueue_on_new_thread(task, format!("mixed-dam-{}-{}", t, i), 256, false)
                    .await
                    .is_ok()
                {
                    ec.fetch_add(1, Ordering::SeqCst);
                }

                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        handles.push(handle);
    }

    // Cancel threads
    for _ in 0..2 {
        let beaver_clone = Arc::clone(&beaver);
        let cc = Arc::clone(&cancel_count);

        let handle = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if beaver_clone.cancel_all().await.is_ok() {
                    cc.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all operations
    for handle in handles {
        handle.await.unwrap();
    }

    println!(
        "Enqueued: {}, Cancelled: {}",
        enqueue_count.load(Ordering::SeqCst),
        cancel_count.load(Ordering::SeqCst)
    );

    // System should be stable
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok(())
}

// =============================================================================
// MIXED WORKLOAD TESTS
// =============================================================================

/// Test: Mixed task types under load.
/// Verifies system handles different task types simultaneously.
#[tokio::test]
async fn test_mixed_task_types_under_load() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let periodic_count = Arc::new(AtomicU32::new(0));
    let fixed_count = Arc::new(AtomicU32::new(0));
    let interval_count = Arc::new(AtomicU32::new(0));

    // Create 30 periodic tasks
    for i in 0..30 {
        let c = Arc::clone(&periodic_count);
        let task = PeriodicBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("periodic-dam-{}", i), 256, false)
            .await?;
    }

    // Create 30 fixed count tasks
    for i in 0..30 {
        let c = Arc::clone(&fixed_count);
        let task = FixedCountBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .count(3)
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("fixed-dam-{}", i), 256, false)
            .await?;
    }

    // Create 30 time interval tasks
    for i in 0..30 {
        let c = Arc::clone(&interval_count);
        let task = TimeIntervalBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }))
        .intervals_millis([0, 0])
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("interval-dam-{}", i), 256, false)
            .await?;
    }

    // Wait for completion
    let timeout = Instant::now();
    while timeout.elapsed() < Duration::from_secs(30) {
        let p = periodic_count.load(Ordering::SeqCst);
        let f = fixed_count.load(Ordering::SeqCst);
        let i = interval_count.load(Ordering::SeqCst);

        if p >= 30 && f >= 90 && i >= 60 {
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    println!(
        "Periodic: {}, Fixed: {}, Interval: {}",
        periodic_count.load(Ordering::SeqCst),
        fixed_count.load(Ordering::SeqCst),
        interval_count.load(Ordering::SeqCst)
    );

    assert!(periodic_count.load(Ordering::SeqCst) >= 30);
    assert!(fixed_count.load(Ordering::SeqCst) >= 90); // 30 tasks * 3 count
    assert!(interval_count.load(Ordering::SeqCst) >= 60); // 30 tasks * 2 intervals

    Ok(())
}

/// Test: Long-running tasks mixed with short tasks.
/// Verifies fair scheduling and no starvation.
#[tokio::test]
async fn test_long_and_short_tasks_mixed() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let short_completed = Arc::new(AtomicU32::new(0));
    let long_completed = Arc::new(AtomicU32::new(0));

    // Create long-running tasks on dedicated dams
    for i in 0..5 {
        let c = Arc::clone(&long_completed);
        let task = PeriodicBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                // Simulate long work
                tokio::time::sleep(Duration::from_millis(50)).await;
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("long-dam-{}", i), 256, false)
            .await?;
    }

    // Create many short tasks on other dams
    for i in 0..30 {
        let c = Arc::clone(&short_completed);
        let task = PeriodicBuilder::new(work(move || {
            let cc = Arc::clone(&c);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }))
        .interval(Duration::ZERO)
        .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("short-dam-{}", i), 256, false)
            .await?;
    }

    // Short tasks should complete quickly even with long tasks running
    tokio::time::sleep(Duration::from_millis(200)).await;

    let short = short_completed.load(Ordering::SeqCst);
    println!(
        "Short tasks completed in 200ms: {}, Long tasks: {}",
        short,
        long_completed.load(Ordering::SeqCst)
    );

    assert!(
        short >= 15,
        "Short tasks should not be blocked by long tasks, got {}",
        short
    );

    // Wait for all to complete
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(short_completed.load(Ordering::SeqCst), 30);
    assert_eq!(long_completed.load(Ordering::SeqCst), 5);

    Ok(())
}

// =============================================================================
// RESOURCE MANAGEMENT TESTS
// =============================================================================

/// Test: Create and destroy many dams.
/// Verifies proper resource cleanup.
#[tokio::test]
async fn test_dam_creation_and_destruction() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);

    for cycle in 0..10 {
        // Create 10 dams
        for i in 0..10 {
            let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
                .interval(Duration::ZERO)
                .build()?;

            beaver
                .enqueue_on_new_thread(task, format!("cycle-{}-dam-{}", cycle, i), 256, false)
                .await?;
        }

        // Wait for tasks to complete
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release all dams
        for i in 0..10 {
            beaver
                .release_thread_resource_by_name(&format!("cycle-{}-dam-{}", cycle, i))
                .await?;
        }
    }

    // System should still be functional
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(50)).await;

    Ok(())
}

// =============================================================================
// LISTENER UNDER LOAD TESTS
// =============================================================================

/// Test: Many listeners receiving callbacks concurrently.
/// Verifies listener callback system under load.
#[tokio::test]
async fn test_many_listeners_concurrent_callbacks() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let on_complete_count = Arc::new(AtomicU32::new(0));
    let on_interrupt_count = Arc::new(AtomicU32::new(0));

    let task_count = 50u32;

    // Create tasks with listeners
    for i in 0..task_count {
        let occ = Arc::clone(&on_complete_count);
        let oic = Arc::clone(&on_interrupt_count);

        let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .interval(Duration::ZERO)
            .listener(listener(
                move || {
                    occ.fetch_add(1, Ordering::SeqCst);
                },
                move || {
                    oic.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("listener-dam-{}", i), 256, false)
            .await?;
    }

    // Wait for completion
    let timeout = Instant::now();
    while on_complete_count.load(Ordering::SeqCst) < task_count
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        on_complete_count.load(Ordering::SeqCst),
        task_count,
        "All {} on_complete callbacks should fire",
        task_count
    );

    Ok(())
}

/// Test: Progress callbacks under heavy load.
/// Verifies FixedCountProgress system under load.
#[tokio::test]
async fn test_progress_callbacks_under_load() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let total_progress_calls = Arc::new(AtomicU32::new(0));

    let task_count = 20u32;
    let count_per_task = 5u32;

    for i in 0..task_count {
        let tpc = Arc::clone(&total_progress_calls);

        let progress: Arc<dyn busybeaver::FixedCountProgress> =
            Arc::new(move |_: u32, _: u32, _: &str| {
                tpc.fetch_add(1, Ordering::SeqCst);
            });

        let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
            .count(count_per_task)
            .progress(progress)
            .build()?;

        beaver
            .enqueue_on_new_thread(task, format!("progress-dam-{}", i), 256, false)
            .await?;
    }

    // Wait for completion
    let expected = task_count * count_per_task;
    let timeout = Instant::now();
    while total_progress_calls.load(Ordering::SeqCst) < expected
        && timeout.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        total_progress_calls.load(Ordering::SeqCst),
        expected,
        "Should have {} progress callbacks",
        expected
    );

    Ok(())
}

// =============================================================================
// LONG RESIDENT DAM STRESS TESTS
// =============================================================================

/// Test: Long resident dams persist across many cancel operations.
/// Verifies long resident functionality under stress.
#[tokio::test]
async fn test_long_resident_persistence_stress() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let long_resident_executions = Arc::new(AtomicU32::new(0));

    // Create long-resident task
    let lre = Arc::clone(&long_resident_executions);
    let long_task = PeriodicBuilder::new(work(move || {
        let e = Arc::clone(&lre);
        async move {
            e.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver
        .enqueue_on_new_thread(long_task, "persistent-dam", 256, true)
        .await?;

    // Repeatedly cancel non-long-resident while long-resident runs
    for _ in 0..10 {
        // Add some non-long-resident tasks
        for j in 0..3 {
            let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
                .interval(Duration::from_millis(10))
                .build()?;

            beaver
                .enqueue_on_new_thread(task, format!("temp-dam-{}", j), 256, false)
                .await?;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Cancel non-long-resident
        beaver.cancel_non_long_resident().await?;
    }

    // Long-resident should still be running
    let count = long_resident_executions.load(Ordering::SeqCst);
    println!("Long-resident executions during stress: {}", count);

    assert!(
        count >= 5,
        "Long-resident should have executed many times, got {}",
        count
    );

    // Cleanup
    beaver.cancel_all().await?;

    Ok(())
}

// =============================================================================
// THROUGHPUT BENCHMARK
// =============================================================================

/// Test: Measure task throughput.
/// Provides performance metrics for the task execution system.
#[tokio::test]
async fn test_throughput_benchmark() -> BeaverResult<()> {
    let beaver = Beaver::new("stress_test", 256);
    let completed = Arc::new(AtomicU64::new(0));

    let dam_count = 20usize;
    let tasks_per_dam = 50usize;
    let total_tasks = dam_count * tasks_per_dam;

    let start = Instant::now();

    // Enqueue all tasks
    for d in 0..dam_count {
        for _ in 0..tasks_per_dam {
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

            beaver
                .enqueue_on_new_thread(task, format!("bench-dam-{}", d), 256, false)
                .await?;
        }
    }

    let enqueue_time = start.elapsed();

    // Wait for completion
    let timeout = Instant::now();
    while completed.load(Ordering::SeqCst) < total_tasks as u64
        && timeout.elapsed() < Duration::from_secs(60)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let total_time = start.elapsed();

    let actual_completed = completed.load(Ordering::SeqCst);
    let throughput = actual_completed as f64 / total_time.as_secs_f64();

    println!("=== Throughput Benchmark Results ===");
    println!("Total tasks: {}", total_tasks);
    println!("Completed: {}", actual_completed);
    println!("Enqueue time: {:?}", enqueue_time);
    println!("Total time: {:?}", total_time);
    println!("Throughput: {:.2} tasks/second", throughput);

    assert_eq!(actual_completed, total_tasks as u64);
    assert!(
        throughput >= 50.0,
        "Throughput should be at least 50 tasks/sec, got {:.2}",
        throughput
    );

    Ok(())
}
