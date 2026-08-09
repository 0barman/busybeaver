use busybeaver::{
    CancelReason, GroupError, GroupState, Job, LaneConfig, Scheduler, TaskState, TaskTerminal,
    TrySubmitError,
};
use std::time::Duration;

#[tokio::test]
async fn group_shutdown_is_isolated_and_atomically_rejects_new_members() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(4, 2).unwrap())
        .build()
        .unwrap();
    let first_group = scheduler.create_group("first").unwrap();
    let second_group = scheduler.create_group("second").unwrap();
    let first = first_group
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let second = second_group
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let second_controller = second.controller();
    while first.observer().state() != TaskState::Running
        || second_controller.state() != TaskState::Running
    {
        tokio::task::yield_now().await;
    }

    let report = first_group.shutdown().await;
    assert!(report.is_complete());
    assert_eq!(report.cancelled(), 1);
    assert_eq!(second_controller.state(), TaskState::Running);
    assert!(matches!(
        first.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::GroupShutdown
        }
    ));
    assert!(matches!(
        first_group.try_submit(Job::once(|_| async { Ok::<_, ()>(()) })),
        Err(TrySubmitError::Closed(_))
    ));

    assert!(second_controller.cancel(CancelReason::User));
    assert!(matches!(
        second.join().await,
        TaskTerminal::Cancelled { .. }
    ));
}

#[tokio::test]
async fn same_name_group_requires_terminated_prior_generation() {
    let scheduler = Scheduler::builder().build().unwrap();
    let first = scheduler.create_group("workers").unwrap();
    assert!(matches!(
        scheduler.create_group("workers"),
        Err(GroupError::NameInUse {
            state: GroupState::Open,
            ..
        })
    ));
    let first_generation = first.generation();
    assert!(first.shutdown().await.is_complete());
    assert_eq!(first.state(), GroupState::Terminated);

    let second = scheduler.create_group("workers").unwrap();
    assert!(second.generation() > first_generation);
    assert_eq!(second.active_task_count(), 0);
}

#[tokio::test]
async fn task_context_exposes_group_identity_without_retaining_membership() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("scope").unwrap();
    let expected_generation = group.generation();
    let handle = group
        .submit(Job::once(|context| async move {
            Ok::<_, ()>((context.group_name(), context.group_generation()))
        }))
        .await
        .unwrap();

    match handle.join().await {
        TaskTerminal::Completed((Some(name), Some(generation))) => {
            assert_eq!(&*name, "scope");
            assert_eq!(generation, expected_generation);
        }
        other => panic!("unexpected terminal: {other:?}"),
    }
    assert_eq!(group.active_task_count(), 0);
}

#[tokio::test]
async fn task_can_request_its_own_group_shutdown_without_self_join() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("self-stop").unwrap();
    let handle = group
        .submit(Job::once(|context| async move {
            assert!(context.request_group_shutdown());
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();

    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::GroupShutdown
        }
    ));
    assert!(group.shutdown().await.is_complete());
    assert_eq!(group.state(), GroupState::Terminated);
}

#[tokio::test]
async fn task_can_request_scheduler_shutdown_without_self_join() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            assert!(context.request_scheduler_shutdown());
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();

    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::Shutdown
        }
    ));
    assert!(scheduler.shutdown().await.is_complete());
}

#[tokio::test]
async fn direct_shutdown_calls_from_owned_scope_handles_do_not_self_join() {
    let scheduler = Scheduler::builder().build().unwrap();
    let scheduler_for_job = scheduler.clone();
    let handle = scheduler
        .submit(Job::once(move |_| async move {
            let report = scheduler_for_job.shutdown().await;
            assert!(!report.is_complete());
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled { .. }
    ));
    assert!(scheduler.shutdown().await.is_complete());

    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("direct-self").unwrap();
    let group_for_job = group.clone();
    let handle = group
        .submit(Job::once(move |_| async move {
            let report = group_for_job.shutdown().await;
            assert!(!report.is_complete());
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled { .. }
    ));
    assert!(group.shutdown().await.is_complete());
}

#[tokio::test]
async fn dropping_last_group_handle_is_best_effort_cancel_not_blocking() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("drop-scope").unwrap();
    let handle = group
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    while observer.state() != TaskState::Running {
        tokio::task::yield_now().await;
    }

    drop(group);

    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::GroupShutdown
        }
    ));
    assert_eq!(observer.wait_terminal().await.state(), TaskState::Cancelled);
}

#[tokio::test]
async fn dropping_one_of_multiple_group_handles_does_not_cancel() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("cloned").unwrap();
    let remaining = group.clone();
    let handle = group
        .submit(Job::once(|_| async { Ok::<_, ()>(7_u8) }))
        .await
        .unwrap();
    drop(group);

    assert!(matches!(handle.join().await, TaskTerminal::Completed(7)));
    assert_eq!(remaining.state(), GroupState::Open);
}

#[test]
fn stopped_bound_runtime_settles_in_flight_group_shutdown_report() {
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
    let group = scheduler.create_group("runtime-stop").unwrap();
    let handle = runtime
        .block_on(group.submit(Job::once(|_| async {
            std::future::pending::<Result<(), ()>>().await
        })))
        .unwrap();
    let observer = handle.observer();
    while observer.state() != TaskState::Running {
        std::thread::yield_now();
    }
    let shutdown_group = group.clone();
    let shutdown_thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(shutdown_group.shutdown())
    });

    while group.state() != GroupState::Closing {
        std::thread::yield_now();
    }
    drop(runtime);

    let report = shutdown_thread.join().unwrap();
    assert!(report.is_complete());
    assert_eq!(report.executor_stopped(), 1);
    assert!(report.still_running().is_empty());
    assert_eq!(group.state(), GroupState::Terminated);
    let terminal = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(handle.join());
    assert!(matches!(terminal, TaskTerminal::ExecutorStopped));
}
