use busybeaver::{
    Beaver, CancelReason, Jitter, PanicPolicy, PanicSource, RecurringBuildError, RecurringBuilder,
    RecurringFailure, RestartPolicy, RetryBuilder, Schedule, TaskExit, TaskFailure, TickOutcome,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn recurring_schedule_validates_seeded_jitter() {
    let result = RecurringBuilder::<(), ()>::new(|_| async { Ok(TickOutcome::Stop(())) })
        .schedule(Schedule::fixed_delay(Duration::from_secs(1)).jitter(Jitter::seeded(7, f64::NAN)))
        .build();
    assert!(matches!(
        result,
        Err(RecurringBuildError::InvalidJitterRatio)
    ));
}

#[tokio::test(start_paused = true)]
async fn step_schedule_has_independent_initial_delay_and_repeats_last(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RecurringBuilder::new(move |_| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            let number = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Ok::<_, ()>(if number == 4 {
                TickOutcome::Stop(number)
            } else {
                TickOutcome::Continue
            })
        }
    })
    .schedule(
        Schedule::steps([
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_millis(1500),
        ])
        .repeat_last(),
    )
    .initial_delay(Duration::from_secs(10))
    .build()?;
    let mut handle = beaver.spawn_recurring(spec)?;

    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(500)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    tokio::time::advance(Duration::from_millis(1500)).await;
    tokio::task::yield_now().await;
    assert!(matches!(handle.join().await?, TaskExit::Completed(4)));
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test(start_paused = true)]
async fn recurring_cancel_wakes_schedule_and_prevents_an_extra_tick(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RecurringBuilder::new(move |_| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok::<TickOutcome<()>, ()>(TickOutcome::Continue)
        }
    })
    .schedule(Schedule::fixed_delay(Duration::from_secs(60)))
    .build()?;
    let handle = beaver.spawn_recurring(spec)?;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let exit = handle
        .control()
        .cancel_and_wait(CancelReason::UserRequested)
        .await;
    assert!(matches!(
        exit,
        busybeaver::TaskExitSummary::Cancelled { .. }
    ));
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test(start_paused = true)]
async fn panic_restart_policy_is_bounded_and_throttled() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RecurringBuilder::<(), ()>::new(move |_| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            panic!("tick panic")
        }
    })
    .schedule(Schedule::fixed_delay(Duration::ZERO))
    .panic_policy(PanicPolicy::Restart(
        RestartPolicy::new(2)
            .window(Duration::from_secs(10))
            .backoff(Duration::from_secs(1)),
    ))
    .build()?;
    let mut handle = beaver.spawn_recurring(spec)?;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    let exit = handle.join().await?;
    assert!(matches!(
        exit,
        TaskExit::Failed(TaskFailure::Recurring(
            RecurringFailure::RestartLimitExceeded
        ))
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test]
async fn dynamic_schedule_panic_is_classified_and_not_restarted(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let spec = RecurringBuilder::<(), ()>::new(|_| async { Ok::<_, ()>(TickOutcome::Continue) })
        .schedule(Schedule::dynamic(|_| panic!("schedule panic")))
        .build()?;
    let mut handle = beaver.spawn_recurring(spec)?;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Panicked {
            source: PanicSource::Schedule,
            ..
        }
    ));
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test(start_paused = true)]
async fn explicit_resume_notification_can_run_the_next_tick_immediately(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RecurringBuilder::new(move |_| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            let number = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Ok::<_, ()>(if number == 2 {
                TickOutcome::Stop(())
            } else {
                TickOutcome::Continue
            })
        }
    })
    .schedule(
        Schedule::fixed_delay(Duration::from_secs(3600))
            .resume_policy(busybeaver::ResumePolicy::RunImmediately),
    )
    .build()?;
    let mut handle = beaver.spawn_recurring(spec)?;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(beaver.notify_resumed(), 1);
    tokio::task::yield_now().await;
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test(start_paused = true)]
async fn recurring_tick_can_compose_typed_retry_without_overlap(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let retry = RetryBuilder::new(move |_| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            match attempt {
                1 => Err("temporary"),
                2 => Ok(TickOutcome::Continue),
                _ => Ok(TickOutcome::Stop(attempt)),
            }
        }
    })
    .max_attempts(2)
    .retry_all_errors()
    .build()?;
    let recurring = RecurringBuilder::from_retry(retry)
        .schedule(Schedule::fixed_delay(Duration::ZERO))
        .build()?;
    let mut handle = beaver.spawn_recurring(recurring)?;
    tokio::task::yield_now().await;
    assert!(matches!(handle.join().await?, TaskExit::Completed(3)));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    Ok(())
}
