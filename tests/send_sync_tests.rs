//! # Beaver Send + Sync tests
//!
//! Verifies that Beaver can be used safely across threads and tasks:
//! - **Send**: can be transferred across threads/spawns (e.g. `Arc<Beaver>` passed into `tokio::spawn` / `std::thread::spawn`)
//! - **Sync**: can be shared by multiple tasks/threads via references (e.g. multiple spawns holding `Arc<Beaver>`)
//! All tests use short-lived tasks and timeouts to avoid long waits.

use busybeaver::{listener, work, Beaver, BeaverResult, FixedCountBuilder, WorkResult};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Maximum wait time allowed in tests to avoid hanging.
const TEST_TIMEOUT: Duration = Duration::from_millis(500);
/// Brief sleep after task execution.
const SHORT_SLEEP: Duration = Duration::from_millis(30);

// =============================================================================
// Compile-time requirements: Beaver must implement Send + Sync
// =============================================================================

/// Compiles only when T: Send + Sync; used to ensure Beaver is safe for multi-threaded use.
/// If Beaver's `unsafe impl Send/Sync` is removed, this test will fail to compile.
fn require_send_sync<T: Send + Sync>() {}

#[tokio::test]
async fn test_beaver_is_send_and_sync_compile_requirement() {
    require_send_sync::<Beaver>();
    require_send_sync::<Arc<Beaver>>();
}

// =============================================================================
// Send: transfer across spawn / across threads
// =============================================================================

/// Send: pass Arc<Beaver> into tokio::spawn, enqueue a one-shot task inside the spawn, with timeout.
#[tokio::test]
async fn test_beaver_send_via_tokio_spawn() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("send_spawn", 8));
    let done = Arc::new(AtomicU32::new(0));

    let task = FixedCountBuilder::new(work({
        let done = Arc::clone(&done);
        move || {
            let d = Arc::clone(&done);
            async move {
                d.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }
    }))
    .count(1)
    .build()?;

    let beaver_clone = Arc::clone(&beaver);
    let join = tokio::spawn(async move {
        let _ = beaver_clone.enqueue(task).await;
    });

    let _ = tokio::time::timeout(TEST_TIMEOUT, join).await;
    tokio::time::sleep(SHORT_SLEEP).await;

    assert!(
        done.load(Ordering::SeqCst) >= 1,
        "task should run at least once"
    );
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

/// Send: pass Arc<Beaver> to std::thread, create a dedicated runtime in that thread and enqueue; proves cross-thread transfer.
#[tokio::test]
async fn test_beaver_send_via_std_thread() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("send_thread", 8));
    let done = Arc::new(AtomicU32::new(0));

    let task = FixedCountBuilder::new(work({
        let done = Arc::clone(&done);
        move || {
            let d = Arc::clone(&done);
            async move {
                d.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }
    }))
    .count(1)
    .build()?;

    let beaver_clone = Arc::clone(&beaver);
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = beaver_clone.enqueue(task).await;
        });
    });

    let _ = handle.join();
    tokio::time::sleep(SHORT_SLEEP).await;

    assert!(done.load(Ordering::SeqCst) >= 1);
    beaver.cancel_all().await?;
    beaver.destroy().await?;
    Ok(())
}

// =============================================================================
// Sync: multiple tasks sharing the same Arc<Beaver>
// =============================================================================

/// Sync: multiple tokio tasks hold Arc<Beaver> and enqueue; short tasks, short waits, timeout to finish.
#[tokio::test]
async fn test_beaver_sync_multiple_tasks_share_arc() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("sync_shared", 32));
    let counter = Arc::new(AtomicU32::new(0));
    let n = 4usize;

    let mut joins = Vec::with_capacity(n);
    for _ in 0..n {
        let beaver_clone = Arc::clone(&beaver);
        let counter_clone = Arc::clone(&counter);
        let j = tokio::spawn(async move {
            let task = FixedCountBuilder::new(work(move || {
                let c = Arc::clone(&counter_clone);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    WorkResult::Done(())
                }
            }))
            .count(1)
            .build()
            .unwrap();
            beaver_clone.enqueue(task).await
        });
        joins.push(j);
    }

    for j in joins {
        let _ = tokio::time::timeout(TEST_TIMEOUT, j).await;
    }

    tokio::time::sleep(SHORT_SLEEP).await;
    assert!(
        counter.load(Ordering::SeqCst) >= 1,
        "at least one task should run"
    );

    let _ = tokio::time::timeout(TEST_TIMEOUT, beaver.cancel_all()).await;
    beaver.destroy().await?;
    Ok(())
}

