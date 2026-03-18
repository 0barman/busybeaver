//! # Work and WorkResult Tests
//!
//! Comprehensive tests for the Work trait, work() function, and WorkResult enum.
//! These are the fundamental building blocks for defining task behavior.

use async_trait::async_trait;
use busybeaver::{work, Beaver, BeaverResult, PeriodicBuilder, Work, WorkResult};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// WORK RESULT TESTS
// =============================================================================

/// Test: WorkResult::NeedRetry indicates more work is needed.
#[test]
fn test_work_result_need_retry() {
    let result: WorkResult<()> = WorkResult::NeedRetry;

    assert!(
        result.need_retry(),
        "NeedRetry should return true for need_retry()"
    );
    assert!(
        result.as_done().is_none(),
        "NeedRetry should have no done value"
    );
}

/// Test: WorkResult::Done indicates work is complete.
#[test]
fn test_work_result_done() {
    let result: WorkResult<i32> = WorkResult::Done(42);

    assert!(
        !result.need_retry(),
        "Done should return false for need_retry()"
    );
    assert_eq!(result.as_done(), Some(&42), "Done should have the value");
}

/// Test: WorkResult::Done with unit type.
#[test]
fn test_work_result_done_unit() {
    let result: WorkResult<()> = WorkResult::Done(());

    assert!(!result.need_retry());
    assert_eq!(result.as_done(), Some(&()));
}

/// Test: WorkResult::into_done consumes the result.
#[test]
fn test_work_result_into_done() {
    let result: WorkResult<String> = WorkResult::Done("hello".to_string());

    let value = result.into_done();
    assert_eq!(value, Some("hello".to_string()));

    // NeedRetry returns None
    let result2: WorkResult<String> = WorkResult::NeedRetry;
    assert_eq!(result2.into_done(), None);
}

/// Test: WorkResult can be cloned (when T: Clone).
#[test]
fn test_work_result_clone() {
    let result1: WorkResult<i32> = WorkResult::Done(42);
    let result2 = result1.clone();

    assert_eq!(result1.as_done(), result2.as_done());

    let retry1: WorkResult<i32> = WorkResult::NeedRetry;
    let retry2 = retry1.clone();

    assert!(retry2.need_retry());
}

/// Test: WorkResult Debug implementation.
#[test]
fn test_work_result_debug() {
    let done: WorkResult<i32> = WorkResult::Done(42);
    let retry: WorkResult<i32> = WorkResult::NeedRetry;

    let done_str = format!("{:?}", done);
    let retry_str = format!("{:?}", retry);

    assert!(done_str.contains("Done"));
    assert!(done_str.contains("42"));
    assert!(retry_str.contains("NeedRetry"));
}

// =============================================================================
// WORK() CLOSURE FUNCTION TESTS
// =============================================================================

/// Test: work() function creates a Work implementation from async closure.
/// This is the most common way to create work.
#[tokio::test]
async fn test_work_function_basic() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
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

    assert!(executed.load(Ordering::SeqCst));

    Ok(())
}

/// Test: work() with captured variables.
#[tokio::test]
async fn test_work_with_captured_variables() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    // Capture multiple variables
    let multiplier = 2u32;
    let base_value = 10u32;

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            let value = base_value * multiplier; // Use captured values
            c.fetch_add(value, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 20);

    Ok(())
}

/// Test: work() with async operations inside.
#[tokio::test]
async fn test_work_with_async_operations() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let result = Arc::new(AtomicU32::new(0));
    let result_clone = Arc::clone(&result);

    let task = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&result_clone);
        async move {
            // Simulate async I/O
            tokio::time::sleep(Duration::from_millis(10)).await;
            r.store(42, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(result.load(Ordering::SeqCst), 42);

    Ok(())
}

/// Test: work() returning NeedRetry causes continuation.
#[tokio::test]
async fn test_work_returning_need_retry() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            let count = c.fetch_add(1, Ordering::SeqCst);
            if count >= 4 {
                WorkResult::Done(())
            } else {
                WorkResult::NeedRetry
            }
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 5);

    Ok(())
}

/// Test: work() with complex async logic.
#[tokio::test]
async fn test_work_complex_async_logic() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let results_clone = Arc::clone(&results);

    let task = PeriodicBuilder::new(work(move || {
        let r = Arc::clone(&results_clone);
        async move {
            // Complex async workflow
            let step1 = async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                1
            };

            let step2 = async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                2
            };

            // Execute both concurrently
            let (a, b) = tokio::join!(step1, step2);

            r.lock().unwrap().push(a + b);
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(results.lock().unwrap()[0], 3);

    Ok(())
}

// =============================================================================
// CUSTOM WORK TRAIT IMPLEMENTATION TESTS
// =============================================================================

/// Custom Work implementation for testing.
/// Demonstrates how to implement the Work trait directly.
struct CountingWork {
    counter: Arc<AtomicU32>,
    max_count: u32,
}

#[async_trait]
impl Work for CountingWork {
    async fn execute(&self) -> WorkResult<()> {
        let count = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= self.max_count {
            WorkResult::Done(())
        } else {
            WorkResult::NeedRetry
        }
    }
}

/// Test: Custom Work trait implementation.
#[tokio::test]
async fn test_custom_work_implementation() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));

    let custom_work = CountingWork {
        counter: Arc::clone(&counter),
        max_count: 5,
    };

    let task = PeriodicBuilder::new(custom_work)
        .interval(Duration::from_millis(10))
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(counter.load(Ordering::SeqCst), 5);

    Ok(())
}

