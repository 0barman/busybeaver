use busybeaver::{work, Beaver, BeaverResult, FixedCountBuilder, PeriodicBuilder, WorkResult};
use std::sync::Arc;
use std::time::Duration;

async fn drive_scheduler() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// F06: all concurrent destroy callers must wait for the same shutdown
/// barrier; a later caller cannot report success while an earlier caller is
/// still waiting for a running task.
#[tokio::test]
async fn concurrent_destroy_callers_share_the_same_barrier() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("shared-destroy", 8)?);
    let release = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());

    let release_c = Arc::clone(&release);
    let started_c = Arc::clone(&started);
    let task = FixedCountBuilder::new(work(move || {
        let release = Arc::clone(&release_c);
        let started = Arc::clone(&started_c);
        async move {
            started.notify_one();
            release.notified().await;
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let first_beaver = Arc::clone(&beaver);
    let first = tokio::spawn(async move { first_beaver.destroy().await });
    drive_scheduler().await;
    assert!(!first.is_finished());

    let second_beaver = Arc::clone(&beaver);
    let second = tokio::spawn(async move { second_beaver.destroy().await });
    drive_scheduler().await;
    assert!(
        !second.is_finished(),
        "a concurrent destroy caller must wait for the existing shutdown"
    );

    release.notify_waiters();
    first.await.expect("first destroy task")?;
    second.await.expect("second destroy task")?;
    Ok(())
}

/// F07: shutdown is irreversible; neither the default nor a named lane may be
/// recreated through a legacy enqueue entry point.
#[tokio::test]
async fn named_lane_cannot_be_recreated_after_destroy() -> BeaverResult<()> {
    let beaver = Beaver::new("irreversible-shutdown", 8)?;
    beaver.destroy().await?;

    let task = FixedCountBuilder::new(work(|| async { WorkResult::Done(()) }))
        .count(1)
        .build()?;
    let result = beaver
        .enqueue_on_new_thread(task, "must-not-revive", 8, false)
        .await;
    assert!(result.is_err(), "named enqueue must fail after destroy");
    Ok(())
}

/// F05: a shutdown timeout must be observable instead of being discarded and
/// returned as unconditional success.
#[tokio::test(start_paused = true)]
async fn destroy_reports_timeout_for_non_cooperative_work() -> BeaverResult<()> {
    let beaver = Arc::new(Beaver::new("shutdown-timeout", 8)?);
    let started = Arc::new(tokio::sync::Notify::new());
    let started_c = Arc::clone(&started);
    let task = PeriodicBuilder::new(work(move || {
        let started = Arc::clone(&started_c);
        async move {
            started.notify_one();
            std::future::pending::<()>().await;
            #[allow(unreachable_code)]
            WorkResult::NeedRetry
        }
    }))
    .interval(Duration::ZERO)
    .build()?;
    beaver.enqueue(task).await?;
    started.notified().await;

    let shutdown_beaver = Arc::clone(&beaver);
    let shutdown = tokio::spawn(async move { shutdown_beaver.destroy().await });
    drive_scheduler().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    drive_scheduler().await;

    let result = shutdown.await.expect("destroy task");
    assert!(result.is_err(), "destroy must report its timeout");
    Ok(())
}
