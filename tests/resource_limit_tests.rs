use busybeaver::{
    Beaver, BeaverError, CancelReason, EventRecvError, LaneConfig, ResourceLimits, SpawnChildError,
    SpawnError, TaskExit, TaskSpec,
};
use std::future::pending;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn child_and_tag_limits_fail_without_leaking_execution_state() -> TestResult {
    let beaver = Beaver::builder("limits", 8)
        .resource_limits(ResourceLimits {
            max_children_per_execution: 1,
            max_tag_bytes: 4,
            ..ResourceLimits::default()
        })
        .build()?;

    assert!(matches!(
        beaver.spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) }).tag("five!"),),
        Err(BeaverError::ResourceLimitExceeded {
            resource: "tag bytes"
        })
    ));
    assert!(beaver.snapshot().active.is_empty());

    let mut parent = beaver.spawn(TaskSpec::new(|context| async move {
        let _first = context
            .spawn_child(pending::<Result<(), ()>>())
            .expect("first child admitted");
        assert!(matches!(
            context.spawn_child(async { Ok::<_, ()>(()) }),
            Err(SpawnChildError::LimitReached { maximum: 1 })
        ));
        Ok::<_, ()>(())
    }))?;
    assert!(matches!(parent.join().await?, TaskExit::Completed(())));
    assert!(beaver.snapshot().active.is_empty());
    Ok(())
}

#[tokio::test]
async fn waiting_producer_limit_is_bounded_and_cancel_safe() -> TestResult {
    let beaver = Beaver::builder("limits", 8)
        .resource_limits(ResourceLimits {
            max_waiting_producers_per_lane: 1,
            ..ResourceLimits::default()
        })
        .build()?;
    let lane = beaver.create_lane(LaneConfig::new("waiters").capacity(1))?;
    let waiting = TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok::<_, ()>(())
    });
    let mut running = lane.try_spawn(waiting.clone())?;
    while lane.stats().running != 1 {
        tokio::task::yield_now().await;
    }
    let mut queued = lane.try_spawn(waiting.clone())?;

    let first_lane = lane.clone();
    let first_spec = waiting.clone();
    let first_waiter = tokio::spawn(async move { first_lane.spawn(first_spec).await });
    while lane.stats().waiting_producers != 1 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        lane.spawn(waiting).await,
        Err(SpawnError::WaitingProducerLimitReached)
    ));
    first_waiter.abort();
    let _ = first_waiter.await;
    while lane.stats().waiting_producers != 0 {
        tokio::task::yield_now().await;
    }

    running.control().cancel(CancelReason::UserRequested);
    queued.control().cancel(CancelReason::UserRequested);
    assert!(matches!(running.join().await?, TaskExit::Cancelled { .. }));
    assert!(matches!(queued.join().await?, TaskExit::Cancelled { .. }));
    Ok(())
}

#[tokio::test]
async fn event_capacity_reports_lag_and_zero_limits_are_rejected() -> TestResult {
    assert!(matches!(
        Beaver::builder("invalid", 8)
            .resource_limits(ResourceLimits {
                max_active_executions: 0,
                ..ResourceLimits::default()
            })
            .build(),
        Err(BeaverError::InvalidResourceLimit {
            field: "max_active_executions"
        })
    ));

    let beaver = Beaver::builder("events", 8)
        .resource_limits(ResourceLimits {
            event_capacity: 1,
            ..ResourceLimits::default()
        })
        .build()?;
    let mut events = beaver.subscribe_events()?;
    let mut handle = beaver.spawn_future(async { Ok::<_, ()>(()) })?;
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    assert!(matches!(
        events.recv().await,
        Err(EventRecvError::Lagged { .. })
    ));
    Ok(())
}
