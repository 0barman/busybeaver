use busybeaver::{
    Backoff, CancelReason, FirstRun, JitterSource, RetryPolicy, ReusableJob, Schedule,
    ScheduledJob, Scheduler, TaskControl, TaskState, TaskTerminal, TimeoutScope,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::test]
async fn retry_attempts_start_at_one_and_exhaustion_preserves_last_error() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let attempts_for_job = Arc::clone(&attempts);
    let policy = RetryPolicy::builder(3).build().unwrap();
    let job = ReusableJob::new(move |context| {
        let attempts = Arc::clone(&attempts_for_job);
        async move {
            attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context.attempt());
            Err::<(), _>(format!("error-{}", context.attempt()))
        }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();

    match scheduler
        .submit(job.instantiate())
        .await
        .unwrap()
        .join()
        .await
    {
        TaskTerminal::RetriesExhausted {
            attempts,
            last_error,
        } => {
            assert_eq!(attempts, 3);
            assert_eq!(last_error, "error-3");
        }
        other => panic!("unexpected terminal: {other:?}"),
    }
    assert_eq!(
        *attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1, 2, 3]
    );
}

#[tokio::test]
async fn retry_success_and_non_retryable_failure_are_distinct() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let retry_all = RetryPolicy::builder(3).build().unwrap();
    let succeeds = ReusableJob::new(move |_| {
        let call = calls_for_job.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if call < 2 {
                Err("transient")
            } else {
                Ok(9_u8)
            }
        }
    })
    .retry(retry_all);
    let scheduler = Scheduler::builder().build().unwrap();
    assert!(matches!(
        scheduler
            .submit(succeeds.instantiate())
            .await
            .unwrap()
            .join()
            .await,
        TaskTerminal::Completed(9)
    ));

    let retry_selected = RetryPolicy::builder(4)
        .retry_if(|error: &&str| *error == "retry")
        .build()
        .unwrap();
    let fails = ReusableJob::new(|_| async { Err::<(), _>("permanent") }).retry(retry_selected);
    assert!(matches!(
        scheduler
            .submit(fails.instantiate())
            .await
            .unwrap()
            .join()
            .await,
        TaskTerminal::Failed("permanent")
    ));
}

