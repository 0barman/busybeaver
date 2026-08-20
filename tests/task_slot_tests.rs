use busybeaver::{
    Beaver, LaneConfig, ReplaceError, ReplaceOutcome, ReplacePolicy, SlotKey, TaskExit, TaskSpec,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Notify;

#[tokio::test]
async fn strict_replace_is_newest_wins_and_old_completion_cannot_clear_new(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("slot-lane").capacity(8).concurrency(2))?;
    let slot = beaver.create_task_slot(SlotKey::new("qr-code")?, lane)?;

    let old_stopped = Arc::new(AtomicBool::new(false));
    let old_stopped_c = Arc::clone(&old_stopped);
    let first = slot
        .replace(
            1,
            TaskSpec::new(move |context| {
                let old_stopped = Arc::clone(&old_stopped_c);
                async move {
                    context.cancelled().await;
                    old_stopped.store(true, Ordering::SeqCst);
                    Ok::<_, ()>(1u8)
                }
            }),
            ReplacePolicy::StrictSingleInstance,
        )?
        .await?;
    let old = match first {
        ReplaceOutcome::Replaced { handle, .. } => handle,
        _ => panic!("first revision was not admitted"),
    };
    tokio::task::yield_now().await;

    let finish_fresh = Arc::new(Notify::new());
    let finish_fresh_c = Arc::clone(&finish_fresh);
    let second = slot
        .replace(
            2,
            TaskSpec::new(move |_| {
                let finish_fresh = Arc::clone(&finish_fresh_c);
                async move {
                    finish_fresh.notified().await;
                    Ok::<_, ()>(2u8)
                }
            }),
            ReplacePolicy::StrictSingleInstance,
        )?
        .await?;
    let mut fresh = match second {
        ReplaceOutcome::Replaced { handle, .. } => handle,
        _ => panic!("second revision was not admitted"),
    };
    assert!(old_stopped.load(Ordering::SeqCst));
    assert!(matches!(
        old.wait().await,
        busybeaver::TaskExitSummary::Cancelled { .. }
    ));
    let fresh_id = fresh.execution_id();
    assert_eq!(slot.snapshot().current_execution, Some(fresh_id));
    finish_fresh.notify_one();
    assert!(matches!(fresh.join().await?, TaskExit::Completed(2)));
    for _ in 0..20 {
        if slot.snapshot().current_execution.is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(slot.snapshot().revision, Some(2));
    assert_eq!(slot.snapshot().current_execution, None);
    Ok(())
}

#[tokio::test]
async fn stale_and_same_revision_conflicts_are_explicit() -> Result<(), Box<dyn std::error::Error>>
{
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("slot-errors").capacity(8))?;
    let slot = beaver.create_task_slot(SlotKey::new("latest")?, lane)?;
    let spec = TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok::<_, ()>(())
    });
    let admitted = slot
        .replace(5, spec.clone(), ReplacePolicy::StrictSingleInstance)?
        .await?;
    assert!(matches!(admitted, ReplaceOutcome::Replaced { .. }));
    assert!(matches!(
        slot.replace(4, spec.clone(), ReplacePolicy::StrictSingleInstance),
        Err(ReplaceError::StaleRevision { .. })
    ));
    let conflicting = TaskSpec::new(|_| async { Ok::<_, ()>(()) });
    assert!(matches!(
        slot.replace(5, conflicting, ReplacePolicy::StrictSingleInstance),
        Err(ReplaceError::RevisionConflict { revision: 5 })
    ));
    slot.close();
    Ok(())
}

#[tokio::test]
async fn dropping_replace_future_does_not_cancel_accepted_transaction(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("slot-drop").capacity(8))?;
    let slot = beaver.create_task_slot(SlotKey::new("drop-safe")?, lane)?;
    let replace = slot.replace(
        1,
        TaskSpec::new(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }),
        ReplacePolicy::StrictSingleInstance,
    )?;
    drop(replace);
    for _ in 0..50 {
        if slot.snapshot().current_execution.is_some() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(slot.snapshot().revision, Some(1));
    assert!(slot.snapshot().current_execution.is_some());
    slot.close();
    Ok(())
}

#[tokio::test]
async fn strict_self_replace_is_rejected_before_acceptance(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("slot-self").capacity(8))?;
    let slot = beaver.create_task_slot(SlotKey::new("self")?, lane)?;
    let slot_cell = Arc::new(OnceLock::new());
    slot_cell.set(slot.clone()).expect("set slot once");
    let gate = Arc::new(Notify::new());
    let task_slot = Arc::clone(&slot_cell);
    let task_gate = Arc::clone(&gate);
    let first = slot
        .replace(
            1,
            TaskSpec::new(move |_| {
                let task_slot = Arc::clone(&task_slot);
                let task_gate = Arc::clone(&task_gate);
                async move {
                    task_gate.notified().await;
                    let result = task_slot.get().expect("slot initialized").replace(
                        2,
                        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
                        ReplacePolicy::StrictSingleInstance,
                    );
                    assert!(matches!(result, Err(ReplaceError::WouldJoin { .. })));
                    Ok::<_, ()>(())
                }
            }),
            ReplacePolicy::StrictSingleInstance,
        )?
        .await?;
    let mut handle = match first {
        ReplaceOutcome::Replaced { handle, .. } => handle,
        _ => panic!("first revision was not admitted"),
    };
    gate.notify_one();
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    assert_eq!(slot.snapshot().revision, Some(1));
    Ok(())
}
