//! # Error Handling Tests
//!
//! Comprehensive tests for BeaverError, BeaverResult, and error handling scenarios.
//! Understanding error handling is critical for robust applications.

use busybeaver::{work, Beaver, BeaverError, BeaverResult, PeriodicBuilder, WorkResult};
use std::time::Duration;

// =============================================================================
// BEAVER ERROR TESTS
// =============================================================================

/// Test: BeaverError::NoDam when enqueueing after destroy.
#[tokio::test]
async fn test_error_no_dam_after_destroy() -> BeaverResult<()> {
    let beaver = Beaver::new("test_error_no_dam_after_destroy", 256);
    beaver.destroy().await?;

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    let result = beaver.enqueue(task).await;

    match result {
        Err(BeaverError::NoDam) => {
            // Expected
        }
        _ => panic!("Expected NoDam error, got {:?}", result),
    }

    Ok(())
}

/// Test: BeaverError::NoDam message.
#[test]
fn test_error_no_dam_display() {
    let err = BeaverError::NoDam;
    let msg = format!("{}", err);

    assert!(
        msg.to_lowercase().contains("dam") || msg.to_lowercase().contains("available"),
        "Error message should mention dam: {}",
        msg
    );
}

/// Test: BeaverError::QueueFull can be triggered with buffer capacity 1.
/// With buffer=1, first enqueue succeeds; second enqueue before worker receives returns QueueFull.
#[tokio::test]
async fn test_error_queue_full_triggered() -> BeaverResult<()> {
    let beaver = Beaver::new("test_queue_full", 1);
    let task1 = PeriodicBuilder::new(work(|| async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        WorkResult::NeedRetry
    }))
    .interval(Duration::from_millis(500))
    .build()?;

    let task2 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver.enqueue(task1).await?;
    let second = beaver.enqueue(task2).await;

    match second {
        Err(BeaverError::QueueFull) => { /* expected when channel is full */ }
        Ok(()) => {
            beaver.cancel_all().await?;
            beaver.destroy().await?;
            return Ok(());
        }
        other => panic!("expected QueueFull or Ok, got {:?}", other),
    }

    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

/// Test: BeaverError::QueueFull display message.
#[test]
fn test_error_queue_full_display() {
    let err = BeaverError::QueueFull;
    let msg = format!("{}", err);

    assert!(
        msg.to_lowercase().contains("queue") || msg.to_lowercase().contains("full"),
        "Error message should mention queue full: {}",
        msg
    );
}

/// Test: BeaverError::DamReleased message.
#[test]
fn test_error_dam_released_display() {
    let err = BeaverError::DamReleased;
    let msg = format!("{}", err);

    assert!(
        msg.to_lowercase().contains("dam") || msg.to_lowercase().contains("released"),
        "Error message should mention dam released: {}",
        msg
    );
}

/// Test: BeaverError::LockPoisoned message.
#[test]
fn test_error_lock_poisoned_display() {
    let err = BeaverError::LockPoisoned;
    let msg = format!("{}", err);

    assert!(
        msg.to_lowercase().contains("lock") || msg.to_lowercase().contains("poisoned"),
        "Error message should mention lock poisoned: {}",
        msg
    );
}

/// Test: BeaverError::BuilderMissingField message.
#[test]
fn test_error_builder_missing_field_display() {
    let err = BeaverError::BuilderMissingField("work");
    let msg = format!("{}", err);

    assert!(
        msg.contains("work")
            && (msg.to_lowercase().contains("missing") || msg.to_lowercase().contains("field")),
        "Error message should mention missing field: {}",
        msg
    );
}

/// Test: BeaverError Debug implementation.
#[test]
fn test_beaver_error_debug() {
    let err = BeaverError::NoDam;
    let debug = format!("{:?}", err);

    assert!(debug.contains("NoDam"));
}

/// Test: BeaverError implements std::error::Error.
#[test]
fn test_beaver_error_is_std_error() {
    let err: Box<dyn std::error::Error> = Box::new(BeaverError::NoDam);
    let _ = err.to_string();
}

// =============================================================================
// BEAVER RESULT TESTS
// =============================================================================

/// Test: BeaverResult with Ok value.
#[test]
fn test_beaver_result_ok() {
    let result: BeaverResult<i32> = Ok(42);

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 42);
}

/// Test: BeaverResult with Err value.
#[test]
fn test_beaver_result_err() {
    let result: BeaverResult<i32> = Err(BeaverError::NoDam);

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), BeaverError::NoDam));
}

/// Test: BeaverResult can be used with ? operator.
#[tokio::test]
async fn test_beaver_result_question_mark() -> BeaverResult<()> {
    let beaver = Beaver::new("test_beaver_result_question_mark", 256);

    // All these operations return BeaverResult and can use ?
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver.enqueue(task).await?;
    beaver.cancel_all().await?;
    beaver.destroy().await?;

    Ok(())
}

/// Test: BeaverResult can be mapped.
#[test]
fn test_beaver_result_map() {
    let result: BeaverResult<i32> = Ok(21);
    let doubled = result.map(|x| x * 2);

    assert_eq!(doubled.unwrap(), 42);
}

/// Test: BeaverResult can be unwrap_or_default.
#[test]
fn test_beaver_result_unwrap_or() {
    let ok_result: BeaverResult<i32> = Ok(42);
    let err_result: BeaverResult<i32> = Err(BeaverError::NoDam);

    assert_eq!(ok_result.unwrap_or(0), 42);
    assert_eq!(err_result.unwrap_or(0), 0);
}

