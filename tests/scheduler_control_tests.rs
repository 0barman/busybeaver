use busybeaver::{
    Backoff, CancelReason, FirstRun, Job, RetryPolicy, ReusableJob, Schedule, ScheduledJob,
    Scheduler, TaskCommandError, TaskControl, TaskState, TaskTerminal, TimeoutScope,
    TriggerOutcome, TriggerPolicy, TriggerPolicyError,
};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test(start_paused = true)]
async fn pause_releases_schedule_wait_and_overdue_resume_runs_immediately() {
    let schedule = Schedule::fixed_delay(
        Duration::from_secs(60),
        FirstRun::After(Duration::from_secs(10)),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let job = ScheduledJob::new(schedule, move |_| {
        let calls = Arc::clone(&calls_for_job);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(TaskControl::Complete(()))
        }
    });
    let scheduler = Scheduler::builder().global_concurrency(1).build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    let controller = handle.controller();
    while controller.state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    assert_eq!(controller.pause(), Ok(true));
    while controller.state() != TaskState::Paused {
        tokio::task::yield_now().await;
    }
    assert_eq!(controller.run_now(), Ok(TriggerOutcome::Paused));
    let ordinary = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(ordinary.join().await, TaskTerminal::Completed(())));
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    assert_eq!(controller.resume(), Ok(true));
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn pause_does_not_freeze_retry_total_elapsed_budget() {
    let policy = RetryPolicy::builder(3)
        .backoff(Backoff::fixed(Duration::from_secs(60)))
        .max_elapsed(Duration::from_secs(5))
        .build()
        .unwrap();
    let job = ReusableJob::new(|_| async { Err::<(), _>("last") }).retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    let controller = handle.controller();
    while controller.state() != TaskState::WaitingForRetry {
        tokio::task::yield_now().await;
    }
    assert_eq!(controller.pause(), Ok(true));
    while controller.state() != TaskState::Paused {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        handle.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::TotalElapsed,
            attempts: 1,
            last_error: Some("last")
        }
    ));
}

