use busybeaver::{
    Backpressure, CancelReason, Job, LaneConfig, LaneError, ReusableJob, Scheduler,
    SchedulerBuildError, SubmissionFailure, TaskState, TaskTerminal, TrySubmitError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

fn assert_std_error<T: std::error::Error>() {}

#[test]
fn public_submission_errors_implement_std_error_without_job_debug_bounds() {
    struct OpaqueJob;

    assert_std_error::<TrySubmitError<OpaqueJob>>();
    assert_std_error::<busybeaver::KeyedSubmitError<OpaqueJob>>();
    assert_std_error::<busybeaver::ReplaceError<OpaqueJob>>();
    assert_std_error::<busybeaver::DispatchSubmitError<OpaqueJob>>();
}

struct NonCloneOutput(u32);

#[tokio::test]
async fn non_clone_output_has_one_join_owner() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(NonCloneOutput(7)) }))
        .await
        .unwrap();
    let observer = handle.observer();

    match handle.join().await {
        TaskTerminal::Completed(value) => assert_eq!(value.0, 7),
        other => panic!("unexpected terminal: {other:?}"),
    }
    assert_eq!(observer.state(), TaskState::Completed);
    assert_eq!(scheduler.active_task_count(), 0);
}

#[tokio::test]
async fn same_reusable_job_has_distinct_task_run_ids() {
    let scheduler = Scheduler::builder().build().unwrap();
    let job = ReusableJob::new(|_| async { Ok::<_, ()>(()) });
    let first = scheduler.submit(job.instantiate()).await.unwrap();
    let second = scheduler.submit(job.instantiate()).await.unwrap();

    assert_eq!(first.job_id(), second.job_id());
    assert_ne!(first.id(), second.id());
    assert!(matches!(first.join().await, TaskTerminal::Completed(())));
    assert!(matches!(second.join().await, TaskTerminal::Completed(())));
}

#[tokio::test(start_paused = true)]
async fn task_context_sleep_is_cancel_safe() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            let _ = context.sleep(Duration::from_secs(60)).await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let controller = handle.controller();
    tokio::task::yield_now().await;
    assert!(controller.cancel(CancelReason::User));

    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::User
        }
    ));
}

