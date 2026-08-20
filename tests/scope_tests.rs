use busybeaver::{
    Beaver, LaneConfig, RotationOutcome, RotationPolicy, ScopeError, ScopeSpawnError, TaskExit,
    TaskSpec,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Notify;

#[tokio::test]
async fn scoped_execution_exposes_generation_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("scope-lane").capacity(8))?;
    let scope = beaver.create_scope("session", lane)?;
    let generation = scope.current();
    let expected_id = generation.id();
    let expected_number = generation.number();
    let mut handle = generation.try_spawn(TaskSpec::new(move |context| async move {
        Ok::<_, ()>((context.scope_id(), context.scope_generation()))
    }))?;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Completed((Some(id), Some(number)))
            if id == expected_id && number == expected_number
    ));
    Ok(())
}

#[tokio::test]
async fn strict_rotation_closes_old_admission_and_waits_for_old_terminal(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("rotate-lane").capacity(8))?;
    let scope = beaver.create_scope("page", lane)?;
    let old = scope.current();
    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_c = Arc::clone(&stopped);
    let old_handle = old.try_spawn(TaskSpec::new(move |context| {
        let stopped = Arc::clone(&stopped_c);
        async move {
            context.cancelled().await;
            stopped.store(true, Ordering::SeqCst);
            Ok::<_, ()>(())
        }
    }))?;
    tokio::task::yield_now().await;

    let rotation = scope.rotate(RotationPolicy::Strict)?;
    assert!(matches!(
        old.try_spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) })),
        Err(ScopeSpawnError::StaleGeneration)
    ));
    let outcome = rotation.wait().await;
    let new_id = match outcome {
        RotationOutcome::Rotated { old: old_id, new } => {
            assert_eq!(old_id, old.id());
            new
        }
        other => panic!("unexpected rotation outcome: {other:?}"),
    };
    assert!(stopped.load(Ordering::SeqCst));
    assert!(matches!(
        old_handle.wait().await,
        busybeaver::TaskExitSummary::Cancelled { .. }
    ));
    let current = scope.current();
    assert_eq!(current.id(), new_id);
    assert_eq!(current.number(), old.number() + 1);
    let mut fresh = current.try_spawn(TaskSpec::new(|_| async { Ok::<_, ()>(7u8) }))?;
    assert!(matches!(fresh.join().await?, TaskExit::Completed(7)));
    Ok(())
}

#[tokio::test]
async fn cancelling_parent_scope_cancels_child_scope_work() -> Result<(), Box<dyn std::error::Error>>
{
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("parent-lane").capacity(8).concurrency(2))?;
    let parent = beaver.create_scope("parent", lane.clone())?;
    let child = parent.child("child", lane)?;
    let child_handle = child
        .current()
        .try_spawn(TaskSpec::new(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))?;
    tokio::task::yield_now().await;
    parent
        .cancel_and_wait(busybeaver::CancelReason::ScopeCancelled)
        .await;
    assert!(matches!(
        child_handle.wait().await,
        busybeaver::TaskExitSummary::Cancelled { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn strict_self_rotation_is_rejected_without_closing_generation(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("scope-self").capacity(8))?;
    let scope = beaver.create_scope("self", lane)?;
    let scope_cell = Arc::new(OnceLock::new());
    scope_cell.set(scope.clone()).expect("set scope once");
    let gate = Arc::new(Notify::new());
    let task_scope = Arc::clone(&scope_cell);
    let task_gate = Arc::clone(&gate);
    let generation = scope.current();
    let mut handle = generation.try_spawn(TaskSpec::new(move |_| {
        let task_scope = Arc::clone(&task_scope);
        let task_gate = Arc::clone(&task_gate);
        async move {
            task_gate.notified().await;
            assert!(matches!(
                task_scope
                    .get()
                    .expect("scope initialized")
                    .rotate(RotationPolicy::Strict),
                Err(ScopeError::WouldJoin { .. })
            ));
            Ok::<_, ()>(())
        }
    }))?;
    gate.notify_one();
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    assert_eq!(scope.current().id(), generation.id());
    scope.current().ensure_current()?;
    Ok(())
}