#[tokio::test(start_paused = true)]
async fn retry_backoff_releases_permit_and_is_cancellation_aware() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_job = Arc::clone(&attempts);
    let policy = RetryPolicy::builder(2)
        .backoff(Backoff::fixed(Duration::from_secs(60)))
        .build()
        .unwrap();
    let retrying = ReusableJob::new(move |_| {
        attempts_for_job.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>("retry") }
    })
    .retry(policy);
    let scheduler = Scheduler::builder()
        .global_concurrency(1)
        .default_lane(busybeaver::LaneConfig::new(2, 1).unwrap())
        .build()
        .unwrap();
    let handle = scheduler.submit(retrying.instantiate()).await.unwrap();
    let controller = handle.controller();
    while controller.state() != TaskState::WaitingForRetry {
        tokio::task::yield_now().await;
    }

    let ordinary = scheduler
        .submit(busybeaver::Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(ordinary.join().await, TaskTerminal::Completed(())));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    assert!(controller.cancel(CancelReason::User));
    assert!(matches!(
        handle.join().await,
        TaskTerminal::Cancelled {
            reason: CancelReason::User
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn retry_fixed_backoff_uses_scheduler_clock() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_job = Arc::clone(&attempts);
    let policy = RetryPolicy::builder(2)
        .backoff(Backoff::fixed(Duration::from_secs(10)))
        .build()
        .unwrap();
    let job = ReusableJob::new(move |_| {
        attempts_for_job.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>("retry") }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForRetry {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;

    assert!(matches!(
        handle.join().await,
        TaskTerminal::RetriesExhausted { attempts: 2, .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn total_elapsed_and_attempt_timeout_have_distinct_typed_terminals() {
    let elapsed_policy = RetryPolicy::builder(3)
        .backoff(Backoff::fixed(Duration::from_secs(10)))
        .max_elapsed(Duration::from_secs(5))
        .build()
        .unwrap();
    let elapsed_job = ReusableJob::new(|_| async { Err::<(), _>("last") }).retry(elapsed_policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let elapsed = scheduler.submit(elapsed_job.instantiate()).await.unwrap();
    while elapsed.observer().state() != TaskState::WaitingForRetry {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;
    match elapsed.join().await {
        TaskTerminal::TimedOut {
            scope: TimeoutScope::TotalElapsed,
            attempts,
            last_error,
        } => {
            assert_eq!(attempts, 1);
            assert_eq!(last_error, Some("last"));
        }
        other => panic!("unexpected terminal: {other:?}"),
    }

    let attempt_policy = RetryPolicy::<()>::builder(2)
        .attempt_timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let timeout_job =
        ReusableJob::new(|_| std::future::pending::<Result<(), ()>>()).retry(attempt_policy);
    let timed = scheduler.submit(timeout_job.instantiate()).await.unwrap();
    while timed.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(matches!(
        timed.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::Attempt,
            attempts: 1,
            last_error: None
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn attempt_timeout_preserves_previous_retryable_error() {
    #[derive(Debug, Eq, PartialEq)]
    struct NonCloneError(&'static str);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let policy = RetryPolicy::builder(3)
        .attempt_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let job = ReusableJob::new(move |_| {
        let call = calls_for_job.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if call == 1 {
                Err(NonCloneError("first-error"))
            } else {
                std::future::pending::<Result<(), NonCloneError>>().await
            }
        }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(5)).await;

    assert!(matches!(
        handle.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::Attempt,
            attempts: 2,
            last_error: Some(NonCloneError("first-error"))
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn total_elapsed_during_later_attempt_preserves_previous_retryable_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let policy = RetryPolicy::builder(3)
        .max_elapsed(Duration::from_secs(5))
        .build()
        .unwrap();
    let job = ReusableJob::new(move |_| {
        let call = calls_for_job.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if call == 1 {
                Err("first-error")
            } else {
                std::future::pending::<Result<(), &'static str>>().await
            }
        }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(5)).await;

    assert!(matches!(
        handle.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::TotalElapsed,
            attempts: 2,
            last_error: Some("first-error")
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn completed_scheduled_invocation_clears_its_retry_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let policy = RetryPolicy::builder(2)
        .attempt_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let schedule = Schedule::fixed_delay(Duration::from_secs(1), FirstRun::Immediate).unwrap();
    let job = ScheduledJob::new(schedule, move |_| {
        let call = calls_for_job.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            match call {
                1 => Err("first-invocation-error"),
                2 => Ok(TaskControl::Continue),
                _ => std::future::pending::<Result<TaskControl<()>, &'static str>>().await,
            }
        }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    while handle.observer().state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(1)).await;
    while calls.load(Ordering::SeqCst) < 3 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;

    assert!(matches!(
        handle.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::Attempt,
            attempts: 1,
            last_error: None
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn total_elapsed_includes_execution_permit_queue_time() {
    let scheduler = Scheduler::builder()
        .global_concurrency(1)
        .default_lane(busybeaver::LaneConfig::new(2, 1).unwrap())
        .build()
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_job = Arc::clone(&release);
    let blocker = scheduler
        .submit(busybeaver::Job::once(move |_| async move {
            release_for_job.notified().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    while blocker.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_job = Arc::clone(&calls);
    let policy = RetryPolicy::<()>::builder(2)
        .max_elapsed(Duration::from_secs(5))
        .build()
        .unwrap();
    let waiting = scheduler
        .submit(
            ReusableJob::new(move |_| {
                calls_for_job.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(()) }
            })
            .retry(policy)
            .instantiate(),
        )
        .await
        .unwrap();
    while waiting.observer().state() != TaskState::WaitingForPermit {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;

    assert!(matches!(
        waiting.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::TotalElapsed,
            attempts: 0,
            last_error: None
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    release.notify_one();
    assert!(matches!(blocker.join().await, TaskTerminal::Completed(())));
}

#[tokio::test(start_paused = true)]
async fn attempt_timeout_drops_attempt_future_before_publishing_terminal() {
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let drops_for_job = Arc::clone(&drops);
    let policy = RetryPolicy::<()>::builder(2)
        .attempt_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(
            ReusableJob::new(move |_| {
                let guard = DropProbe(Arc::clone(&drops_for_job));
                async move {
                    let _guard = guard;
                    std::future::pending::<Result<(), ()>>().await
                }
            })
            .retry(policy)
            .instantiate(),
        )
        .await
        .unwrap();
    while handle.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(2)).await;

    assert!(matches!(
        handle.join().await,
        TaskTerminal::TimedOut {
            scope: TimeoutScope::Attempt,
            ..
        }
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_user_callback_panics_become_panicked_terminal() {
    let predicate = RetryPolicy::builder(2)
        .retry_if(|_: &&str| panic!("predicate panic"))
        .build()
        .unwrap();
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler
        .submit(
            ReusableJob::new(|_| async { Err::<(), _>("error") })
                .retry(predicate)
                .instantiate(),
        )
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Panicked(_)));

    let override_policy = RetryPolicy::builder(2)
        .delay_override(|_: &&str, _| panic!("override panic"))
        .build()
        .unwrap();
    let handle = scheduler
        .submit(
            ReusableJob::new(|_| async { Err::<(), _>("error") })
                .retry(override_policy)
                .instantiate(),
        )
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Panicked(_)));

    let survivor = scheduler
        .submit(busybeaver::Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(survivor.join().await, TaskTerminal::Completed(())));
}

#[tokio::test(start_paused = true)]
async fn injected_jitter_is_reproducible() {
    struct FixedJitter(Duration);

    impl JitterSource for FixedJitter {
        fn sample(&self, upper_bound: Duration) -> Duration {
            assert!(self.0 <= upper_bound);
            self.0
        }
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_job = Arc::clone(&attempts);
    let policy = RetryPolicy::builder(2)
        .backoff(Backoff::fixed(Duration::from_secs(3)))
        .jitter(Duration::from_secs(4), FixedJitter(Duration::from_secs(2)))
        .build()
        .unwrap();
    let job = ReusableJob::new(move |_| {
        attempts_for_job.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>(()) }
    })
    .retry(policy);
    let scheduler = Scheduler::builder().build().unwrap();
    let handle = scheduler.submit(job.instantiate()).await.unwrap();
    while handle.observer().state() != TaskState::WaitingForRetry {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        handle.join().await,
        TaskTerminal::RetriesExhausted { attempts: 2, .. }
    ));
}
