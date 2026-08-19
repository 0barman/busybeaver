//! Public `Task` / `TaskId` surface: identifiers, tags, and interrupt flag visibility.

use busybeaver::{work, Beaver, BeaverResult, PeriodicBuilder, Task, TaskId, WorkResult};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn task_id_display_is_uuid_text() {
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build()
        .unwrap();
    let s = task.id().to_string();
    assert_eq!(s.len(), 36, "UUID hyphenated form: {}", s);
    assert!(s.chars().filter(|c| *c == '-').count() == 4);
}

#[test]
fn task_id_default_creates_new_id() {
    let a = TaskId::default();
    let b = TaskId::default();
    assert_ne!(a.to_string(), b.to_string());
}

#[test]
fn task_id_hash_eq_consistent() {
    use std::hash::{Hash, Hasher};
    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build()
        .unwrap();
    let id = *task.id();
    let mut set = HashSet::new();
    set.insert(id);
    assert!(set.contains(&id));

    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h1);
    id.hash(&mut h2);
    assert_eq!(h1.finish(), h2.finish());
}

#[tokio::test]
async fn task_tag_and_interrupted_reflect_builder_and_cancel() -> BeaverResult<()> {
    let beaver = Beaver::new("task_api", 256);
    let task = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::from_millis(40))
        .tag("api-tag")
        .build()?;

    assert_eq!(task.tag(), "api-tag");
    assert!(!task.interrupted());

    beaver.enqueue(Arc::clone(&task)).await?;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(!task.interrupted());

    beaver.cancel_all().await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(task.interrupted());

    beaver.destroy().await?;
    Ok(())
}

#[test]
fn distinct_builds_yield_distinct_task_ids() {
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let t: Arc<Task> = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
            .interval(Duration::ZERO)
            .build()
            .unwrap();
        assert!(seen.insert(*t.id()));
    }
}

#[test]
fn new_with_handle_destroy_then_default_enqueue_fails_no_dam() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let beaver = Beaver::new_with_handle("nh", 8, rt.handle().clone());
    rt.block_on(beaver.destroy()).unwrap();

    let task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) }))
        .interval(Duration::ZERO)
        .build()
        .unwrap();
    let r = rt.block_on(beaver.enqueue(task));
    assert!(matches!(
        r,
        Err(busybeaver::BeaverError::ExecutorShuttingDown)
    ));
}

#[tokio::test]
async fn cancel_all_sets_interrupted_on_queued_not_yet_running_task() -> BeaverResult<()> {
    let beaver = Beaver::new("queued_interrupt", 4);
    let started = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&started);

    let blocker = PeriodicBuilder::new(work(move || {
        let s = Arc::clone(&s);
        async move {
            s.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(400)).await;
            WorkResult::Done(())
        }
    }))
    .interval(Duration::ZERO)
    .build()?;

    let victim = PeriodicBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .interval(Duration::ZERO)
        .build()?;

    beaver.enqueue(blocker).await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(started.load(Ordering::SeqCst));

    // The victim must be enqueued **before** cancel_all, so that at cancel time it
    // is sitting in the queue behind the still-running blocker. cancel_all raises the
    // cancellation watermark above the victim's sequence number, so the worker
    // drops+interrupts it when it dequeues it — it never runs. (Tasks enqueued
    // *after* cancel_all are, by contract, still executed; see
    // cancel_backlog_tests::enqueue_after_cancel_all_still_runs.)
    beaver.enqueue(victim.clone()).await?;
    beaver.cancel_all().await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        victim.interrupted(),
        "task queued before cancel_all should be interrupted and never run"
    );

    beaver.destroy().await?;
    Ok(())
}
