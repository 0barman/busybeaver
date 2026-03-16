//! # Listener Tests
//!
//! Comprehensive tests for WorkListener, FixedCountProgress, and listener helper functions.
//! Listeners allow monitoring task execution and receiving callbacks.

use busybeaver::{
    listener, listener_with_error, work, Beaver, BeaverResult, FixedCountBuilder,
    FixedCountProgress, PeriodicBuilder, RuntimeError, TimeIntervalBuilder, WorkListener,
    WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// LISTENER HELPER FUNCTION TESTS
// =============================================================================

/// Test: listener() creates a WorkListener from two closures.
/// This is the simplest way to create a listener.
#[tokio::test]
async fn test_listener_helper_function() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));

    let comp = Arc::clone(&completed);
    let intr = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(10)
        .listener(listener(
            move || comp.store(true, Ordering::SeqCst),
            move || intr.store(true, Ordering::SeqCst),
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        completed.load(Ordering::SeqCst),
        "on_complete should be called"
    );
    assert!(
        !interrupted.load(Ordering::SeqCst),
        "on_interrupt should not be called"
    );

    Ok(())
}

/// Test: listener_with_error() creates a WorkListener with error handling.
#[tokio::test]
async fn test_listener_with_error_helper() -> BeaverResult<()> {
    let completed = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let error_received = Arc::new(AtomicBool::new(false));

    let comp = Arc::clone(&completed);
    let intr = Arc::clone(&interrupted);
    let err = Arc::clone(&error_received);

    let my_listener = listener_with_error(
        move || comp.store(true, Ordering::SeqCst),
        move || intr.store(true, Ordering::SeqCst),
        move |_: RuntimeError| err.store(true, Ordering::SeqCst),
    );

    // Test on_complete
    my_listener.on_complete();
    assert!(completed.load(Ordering::SeqCst));

    // Test on_interrupt
    my_listener.on_interrupt();
    assert!(interrupted.load(Ordering::SeqCst));

    // Test on_error
    my_listener.on_error(RuntimeError::LockPoisoned);
    assert!(error_received.load(Ordering::SeqCst));

    Ok(())
}

/// Test: Default on_error in listener() does nothing.
#[tokio::test]
async fn test_listener_default_on_error() -> BeaverResult<()> {
    let completed = Arc::new(AtomicBool::new(false));
    let comp = Arc::clone(&completed);

    let my_listener = listener(move || comp.store(true, Ordering::SeqCst), || {});

    // Default on_error should not panic
    my_listener.on_error(RuntimeError::LockPoisoned);
    my_listener.on_error(RuntimeError::TaskExecutionFailed("test".to_string()));

    Ok(())
}

// =============================================================================
// CUSTOM WORK LISTENER TESTS
// =============================================================================

/// Custom WorkListener implementation for testing.
struct TestListener {
    complete_count: AtomicU32,
    interrupt_count: AtomicU32,
    errors: std::sync::Mutex<Vec<String>>,
}

impl TestListener {
    fn new() -> Self {
        TestListener {
            complete_count: AtomicU32::new(0),
            interrupt_count: AtomicU32::new(0),
            errors: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl WorkListener for TestListener {
    fn on_complete(&self) {
        self.complete_count.fetch_add(1, Ordering::SeqCst);
    }

    fn on_interrupt(&self) {
        self.interrupt_count.fetch_add(1, Ordering::SeqCst);
    }

    fn on_error(&self, error: RuntimeError) {
        self.errors.lock().unwrap().push(format!("{}", error));
    }
}

/// Test: Custom WorkListener implementation receives callbacks.
#[tokio::test]
async fn test_custom_work_listener() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let test_listener = Arc::new(TestListener::new());
    let listener_ref = Arc::clone(&test_listener);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(10)
        .listener(test_listener)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        listener_ref.complete_count.load(Ordering::SeqCst),
        1,
        "on_complete should be called once"
    );
    assert_eq!(
        listener_ref.interrupt_count.load(Ordering::SeqCst),
        0,
        "on_interrupt should not be called"
    );

    Ok(())
}

/// Test: Custom listener receives on_interrupt when cancelled.
#[tokio::test]
async fn test_custom_listener_interrupt() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let test_listener = Arc::new(TestListener::new());
    let listener_ref = Arc::clone(&test_listener);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval_ms(50)
        .listener(test_listener)
        .build()?;

    beaver.enqueue(task)?;

    // Let it start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancel
    beaver.cancel_all()?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        listener_ref.interrupt_count.load(Ordering::SeqCst),
        1,
        "on_interrupt should be called"
    );

    beaver.destroy()?;
    Ok(())
}