#[tokio::test]
async fn coalesce_one_bounds_pending_run_now_triggers() {
    let schedule = Schedule::fixed_delay(Duration::from_secs(60), FirstRun::Immediate).unwrap();
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let job = ScheduledJob::new(schedule, {
        let started = Arc::clone(&started);
        let finish = Arc::clone(&finish);
        let calls = Arc::clone(&calls);
        move |_| {
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    started.notify_one();
                    finish.notified().await;
                }
                Ok::<_, ()>(if call == 2 {
                    TaskControl::Complete(())
                } else {
                    TaskControl::Continue
                })
            }
        }
    })
    .trigger_policy(TriggerPolicy::CoalesceOne);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    let controller = handle.controller();
    started.notified().await;

    assert_eq!(controller.run_now(), Ok(TriggerOutcome::Scheduled));
    assert_eq!(controller.run_now(), Ok(TriggerOutcome::Coalesced));
    assert_eq!(controller.run_now(), Ok(TriggerOutcome::Coalesced));
    finish.notify_one();

    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn queue_all_has_a_hard_pending_trigger_limit() {
    let schedule = Schedule::fixed_delay(Duration::from_secs(60), FirstRun::Immediate).unwrap();
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let job = ScheduledJob::new(schedule, {
        let started = Arc::clone(&started);
        let finish = Arc::clone(&finish);
        let calls = Arc::clone(&calls);
        move |_| {
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    started.notify_one();
                    finish.notified().await;
                }
                Ok::<_, ()>(if call == 3 {
                    TaskControl::Complete(())
                } else {
                    TaskControl::Continue
                })
            }
        }
    })
    .trigger_policy(TriggerPolicy::QueueAll {
        capacity: NonZeroUsize::new(2).unwrap(),
    });
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    let controller = handle.controller();
    started.notified().await;

    assert_eq!(
        controller.run_now(),
        Ok(TriggerOutcome::Queued { pending: 1 })
    );
    assert_eq!(
        controller.run_now(),
        Ok(TriggerOutcome::Queued { pending: 2 })
    );
    assert_eq!(controller.run_now(), Ok(TriggerOutcome::Dropped));
    finish.notify_one();

    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn run_now_drop_and_replace_policies_are_explicit() {
    for (policy, second) in [
        (TriggerPolicy::Drop, TriggerOutcome::Dropped),
        (TriggerPolicy::Replace, TriggerOutcome::Replaced),
    ] {
        let schedule = Schedule::fixed_delay(Duration::from_secs(60), FirstRun::Immediate).unwrap();
        let job = ScheduledJob::new(schedule, |_| async {
            Ok::<_, ()>(TaskControl::<()>::Continue)
        })
        .trigger_policy(policy);
        let scheduler = Scheduler::builder().build().unwrap();
        let handle = scheduler.submit(job.instantiate()).await.unwrap();
        let controller = handle.controller();
        while controller.state() != TaskState::WaitingForSchedule {
            tokio::task::yield_now().await;
        }

        assert_eq!(controller.run_now(), Ok(TriggerOutcome::Scheduled));
        assert_eq!(controller.run_now(), Ok(second));
        assert!(controller.cancel(CancelReason::User));
        assert!(matches!(
            handle.join().await,
            TaskTerminal::Cancelled { .. }
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn run_now_racing_natural_deadline_does_not_create_an_extra_run() {
    let schedule = Schedule::fixed_delay(
        Duration::from_secs(60),
        FirstRun::After(Duration::from_secs(10)),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let job = ScheduledJob::new(schedule, move |_| {
        let calls = Arc::clone(&calls_for_job);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(TaskControl::<()>::Continue)
        }
    })
    .trigger_policy(TriggerPolicy::Drop);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    let controller = handle.controller();
    while controller.state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(10)).await;
    let _ = controller.run_now();
    while calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    while controller.state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(controller.cancel(CancelReason::User));
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled { .. }
    ));
}

#[tokio::test]
async fn paused_group_holds_new_members_until_resume() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("paused").unwrap();
    assert_eq!(group.pause(), 0);
    assert!(group.is_paused());
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let handle = group
        .submit(Job::once(move |_| {
            let calls = Arc::clone(&calls_for_job);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            }
        }))
        .await
        .unwrap();
    while handle.observer().state() != TaskState::Paused {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        handle.controller().resume(),
        Err(TaskCommandError::ScopePaused)
    );

    assert_eq!(group.resume(), 1);
    assert!(!group.is_paused());
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn group_resume_preserves_an_existing_task_level_pause() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("layered-pause").unwrap();
    let handle = group
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    let controller = handle.controller();

    assert_eq!(controller.pause(), Ok(true));
    assert_eq!(group.pause(), 1);
    while controller.state() != TaskState::Paused {
        tokio::task::yield_now().await;
    }

    assert_eq!(group.resume(), 1);
    assert!(controller.is_paused());
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(controller.state(), TaskState::Paused);

    assert_eq!(controller.resume(), Ok(true));
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn cancel_and_shutdown_wake_paused_tasks() {
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    let controller = handle.controller();
    assert_eq!(controller.pause(), Ok(true));
    while controller.state() != TaskState::Paused {
        tokio::task::yield_now().await;
    }
    assert!(controller.cancel(CancelReason::User));
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled { .. }
    ));

    let second = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert_eq!(second.controller().pause(), Ok(true));
    let report = scheduler.shutdown().await;
    assert!(report.is_complete());
    assert!(matches!(
        second.join().await,
        TaskTerminal::Cancelled { .. }
    ));
}

#[tokio::test]
async fn terminal_and_non_scheduled_command_errors_are_typed() {
    assert_eq!(
        TriggerPolicy::queue_all(0),
        Err(TriggerPolicyError::ZeroCapacity)
    );
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    let controller = handle.controller();
    assert_eq!(controller.run_now(), Err(TaskCommandError::NotScheduled));
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    assert_eq!(controller.pause(), Err(TaskCommandError::Terminal));
    assert_eq!(controller.resume(), Err(TaskCommandError::Terminal));
}