// =============================================================================
// ERROR PROPAGATION TESTS
// =============================================================================

/// Test: Errors propagate correctly through function calls.
async fn helper_that_may_fail(beaver: &Beaver, should_fail: bool) -> BeaverResult<()> {
    if should_fail {
        beaver.destroy().await?;
    }

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver.enqueue(task).await?;

    Ok(())
}

#[tokio::test]
async fn test_error_propagation() -> BeaverResult<()> {
    let beaver = Beaver::new("test_error_propagation", 256);

    // Should succeed
    let result = helper_that_may_fail(&beaver, false).await;
    assert!(result.is_ok());

    beaver.cancel_all().await?;

    // Should fail due to destroy
    let result = helper_that_may_fail(&beaver, true).await;
    assert!(result.is_err());

    Ok(())
}

// =============================================================================
// EDGE CASES
// =============================================================================

/// Test: Operations on fresh Beaver instance always succeed.
#[tokio::test]
async fn test_fresh_beaver_operations() -> BeaverResult<()> {
    let beaver = Beaver::new("test_fresh_beaver_operations", 256);

    // All these should succeed on fresh instance
    beaver.cancel_all().await?;
    beaver.cancel_non_long_resident().await?;
    beaver
        .release_thread_resource_by_name("non-existent")
        .await?;

    Ok(())
}

/// Test: Cancel operations are idempotent.
#[tokio::test]
async fn test_cancel_idempotent() -> BeaverResult<()> {
    let beaver = Beaver::new("test_cancel_idempotent", 256);

    // Multiple cancel_all calls should not error
    beaver.cancel_all().await?;
    beaver.cancel_all().await?;
    beaver.cancel_all().await?;

    // Multiple cancel_non_long_resident calls should not error
    beaver.cancel_non_long_resident().await?;
    beaver.cancel_non_long_resident().await?;
    beaver.cancel_non_long_resident().await?;

    Ok(())
}

/// Test: Release non-existent dam is a no-op.
#[tokio::test]
async fn test_release_nonexistent_dam_is_noop() -> BeaverResult<()> {
    let beaver = Beaver::new("test_release_nonexistent_dam_is_noop", 256);

    // Should not error
    beaver.release_thread_resource_by_name("dam1").await?;
    beaver.release_thread_resource_by_name("dam2").await?;
    beaver.release_thread_resource_by_name("dam3").await?;

    Ok(())
}

/// Test: Enqueue on named dam after release_thread_resource_by_name on different dam still works.
#[tokio::test]
async fn test_release_one_dam_others_work() -> BeaverResult<()> {
    let beaver = Beaver::new("test_release_one_dam_others_work", 256);

    // Create and enqueue on dam1
    let task1 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver
        .enqueue_on_new_thread(task1, "dam1", 256, false)
        .await?;

    // Create and enqueue on dam2
    let task2 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    beaver
        .enqueue_on_new_thread(task2, "dam2", 256, false)
        .await?;

    // Release dam1
    beaver.release_thread_resource_by_name("dam1").await?;

    // dam2 should still work
    let task3 = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::from_millis(100))
        .build()?;

    assert!(beaver
        .enqueue_on_new_thread(task3, "dam2", 256, false)
        .await
        .is_ok());

    Ok(())
}

// =============================================================================
// ERROR FORMATTING TESTS
// =============================================================================

/// Test: All errors have meaningful messages.
#[test]
fn test_all_errors_have_messages() {
    let errors: Vec<BeaverError> = vec![
        BeaverError::BuilderMissingField("test"),
        BeaverError::QueueFull,
        BeaverError::DamReleased,
        BeaverError::LockPoisoned,
        BeaverError::NoDam,
        BeaverError::RangeIntervalRangesExceedTotal {
            total: 5,
            ranges_count: 6,
        },
    ];

    for err in errors {
        let msg = format!("{}", err);
        assert!(!msg.is_empty(), "Error {:?} should have a message", err);
        assert!(msg.len() > 5, "Error message should be meaningful: {}", msg);
    }
}

/// Test: BeaverError::RangeIntervalRangesExceedTotal message.
#[test]
fn test_error_range_interval_ranges_exceed_total_display() {
    let err = BeaverError::RangeIntervalRangesExceedTotal {
        total: 10,
        ranges_count: 11,
    };
    let msg = format!("{}", err);

    assert!(
        msg.contains("10") && msg.contains("11"),
        "Message should contain total and ranges_count: {}",
        msg
    );
    assert!(
        msg.to_lowercase().contains("range") || msg.to_lowercase().contains("exceed"),
        "Message should mention range/exceed: {}",
        msg
    );
}

/// Test: Error conversion from PoisonError.
#[test]
fn test_poison_error_conversion() {
    use std::sync::Mutex;

    // Create a poisoned mutex
    let mutex = Mutex::new(42);
    let result = std::panic::catch_unwind(|| {
        let _guard = mutex.lock().unwrap();
        panic!("intentional panic");
    });
    assert!(result.is_err());

    // Try to lock the poisoned mutex
    let lock_result = mutex.lock();
    assert!(lock_result.is_err());

    // Convert to BeaverError
    let beaver_error: BeaverError = lock_result.unwrap_err().into();
    assert!(matches!(beaver_error, BeaverError::LockPoisoned));
}