// =============================================================================
// ON_COMPLETE CALLBACK TESTS
// =============================================================================

/// Test: on_complete is called when periodic task returns Done.
#[tokio::test]
async fn test_on_complete_periodic_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval_ms(10)
        .listener(listener(
            move || completed_clone.store(true, Ordering::SeqCst),
            || {},
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(completed.load(Ordering::SeqCst));

    Ok(())
}

/// Test: on_complete is called when fixed count exhausts all retries.
#[tokio::test]
async fn test_on_complete_fixed_count_exhausted() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(3)
        .listener(listener(
            move || completed_clone.store(true, Ordering::SeqCst),
            || {},
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(completed.load(Ordering::SeqCst));

    Ok(())
}

/// Test: on_complete is called when time interval exhausts all intervals.
#[tokio::test]
async fn test_on_complete_time_interval_exhausted() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = TimeIntervalBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .intervals_millis([0, 0])
        .listener(listener(
            move || completed_clone.store(true, Ordering::SeqCst),
            || {},
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(completed.load(Ordering::SeqCst));

    Ok(())
}

/// Test: on_complete is NOT called when task returns Done early.
#[tokio::test]
async fn test_on_complete_not_called_on_early_done() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let completed = Arc::new(AtomicBool::new(false));
    let completed_clone = Arc::clone(&completed);

    let task = FixedCountBuilder::new(work(|| async {
        WorkResult::Done(()) // Return Done immediately
    }))
    .count(10) // Would run 10 times if not Done
    .listener(listener(
        move || completed_clone.store(true, Ordering::SeqCst),
        || {},
    ))
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // on_complete should NOT be called because task succeeded early
    assert!(!completed.load(Ordering::SeqCst));

    Ok(())
}

// =============================================================================
// ON_INTERRUPT CALLBACK TESTS
// =============================================================================

/// Test: on_interrupt is called when task is cancelled via cancel_all.
#[tokio::test]
async fn test_on_interrupt_cancel_all() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval_ms(50)
        .listener(listener(
            || {},
            move || interrupted_clone.store(true, Ordering::SeqCst),
        ))
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    beaver.cancel_all()?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(interrupted.load(Ordering::SeqCst));

    beaver.destroy()?;
    Ok(())
}

/// Test: destroy() releases named dams (which triggers on_interrupt).
/// Note: destroy() stops all dams and interrupts their tasks.
/// but it releases named dams properly.
#[tokio::test]
async fn test_on_interrupt_destroy_named_dam() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval_ms(50)
        .listener(listener(
            || {},
            move || interrupted_clone.store(true, Ordering::SeqCst),
        ))
        .build()?;

    // Use named dam instead of default
    beaver.enqueue_on_new_thread(task, "test-dam", 256, false)?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    beaver.destroy()?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        interrupted.load(Ordering::SeqCst),
        "Named dam tasks should be interrupted on destroy"
    );

    Ok(())
}

/// Test: on_interrupt is called when dam is released.
#[tokio::test]
async fn test_on_interrupt_release_thread_resource_by_name() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval_ms(50)
        .listener(listener(
            || {},
            move || interrupted_clone.store(true, Ordering::SeqCst),
        ))
        .build()?;

    beaver.enqueue_on_new_thread(task, "my-dam", 256, false)?;

    tokio::time::sleep(Duration::from_millis(100)).await;
    beaver.release_thread_resource_by_name("my-dam")?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(interrupted.load(Ordering::SeqCst));

    Ok(())
}

/// Test: Multiple tasks can be cancelled together.
/// This tests that cancel_all affects running tasks.
#[tokio::test]
async fn test_on_interrupt_multiple_tasks() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);

    let interrupt_count = Arc::new(AtomicU32::new(0));

    // Create multiple tasks on different dams
    for i in 0..3 {
        let ic = Arc::clone(&interrupt_count);
        let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
            .interval_ms(50)
            .listener(listener(
                || {},
                move || {
                    ic.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .build()?;

        beaver.enqueue_on_new_thread(task, format!("dam-{}", i), 256, false)?;
    }

    // Let them all start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancel all
    beaver.cancel_all()?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        interrupt_count.load(Ordering::SeqCst),
        3,
        "All tasks should receive on_interrupt"
    );

    beaver.destroy()?;
    Ok(())
}

// =============================================================================
// FIXED COUNT PROGRESS TESTS
// =============================================================================

/// Test: FixedCountProgress receives progress updates.
#[tokio::test]
async fn test_fixed_count_progress_callback() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let progress_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let progress_clone = Arc::clone(&progress_log);

    let progress: Arc<dyn FixedCountProgress> =
        Arc::new(move |current: u32, total: u32, tag: &str| {
            progress_clone
                .lock()
                .unwrap()
                .push((current, total, tag.to_string()));
        });

    let task = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(5)
        .tag("test-progress")
        .progress(progress)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = progress_log.lock().unwrap();
    assert_eq!(log.len(), 5);

    // Verify all progress entries
    for (i, entry) in log.iter().enumerate() {
        assert_eq!(entry.0, (i + 1) as u32, "current should be {}", i + 1);
        assert_eq!(entry.1, 5, "total should be 5");
        assert_eq!(entry.2, "test-progress");
    }

    Ok(())
}

