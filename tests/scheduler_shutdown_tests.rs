use busybeaver::{
    CancelReason, Job, Scheduler, ShutdownPolicy, SubmissionFailure, TaskState, TaskTerminal,
    TrySubmitError,
};
use std::future::pending;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test(start_paused = true)]
async fn graceful_shutdown_cancels_context_wait() {
    let scheduler = Scheduler::builder()
        .shutdown_policy(ShutdownPolicy::graceful(Duration::from_secs(5)))
        .build()
        .unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            let _ = context.sleep(Duration::from_secs(60)).await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    tokio::task::yield_now().await;

    let report = scheduler.shutdown().await;
    assert!(report.is_complete());
    assert_eq!(report.cancelled(), 1);
    assert_eq!(report.aborted(), 0);
    assert!(!report.grace_timed_out());
    assert_eq!(observer.state(), TaskState::Cancelled);
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::Shutdown
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn abort_is_reported_only_after_join_confirmation() {
    let scheduler = Scheduler::builder()
        .shutdown_policy(ShutdownPolicy::graceful_then_abort(
            Duration::from_secs(5),
            Duration::from_secs(5),
        ))
        .build()
        .unwrap();
    let started = Arc::new(Notify::new());
    let started_for_job = Arc::clone(&started);
    let handle = scheduler
        .submit(Job::once(move |_| {
            let started = Arc::clone(&started_for_job);
            async move {
                started.notify_one();
                pending::<Result<(), ()>>().await
            }
        }))
        .await
        .unwrap();
    started.notified().await;

    let shutdown = scheduler.shutdown();
    tokio::pin!(shutdown);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let report = shutdown.await;

    assert!(report.is_complete());
    assert_eq!(report.abort_requested(), 1);
    assert_eq!(report.aborted(), 1);
    assert!(report.grace_timed_out());
    assert!(matches!(handle.join().await, TaskTerminal::Aborted));
}

#[tokio::test]
async fn concurrent_shutdown_callers_share_result() {
    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let started_for_job = Arc::clone(&started);
    let _handle = scheduler
        .submit(Job::once(move |_| {
            let finish = Arc::clone(&finish_for_job);
            let started = Arc::clone(&started_for_job);
            async move {
                started.notify_one();
                finish.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    started.notified().await;

    let first = scheduler.clone();
    let second = scheduler.clone();
    let first = tokio::spawn(async move { first.shutdown().await });
    let second = tokio::spawn(async move { second.shutdown().await });
    tokio::task::yield_now().await;
    finish.notify_waiters();
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first, second);
    assert!(first.is_complete());
}

#[tokio::test]
async fn dropping_shutdown_waiter_does_not_stop_coordinator() {
    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let started_for_job = Arc::clone(&started);
    let _handle = scheduler
        .submit(Job::once(move |_| {
            let finish = Arc::clone(&finish_for_job);
            let started = Arc::clone(&started_for_job);
            async move {
                started.notify_one();
                finish.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    started.notified().await;

    let caller_scheduler = scheduler.clone();
    let caller = tokio::spawn(async move { caller_scheduler.shutdown().await });
    tokio::task::yield_now().await;
    caller.abort();
    let _ = caller.await;
    finish.notify_waiters();

    assert!(scheduler.shutdown().await.is_complete());
}

#[tokio::test]
async fn submit_after_shutdown_starts_returns_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    assert!(scheduler.shutdown().await.is_complete());
    let job = Job::once(|_| async { Ok::<_, ()>(()) });
    let error = match scheduler.try_submit(job) {
        Err(error) => error,
        Ok(_) => panic!("terminated Scheduler must reject new work"),
    };
    assert_eq!(error.reason(), SubmissionFailure::Closed);
    assert!(matches!(error, TrySubmitError::Closed(_)));
}

#[tokio::test]
async fn submit_during_shutdown_reports_shutting_down_and_returns_job() {
    let scheduler = Scheduler::builder()
        .shutdown_policy(ShutdownPolicy::graceful(Duration::from_secs(30)))
        .build()
        .unwrap();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let running = scheduler
        .submit(Job::once({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            move |_| async move {
                started.notify_one();
                release.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    started.notified().await;
    let shutdown = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.shutdown().await }
    });
    tokio::task::yield_now().await;

    let error = match scheduler.try_submit(Job::once(|_| async { Ok::<_, ()>(()) })) {
        Err(error) => error,
        Ok(_) => panic!("closing Scheduler must reject new work"),
    };
    assert_eq!(error.reason(), SubmissionFailure::ShuttingDown);
    assert!(matches!(error, TrySubmitError::ShuttingDown(_)));

    release.notify_one();
    let _ = running.join().await;
    assert!(shutdown.await.unwrap().is_complete());
}

#[tokio::test]
async fn dropping_cancel_and_wait_after_signal_keeps_cancellation() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    let waiter = tokio::spawn(handle.cancel_and_wait(CancelReason::User));
    while observer.snapshot().cancel_reason() != Some(CancelReason::User) {
        tokio::task::yield_now().await;
    }
    waiter.abort();
    let _ = waiter.await;

    let terminal = observer.wait_terminal().await;
    assert_eq!(terminal.state(), TaskState::Cancelled);
    assert_eq!(terminal.cancel_reason(), Some(CancelReason::User));
}

#[tokio::test]
async fn last_scheduler_clone_drop_requests_best_effort_cancel() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    drop(scheduler);

    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::Shutdown
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn observer_timeout_does_not_change_global_shutdown_policy() {
    let scheduler = Scheduler::builder()
        .shutdown_policy(ShutdownPolicy::graceful(Duration::from_secs(30)))
        .build()
        .unwrap();
    let finish = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let started_for_job = Arc::clone(&started);
    let _handle = scheduler
        .submit(Job::once(move |_| {
            let finish = Arc::clone(&finish_for_job);
            let started = Arc::clone(&started_for_job);
            async move {
                started.notify_one();
                finish.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    started.notified().await;

    let observation = scheduler.shutdown_with_timeout(Duration::from_secs(1));
    tokio::pin!(observation);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let timeout = observation
        .await
        .expect_err("observer deadline must expire");
    assert_eq!(timeout.report().still_running().len(), 1);

    finish.notify_waiters();
    let final_report = scheduler.shutdown().await;
    assert!(final_report.is_complete());
    assert!(!final_report.grace_timed_out());
}
