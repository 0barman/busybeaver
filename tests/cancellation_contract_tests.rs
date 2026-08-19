use busybeaver::{
    listener, work, Beaver, BeaverResult, PeriodicBuilder, RangeIntervalBuilder,
    TimeIntervalBuilder, WorkResult,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

async fn drive_scheduler() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// F01/F02: cancelling a task that is sleeping in an SDK-managed interval
/// must wake it immediately and must not allow the next side effect to start.
#[tokio::test(start_paused = true)]
async fn time_interval_cancel_wakes_sleep_and_prevents_next_attempt() -> BeaverResult<()> {
    let beaver = Beaver::new("cancel-aware-time-interval", 8);
    let attempts = Arc::new(AtomicU32::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));

    let attempts_c = Arc::clone(&attempts);
    let interrupted_c = Arc::clone(&interrupted);
    let task = TimeIntervalBuilder::new(work(move || {
        let attempts = Arc::clone(&attempts_c);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .intervals_millis([0, 10_000])
    .listener(listener(
        || {},
        move || interrupted_c.store(true, Ordering::SeqCst),
    ))
    .build()?;

    beaver.enqueue(task).await?;
    drive_scheduler().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    beaver.cancel_all().await?;
    drive_scheduler().await;
    assert!(
        interrupted.load(Ordering::SeqCst),
        "SDK-managed sleep must be woken by cancellation"
    );

    tokio::time::advance(Duration::from_secs(11)).await;
    drive_scheduler().await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "cancellation during sleep must not allow one extra attempt"
    );

    beaver.destroy().await
}

#[tokio::test(start_paused = true)]
async fn range_interval_cancel_wakes_backoff_and_prevents_retry() -> BeaverResult<()> {
    let beaver = Beaver::new("cancel-aware-range", 8);
    let attempts = Arc::new(AtomicU32::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));
    let attempts_c = Arc::clone(&attempts);
    let interrupted_c = Arc::clone(&interrupted);

    let task = RangeIntervalBuilder::new(
        work(move || {
            let attempts = Arc::clone(&attempts_c);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                WorkResult::NeedRetry
            }
        }),
        2,
    )
    .add_range(0, 1, Duration::from_secs(10))
    .listener(listener(
        || {},
        move || interrupted_c.store(true, Ordering::SeqCst),
    ))
    .build()?;

    beaver.enqueue(task).await?;
    drive_scheduler().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    beaver.cancel_all().await?;
    drive_scheduler().await;
    assert!(interrupted.load(Ordering::SeqCst));

    tokio::time::advance(Duration::from_secs(11)).await;
    drive_scheduler().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    beaver.destroy().await
}

#[tokio::test(start_paused = true)]
async fn periodic_cancel_wakes_initial_delay_without_running_body() -> BeaverResult<()> {
    let beaver = Beaver::new("cancel-aware-periodic", 8);
    let attempts = Arc::new(AtomicU32::new(0));
    let interrupted = Arc::new(AtomicBool::new(false));
    let attempts_c = Arc::clone(&attempts);
    let interrupted_c = Arc::clone(&interrupted);

    let task = PeriodicBuilder::new(work(move || {
        let attempts = Arc::clone(&attempts_c);
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::from_secs(10))
    .initial_delay(true)
    .listener(listener(
        || {},
        move || interrupted_c.store(true, Ordering::SeqCst),
    ))
    .build()?;

    beaver.enqueue(task).await?;
    drive_scheduler().await;
    beaver.cancel_all().await?;
    drive_scheduler().await;
    assert!(interrupted.load(Ordering::SeqCst));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);

    tokio::time::advance(Duration::from_secs(11)).await;
    drive_scheduler().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    beaver.destroy().await
}
