use busybeaver::{
    AbortPolicy, Beaver, CancelReason, ForcedCancellationError, ShutdownOptions, ShutdownOutcome,
    ShutdownTimeoutAction, TaskExitSummary, TaskSpec,
};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn checked_shutdown_escalates_only_abort_allowed_tracked_futures(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let handle = beaver.spawn(
        TaskSpec::<(), ()>::new(|_| async { std::future::pending::<Result<(), ()>>().await })
            .abort_policy(AbortPolicy::Allowed),
    )?;
    tokio::task::yield_now().await;
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .grace_period(Duration::ZERO)
            .on_timeout(ShutdownTimeoutAction::AbortAllowed),
    )?;
    let outcome = shutdown.wait_grace_outcome().await?;
    match outcome {
        ShutdownOutcome::TimedOut { snapshot, .. } => {
            assert_eq!(snapshot.tasks.len(), 1);
            assert!(snapshot.tasks[0].forced_cancellation_requested);
        }
        ShutdownOutcome::Stopped(report) => {
            assert!(report.tasks[0].forced_cancellation_requested);
        }
        _ => panic!("unexpected shutdown outcome"),
    }
    let report = shutdown.wait_final().await?;
    assert!(report.tasks[0].forced_cancellation_requested);
    assert!(matches!(
        handle.wait().await,
        TaskExitSummary::Aborted {
            preceding_stop: Some(_)
        }
    ));
    Ok(())
}

#[tokio::test]
async fn cooperative_only_execution_rejects_direct_forced_cancellation(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let handle = beaver.spawn(TaskSpec::<(), ()>::new(|context| async move {
        context.cancelled().await;
        Ok(())
    }))?;
    tokio::task::yield_now().await;
    assert_eq!(
        handle.control().request_forced_cancellation(),
        Err(ForcedCancellationError::NotAllowed)
    );
    handle.control().cancel(CancelReason::UserRequested);
    assert!(matches!(
        handle.wait().await,
        TaskExitSummary::Cancelled { .. }
    ));
    Ok(())
}
