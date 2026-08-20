use busybeaver::{
    Beaver, CancelReason, ExecutorStopReason, LaneConfig, RetryBuilder, SpawnError, TaskExit,
    TaskSpec, TaskState,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TERMINAL_TIMEOUT: Duration = Duration::from_secs(2);

fn waiting_spec() -> TaskSpec<(), &'static str> {
    TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok(())
    })
}

async fn drive_until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..64 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        predicate(),
        "condition did not become true after scheduler progress"
    );
}

#[test]
fn runtime_shutdown_terminalizes_running_and_queued_lane_executions() -> TestResult {
    let worker = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let beaver = Beaver::new_with_handle("runtime-loss", 8, worker.handle().clone())?;
    let lane = beaver.create_lane(
        LaneConfig::new("runtime-loss-lane")
            .capacity(2)
            .concurrency(1),
    )?;
    let started = Arc::new(tokio::sync::Notify::new());
    let started_c = Arc::clone(&started);
    let mut running = lane.try_spawn(TaskSpec::new(move |ctx| {
        let started = Arc::clone(&started_c);
        async move {
            started.notify_one();
            ctx.cancelled().await;
            Ok::<_, &'static str>(())
        }
    }))?;
    worker.block_on(async {
        tokio::time::timeout(TERMINAL_TIMEOUT, started.notified())
            .await
            .expect("running execution must start before runtime shutdown");
    });
    let mut queued = lane.try_spawn(waiting_spec())?;
    assert!(matches!(queued.state(), TaskState::Queued));

    // The observer stays alive after the execution runtime disappears. Every
    // admitted handle must still receive a terminal outcome.
    worker.shutdown_background();
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    observer.block_on(async {
        let running_exit = tokio::time::timeout(TERMINAL_TIMEOUT, running.join())
            .await
            .expect("running execution must become terminal after runtime loss")?;
        assert!(matches!(
            running_exit,
            TaskExit::ExecutorStopped {
                reason: ExecutorStopReason::RunnerCancelled
                    | ExecutorStopReason::RuntimeUnavailable
            } | TaskExit::Cancelled {
                reason: CancelReason::ExecutorShutdown
            }
        ));

        let queued_exit = tokio::time::timeout(TERMINAL_TIMEOUT, queued.join())
            .await
            .expect("queued execution must become terminal after runtime loss")?;
        assert!(matches!(
            queued_exit,
            TaskExit::ExecutorStopped {
                reason: ExecutorStopReason::RunnerCancelled
                    | ExecutorStopReason::RuntimeUnavailable
            }
        ));
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    drop(lane);
    drop(beaver);
    Ok(())
}

#[test]
fn runtime_shutdown_wakes_waiting_lane_producer_with_explicit_error() -> TestResult {
    let worker = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let beaver = Beaver::new_with_handle("runtime-waiter-loss", 8, worker.handle().clone())?;
    let lane = beaver.create_lane(LaneConfig::new("runtime-waiter-loss").capacity(1))?;
    let started = Arc::new(tokio::sync::Notify::new());
    let started_c = Arc::clone(&started);
    let mut running = lane.try_spawn(TaskSpec::new(move |context| {
        let started = Arc::clone(&started_c);
        async move {
            started.notify_one();
            context.cancelled().await;
            Ok::<_, &'static str>(())
        }
    }))?;
    worker.block_on(async {
        tokio::time::timeout(TERMINAL_TIMEOUT, started.notified())
            .await
            .expect("running execution must start");
    });
    let mut queued = lane.try_spawn(waiting_spec())?;
    let waiting_lane = lane.clone();
    let waiter = observer.spawn(async move { waiting_lane.spawn(waiting_spec()).await });
    observer.block_on(async {
        drive_until(|| lane.stats().waiting_producers == 1).await;
    });

    worker.shutdown_background();
    observer.block_on(async {
        let result = tokio::time::timeout(TERMINAL_TIMEOUT, waiter)
            .await
            .expect("waiting producer must wake after runtime loss")?;
        assert!(matches!(result, Err(SpawnError::ExecutorUnavailable)));
        let _ = running.join().await?;
        let _ = queued.join().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    drop(lane);
    drop(beaver);
    Ok(())
}

struct DropSignal(Option<mpsc::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("intentional queued future drop panic");
    }
}

#[tokio::test]
async fn dropping_last_beaver_owner_cancels_and_releases_direct_typed_work() -> TestResult {
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let mut handle = {
        let beaver = Beaver::new("drop-direct-typed", 8)?;
        let signal = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
        let handle = beaver.spawn(TaskSpec::new(move |ctx| {
            let signal = DropSignal(signal.lock().expect("drop signal lock").take());
            async move {
                let _signal = signal;
                ctx.cancelled().await;
                Ok::<_, &'static str>(())
            }
        }))?;
        drive_until(|| matches!(handle.state(), TaskState::Running { .. })).await;
        handle
    };

    let exit = tokio::time::timeout(TERMINAL_TIMEOUT, handle.join())
        .await
        .expect("dropping the final executor owner must stop direct typed work")?;
    assert!(matches!(
        exit,
        TaskExit::Cancelled {
            reason: CancelReason::ExecutorShutdown
        }
    ));
    dropped_rx
        .recv_timeout(TERMINAL_TIMEOUT)
        .expect("the user future must be released while the runtime remains alive");
    Ok(())
}

#[tokio::test]
async fn lane_outlives_beaver_but_last_lane_drop_cleans_running_and_queued_work() -> TestResult {
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let lane = {
        let beaver = Beaver::new("drop-lane-owner", 8)?;
        beaver.create_lane(
            LaneConfig::new("drop-lane-owner")
                .capacity(2)
                .concurrency(1),
        )?
    };

    // A public Lane is an executor owner: dropping the Beaver facade alone
    // must not invalidate it.
    let mut completed = lane.try_spawn_future(async { Ok::<_, &'static str>(7_u8) })?;
    assert!(matches!(completed.join().await?, TaskExit::Completed(7)));

    let signal = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
    let mut running = lane.try_spawn(TaskSpec::new(move |ctx| {
        let signal = DropSignal(signal.lock().expect("drop signal lock").take());
        async move {
            let _signal = signal;
            ctx.cancelled().await;
            Ok::<_, &'static str>(())
        }
    }))?;
    drive_until(|| matches!(running.state(), TaskState::Running { .. })).await;
    let mut queued = lane.try_spawn(waiting_spec())?;

    drop(lane);

    for handle in [&mut running, &mut queued] {
        let exit = tokio::time::timeout(TERMINAL_TIMEOUT, handle.join())
            .await
            .expect("last Lane drop must terminalize every admitted execution")?;
        assert!(matches!(
            exit,
            TaskExit::Cancelled {
                reason: CancelReason::ExecutorShutdown
            }
        ));
    }
    dropped_rx
        .recv_timeout(TERMINAL_TIMEOUT)
        .expect("running lane future must be released without ending the runtime");
    Ok(())
}

#[tokio::test]
async fn last_lane_drop_isolates_panicking_queued_future_destructor() -> TestResult {
    let lane = {
        let beaver = Beaver::new("drop-lane-panic", 8)?;
        beaver.create_lane(
            LaneConfig::new("drop-lane-panic")
                .capacity(1)
                .concurrency(1),
        )?
    };
    let mut running = lane.try_spawn(waiting_spec())?;
    drive_until(|| matches!(running.state(), TaskState::Running { .. })).await;
    let bomb = PanicOnDrop;
    let mut queued = lane.try_spawn_future(async move {
        let _bomb = bomb;
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok::<_, &'static str>(())
    })?;

    let drop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(lane)));
    let queued_exit = tokio::time::timeout(TERMINAL_TIMEOUT, queued.join())
        .await
        .expect("panicking user drop must not prevent terminal publication")?;
    assert!(matches!(
        queued_exit,
        TaskExit::Cancelled {
            reason: CancelReason::ExecutorShutdown
        }
    ));
    assert!(
        drop_result.is_ok(),
        "user destructor panic must not escape executor Drop"
    );

    let running_exit = tokio::time::timeout(TERMINAL_TIMEOUT, running.join())
        .await
        .expect("other executions must still be cleaned")?;
    assert!(matches!(
        running_exit,
        TaskExit::Cancelled {
            reason: CancelReason::ExecutorShutdown
        }
    ));
    Ok(())
}

#[tokio::test]
async fn cancel_all_covers_direct_lane_queue_and_retry_without_closing_admission() -> TestResult {
    let beaver = Beaver::new("typed-cancel-all", 8)?;
    let serial = beaver.create_lane(
        LaneConfig::new("typed-cancel-all-serial")
            .capacity(2)
            .concurrency(1),
    )?;
    let retry_lane = beaver.create_lane(
        LaneConfig::new("typed-cancel-all-retry")
            .capacity(2)
            .concurrency(1),
    )?;

    let mut direct = beaver.spawn(waiting_spec())?;
    let mut running = serial.try_spawn(waiting_spec())?;
    drive_until(|| matches!(running.state(), TaskState::Running { .. })).await;
    let mut queued = serial.try_spawn(waiting_spec())?;

    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let retry = RetryBuilder::new(move |_| {
        attempts_c.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>("transient") }
    })
    .max_attempts(2)
    .fixed_delay(Duration::from_secs(3_600))
    .retry_all_errors()
    .build()?;
    let mut retrying = retry_lane.try_spawn_retry(retry)?;
    drive_until(|| matches!(retrying.state(), TaskState::Sleeping { .. })).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    beaver.cancel_all().await?;

    for handle in [&mut direct, &mut running, &mut queued, &mut retrying] {
        let exit = tokio::time::timeout(TERMINAL_TIMEOUT, handle.join())
            .await
            .expect("cancel_all must terminalize its typed snapshot")?;
        assert!(matches!(
            exit,
            TaskExit::Cancelled {
                reason: CancelReason::UserRequested
            }
        ));
    }

    let mut after_direct = beaver.spawn_future(async { Ok::<_, &'static str>(11_u8) })?;
    let mut after_lane = serial.try_spawn_future(async { Ok::<_, &'static str>(12_u8) })?;
    assert!(matches!(
        after_direct.join().await?,
        TaskExit::Completed(11)
    ));
    assert!(matches!(after_lane.join().await?, TaskExit::Completed(12)));
    beaver.destroy().await?;
    Ok(())
}
