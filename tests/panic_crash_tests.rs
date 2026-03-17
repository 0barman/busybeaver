//! # Panic and crash isolation tests
//!
//! Verifies that panics and other crashes inside `work()` (e.g. `panic!`, `.unwrap()`, `.expect()`,
//! `todo!`, `unimplemented!`, `unreachable!`, or code in `unsafe` blocks) are caught and reported
//! via `on_error`, and do not affect other tasks on the same execution thread.

use busybeaver::{
    listener_with_error, work, Beaver, BeaverResult, FixedCountBuilder, RuntimeError, WorkResult,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runs a task whose work crashes, asserts on_error was called, then enqueues a second task
/// and asserts it runs (worker survived).
async fn run_crash_test<F, Fut>(name: &str, f: F) -> BeaverResult<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = WorkResult<()>> + Send + 'static,
{
    let beaver = Beaver::new("test", 256);
    let error_msg = Arc::new(Mutex::new(None::<String>));
    let error_msg_c = Arc::clone(&error_msg);

    let task = FixedCountBuilder::new(work(f))
        .count(1)
        .listener(listener_with_error(
            || {},
            || {},
            move |e: RuntimeError| {
                if let RuntimeError::TaskExecutionFailed(ref msg) = e {
                    *error_msg_c.lock().unwrap() = Some(msg.clone());
                }
            },
        ))
        .build()?;

    beaver.enqueue(task)?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let msg = error_msg.lock().unwrap().take();
    assert!(
        msg.is_some(),
        "{}: on_error should be called with TaskExecutionFailed, got {:?}",
        name,
        msg
    );

    let second_ran = Arc::new(AtomicBool::new(false));
    let second_ran_c = Arc::clone(&second_ran);
    let task2 = FixedCountBuilder::new(work(move || {
        let value = Arc::clone(&second_ran_c);
        async move {
            value.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task2)?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        second_ran.load(Ordering::SeqCst),
        "{}: second task should run (worker survived)",
        name
    );

    Ok(())
}

// =============================================================================
// panic!
// =============================================================================

#[tokio::test]
async fn test_work_panic_macro_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("panic!", || async move {
        panic!("intentional panic in work");
    })
    .await
}

// =============================================================================
// .unwrap() that panics (e.g. None)
// =============================================================================

#[tokio::test]
async fn test_work_unwrap_panics_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("unwrap", || async move {
        let _: i32 = None.unwrap();
        WorkResult::Done(())
    })
    .await
}

// =============================================================================
// .expect() that panics
// =============================================================================

#[tokio::test]
async fn test_work_expect_panics_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("expect", || async move {
        let _: i32 = None.expect("intentional expect panic");
        WorkResult::Done(())
    })
    .await
}

// =============================================================================
// todo!()
// =============================================================================

#[tokio::test]
async fn test_work_todo_macro_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("todo!", || async move {
        todo!("not implemented yet");
    })
    .await
}

// =============================================================================
// unimplemented!()
// =============================================================================

#[tokio::test]
async fn test_work_unimplemented_macro_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("unimplemented!", || async move {
        unimplemented!("this path is not implemented");
    })
    .await
}

// =============================================================================
// unreachable!()
// =============================================================================

#[tokio::test]
async fn test_work_unreachable_macro_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("unreachable!", || async move {
        unreachable!("intentional unreachable in work");
    })
    .await
}

// =============================================================================
// panic inside unsafe block (unsafe does not by itself crash; we trigger panic inside it)
// =============================================================================

#[tokio::test]
async fn test_work_unsafe_block_panic_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("unsafe block + panic", || async move {
        #[allow(unused_unsafe)]
        unsafe {
            // Intentionally panic inside unsafe block to verify such crashes are also isolated.
            panic!("panic inside unsafe block");
        }
    })
    .await
}

// =============================================================================
// index out of bounds (e.g. slice[i] panic)
// =============================================================================

#[tokio::test]
async fn test_work_index_panic_triggers_on_error_and_worker_survives() -> BeaverResult<()> {
    run_crash_test("index panic", || async move {
        let a: [i32; 0] = [];
        let _ = a[1];
        WorkResult::Done(())
    })
    .await
}
