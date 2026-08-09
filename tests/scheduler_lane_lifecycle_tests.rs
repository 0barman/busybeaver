use busybeaver::{
    Backpressure, CancelReason, Job, LaneConfig, LaneError, Scheduler, TaskState, TaskTerminal,
    TrySubmitError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

fn update_max(maximum: &AtomicUsize, value: usize) {
    let _ = maximum.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        (value > current).then_some(value)
    });
}

#[tokio::test]
async fn cancel_on_drop_guard_is_explicit_and_armed_until_join() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();

    drop(handle.cancel_on_drop(CancelReason::User));

    let snapshot = observer.wait_terminal().await;
    assert_eq!(snapshot.state(), TaskState::Cancelled);
    assert_eq!(snapshot.cancel_reason(), Some(CancelReason::User));

    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(7_u8) }))
        .await
        .unwrap();
    assert!(matches!(
        handle.cancel_on_drop(CancelReason::User).join().await,
        TaskTerminal::Completed(7)
    ));
}

#[tokio::test]
async fn disarming_cancel_on_drop_restores_detach_semantics() {
    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let handle = scheduler
        .submit(Job::once(move |_| async move {
            finish_for_job.notified().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();

    drop(handle.cancel_on_drop(CancelReason::User).disarm());
    finish.notify_one();

    assert_eq!(observer.wait_terminal().await.state(), TaskState::Completed);
}

#[tokio::test]
async fn dropped_result_receiver_releases_large_output_after_completion() {
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let scheduler = Scheduler::builder().build().unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let drops = Arc::new(AtomicUsize::new(0));
    let drops_for_job = Arc::clone(&drops);
    let handle = scheduler
        .submit(Job::once(move |_| async move {
            finish_for_job.notified().await;
            Ok::<_, ()>((vec![0_u8; 1024 * 1024], DropProbe(drops_for_job)))
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    drop(handle);

    finish.notify_one();
    assert_eq!(observer.wait_terminal().await.state(), TaskState::Completed);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lane_close_delete_and_recreate_use_distinct_generations() {
    let scheduler = Scheduler::builder().build().unwrap();
    let config = LaneConfig::new(2, 1)
        .unwrap()
        .backpressure(Backpressure::Wait);
    scheduler.ensure_lane("batch", config.clone()).unwrap();
    let first_generation = scheduler.lane_generation("batch").unwrap();

    let finish = Arc::new(Notify::new());
    let finish_for_job = Arc::clone(&finish);
    let first = scheduler
        .submit(
            Job::once(move |_| async move {
                finish_for_job.notified().await;
                Ok::<_, ()>(())
            })
            .on_lane("batch"),
        )
        .await
        .unwrap();
    let first_snapshot = first.observer().snapshot();
    assert_eq!(first_snapshot.lane(), "batch");
    assert_eq!(first_snapshot.lane_generation(), first_generation);

    assert_eq!(scheduler.close_lane("batch").unwrap(), 1);
    let returned =
        match scheduler.try_submit(Job::once(|_| async { Ok::<_, ()>(()) }).on_lane("batch")) {
            Err(TrySubmitError::Closed(job)) => job,
            _ => panic!("closed lane must reject and return the job"),
        };
    assert!(matches!(
        scheduler.delete_lane("batch"),
        Err(LaneError::Busy { .. })
    ));

    finish.notify_one();
    assert!(matches!(
        first.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::LaneClosed
        }
    ));
    assert_eq!(scheduler.delete_lane("batch").unwrap(), first_generation);

    scheduler.ensure_lane("batch", config).unwrap();
    let second_generation = scheduler.lane_generation("batch").unwrap();
    assert!(second_generation > first_generation);
    let second = scheduler.try_submit(returned).unwrap();
    assert_eq!(
        second.observer().snapshot().lane_generation(),
        second_generation
    );
    assert!(matches!(second.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn closing_lane_wakes_waiting_submit_and_returns_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    scheduler
        .ensure_lane("limited", LaneConfig::new(1, 1).unwrap())
        .unwrap();
    let finish = Arc::new(Notify::new());
    let finish_for_first = Arc::clone(&finish);
    let first = scheduler
        .submit(
            Job::once(move |_| async move {
                finish_for_first.notified().await;
                Ok::<_, ()>(())
            })
            .on_lane("limited"),
        )
        .await
        .unwrap();
    while first.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let second = scheduler
        .submit(
            Job::once(|context| async move {
                context.cancelled().await;
                Ok::<_, ()>(())
            })
            .on_lane("limited"),
        )
        .await
        .unwrap();

    let scheduler_for_submit = scheduler.clone();
    let waiting = tokio::spawn(async move {
        scheduler_for_submit
            .submit(Job::once(|_| async { Ok::<_, ()>(()) }).on_lane("limited"))
            .await
    });
    tokio::task::yield_now().await;

    assert_eq!(scheduler.close_lane("limited").unwrap(), 2);
    assert!(matches!(
        waiting.await.unwrap(),
        Err(TrySubmitError::Closed(_))
    ));
    assert!(matches!(
        second.join().await,
        TaskTerminal::Cancelled { .. }
    ));
    finish.notify_one();
    assert!(matches!(first.join().await, TaskTerminal::Cancelled { .. }));
}

#[tokio::test]
async fn lane_lifecycle_errors_are_typed() {
    let scheduler = Scheduler::builder().build().unwrap();
    assert!(matches!(
        scheduler.close_lane("missing"),
        Err(LaneError::NotFound { .. })
    ));
    assert!(matches!(
        scheduler.delete_lane("default"),
        Err(LaneError::StillOpen { .. })
    ));
    scheduler.close_lane("default").unwrap();
    assert!(matches!(
        scheduler.ensure_lane("default", LaneConfig::default()),
        Err(LaneError::Closed { .. })
    ));
}

#[tokio::test]
async fn lane_and_global_concurrency_limits_are_never_exceeded() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(8, 4).unwrap())
        .build()
        .unwrap();
    scheduler
        .ensure_lane("other", LaneConfig::new(8, 4).unwrap())
        .unwrap();
    let running = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut handles = Vec::new();

    for index in 0..6 {
        let running = Arc::clone(&running);
        let maximum = Arc::clone(&maximum);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let lane = if index % 2 == 0 { "default" } else { "other" };
        handles.push(
            scheduler
                .submit(
                    Job::once(move |_| async move {
                        let current = running.fetch_add(1, Ordering::SeqCst) + 1;
                        update_max(&maximum, current);
                        started.notify_one();
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        running.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, ()>(())
                    })
                    .on_lane(lane),
                )
                .await
                .unwrap(),
        );
    }

    while maximum.load(Ordering::SeqCst) < 2 {
        started.notified().await;
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    release.add_permits(handles.len());
    for handle in handles {
        assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    }
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn lane_waiter_does_not_hold_global_capacity_needed_by_another_lane() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(4, 1).unwrap())
        .build()
        .unwrap();
    scheduler
        .ensure_lane("other", LaneConfig::new(4, 1).unwrap())
        .unwrap();

    let first_started = Arc::new(Notify::new());
    let first_release = Arc::new(Notify::new());
    let first = scheduler
        .submit(Job::once({
            let started = Arc::clone(&first_started);
            let release = Arc::clone(&first_release);
            move |_| async move {
                started.notify_one();
                release.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    first_started.notified().await;

    let same_lane = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    while same_lane.observer().state() != TaskState::WaitingForPermit {
        tokio::task::yield_now().await;
    }
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    let other_started = Arc::new(Notify::new());
    let other = scheduler
        .submit(
            Job::once({
                let started = Arc::clone(&other_started);
                move |_| async move {
                    started.notify_one();
                    Ok::<_, ()>(())
                }
            })
            .on_lane("other"),
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), other_started.notified())
        .await
        .expect(
            "a waiter on one lane must not consume global capacity while waiting for that lane",
        );
    first_release.notify_one();

    assert!(matches!(first.join().await, TaskTerminal::Completed(())));
    assert!(matches!(
        same_lane.join().await,
        TaskTerminal::Completed(())
    ));
    assert!(matches!(other.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn panic_and_cooperative_cancel_release_execution_permits() {
    let scheduler = Scheduler::builder()
        .global_concurrency(1)
        .default_lane(LaneConfig::new(2, 1).unwrap())
        .build()
        .unwrap();
    let panicked = scheduler
        .submit(Job::<(), ()>::once(|_| async move {
            panic!("intentional permit release test");
        }))
        .await
        .unwrap();
    assert!(matches!(panicked.join().await, TaskTerminal::Panicked(_)));

    let cancelled = scheduler
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let controller = cancelled.controller();
    while controller.state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    assert!(controller.cancel(CancelReason::User));
    assert!(matches!(
        cancelled.join().await,
        TaskTerminal::Cancelled { .. }
    ));

    let final_run = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(
        final_run.join().await,
        TaskTerminal::Completed(())
    ));
}

#[tokio::test]
async fn retained_context_and_dropped_observers_do_not_control_run_lifetime() {
    let scheduler = Scheduler::builder().build().unwrap();
    let (context_tx, context_rx) = tokio::sync::oneshot::channel();
    let handle = scheduler
        .submit(Job::once(move |context| async move {
            assert!(context_tx.send(context.clone()).is_ok());
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    let observer = handle.observer();
    let controller = handle.controller();
    let retained_context = context_rx.await.unwrap();
    drop(observer);
    drop(controller);

    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(scheduler.active_task_count(), 0);
    assert_eq!(retained_context.cancellation_reason(), None);
}
