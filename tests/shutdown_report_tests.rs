use busybeaver::{work, Beaver, BeaverError, BeaverResult, FixedCountBuilder, WorkResult};
use std::future::pending;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[tokio::test(start_paused = true)]
async fn destroy_timeout_reports_unfinished_worker() -> BeaverResult<()> {
    let beaver = Beaver::new("shutdown-timeout", 1);
    let started = Arc::new(Notify::new());
    let started_for_work = Arc::clone(&started);
    let task = FixedCountBuilder::new(work(move || {
        let started = Arc::clone(&started_for_work);
        async move {
            started.notify_one();
            pending::<WorkResult<()>>().await
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let shutdown = beaver.destroy_with_timeout(Duration::from_secs(5));
    tokio::pin!(shutdown);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let report = shutdown.await;

    assert_eq!(report.total_workers(), 1);
    assert_eq!(report.stopped_workers(), 0);
    assert!(report.timed_out());
    assert!(!report.is_complete());
    Ok(())
}

#[tokio::test]
async fn concurrent_destroy_callers_share_one_report() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("shutdown-shared", 1));
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let started_for_work = Arc::clone(&started);
    let finish_for_work = Arc::clone(&finish);
    let task = FixedCountBuilder::new(work(move || {
        let started = Arc::clone(&started_for_work);
        let finish = Arc::clone(&finish_for_work);
        async move {
            started.notify_one();
            finish.notified().await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let first = Arc::clone(&beaver);
    let second = Arc::clone(&beaver);
    let first_shutdown = tokio::spawn(async move { first.destroy_with_report().await });
    let second_shutdown = tokio::spawn(async move { second.destroy_with_report().await });
    tokio::task::yield_now().await;
    finish.notify_waiters();

    let first_report = first_shutdown.await.unwrap();
    let second_report = second_shutdown.await.unwrap();
    assert_eq!(first_report, second_report);
    assert!(first_report.is_complete());
    assert_eq!(first_report.total_workers(), 1);
    Ok(())
}

#[tokio::test]
async fn dropping_destroy_future_does_not_abandon_shutdown() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("shutdown-drop", 1));
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let started_for_work = Arc::clone(&started);
    let finish_for_work = Arc::clone(&finish);
    let task = FixedCountBuilder::new(work(move || {
        let started = Arc::clone(&started_for_work);
        let finish = Arc::clone(&finish_for_work);
        async move {
            started.notify_one();
            finish.notified().await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let caller_beaver = Arc::clone(&beaver);
    let caller = tokio::spawn(async move { caller_beaver.destroy_with_report().await });
    tokio::task::yield_now().await;
    caller.abort();
    let _ = caller.await;

    finish.notify_waiters();
    let report = beaver.destroy_with_report().await;
    assert!(report.is_complete());
    assert_eq!(report.stopped_workers(), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn legacy_destroy_maps_timeout_to_existing_error_variant() -> BeaverResult<()> {
    let beaver = Beaver::new("shutdown-compat", 1);
    let started = Arc::new(Notify::new());
    let started_for_work = Arc::clone(&started);
    let task = FixedCountBuilder::new(work(move || {
        let started = Arc::clone(&started_for_work);
        async move {
            started.notify_one();
            pending::<WorkResult<()>>().await
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let shutdown = beaver.destroy();
    tokio::pin!(shutdown);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(shutdown.await, Err(BeaverError::DamReleased)));
    Ok(())
}
