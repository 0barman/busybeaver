use busybeaver::{
    CancelReason, Job, LaneConfig, ReplaceError, ReplacePolicy, ReplaceTimeoutAction, Scheduler,
    TaskState, TaskTerminal,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test]
async fn replace_after_confirmed_stop_never_overlaps_runs() {
    let scheduler = Scheduler::builder().build().unwrap();
    let may_stop = Arc::new(Notify::new());
    let may_stop_for_old = Arc::clone(&may_stop);
    let cancellation_seen = Arc::new(Notify::new());
    let cancellation_seen_by_old = Arc::clone(&cancellation_seen);
    let old = scheduler
        .start_if_absent(
            "service",
            Job::once(move |context| async move {
                context.cancelled().await;
                cancellation_seen_by_old.notify_one();
                may_stop_for_old.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }

    let new_started = Arc::new(AtomicBool::new(false));
    let new_started_by_job = Arc::clone(&new_started);
    let scheduler_for_replace = scheduler.clone();
    let replacing = tokio::spawn(async move {
        scheduler_for_replace
            .replace(
                "service",
                Job::once(move |_| async move {
                    new_started_by_job.store(true, Ordering::SeqCst);
                    Ok::<_, ()>(7_u8)
                }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(30),
                    ReplaceTimeoutAction::Fail,
                ),
            )
            .await
    });
    cancellation_seen.notified().await;
    assert!(!new_started.load(Ordering::SeqCst));
    may_stop.notify_one();

    let replacement = replacing.await.unwrap().unwrap();
    assert!(matches!(
        old.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::Replaced
        }
    ));
    assert!(matches!(
        replacement.join().await,
        TaskTerminal::Completed(7)
    ));
}

#[tokio::test]
async fn overlap_replace_keeps_new_generation_when_old_finishes_late() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(4, 2).unwrap())
        .build()
        .unwrap();
    let old_release = Arc::new(Notify::new());
    let old_release_for_job = Arc::clone(&old_release);
    let old = scheduler
        .start_if_absent(
            "rolling",
            Job::once(move |_| async move {
                old_release_for_job.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let old_generation = old.observer().snapshot().key_generation().unwrap();

    let new_release = Arc::new(Notify::new());
    let new_release_for_job = Arc::clone(&new_release);
    let replacement = scheduler
        .replace(
            "rolling",
            Job::once(move |_| async move {
                new_release_for_job.notified().await;
                Ok::<_, ()>(())
            }),
            ReplacePolicy::allow_overlap(),
        )
        .await
        .unwrap();
    while replacement.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let new_generation = replacement.observer().snapshot().key_generation().unwrap();
    assert!(new_generation > old_generation);

    old_release.notify_one();
    assert!(matches!(old.join().await, TaskTerminal::Cancelled { .. }));
    let status = scheduler.key_status("rolling").unwrap();
    assert_eq!(status.generation(), new_generation);
    assert_eq!(status.run_id(), Some(replacement.id()));

    new_release.notify_one();
    assert!(matches!(
        replacement.join().await,
        TaskTerminal::Completed(())
    ));
}

#[tokio::test(start_paused = true)]
async fn replace_timeout_rolls_back_reservation_and_returns_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    let release = Arc::new(Notify::new());
    let release_for_old = Arc::clone(&release);
    let old = scheduler
        .start_if_absent(
            "timeout",
            Job::once(move |_| async move {
                release_for_old.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let old_generation = old.observer().snapshot().key_generation().unwrap();
    let scheduler_for_replace = scheduler.clone();
    let replacing = tokio::spawn(async move {
        scheduler_for_replace
            .replace(
                "timeout",
                Job::once(|_| async { Ok::<_, ()>(()) }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(5),
                    ReplaceTimeoutAction::Fail,
                ),
            )
            .await
    });
    while !scheduler
        .key_status("timeout")
        .is_some_and(|status| status.is_replacing())
    {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;

    let returned = match replacing.await.unwrap() {
        Err(ReplaceError::TimedOut { job, previous }) => {
            assert_eq!(previous.generation(), old_generation);
            job
        }
        _ => panic!("replace must return the unsubmitted job on timeout"),
    };
    assert_eq!(
        scheduler.key_status("timeout").unwrap().generation(),
        old_generation
    );
    drop(returned);
    release.notify_one();
    assert!(matches!(old.join().await, TaskTerminal::Cancelled { .. }));
}

#[tokio::test(start_paused = true)]
async fn replace_can_escalate_to_confirmed_abort() {
    let scheduler = Scheduler::builder().build().unwrap();
    let old = scheduler
        .start_if_absent(
            "abort",
            Job::once(|_| std::future::pending::<Result<(), ()>>()),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let scheduler_for_replace = scheduler.clone();
    let replacing = tokio::spawn(async move {
        scheduler_for_replace
            .replace(
                "abort",
                Job::once(|_| async { Ok::<_, ()>(11_u8) }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(2),
                    ReplaceTimeoutAction::Abort {
                        confirmation_timeout: Duration::from_secs(2),
                    },
                ),
            )
            .await
    });
    while !scheduler
        .key_status("abort")
        .is_some_and(|status| status.is_replacing())
    {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;

    let replacement = replacing.await.unwrap().unwrap();
    assert!(matches!(old.join().await, TaskTerminal::Aborted));
    assert!(matches!(
        replacement.join().await,
        TaskTerminal::Completed(11)
    ));
}

#[tokio::test]
async fn stop_supersedes_replace_reservation() {
    let scheduler = Scheduler::builder().build().unwrap();
    let release = Arc::new(Notify::new());
    let release_for_old = Arc::clone(&release);
    let old = scheduler
        .start_if_absent(
            "race",
            Job::once(move |_| async move {
                release_for_old.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let scheduler_for_replace = scheduler.clone();
    let replacing = tokio::spawn(async move {
        scheduler_for_replace
            .replace(
                "race",
                Job::once(|_| async { Ok::<_, ()>(()) }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(1),
                    ReplaceTimeoutAction::ContinueWait,
                ),
            )
            .await
    });
    while !scheduler
        .key_status("race")
        .is_some_and(|status| status.is_replacing())
    {
        tokio::task::yield_now().await;
    }
    assert!(scheduler.stop("race"));
    release.notify_one();

    assert!(matches!(
        replacing.await.unwrap(),
        Err(ReplaceError::Superseded(_))
    ));
    assert!(matches!(old.join().await, TaskTerminal::Cancelled { .. }));
    assert_eq!(scheduler.key_status("race"), None);
}

#[tokio::test]
async fn self_replace_requires_overlap_and_overlap_does_not_self_join() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(4, 2).unwrap())
        .build()
        .unwrap();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let current = scheduler
        .start_if_absent(
            "self",
            Job::once(move |context| async move {
                let result = context
                    .replace_in_scheduler(
                        "self",
                        Job::once(|_| async { Ok::<_, ()>(()) }),
                        ReplacePolicy::after_confirmed_stop(
                            Duration::from_secs(1),
                            ReplaceTimeoutAction::Fail,
                        ),
                    )
                    .await;
                assert!(result_tx.send(result).is_ok());
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    assert!(matches!(
        result_rx.await.unwrap(),
        Err(ReplaceError::SelfReplacementRequiresOverlap(_))
    ));
    assert!(matches!(current.join().await, TaskTerminal::Completed(())));

    let (next_tx, next_rx) = tokio::sync::oneshot::channel();
    let current = scheduler
        .start_if_absent(
            "self-overlap",
            Job::once(move |context| async move {
                let next = context
                    .replace_in_scheduler(
                        "self-overlap",
                        Job::once(|_| async { Ok::<_, ()>(19_u8) }),
                        ReplacePolicy::allow_overlap(),
                    )
                    .await
                    .unwrap();
                assert!(next_tx.send(next).is_ok());
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    let next = next_rx.await.unwrap();
    assert!(matches!(
        current.join().await,
        TaskTerminal::Cancelled { .. }
    ));
    assert!(matches!(next.join().await, TaskTerminal::Completed(19)));
}

#[tokio::test]
async fn public_replace_detects_current_task_without_context_helper() {
    let scheduler = Scheduler::builder().build().unwrap();
    let scheduler_for_job = scheduler.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let current = scheduler
        .start_if_absent(
            "captured-self",
            Job::once(move |_| async move {
                let result = scheduler_for_job
                    .replace(
                        "captured-self",
                        Job::once(|_| async { Ok::<_, ()>(()) }),
                        ReplacePolicy::after_confirmed_stop(
                            Duration::from_secs(30),
                            ReplaceTimeoutAction::Fail,
                        ),
                    )
                    .await;
                assert!(result_tx.send(result).is_ok());
                Ok::<_, ()>(())
            }),
        )
        .unwrap();

    assert!(matches!(
        result_rx.await.unwrap(),
        Err(ReplaceError::SelfReplacementRequiresOverlap(_))
    ));
    assert!(matches!(current.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn concurrent_replace_reservation_has_one_winner_at_a_time() {
    let scheduler = Scheduler::builder().build().unwrap();
    let release = Arc::new(Notify::new());
    let release_for_old = Arc::clone(&release);
    let old = scheduler
        .start_if_absent(
            "contended",
            Job::once(move |_| async move {
                release_for_old.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let scheduler_for_first = scheduler.clone();
    let first = tokio::spawn(async move {
        scheduler_for_first
            .replace(
                "contended",
                Job::once(|_| async { Ok::<_, ()>(1_u8) }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(1),
                    ReplaceTimeoutAction::ContinueWait,
                ),
            )
            .await
    });
    while !scheduler
        .key_status("contended")
        .is_some_and(|status| status.is_replacing())
    {
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        scheduler
            .replace(
                "contended",
                Job::once(|_| async { Ok::<_, ()>(2_u8) }),
                ReplacePolicy::allow_overlap(),
            )
            .await,
        Err(ReplaceError::Busy { .. })
    ));
    release.notify_one();
    let winner = first.await.unwrap().unwrap();
    assert!(matches!(old.join().await, TaskTerminal::Cancelled { .. }));
    assert!(matches!(winner.join().await, TaskTerminal::Completed(1)));
}

#[tokio::test]
async fn group_shutdown_prevents_reserved_replace_from_committing() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("replace-scope").unwrap();
    let release = Arc::new(Notify::new());
    let release_for_old = Arc::clone(&release);
    let old = group
        .start_if_absent(
            "member",
            Job::once(move |_| async move {
                release_for_old.notified().await;
                Ok::<_, ()>(())
            }),
        )
        .unwrap();
    while old.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let group_for_replace = group.clone();
    let replacing = tokio::spawn(async move {
        group_for_replace
            .replace(
                "member",
                Job::once(|_| async { Ok::<_, ()>(()) }),
                ReplacePolicy::after_confirmed_stop(
                    Duration::from_secs(1),
                    ReplaceTimeoutAction::ContinueWait,
                ),
            )
            .await
    });
    while !group
        .key_status("member")
        .is_some_and(|status| status.is_replacing())
    {
        tokio::task::yield_now().await;
    }
    let group_for_shutdown = group.clone();
    let shutdown = tokio::spawn(async move { group_for_shutdown.shutdown().await });
    tokio::task::yield_now().await;
    release.notify_one();

    assert!(matches!(
        replacing.await.unwrap(),
        Err(ReplaceError::Submit(busybeaver::TrySubmitError::Closed(_)))
    ));
    assert!(matches!(old.join().await, TaskTerminal::Cancelled { .. }));
    assert!(shutdown.await.unwrap().is_complete());
    assert_eq!(group.key_status("member"), None);
}
