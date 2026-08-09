use busybeaver::{
    Backpressure, CancelReason, FirstRun, Job, LaneConfig, RetryExhaustedAction, RetryPolicy,
    Schedule, ScheduleError, ScheduleTime, ScheduledJob, Scheduler, TaskControl, TaskState,
    TaskTerminal, TrySubmitError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn explicit_first_delay_waits_without_execution_permit() {
    let schedule = Schedule::fixed_delay(
        Duration::from_secs(20),
        FirstRun::After(Duration::from_secs(10)),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let scheduled = ScheduledJob::new(schedule, move |_| {
        calls_for_job.fetch_add(1, Ordering::SeqCst);
        async { Ok::<_, ()>(TaskControl::Complete(())) }
    });
    let scheduler = Scheduler::builder().global_concurrency(1).build().unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    let ordinary = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(ordinary.join().await, TaskTerminal::Completed(())));
    tokio::time::advance(Duration::from_secs(9)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
}

#[tokio::test(start_paused = true)]
async fn schedule_wait_releases_permit_and_responds_to_cancel() {
    let schedule = Schedule::fixed_delay(Duration::from_secs(60), FirstRun::Immediate).unwrap();
    let scheduled = ScheduledJob::new(schedule, |_| async {
        Ok::<_, ()>(TaskControl::<()>::Continue)
    });
    let scheduler = Scheduler::builder()
        .global_concurrency(1)
        .default_lane(busybeaver::LaneConfig::new(2, 1).unwrap())
        .build()
        .unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    let controller = handle.controller();
    while controller.state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    let ordinary = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(ordinary.join().await, TaskTerminal::Completed(())));

    assert!(controller.cancel(CancelReason::User));
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::User
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn schedule_run_index_and_retry_attempt_are_independent() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_job = Arc::clone(&seen);
    let schedule = Schedule::fixed_delay(Duration::from_secs(5), FirstRun::Immediate).unwrap();
    let retry = RetryPolicy::builder(2).build().unwrap();
    let scheduled = ScheduledJob::new(schedule, move |context| {
        seen_for_job
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((context.run_index(), context.attempt()));
        async move {
            match (context.run_index(), context.attempt()) {
                (0, 1) => Err("retry"),
                (0, 2) => Ok(TaskControl::Continue),
                (1, 1) => Ok(TaskControl::Complete(17_u8)),
                _ => panic!("unexpected run/attempt"),
            }
        }
    })
    .retry(retry);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;

    assert!(matches!(handle.join().await, TaskTerminal::Completed(17)));
    assert_eq!(
        *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![(0, 1), (0, 2), (1, 1)]
    );
}

#[tokio::test(start_paused = true)]
async fn retry_exhaustion_can_explicitly_continue_next_scheduled_run() {
    let schedule = Schedule::fixed_delay(Duration::from_secs(3), FirstRun::Immediate).unwrap();
    let retry = RetryPolicy::builder(1).build().unwrap();
    let scheduled = ScheduledJob::new(schedule, |context| async move {
        if context.run_index() == 0 {
            Err("exhausted")
        } else {
            Ok(TaskControl::Complete(5_u8))
        }
    })
    .retry(retry)
    .retry_exhausted_action(RetryExhaustedAction::ContinueSchedule);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(3)).await;

    assert!(matches!(handle.join().await, TaskTerminal::Completed(5)));
}

#[tokio::test(start_paused = true)]
async fn dynamic_schedule_honors_relative_continue_at() {
    let schedule = Schedule::dynamic(FirstRun::Immediate).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let scheduled = ScheduledJob::new(schedule, move |_| {
        let call = calls_for_job.fetch_add(1, Ordering::SeqCst);
        async move {
            if call == 0 {
                Ok::<_, ()>(TaskControl::ContinueAt(ScheduleTime::after_start(
                    Duration::from_secs(8),
                )))
            } else {
                Ok(TaskControl::Complete(()))
            }
        }
    });
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(7)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
}

#[tokio::test(start_paused = true)]
async fn recurring_run_reenters_lane_pending_capacity_before_execution() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(
            LaneConfig::new(1, 1)
                .unwrap()
                .backpressure(Backpressure::Reject),
        )
        .build()
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let scheduled = scheduler
        .submit(
            ScheduledJob::new(
                Schedule::fixed_delay(Duration::from_secs(5), FirstRun::Immediate).unwrap(),
                {
                    let calls = Arc::clone(&calls);
                    move |_| {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if call == 0 {
                                Ok::<_, ()>(TaskControl::Continue)
                            } else {
                                Ok(TaskControl::Complete(()))
                            }
                        }
                    }
                },
            )
            .instantiate(),
        )
        .await
        .unwrap();
    while scheduled.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    let blocker_started = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker = scheduler
        .submit(Job::once({
            let started = Arc::clone(&blocker_started);
            let release = Arc::clone(&blocker_release);
            move |_| async move {
                started.notify_one();
                release.notified().await;
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    blocker_started.notified().await;

    tokio::time::advance(Duration::from_secs(5)).await;
    while scheduled.observer().state() == TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let rejected = scheduler.try_submit(Job::once(|_| async { Ok::<_, ()>(()) }));
    let unexpected = match rejected {
        Err(TrySubmitError::Full(_)) => None,
        Ok(handle) => Some(handle),
        Err(error) => panic!("unexpected submission result: {error:?}"),
    };

    if let Some(handle) = unexpected {
        handle.controller().cancel(CancelReason::User);
        blocker_release.notify_one();
        scheduled.controller().cancel(CancelReason::User);
        let _ = handle.join().await;
        let _ = blocker.join().await;
        let _ = scheduled.join().await;
        panic!("a recurring run bypassed the configured pending capacity");
    }

    scheduled.controller().cancel(CancelReason::User);
    blocker_release.notify_one();
    let _ = blocker.join().await;
    let _ = scheduled.join().await;
}

#[tokio::test]
async fn finite_sequence_exhaustion_is_a_typed_terminal() {
    let schedule = Schedule::sequence(vec![Duration::ZERO], false, FirstRun::Immediate).unwrap();
    let scheduled = ScheduledJob::new(schedule, |_| async {
        Ok::<_, ()>(TaskControl::<()>::Continue)
    });
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();

    assert!(matches!(
        handle.join().await,
        TaskTerminal::ScheduleError(ScheduleError::SequenceExhausted)
    ));
}

#[tokio::test]
async fn invalid_dynamic_control_and_overflow_are_typed_terminals() {
    let scheduler = Scheduler::builder().build().unwrap();
    let dynamic = Schedule::dynamic(FirstRun::Immediate).unwrap();
    let missing_delay = scheduler
        .submit(
            ScheduledJob::new(dynamic.clone(), |_| async {
                Ok::<_, ()>(TaskControl::<()>::Continue)
            })
            .instantiate(),
        )
        .await
        .unwrap();
    assert!(matches!(
        missing_delay.join().await,
        TaskTerminal::ScheduleError(ScheduleError::DynamicControlRequired)
    ));

    let overflow = scheduler
        .submit(
            ScheduledJob::new(dynamic, |_| async {
                Ok::<_, ()>(TaskControl::<()>::ContinueAfter(Duration::MAX))
            })
            .instantiate(),
        )
        .await
        .unwrap();
    assert!(matches!(
        overflow.join().await,
        TaskTerminal::ScheduleError(ScheduleError::DurationOverflow)
    ));
}
