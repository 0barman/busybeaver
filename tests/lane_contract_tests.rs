use busybeaver::{Beaver, BeaverError, CancelReason, LaneConfig, SpawnError, TaskExit, TaskSpec};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn waiting_spec() -> TaskSpec<(), &'static str> {
    TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok(())
    })
}

#[tokio::test]
async fn lane_rejects_zero_capacity_and_concurrency() {
    let beaver = Beaver::new("lane-config", 8).expect("valid test executor");
    assert!(matches!(
        beaver.create_lane(LaneConfig::new("zero-capacity").capacity(0)),
        Err(BeaverError::InvalidLaneCapacity)
    ));
    assert!(matches!(
        beaver.create_lane(LaneConfig::new("zero-concurrency").concurrency(0)),
        Err(BeaverError::InvalidLaneConcurrency)
    ));
    beaver.destroy().await.expect("destroy");
}

#[tokio::test]
async fn same_lane_name_requires_identical_immutable_config() -> TestResult {
    let beaver = Beaver::new("lane-conflict", 8)?;
    let first = beaver.create_lane(LaneConfig::new("api").capacity(4).concurrency(2))?;
    let same = beaver.create_lane(LaneConfig::new("api").capacity(4).concurrency(2))?;
    assert_eq!(first.id(), same.id());
    assert!(matches!(
        beaver.create_lane(LaneConfig::new("api").capacity(5).concurrency(2)),
        Err(BeaverError::LaneConfigConflict { .. })
    ));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn try_spawn_distinguishes_full_and_closed() -> TestResult {
    let beaver = Beaver::new("lane-errors", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let mut queued = lane.try_spawn(waiting_spec())?;
    assert!(matches!(
        lane.try_spawn(waiting_spec()),
        Err(SpawnError::QueueFull)
    ));

    lane.close();
    assert!(matches!(
        lane.try_spawn(waiting_spec()),
        Err(SpawnError::LaneClosing)
    ));
    running.control().cancel(CancelReason::UserRequested);
    queued.control().cancel(CancelReason::UserRequested);
    assert!(matches!(running.join().await?, TaskExit::Cancelled { .. }));
    assert!(matches!(queued.join().await?, TaskExit::Cancelled { .. }));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn cancelling_middle_queued_entry_immediately_restores_capacity() -> TestResult {
    let beaver = Beaver::new("lane-cancel-queue", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(2).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let mut first_queued = lane.try_spawn(waiting_spec())?;
    let mut middle_queued = lane.try_spawn(waiting_spec())?;
    assert!(matches!(
        lane.try_spawn(waiting_spec()),
        Err(SpawnError::QueueFull)
    ));

    middle_queued.control().cancel(CancelReason::UserRequested);
    assert!(matches!(
        middle_queued.join().await?,
        TaskExit::Cancelled { .. }
    ));
    let mut replacement = lane.try_spawn(waiting_spec())?;
    let stats = lane.stats();
    assert_eq!(stats.queued_live, 2);
    assert_eq!(stats.running, 1);

    for handle in [&mut running, &mut first_queued, &mut replacement] {
        handle.control().cancel(CancelReason::UserRequested);
        assert!(matches!(handle.join().await?, TaskExit::Cancelled { .. }));
    }
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn bounded_concurrency_is_never_exceeded() -> TestResult {
    let beaver = Beaver::new("lane-concurrency", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("parallel").capacity(8).concurrency(2))?;
    let running = Arc::new(AtomicU32::new(0));
    let maximum = Arc::new(AtomicU32::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut handles = Vec::new();

    for _ in 0..6 {
        let running_c = Arc::clone(&running);
        let maximum_c = Arc::clone(&maximum);
        let release_c = Arc::clone(&release);
        handles.push(lane.try_spawn(TaskSpec::new(move |_ctx| {
            let running = Arc::clone(&running_c);
            let maximum = Arc::clone(&maximum_c);
            let release = Arc::clone(&release_c);
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                let _permit = release.acquire().await.expect("release semaphore open");
                running.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, &'static str>(())
            }
        }))?);
    }

    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    release.add_permits(6);
    for handle in &mut handles {
        assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    }
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn serial_lane_starts_in_admission_order() -> TestResult {
    let beaver = Beaver::new("lane-fifo", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("fifo").capacity(8).concurrency(1))?;
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for number in 0_u8..5 {
        let order_c = Arc::clone(&order);
        handles.push(lane.try_spawn(TaskSpec::new(move |_ctx| {
            let order = Arc::clone(&order_c);
            async move {
                order.lock().expect("order").push(number);
                Ok::<_, &'static str>(())
            }
        }))?);
    }
    for handle in &mut handles {
        assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    }
    assert_eq!(*order.lock().expect("order"), vec![0, 1, 2, 3, 4]);
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn dropping_waiting_spawn_does_not_create_execution_or_leak_capacity() -> TestResult {
    let beaver = Beaver::new("lane-spawn-drop", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let mut queued = lane.try_spawn(waiting_spec())?;

    let waiting_lane = lane.clone();
    let waiter = tokio::spawn(async move { waiting_lane.spawn(waiting_spec()).await });
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(lane.stats().waiting_producers, 0);

    queued.control().cancel(CancelReason::UserRequested);
    assert!(matches!(queued.join().await?, TaskExit::Cancelled { .. }));
    let mut replacement = lane.try_spawn(waiting_spec())?;
    assert_eq!(lane.stats().queued_live, 1);
    running.control().cancel(CancelReason::UserRequested);
    replacement.control().cancel(CancelReason::UserRequested);
    let _ = running.join().await?;
    let _ = replacement.join().await?;
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn waiting_producers_are_fifo_and_try_spawn_cannot_barge() -> TestResult {
    let beaver = Beaver::new("lane-waiter-fifo", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let mut queued = lane.try_spawn(waiting_spec())?;
    let (admitted_tx, mut admitted_rx) = tokio::sync::mpsc::unbounded_channel();

    for number in 0_u8..3 {
        let waiting_lane = lane.clone();
        let admitted_tx = admitted_tx.clone();
        tokio::spawn(async move {
            let handle = waiting_lane
                .spawn(waiting_spec())
                .await
                .expect("waiting producer should eventually be admitted");
            admitted_tx.send((number, handle)).expect("receiver open");
        });
        while lane.stats().waiting_producers != usize::from(number) + 1 {
            tokio::task::yield_now().await;
        }
    }
    drop(admitted_tx);

    queued.control().cancel(CancelReason::UserRequested);
    assert!(matches!(queued.join().await?, TaskExit::Cancelled { .. }));
    assert!(matches!(
        lane.try_spawn(waiting_spec()),
        Err(SpawnError::QueueFull)
    ));

    for expected in 0_u8..3 {
        let (actual, mut admitted) = admitted_rx.recv().await.expect("one admitted waiter");
        assert_eq!(actual, expected);
        admitted.control().cancel(CancelReason::UserRequested);
        assert!(matches!(admitted.join().await?, TaskExit::Cancelled { .. }));
    }
    assert_eq!(lane.stats().waiting_producers, 0);

    running.control().cancel(CancelReason::UserRequested);
    assert!(matches!(running.join().await?, TaskExit::Cancelled { .. }));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn spawn_timeout_deadline_is_captured_when_method_is_called() -> TestResult {
    let beaver = Beaver::new("lane-timeout", 8)?;
    let lane = beaver.create_lane(LaneConfig::new("serial").capacity(1).concurrency(1))?;
    let mut running = lane.try_spawn(waiting_spec())?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let mut queued = lane.try_spawn(waiting_spec())?;
    let future = lane.spawn_timeout(waiting_spec(), Duration::from_secs(1));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(future.await, Err(SpawnError::AdmissionTimedOut)));

    running.control().cancel(CancelReason::UserRequested);
    queued.control().cancel(CancelReason::UserRequested);
    let _ = running.join().await?;
    let _ = queued.join().await?;
    beaver.destroy().await?;
    Ok(())
}
