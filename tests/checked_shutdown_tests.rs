use busybeaver::{
    listener, work, Beaver, BeaverError, FixedCountBuilder, ShutdownError, ShutdownMode,
    ShutdownOptions, ShutdownOutcome, ShutdownTimeoutAction, TaskExitSummary, WorkResult,
};
use std::sync::Arc;
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn shutdown_is_accepted_synchronously_before_waiting() -> TestResult {
    let beaver = Beaver::new("sync-shutdown", 8)?;
    let shutdown = beaver.shutdown(ShutdownOptions::new())?;

    let legacy = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    assert!(matches!(
        beaver.enqueue(legacy).await,
        Err(BeaverError::ExecutorShuttingDown)
    ));
    assert!(matches!(
        shutdown.wait_grace_outcome().await?,
        ShutdownOutcome::Stopped(_)
    ));
    Ok(())
}

#[tokio::test]
async fn same_shutdown_options_share_handle_and_conflicts_are_explicit() -> TestResult {
    let beaver = Beaver::new("shutdown-config", 8)?;
    let options = ShutdownOptions::new().grace_period(Duration::from_secs(2));
    let first = beaver.shutdown(options.clone())?;
    let second = beaver.shutdown(options)?;
    assert_eq!(first.id(), second.id());

    let conflict = beaver
        .shutdown(ShutdownOptions::new().grace_period(Duration::from_secs(3)))
        .expect_err("different shutdown options must conflict");
    assert!(matches!(conflict, ShutdownError::ConfigConflict { .. }));
    assert!(matches!(
        first.wait_grace_outcome().await?,
        ShutdownOutcome::Stopped(_)
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn timeout_snapshot_keeps_controls_and_can_later_reach_final_report() -> TestResult {
    let beaver = Beaver::new("shutdown-timeout-report", 8)?;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let started_c = Arc::clone(&started);
    let release_c = Arc::clone(&release);
    let handle = beaver.spawn_future(async move {
        started_c.notify_one();
        release_c.notified().await;
        Ok::<_, &'static str>(())
    })?;
    let execution_id = handle.execution_id();
    started.notified().await;

    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::CancelAll)
            .grace_period(Duration::from_secs(1))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    match shutdown.wait_grace_outcome().await? {
        ShutdownOutcome::TimedOut {
            snapshot,
            pending,
            shutdown: retry,
        } => {
            assert_eq!(retry.id(), shutdown.id());
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].execution_id(), execution_id);
            assert_eq!(snapshot.tasks.len(), 1);
            assert!(snapshot.tasks[0].final_exit.is_none());
            assert!(snapshot.tasks[0].grace_deadline_exceeded);
        }
        ShutdownOutcome::Stopped(_) => panic!("non-cooperative work must time out"),
        _ => panic!("unexpected future shutdown outcome"),
    }

    release.notify_one();
    let report = shutdown.wait_final().await?;
    assert_eq!(report.tasks.len(), 1);
    assert_eq!(report.tasks[0].execution_id, execution_id);
    assert!(report.tasks[0].grace_deadline_exceeded);
    assert!(matches!(
        report.tasks[0].final_exit,
        TaskExitSummary::Cancelled { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn repeated_and_cancelled_shutdown_waits_do_not_stop_supervisor() -> TestResult {
    let beaver = Beaver::new("shutdown-wait-repeat", 8)?;
    let shutdown = beaver.shutdown(ShutdownOptions::new())?;

    tokio::select! {
        biased;
        outcome = shutdown.wait_grace_outcome() => {
            assert!(matches!(outcome?, ShutdownOutcome::Stopped(_)));
        }
        _ = tokio::task::yield_now() => {}
    }
    assert!(matches!(
        shutdown.wait_grace_outcome().await?,
        ShutdownOutcome::Stopped(_)
    ));
    let first = shutdown.wait_final().await?;
    let second = shutdown.wait_final().await?;
    assert!(Arc::ptr_eq(&first, &second));
    Ok(())
}

#[tokio::test]
async fn drain_finite_closes_admission_without_cancelling_finite_typed_work() -> TestResult {
    let beaver = Beaver::new("shutdown-drain-finite", 8)?;
    let lane = beaver.create_lane(
        busybeaver::LaneConfig::new("shutdown-drain-finite")
            .capacity(2)
            .concurrency(1),
    )?;
    let release = Arc::new(tokio::sync::Notify::new());
    let release_c = Arc::clone(&release);
    let mut first = lane.try_spawn_future(async move {
        release_c.notified().await;
        Ok::<_, &'static str>(1_u32)
    })?;
    let mut second = lane.try_spawn_future(async { Ok::<_, &'static str>(2_u32) })?;
    tokio::task::yield_now().await;

    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::DrainFinite)
            .grace_period(Duration::from_secs(2)),
    )?;
    assert!(matches!(
        lane.try_spawn_future(async { Ok::<_, ()>(()) }),
        Err(busybeaver::SpawnError::ExecutorShuttingDown)
    ));
    release.notify_one();

    assert!(matches!(
        first.join().await?,
        busybeaver::TaskExit::Completed(1)
    ));
    assert!(matches!(
        second.join().await?,
        busybeaver::TaskExit::Completed(2)
    ));
    assert!(matches!(
        shutdown.wait_grace_outcome().await?,
        ShutdownOutcome::Stopped(_)
    ));
    Ok(())
}

#[tokio::test]
async fn legacy_callback_panic_is_preserved_in_shutdown_report() -> TestResult {
    let beaver = Beaver::new("shutdown-callback-report", 8)?;
    let callback_started = Arc::new(tokio::sync::Notify::new());
    let callback_started_c = Arc::clone(&callback_started);
    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .listener(listener(
            move || {
                callback_started_c.notify_one();
                panic!("callback report marker");
            },
            || {},
        ))
        .build()?;
    beaver.enqueue(task).await?;
    callback_started.notified().await;

    let report = beaver
        .shutdown(ShutdownOptions::new())?
        .wait_final()
        .await?;
    assert_eq!(report.callback_failures.len(), 1);
    assert!(report.callback_failures[0]
        .message
        .contains("on_complete: callback report marker"));
    Ok(())
}