// =============================================================================
// Send + Sync combined: concurrent enqueue + cancel across spawns, quick finish
// =============================================================================

/// Send+Sync: multiple spawns enqueue and one cancels; all use short timeouts and one-shot tasks.
#[tokio::test]
async fn test_beaver_send_sync_concurrent_enqueue_and_cancel() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("send_sync_concurrent", 32));
    let counter = Arc::new(AtomicU32::new(0));

    let mut joins = Vec::new();
    for i in 0..4 {
        let beaver_clone = Arc::clone(&beaver);
        let counter_clone = Arc::clone(&counter);
        let j = tokio::spawn(async move {
            let task = FixedCountBuilder::new(work(move || {
                let c = Arc::clone(&counter_clone);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    WorkResult::Done(())
                }
            }))
            .count(1)
            .tag(format!("t-{}", i))
            .build()
            .unwrap();
            let _ = beaver_clone.enqueue(task).await;
        });
        joins.push(j);
    }

    let cancel_beaver = Arc::clone(&beaver);
    let cancel_join = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = cancel_beaver.cancel_all().await;
    });

    for j in joins {
        let _ = tokio::time::timeout(TEST_TIMEOUT, j).await;
    }
    let _ = tokio::time::timeout(TEST_TIMEOUT, cancel_join).await;

    tokio::time::sleep(Duration::from_millis(30)).await;
    beaver.destroy().await?;
    assert!(counter.load(Ordering::SeqCst) >= 1);
    Ok(())
}

/// Send+Sync: main task and spawn each hold Arc<Beaver>; main does cancel_all + destroy after spawn, with short timeout.
#[tokio::test]
async fn test_beaver_send_sync_main_and_spawn_share_arc() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("main_and_spawn", 8));
    let done = Arc::new(AtomicU32::new(0));

    let task = FixedCountBuilder::new(work({
        let done = Arc::clone(&done);
        move || {
            let d = Arc::clone(&done);
            async move {
                d.fetch_add(1, Ordering::SeqCst);
                WorkResult::Done(())
            }
        }
    }))
    .count(1)
    .listener(listener(|| {}, || {}))
    .build()?;

    let spawn_beaver = Arc::clone(&beaver);
    let join = tokio::spawn(async move {
        let _ = spawn_beaver.enqueue(task).await;
    });

    let _ = tokio::time::timeout(TEST_TIMEOUT, join).await;
    tokio::time::sleep(SHORT_SLEEP).await;

    beaver.cancel_all().await?;
    beaver.destroy().await?;
    assert!(done.load(Ordering::SeqCst) >= 1);
    Ok(())
}

// =============================================================================
// Non-Send / non-Sync constraints (documentation + compile-time guarantee)
// =============================================================================

/// If Beaver did not implement Send, Arc<Beaver> could not be passed into tokio::spawn (spawn requires Future + Send).
/// This test uses require_send_sync::<Beaver>() at compile time to ensure Beaver is Send+Sync,
/// so Arc<Beaver> can be used across tasks. Using a non-Send type like Rc<Beaver> in spawn would fail to compile.
#[tokio::test]
async fn test_arc_beaver_can_be_sent_to_spawn_because_beaver_is_send() {
    require_send_sync::<Beaver>();
    let _: Arc<Beaver> = Arc::new(Beaver::new("doc_send", 8));
}

/// If Beaver did not implement Sync, &Beaver could not be shared across threads safely, and Arc<Beaver> would not be Sync.
/// This test uses require_send_sync::<Arc<Beaver>>() to ensure multiple tasks can hold Arc<Beaver> concurrently.
#[tokio::test]
async fn test_arc_beaver_can_be_shared_because_beaver_is_sync() {
    require_send_sync::<Arc<Beaver>>();
}