/// Work implementation that simulates network request.
struct NetworkRequestWork {
    attempt: Arc<AtomicU32>,
    success_on: u32,
    result: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait]
impl Work for NetworkRequestWork {
    async fn execute(&self) -> WorkResult<()> {
        let current = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;

        // Simulate network latency
        tokio::time::sleep(Duration::from_millis(10)).await;

        if current >= self.success_on {
            // Success!
            *self.result.lock().unwrap() = Some(format!("Success on attempt {}", current));
            WorkResult::Done(())
        } else {
            // Failure, retry
            WorkResult::NeedRetry
        }
    }
}

/// Test: Work implementation simulating network requests with retries.
#[tokio::test]
async fn test_network_request_simulation() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let attempt = Arc::new(AtomicU32::new(0));
    let result = Arc::new(std::sync::Mutex::new(None));

    let network_work = NetworkRequestWork {
        attempt: Arc::clone(&attempt),
        success_on: 3,
        result: Arc::clone(&result),
    };

    let task = PeriodicBuilder::new(network_work)
        .interval(Duration::from_millis(20))
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(attempt.load(Ordering::SeqCst), 3);
    assert_eq!(
        *result.lock().unwrap(),
        Some("Success on attempt 3".to_string())
    );

    Ok(())
}

/// Work implementation with internal state.
struct StatefulWork {
    state: std::sync::Mutex<Vec<String>>,
    max_items: usize,
}

#[async_trait]
impl Work for StatefulWork {
    async fn execute(&self) -> WorkResult<()> {
        let mut state = self.state.lock().unwrap();
        let item = format!("item-{}", state.len());
        state.push(item);

        if state.len() >= self.max_items {
            WorkResult::Done(())
        } else {
            WorkResult::NeedRetry
        }
    }
}

/// Test: Work implementation with mutable internal state.
#[tokio::test]
async fn test_stateful_work() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let work = Arc::new(StatefulWork {
        state: std::sync::Mutex::new(Vec::new()),
        max_items: 3,
    });

    // We need to clone for later access
    let work_ref = Arc::clone(&work);

    let task = PeriodicBuilder::new(StatefulWorkWrapper(work))
        .interval(Duration::from_millis(10))
        .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let state = work_ref.state.lock().unwrap();
    assert_eq!(state.len(), 3);
    assert_eq!(state[0], "item-0");
    assert_eq!(state[1], "item-1");
    assert_eq!(state[2], "item-2");

    Ok(())
}

/// Wrapper to make Arc<StatefulWork> implement Work.
struct StatefulWorkWrapper(Arc<StatefulWork>);

#[async_trait]
impl Work for StatefulWorkWrapper {
    async fn execute(&self) -> WorkResult<()> {
        self.0.execute().await
    }
}

// =============================================================================
// EDGE CASES AND ERROR HANDLING
// =============================================================================

/// Test: Work that always returns Done executes only once.
#[tokio::test]
async fn test_work_always_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::Done(()) // Always done
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "Should execute only once"
    );

    Ok(())
}

/// Test: Work that always returns NeedRetry continues indefinitely.
#[tokio::test]
async fn test_work_always_need_retry() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry // Always retry
        }
    }))
    .interval(Duration::from_millis(10))
    .build()?;

    beaver.enqueue(task).await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should have executed multiple times
    let count = counter.load(Ordering::SeqCst);
    assert!(count >= 5, "Should execute multiple times");

    // Cleanup
    beaver.cancel_all().await?;
    beaver.destroy().await?;

    Ok(())
}

/// Test: Work with conditional retry based on external state.
#[tokio::test]
async fn test_work_conditional_retry() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let should_continue = Arc::new(AtomicBool::new(true));
    let counter = Arc::new(AtomicU32::new(0));

    let continue_flag = Arc::clone(&should_continue);
    let counter_clone = Arc::clone(&counter);

    let task = PeriodicBuilder::new(work(move || {
        let flag = Arc::clone(&continue_flag);
        let c = Arc::clone(&counter_clone);
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            if flag.load(Ordering::SeqCst) {
                WorkResult::NeedRetry
            } else {
                WorkResult::Done(())
            }
        }
    }))
    .interval(Duration::from_millis(20))
    .build()?;

    beaver.enqueue(task).await?;

    // Let it run a few times
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Set flag to stop
    should_continue.store(false, Ordering::SeqCst);

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should have stopped
    let final_count = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Count should not increase after flag is set to false
    let after_count = counter.load(Ordering::SeqCst);
    assert_eq!(final_count, after_count, "Should stop when flag is false");

    Ok(())
}

// =============================================================================
// WORK + SEND + SYNC TESTS
// =============================================================================

/// Test: Work implementations must be Send.
/// This ensures work can be sent across threads.
#[tokio::test]
async fn test_work_is_send() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    // This compiles only if Work is Send
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    // Send to different thread
    tokio::spawn(async move {
        let _ = beaver.enqueue(task).await;
    })
    .await
    .unwrap();

    Ok(())
}

/// Test: Work implementations must be Sync.
/// This ensures work can be shared across threads.
#[tokio::test]
async fn test_work_is_sync() -> BeaverResult<()> {
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

            beaver_clone.enqueue(task).await
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
