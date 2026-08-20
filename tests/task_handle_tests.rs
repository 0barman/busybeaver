use busybeaver::{
    Beaver, CancelReason, CancelRequestOutcome, JoinResultError, TaskExit, TaskSelector, TaskSpec,
    TaskState,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn same_spec_creates_independent_execution_identity_and_cancel_state() -> TestResult {
    let beaver = Beaver::new("typed-default", 8)?;
    let spec = TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok::<_, &'static str>(())
    });

    let mut first = beaver.spawn(spec.clone())?;
    let mut second = beaver.spawn(spec)?;
    assert_ne!(first.execution_id(), second.execution_id());
    assert_eq!(first.task_spec_id(), second.task_spec_id());

    assert_eq!(
        first.control().cancel(CancelReason::UserRequested),
        CancelRequestOutcome::Requested
    );
    assert!(matches!(
        first.join().await?,
        TaskExit::Cancelled {
            reason: CancelReason::UserRequested
        }
    ));
    assert!(!second.state().is_terminal());

    second.control().cancel(CancelReason::UserRequested);
    assert!(matches!(second.join().await?, TaskExit::Cancelled { .. }));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn wait_is_repeatable_and_does_not_consume_typed_result() -> TestResult {
    let beaver = Beaver::new("repeatable-wait", 8)?;
    let mut handle = beaver.spawn_future(async { Ok::<_, &'static str>(41_u32) })?;

    let first = handle.wait().await;
    let second = handle.wait().await;
    assert_eq!(first, second);
    assert!(first.is_completed());
    assert!(matches!(handle.join().await?, TaskExit::Completed(41)));
    assert!(matches!(
        handle.try_join(),
        Err(JoinResultError::AlreadyTaken)
    ));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_join_future_can_be_retried_without_losing_value() -> TestResult {
    let beaver = Beaver::new("cancel-safe-join", 8)?;
    let release = Arc::new(tokio::sync::Notify::new());
    let release_c = Arc::clone(&release);
    let mut handle = beaver.spawn_future(async move {
        release_c.notified().await;
        Ok::<_, &'static str>("value")
    })?;

    tokio::select! {
        biased;
        result = handle.join() => panic!("join unexpectedly completed: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }

    release.notify_one();
    assert!(matches!(handle.join().await?, TaskExit::Completed("value")));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn work_context_sleep_is_cancel_aware() -> TestResult {
    let beaver = Beaver::new("context-sleep", 8)?;
    let side_effects = Arc::new(AtomicU32::new(0));
    let side_effects_c = Arc::clone(&side_effects);
    let spec = TaskSpec::new(move |ctx| {
        let side_effects = Arc::clone(&side_effects_c);
        async move {
            ctx.sleep(Duration::from_secs(60)).await?;
            side_effects.fetch_add(1, Ordering::SeqCst);
            Ok::<_, busybeaver::Cancelled>(())
        }
    });
    let mut handle = beaver.spawn(spec)?;

    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    handle.control().cancel(CancelReason::UserRequested);
    assert!(matches!(handle.join().await?, TaskExit::Cancelled { .. }));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(side_effects.load(Ordering::SeqCst), 0);
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_future_accepts_non_clone_one_shot_state() -> TestResult {
    struct OneShot(String);

    let beaver = Beaver::new("one-shot", 8)?;
    let state = OneShot("owned".to_string());
    let mut handle = beaver.spawn_future(async move { Ok::<_, &'static str>(state.0) })?;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Completed(value) if value == "owned"
    ));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn control_lookup_is_active_only_but_summary_is_retained() -> TestResult {
    let beaver = Beaver::new("lookup", 8)?;
    let mut handle = beaver.spawn_future(async { Err::<(), _>("business") })?;
    let execution_id = handle.execution_id();
    assert!(beaver.execution_control(execution_id).is_some());

    assert!(matches!(handle.join().await?, TaskExit::Failed(_)));
    assert!(beaver.execution_control(execution_id).is_none());
    assert!(beaver
        .execution_summary(execution_id)
        .is_some_and(|summary| summary.is_failed()));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn cancel_and_wait_submits_cancel_before_its_future_is_polled() -> TestResult {
    let beaver = Beaver::new("cancel-and-wait", 8)?;
    let spec = TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok::<_, &'static str>(())
    });
    let mut handle = beaver.spawn(spec)?;

    let cancel_wait = handle.cancel_and_wait(CancelReason::UserRequested);
    drop(cancel_wait);
    assert!(matches!(handle.join().await?, TaskExit::Cancelled { .. }));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn cancel_on_drop_wrapper_requests_cancellation_without_optional_state() -> TestResult {
    let beaver = Beaver::new("cancel-on-drop", 8)?;
    let handle = beaver.spawn(TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok::<_, &'static str>(())
    }))?;
    let control = handle.control();
    let cancel_on_drop = handle.cancel_on_drop(CancelReason::UserRequested);

    drop(cancel_on_drop);

    assert!(matches!(
        control.wait().await,
        busybeaver::TaskExitSummary::Cancelled {
            reason: CancelReason::UserRequested
        }
    ));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn typed_handle_and_control_have_promised_send_bounds() {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send::<busybeaver::TaskHandle<String, String>>();
    assert_send_sync::<busybeaver::TaskControlHandle>();
    assert_send_sync::<TaskSpec<String, String>>();
    assert_send_sync::<busybeaver::TaskSpecId>();
    assert_send_sync::<busybeaver::ExecutionId>();
    assert_send_sync::<TaskState>();
}

#[tokio::test]
async fn spec_selector_cancels_a_linearized_snapshot_only() -> TestResult {
    let beaver = Beaver::new("selector", 8)?;
    let spec = TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok::<_, &'static str>(())
    });
    let mut first = beaver.spawn(spec.clone())?;
    let mut second = beaver.spawn(spec.clone())?;

    let report = beaver.cancel_snapshot(TaskSelector::Spec(spec.id()), CancelReason::UserRequested);
    assert_eq!(report.records.len(), 2);

    let mut admitted_after_snapshot = beaver.spawn(spec)?;
    assert!(matches!(first.join().await?, TaskExit::Cancelled { .. }));
    assert!(matches!(second.join().await?, TaskExit::Cancelled { .. }));
    assert!(!admitted_after_snapshot.state().is_terminal());

    admitted_after_snapshot
        .control()
        .cancel(CancelReason::UserRequested);
    assert!(matches!(
        admitted_after_snapshot.join().await?,
        TaskExit::Cancelled { .. }
    ));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn checked_wait_rejects_self_and_ancestor_cycles() -> TestResult {
    let beaver = Beaver::new("checked-wait-cycle", 8)?;
    let mut handle = beaver.spawn(TaskSpec::new(|context| async move {
        assert!(matches!(
            context.control().wait_checked().await,
            Err(busybeaver::ExecutionWaitError::WouldJoin { .. })
        ));

        let parent = context.control();
        let mut child = context
            .spawn_child(async move {
                assert!(matches!(
                    parent.wait_checked().await,
                    Err(busybeaver::ExecutionWaitError::WouldJoin { .. })
                ));
                Ok::<_, String>(())
            })
            .map_err(|error| error.to_string())?;
        assert!(matches!(child.join().await, Ok(TaskExit::Completed(()))));
        Ok::<_, String>(())
    }))?;
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    beaver.destroy().await?;
    Ok(())
}
