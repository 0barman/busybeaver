#![allow(dead_code)]

use busybeaver::{
    Backoff, Beaver, CancelReason, LaneConfig, RecurringBuilder, ReplacePolicy, RetryBuilder,
    RotationPolicy, Schedule, ServiceBuilder, ShutdownMode, ShutdownOptions, SlotKey, TaskExit,
    TaskSpec, TickOutcome,
};
use std::time::Duration;

async fn typed_execution(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let spec = TaskSpec::new(|context| async move {
        context.sleep(Duration::from_millis(1)).await?;
        Ok::<_, busybeaver::Cancelled>(42_u8)
    });
    let mut handle = beaver.spawn(spec)?;
    assert!(matches!(handle.join().await?, TaskExit::Completed(42)));
    Ok(())
}

async fn lane_retry_and_recurring(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("network").capacity(16).concurrency(2))?;
    let retry = RetryBuilder::new(|attempt| async move {
        if attempt.number() == 1 {
            Err("temporary")
        } else {
            Ok(())
        }
    })
    .max_attempts(2)
    .retry_all_errors()
    .backoff(Backoff::fixed(Duration::ZERO))
    .build()?;
    let mut retry_handle = lane.spawn_retry(retry).await?;
    assert!(matches!(
        retry_handle.join().await?,
        TaskExit::Completed(())
    ));

    let recurring = RecurringBuilder::new(|tick| async move {
        Ok::<_, ()>(if tick.number() == 1 {
            TickOutcome::Stop(7_u8)
        } else {
            TickOutcome::Continue
        })
    })
    .schedule(Schedule::fixed_delay(Duration::ZERO))
    .build()?;
    let mut recurring_handle = lane.spawn_recurring(recurring).await?;
    assert!(matches!(
        recurring_handle.join().await?,
        TaskExit::Completed(7)
    ));
    Ok(())
}

async fn slot_scope_and_service(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let lane = beaver.create_lane(LaneConfig::new("lifecycle").capacity(16).concurrency(2))?;
    let slot = beaver.create_task_slot(SlotKey::new("latest")?, lane.clone())?;
    let replacement = slot
        .replace(
            1,
            TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
            ReplacePolicy::StrictSingleInstance,
        )?
        .await?;
    drop(replacement);

    let scope = beaver.create_scope("session", lane)?;
    let rotation = scope.rotate(RotationPolicy::Strict)?;
    let _ = rotation.wait().await;

    let service = ServiceBuilder::new(|context| async move {
        context.ready();
        context.cancelled().await;
        Ok::<_, ()>(())
    })
    .build()?;
    let mut service = beaver.start_service(service)?;
    service.wait_ready().await?;
    service.control().cancel(CancelReason::UserRequested);
    let _ = service.join().await?;
    Ok(())
}

async fn checked_shutdown(beaver: &Beaver) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::CancelAll)
            .grace_period(Duration::from_secs(1)),
    )?;
    let _ = shutdown.wait_final().await?;
    Ok(())
}

#[test]
fn migration_examples_remain_type_checked() {
    let _ = typed_execution;
    let _ = lane_retry_and_recurring;
    let _ = slot_scope_and_service;
    let _ = checked_shutdown;
}
