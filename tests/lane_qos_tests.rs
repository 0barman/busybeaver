use busybeaver::{
    Beaver, LaneConfig, OrderingKey, Priority, ResourceLimits, SpawnOptions, TaskExit, TaskSpec,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[tokio::test]
async fn priority_selects_higher_ready_work_first() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("qos").capacity(8))?;
    let gate = Arc::new(Notify::new());
    let blocker_gate = Arc::clone(&gate);
    let blocker = lane.try_spawn(TaskSpec::new(move |_| {
        let gate = Arc::clone(&blocker_gate);
        async move {
            gate.notified().await;
            Ok::<_, ()>(())
        }
    }))?;

    tokio::task::yield_now().await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let low_order = Arc::clone(&order);
    let mut low = lane.try_spawn_with_options(
        TaskSpec::new(move |_| {
            let order = Arc::clone(&low_order);
            async move {
                order.lock().unwrap().push("low");
                Ok::<_, ()>(())
            }
        }),
        SpawnOptions::new().priority(Priority::LOWEST),
    )?;
    let high_order = Arc::clone(&order);
    let mut high = lane.try_spawn_with_options(
        TaskSpec::new(move |_| {
            let order = Arc::clone(&high_order);
            async move {
                order.lock().unwrap().push("high");
                Ok::<_, ()>(())
            }
        }),
        SpawnOptions::new().priority(Priority::HIGHEST),
    )?;

    gate.notify_one();
    blocker.wait().await;
    assert!(matches!(high.join().await?, TaskExit::Completed(())));
    assert!(matches!(low.join().await?, TaskExit::Completed(())));
    assert_eq!(&*order.lock().unwrap(), &["high", "low"]);
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test]
async fn aging_dispatch_prevents_low_priority_starvation() -> Result<(), Box<dyn std::error::Error>>
{
    let beaver = Beaver::new("legacy", 16)?;
    let lane = beaver.create_lane(LaneConfig::new("aging").capacity(16))?;
    let gate = Arc::new(Notify::new());
    let blocker_gate = Arc::clone(&gate);
    let blocker = lane.try_spawn(TaskSpec::new(move |_| {
        let gate = Arc::clone(&blocker_gate);
        async move {
            gate.notified().await;
            Ok::<_, ()>(())
        }
    }))?;
    tokio::task::yield_now().await;

    let order = Arc::new(Mutex::new(Vec::new()));
    let low_order = Arc::clone(&order);
    let mut low = lane.try_spawn_with_options(
        TaskSpec::new(move |_| {
            let order = Arc::clone(&low_order);
            async move {
                order.lock().unwrap().push(0usize);
                Ok::<_, ()>(())
            }
        }),
        SpawnOptions::new().priority(Priority::LOWEST),
    )?;
    let mut highs = Vec::new();
    for value in 1..=9 {
        let order = Arc::clone(&order);
        highs.push(lane.try_spawn_with_options(
            TaskSpec::new(move |_| {
                let order = Arc::clone(&order);
                async move {
                    order.lock().unwrap().push(value);
                    Ok::<_, ()>(())
                }
            }),
            SpawnOptions::new().priority(Priority::HIGHEST),
        )?);
    }

    gate.notify_one();
    blocker.wait().await;
    for handle in &highs {
        handle.wait().await;
    }
    low.wait().await;
    let position = order
        .lock()
        .unwrap()
        .iter()
        .position(|value| *value == 0)
        .unwrap();
    assert!(position <= 7, "low priority entry started at {position}");
    assert!(matches!(low.join().await?, TaskExit::Completed(())));
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test]
async fn ordering_key_is_single_flight_and_reclaimed() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("legacy", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("keys").capacity(8).concurrency(3))?;
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let key = OrderingKey::try_from("account-1")?;

    let make = |key: OrderingKey| {
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        let release = Arc::clone(&release);
        lane.try_spawn_with_options(
            TaskSpec::new(move |_| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                let release = Arc::clone(&release);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    release.notified().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                }
            }),
            SpawnOptions::new().ordering_key(key),
        )
    };

    let first = make(key.clone())?;
    let second = make(key)?;
    for _ in 0..20 {
        if active.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert_eq!(lane.stats().blocked_by_ordering_key, 1);
    release.notify_one();
    first.wait().await;
    for _ in 0..20 {
        if active.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    release.notify_one();
    second.wait().await;
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(lane.stats().blocked_by_ordering_key, 0);
    Ok::<_, Box<dyn std::error::Error>>(())
}

#[tokio::test]
async fn high_cardinality_ordering_keys_are_reclaimed_after_terminal(
) -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::builder("legacy", 8)
        .resource_limits(ResourceLimits {
            max_ordering_keys_per_lane: 1,
            ..ResourceLimits::default()
        })
        .build()?;
    let lane = beaver.create_lane(LaneConfig::new("key-reclaim").capacity(2))?;

    for number in 0..256_u16 {
        let key = OrderingKey::new(number.to_le_bytes())?;
        let mut handle = lane.try_spawn_with_options(
            TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
            SpawnOptions::new().ordering_key(key),
        )?;
        assert!(matches!(handle.join().await?, TaskExit::Completed(())));
        while lane.stats().running != 0 {
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(lane.stats().blocked_by_ordering_key, 0);
    assert_eq!(lane.stats().ready_by_priority.iter().sum::<usize>(), 0);
    Ok(())
}