/// Test: Progress is called before each execution, not after.
#[tokio::test]
async fn test_progress_called_before_execution() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let progress_times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let execute_times = Arc::new(std::sync::Mutex::new(Vec::new()));

    let pt = Arc::clone(&progress_times);
    let et = Arc::clone(&execute_times);

    let progress: Arc<dyn FixedCountProgress> = Arc::new(move |_: u32, _: u32, _: &str| {
        pt.lock().unwrap().push(std::time::Instant::now());
    });

    let task = FixedCountBuilder::new(work(move || {
        let exec = Arc::clone(&et);
        async move {
            exec.lock().unwrap().push(std::time::Instant::now());
            WorkResult::NeedRetry
        }
    }))
    .count(3)
    .progress(progress)
    .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let progress = progress_times.lock().unwrap();
    let executes = execute_times.lock().unwrap();

    assert_eq!(progress.len(), 3);
    assert_eq!(executes.len(), 3);

    // Progress should be called before execute
    for i in 0..3 {
        assert!(
            progress[i] <= executes[i],
            "Progress should be called before execution"
        );
    }

    Ok(())
}

/// Test: Progress callback with empty tag.
#[tokio::test]
async fn test_progress_empty_tag() -> BeaverResult<()> {
    let beaver = Beaver::new("test", 256);
    let received_tag = Arc::new(std::sync::Mutex::new(None));
    let tag_clone = Arc::clone(&received_tag);

    let progress: Arc<dyn FixedCountProgress> = Arc::new(move |_: u32, _: u32, tag: &str| {
        *tag_clone.lock().unwrap() = Some(tag.to_string());
    });

    // No tag set
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .progress(progress)
        .build()?;

    beaver.enqueue(task)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(received_tag.lock().unwrap().as_ref().unwrap(), "");

    Ok(())
}

// =============================================================================
// LISTENER THREAD SAFETY TESTS
// =============================================================================

/// Test: Listener callbacks are thread-safe.
#[tokio::test]
async fn test_listener_thread_safety() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("test", 256));
    let completed_count = Arc::new(AtomicU32::new(0));

    let mut handles = vec![];

    for _ in 0..10 {
        let beaver_clone = Arc::clone(&beaver);
        let count = Arc::clone(&completed_count);

        let handle = tokio::spawn(async move {
            let cc = Arc::clone(&count);
            let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
                .interval_ms(10)
                .listener(listener(
                    move || {
                        cc.fetch_add(1, Ordering::SeqCst);
                    },
                    || {},
                ))
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

    assert_eq!(
        completed_count.load(Ordering::SeqCst),
        10,
        "All listeners should complete"
    );

    Ok(())
}

// =============================================================================
// RUNTIME ERROR TESTS
// =============================================================================

/// Test: RuntimeError Display implementation.
#[test]
fn test_runtime_error_display() {
    let lock_err = RuntimeError::LockPoisoned;
    let exec_err = RuntimeError::TaskExecutionFailed("test error".to_string());

    let lock_str = format!("{}", lock_err);
    let exec_str = format!("{}", exec_err);

    assert!(lock_str.contains("lock") || lock_str.contains("poisoned"));
    assert!(exec_str.contains("test error"));
}

/// Test: RuntimeError Debug implementation.
#[test]
fn test_runtime_error_debug() {
    let err = RuntimeError::LockPoisoned;
    let debug_str = format!("{:?}", err);

    assert!(debug_str.contains("LockPoisoned"));
}

/// Test: RuntimeError Clone implementation.
#[test]
fn test_runtime_error_clone() {
    let err1 = RuntimeError::TaskExecutionFailed("test".to_string());
    let err2 = err1.clone();

    if let RuntimeError::TaskExecutionFailed(msg) = err2 {
        assert_eq!(msg, "test");
    } else {
        panic!("Clone failed");
    }
}
