use busybeaver::{
    CancelReason, Job, KeyedSubmitError, Scheduler, TaskState, TaskTerminal, TrySubmitError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_if_absent_creates_exactly_one_run() {
    let scheduler = Scheduler::builder().build().unwrap();
    let release = Arc::new(Notify::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let mut contenders = Vec::new();
    for _ in 0..16 {
        let scheduler = scheduler.clone();
        let release = Arc::clone(&release);
        let executions = Arc::clone(&executions);
        contenders.push(tokio::spawn(async move {
            scheduler.start_if_absent(
                "singleton",
                Job::once(move |context| async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    tokio::select! {
                        _ = release.notified() => {}
                        _ = context.cancelled() => {}
                    }
                    Ok::<_, ()>(())
                }),
            )
        }));
    }

    let mut winner = None;
    let mut occupied = 0;
    for contender in contenders {
        match contender.await.unwrap() {
            Ok(handle) => winner = Some(handle),
            Err(KeyedSubmitError::Occupied { .. }) => occupied += 1,
            _ => panic!("unexpected keyed result"),
        }
    }
    let winner = winner.expect("one contender wins");
    assert_eq!(occupied, 15);
    while winner.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    while executions.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let status = scheduler.key_status("singleton").unwrap();
    assert_eq!(status.run_id(), Some(winner.id()));
    assert!(!status.is_replacing());

    release.notify_one();
    assert!(matches!(winner.join().await, TaskTerminal::Completed(())));
    assert_eq!(scheduler.key_status("singleton"), None);
}

#[tokio::test]
async fn completed_key_can_restart_with_higher_generation() {
    let scheduler = Scheduler::builder().build().unwrap();
    let first = scheduler
        .start_if_absent("refresh", Job::once(|_| async { Ok::<_, ()>(()) }))
        .unwrap();
    let first_generation = first.observer().snapshot().key_generation().unwrap();
    assert!(matches!(first.join().await, TaskTerminal::Completed(())));

    let second = scheduler
        .start_if_absent("refresh", Job::once(|_| async { Ok::<_, ()>(()) }))
        .unwrap();
    let second_generation = second.observer().snapshot().key_generation().unwrap();
    assert!(second_generation > first_generation);
    assert!(matches!(second.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn stop_cancels_current_key_and_is_idempotent_after_cleanup() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .start_if_absent(
            "stoppable",
            Job::once(|context| async move {
                context.cancelled().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while handle.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }

    assert!(scheduler.stop("stoppable"));
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::User
        }
    ));
    assert!(!scheduler.stop("stoppable"));
}

#[tokio::test]
async fn keyed_scopes_are_isolated_between_scheduler_and_groups() {
    let scheduler = Scheduler::builder()
        .global_concurrency(3)
        .default_lane(busybeaver::LaneConfig::new(4, 3).unwrap())
        .build()
        .unwrap();
    let first_group = scheduler.create_group("first").unwrap();
    let second_group = scheduler.create_group("second").unwrap();

    let root = scheduler
        .start_if_absent("same", Job::once(|_| async { Ok::<_, ()>(1_u8) }))
        .unwrap();
    let first = first_group
        .start_if_absent("same", Job::once(|_| async { Ok::<_, ()>(2_u8) }))
        .unwrap();
    let second = second_group
        .start_if_absent("same", Job::once(|_| async { Ok::<_, ()>(3_u8) }))
        .unwrap();

    assert!(matches!(root.join().await, TaskTerminal::Completed(1)));
    assert!(matches!(first.join().await, TaskTerminal::Completed(2)));
    assert!(matches!(second.join().await, TaskTerminal::Completed(3)));
}

#[tokio::test]
async fn failed_keyed_submission_rolls_back_reservation_and_returns_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    let job = Job::once(|_| async { Ok::<_, ()>(()) }).on_lane("missing");
    let job = match scheduler.start_if_absent("recover", job) {
        Err(KeyedSubmitError::Submit(TrySubmitError::LaneNotFound(job))) => job,
        _ => panic!("unexpected result"),
    };
    assert_eq!(scheduler.key_status("recover"), None);

    let handle = scheduler
        .start_if_absent("recover", job.on_lane("default"))
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
}
