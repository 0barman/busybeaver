use busybeaver::{
    Beaver, BeaverError, EventSubscribeError, LaneConfig, ResourceLimits, SlotKey, TaskEvent,
    TaskExit, TaskSpec,
};
use std::future::pending;
use std::time::Duration;

#[test]
fn constructors_outside_runtime_return_errors_instead_of_panicking() {
    let error = Beaver::new("outside", 8)
        .err()
        .expect("construction outside a runtime must fail");
    assert!(matches!(error, BeaverError::RuntimeUnavailable));
    assert_eq!(error.code(), "BB-RUNTIME-UNAVAILABLE");
    assert!(matches!(
        Beaver::try_new("outside", 8),
        Err(BeaverError::RuntimeUnavailable)
    ));
}

#[test]
fn constructors_reject_invalid_capacity_with_a_stable_error() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("valid test runtime");
    assert!(matches!(
        Beaver::new_with_handle("invalid", 0, runtime.handle().clone()),
        Err(BeaverError::InvalidLaneCapacity)
    ));
    assert!(matches!(
        Beaver::try_new_with_handle(
            "invalid",
            tokio::sync::Semaphore::MAX_PERMITS.saturating_add(1),
            runtime.handle().clone(),
        ),
        Err(BeaverError::InvalidLaneCapacity)
    ));
}

#[tokio::test]
async fn events_are_bounded_redacted_and_follow_lifecycle_order(
) -> Result<(), Box<dyn std::error::Error>> {
    let limits = ResourceLimits {
        max_event_subscribers: 1,
        ..ResourceLimits::default()
    };
    let beaver = Beaver::builder("legacy", 8)
        .resource_limits(limits)
        .build()?;
    let mut events = beaver.subscribe_events()?;
    assert!(matches!(
        beaver.subscribe_events(),
        Err(EventSubscribeError::SubscriberLimitReached)
    ));
    let mut handle = beaver.spawn(TaskSpec::new(|_| async { Ok::<_, &'static str>(7u8) }))?;
    let execution_id = handle.execution_id();
    assert!(matches!(handle.join().await?, TaskExit::Completed(7)));

    let mut saw_admitted = false;
    let mut saw_running = false;
    let mut saw_terminal = false;
    for _ in 0..8 {
        let event = events.recv().await?;
        match event {
            TaskEvent::Admitted(snapshot) if snapshot.execution_id == execution_id => {
                saw_admitted = true;
            }
            TaskEvent::StateChanged(snapshot)
                if snapshot.execution_id == execution_id
                    && matches!(snapshot.state, busybeaver::TaskState::Running { .. }) =>
            {
                saw_running = true;
            }
            TaskEvent::Terminal {
                execution_id: id,
                summary,
            } if id == execution_id => {
                assert!(summary.is_completed());
                saw_terminal = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_admitted && saw_running && saw_terminal);
    drop(events);
    assert_eq!(beaver.snapshot().event_subscribers, 0);
    Ok(())
}

#[tokio::test]
async fn resource_limits_fail_before_partial_publication() -> Result<(), Box<dyn std::error::Error>>
{
    let limits = ResourceLimits {
        max_active_executions: 1,
        max_lanes: 1,
        max_scopes: 1,
        max_slots: 1,
        ..ResourceLimits::default()
    };
    let beaver = Beaver::builder("legacy", 8)
        .resource_limits(limits)
        .build()?;
    let first = beaver.spawn_future(pending::<Result<(), ()>>())?;
    assert!(matches!(
        beaver.spawn_future(pending::<Result<(), ()>>()),
        Err(BeaverError::ResourceLimitExceeded {
            resource: "active executions"
        })
    ));
    first
        .control()
        .cancel(busybeaver::CancelReason::UserRequested);

    let lane = beaver.create_lane(LaneConfig::new("one"))?;
    assert!(matches!(
        beaver.create_lane(LaneConfig::new("two")),
        Err(BeaverError::ResourceLimitExceeded { resource: "lanes" })
    ));
    let _scope = beaver.create_scope("one", lane.clone())?;
    assert!(matches!(
        beaver.create_scope("two", lane.clone()),
        Err(BeaverError::ResourceLimitExceeded { resource: "scopes" })
    ));
    let _slot = beaver.create_task_slot(SlotKey::new("one")?, lane.clone())?;
    assert!(matches!(
        beaver.create_task_slot(SlotKey::new("two")?, lane),
        Err(BeaverError::ResourceLimitExceeded {
            resource: "task slots"
        })
    ));
    let snapshot = beaver.snapshot();
    assert_eq!(snapshot.lanes.len(), 1);
    assert_eq!(snapshot.scope_count, 1);
    assert_eq!(snapshot.slot_count, 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn terminal_history_respects_capacity_and_ttl() -> Result<(), Box<dyn std::error::Error>> {
    let limits = ResourceLimits {
        terminal_history_capacity: 2,
        terminal_history_ttl: Some(Duration::from_secs(5)),
        ..ResourceLimits::default()
    };
    let beaver = Beaver::builder("legacy", 8)
        .resource_limits(limits)
        .build()?;
    for value in 0..3u8 {
        let mut handle = beaver.spawn_future(async move { Ok::<_, ()>(value) })?;
        handle.join().await?;
    }
    assert_eq!(beaver.snapshot().terminal_history.len(), 2);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(beaver.snapshot().terminal_history.is_empty());
    Ok(())
}

#[tokio::test]
async fn lane_admission_publishes_metadata_and_rolls_back_registry_failure(
) -> Result<(), Box<dyn std::error::Error>> {
    let limits = ResourceLimits {
        max_active_executions: 1,
        ..ResourceLimits::default()
    };
    let beaver = Beaver::builder("legacy", 8)
        .resource_limits(limits)
        .build()?;
    let lane = beaver.create_lane(LaneConfig::new("observed").capacity(2))?;
    let lane_id = lane.id();
    let mut events = beaver.subscribe_events()?;
    let mut first = lane.try_spawn(TaskSpec::new(|context| async move {
        context.cancelled().await;
        Ok::<_, ()>(())
    }))?;
    let first_id = first.execution_id();
    let admitted = events.recv().await?;
    assert!(matches!(
        admitted,
        TaskEvent::Admitted(snapshot)
            if snapshot.execution_id == first_id && snapshot.lane_id == Some(lane_id)
    ));

    let queued_before_failure = lane.stats().queued_live;
    assert!(matches!(
        lane.try_spawn(TaskSpec::new(|_| async { Ok::<_, ()>(()) })),
        Err(busybeaver::SpawnError::Internal(
            BeaverError::ResourceLimitExceeded {
                resource: "active executions"
            }
        ))
    ));
    assert_eq!(lane.stats().queued_live, queued_before_failure);
    assert_eq!(beaver.snapshot().active.len(), 1);

    first
        .control()
        .cancel(busybeaver::CancelReason::UserRequested);
    assert!(matches!(first.join().await?, TaskExit::Cancelled { .. }));
    Ok(())
}
