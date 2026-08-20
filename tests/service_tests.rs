use busybeaver::{
    AbortPolicy, Beaver, CancelReason, CleanupOutcome, HookOutcome, RestartPolicy, RestartTrigger,
    ServiceBuilder, ServiceStatus, ShutdownOptions, TaskExit, TaskExitSummary,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn service_readiness_cancel_and_shutdown_hook_are_supervised(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let hooks = Arc::new(AtomicUsize::new(0));
    let hooks_c = Arc::clone(&hooks);
    let spec = ServiceBuilder::new(|context| async move {
        assert!(context.ready());
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .shutdown_hook(move |_| {
        let hooks = Arc::clone(&hooks_c);
        async move {
            hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    })
    .build()?;
    let service = beaver.start_service(spec)?;
    assert_eq!(service.wait_ready().await?, 1);
    let exit = service
        .control()
        .cancel_and_wait(CancelReason::UserRequested)
        .await;
    assert!(matches!(exit, TaskExitSummary::Cancelled { .. }));
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    assert!(matches!(
        service.status(),
        ServiceStatus::Stopped {
            hook: HookOutcome::Completed,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn service_restart_is_bounded_and_old_generation_finishes_first(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = ServiceBuilder::new(move |context| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err::<usize, _>("temporary")
            } else {
                assert_eq!(context.generation(), 3);
                Ok(attempt)
            }
        }
    })
    .restart(
        RestartTrigger::OnFailure,
        RestartPolicy::new(2)
            .window(Duration::from_secs(10))
            .backoff(Duration::from_secs(1)),
    )
    .build()?;
    let mut service = beaver.start_service(spec)?;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(matches!(service.join().await?, TaskExit::Completed(3)));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn shutdown_disables_service_restart() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = ServiceBuilder::new(move |context| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            context.ready();
            context.cancelled().await;
            Err::<(), _>("stopped")
        }
    })
    .restart(RestartTrigger::OnFailure, RestartPolicy::new(5))
    .build()?;
    let service = beaver.start_service(spec)?;
    service.wait_ready().await?;
    service.control().cancel(CancelReason::ExecutorShutdown);
    assert!(matches!(
        service.wait().await,
        TaskExitSummary::Cancelled { .. }
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_hook_timeout_is_isolated_and_reported_in_status(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let spec = ServiceBuilder::new(|context| async move {
        context.ready();
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .shutdown_hook(|_| async {
        std::future::pending::<()>().await;
        Ok(())
    })
    .shutdown_hook_timeout(Duration::from_secs(1))
    .build()?;
    let service = beaver.start_service(spec)?;
    service.wait_ready().await?;
    service.control().cancel(CancelReason::UserRequested);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    service.wait().await;
    assert!(matches!(
        service.status(),
        ServiceStatus::Stopped {
            hook: HookOutcome::TimedOut,
            ..
        }
    ));
    Ok(())
}

#[tokio::test]
async fn abort_allowed_service_can_be_forced_after_cooperative_stop(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let spec = ServiceBuilder::<(), ()>::new(|context| async move {
        context.ready();
        std::future::pending::<Result<(), ()>>().await
    })
    .abort_policy(AbortPolicy::Allowed)
    .build()?;
    let mut service = beaver.start_service(spec)?;
    service.wait_ready().await?;
    let control = service.control();
    control.cancel(CancelReason::UserRequested);
    assert_eq!(
        control.request_forced_cancellation()?,
        busybeaver::ForcedCancellationOutcome::Requested
    );
    assert!(matches!(
        service.join().await?,
        TaskExit::Aborted {
            preceding_stop: Some(_)
        }
    ));
    Ok(())
}

#[tokio::test]
async fn shutdown_report_contains_service_hook_failure() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let spec = ServiceBuilder::new(|context| async move {
        context.ready();
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .shutdown_hook(|_| async {
        panic!("hook report marker");
        #[allow(unreachable_code)]
        Ok(())
    })
    .build()?;
    let service = beaver.start_service(spec)?;
    service.wait_ready().await?;
    let execution_id = service.execution_id();

    let report = beaver
        .shutdown(ShutdownOptions::new())?
        .wait_final()
        .await?;
    let record = report
        .tasks
        .iter()
        .find(|record| record.execution_id == execution_id)
        .expect("service shutdown record");
    assert!(matches!(
        &record.cleanup,
        CleanupOutcome::Failed(message) if message == "shutdown hook panicked"
    ));
    Ok(())
}