#[tokio::test]
async fn dropping_join_future_detaches_without_losing_summary() {
    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let handle = scheduler
        .submit(Job::once(move |_| {
            let finish = Arc::clone(&finish_for_job);
            async move {
                finish.notified().await;
                Ok::<_, ()>(vec![0_u8; 1024])
            }
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    let join = handle.join();
    drop(join);

    finish.notify_one();
    assert_eq!(observer.wait_terminal().await.state(), TaskState::Completed);
    assert_eq!(scheduler.active_task_count(), 0);
}

#[tokio::test]
async fn lane_capacity_counts_waiters_and_cancel_releases_slot() {
    let lane = LaneConfig::new(1, 1)
        .unwrap()
        .backpressure(Backpressure::Reject);
    let scheduler = Scheduler::builder()
        .default_lane(lane)
        .global_concurrency(1)
        .build()
        .unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_first = Arc::clone(&finish);
    let first = scheduler
        .submit(Job::once(move |_| {
            let finish = Arc::clone(&finish_for_first);
            async move {
                finish.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    while first.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }

    let second = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    let third = Job::once(|_| async { Ok::<_, ()>(()) });
    let error = match scheduler.try_submit(third) {
        Err(error) => error,
        Ok(_) => panic!("third job must be returned when the wait queue is full"),
    };
    assert_eq!(error.reason(), SubmissionFailure::Full);
    let third = match error {
        TrySubmitError::Full(job) => job,
        _ => unreachable!("reason and owned error variant must agree"),
    };

    assert!(second.controller().cancel(CancelReason::User));
    assert!(matches!(
        second.join().await,
        TaskTerminal::Cancelled { .. }
    ));
    let third = scheduler
        .try_submit(third)
        .expect("cancel releases capacity");
    finish.notify_one();
    assert!(matches!(first.join().await, TaskTerminal::Completed(())));
    assert!(matches!(third.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn lane_configuration_is_idempotent_or_conflicting() {
    let scheduler = Scheduler::builder().build().unwrap();
    let config = LaneConfig::new(4, 2).unwrap();
    scheduler.ensure_lane("secondary", config.clone()).unwrap();
    scheduler.ensure_lane("secondary", config).unwrap();

    let different = LaneConfig::new(5, 2).unwrap();
    assert!(matches!(
        scheduler.ensure_lane("secondary", different),
        Err(LaneError::ConfigConflict { .. })
    ));
}

#[test]
fn scheduler_builder_reports_missing_runtime() {
    assert!(matches!(
        Scheduler::builder().build(),
        Err(SchedulerBuildError::RuntimeUnavailable)
    ));
}

#[test]
fn explicit_runtime_handle_can_be_bound_outside_runtime() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let scheduler = Scheduler::builder()
        .runtime_handle(runtime.handle().clone())
        .build()
        .unwrap();
    runtime.block_on(async {
        scheduler
            .ensure_lane("bound", LaneConfig::new(2, 1).unwrap())
            .unwrap();
        let handle = scheduler
            .submit(Job::once(|_| async { Ok::<_, ()>(()) }).on_lane("bound"))
            .await
            .unwrap();
        assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    });
}

#[tokio::test]
async fn failed_try_submit_returns_original_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let job = Job::once(move |_| {
        let calls = Arc::clone(&calls_for_job);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(())
        }
    })
    .on_lane("missing");

    let error = match scheduler.try_submit(job) {
        Err(error) => error,
        Ok(_) => panic!("missing lane must reject the original job"),
    };
    assert_eq!(error.reason(), SubmissionFailure::LaneNotFound);
    let job = match error {
        TrySubmitError::LaneNotFound(job) => job,
        _ => unreachable!("reason and owned error variant must agree"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let handle = scheduler.try_submit(job.on_lane("default")).unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tracked_children_are_aborted_when_run_finishes() {
    let scheduler = Scheduler::builder().build().unwrap();
    let (child_tx, child_rx) = tokio::sync::oneshot::channel();
    let handle = scheduler
        .submit(Job::once(move |context| async move {
            let child = context.spawn_tracked(std::future::pending::<()>()).unwrap();
            child_tx.send(child).unwrap();
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));

    let child = child_rx.await.unwrap();
    assert!(child.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn tracked_children_are_aborted_when_run_panics() {
    let scheduler = Scheduler::builder().build().unwrap();
    let (child_tx, child_rx) = tokio::sync::oneshot::channel();
    let handle = scheduler
        .submit(Job::<(), ()>::once(move |context| async move {
            let child = context.spawn_tracked(std::future::pending::<()>()).unwrap();
            child_tx.send(child).unwrap();
            panic!("intentional task panic");
        }))
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Panicked(_)));

    let child = child_rx.await.unwrap();
    assert!(child.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn untracked_children_are_not_joined_or_cancelled() {
    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_for_job = Arc::clone(&completed);
    let handle = scheduler
        .submit(Job::once(move |_| async move {
            tokio::spawn(async move {
                finish_for_job.notified().await;
                completed_for_job.fetch_add(1, Ordering::SeqCst);
            });
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));

    finish.notify_one();
    while completed.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn scheduler_public_handles_are_send_sync_static() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Scheduler>();
    assert_send_sync_static::<busybeaver::TaskController>();
    assert_send_sync_static::<busybeaver::TaskObserver>();
}

#[test]
fn stopped_bound_runtime_finalizes_detached_task_and_registry() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let scheduler = runtime.block_on(async { Scheduler::builder().build().unwrap() });
    let handle = runtime
        .block_on(scheduler.submit(Job::once(|_| async {
            std::future::pending::<Result<(), ()>>().await
        })))
        .unwrap();
    let observer = handle.observer();
    drop(handle);

    drop(runtime);

    let verifier = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let snapshot = verifier
        .block_on(async {
            tokio::time::timeout(Duration::from_millis(250), observer.wait_terminal()).await
        })
        .expect("runtime shutdown must wake detached observers");
    assert_eq!(snapshot.state(), TaskState::ExecutorStopped);
    assert_eq!(scheduler.snapshot().active_tasks(), 0);
}

#[test]
fn stopped_bound_runtime_settles_in_flight_shutdown_report() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let scheduler = runtime.block_on(async {
        Scheduler::builder()
            .shutdown_policy(busybeaver::ShutdownPolicy::graceful(Duration::from_secs(
                30,
            )))
            .build()
            .unwrap()
    });
    let handle = runtime
        .block_on(scheduler.submit(Job::once(|_| async {
            std::future::pending::<Result<(), ()>>().await
        })))
        .unwrap();
    let observer = handle.observer();
    while observer.state() != TaskState::Running {
        std::thread::yield_now();
    }
    let shutdown_scheduler = scheduler.clone();
    let shutdown_thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(shutdown_scheduler.shutdown())
    });

    while !scheduler.snapshot().is_shutting_down() {
        std::thread::yield_now();
    }
    drop(runtime);

    let report = shutdown_thread.join().unwrap();
    assert!(report.is_complete());
    assert_eq!(report.executor_stopped(), 1);
    assert!(report.still_running().is_empty());
    assert_eq!(scheduler.snapshot().active_tasks(), 0);
    let terminal = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(handle.join());
    assert!(matches!(terminal, TaskTerminal::ExecutorStopped));
}
