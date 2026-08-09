use busybeaver::{
    CancelReason, LaneConfig, Scheduler, Singleflight, SubmissionFailure, TaskTerminal,
};
use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{Barrier, Notify};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_key_concurrent_callers_share_one_execution_and_arc_result() {
    const WAITERS: usize = 16;
    let scheduler = Scheduler::builder().build().unwrap();
    let flight = scheduler.singleflight::<String, usize, ()>();
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Barrier::new(WAITERS + 1));
    let mut waiters = Vec::new();
    for _ in 0..WAITERS {
        let flight = flight.clone();
        let calls = Arc::clone(&calls);
        let gate = Arc::clone(&gate);
        waiters.push(tokio::spawn(async move {
            gate.wait().await;
            flight
                .run("shared".to_owned(), move |_| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(7)
                })
                .await
        }));
    }
    gate.wait().await;
    let mut results = Vec::new();
    for waiter in waiters {
        results.push(waiter.await.unwrap());
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(results
        .iter()
        .all(|result| matches!(&**result, TaskTerminal::Completed(7))));
    assert!(results
        .windows(2)
        .all(|pair| Arc::ptr_eq(&pair[0], &pair[1])));
    assert_eq!(flight.in_flight_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_keys_can_execute_concurrently_within_lane_limits() {
    let scheduler = Scheduler::builder()
        .global_concurrency(2)
        .default_lane(LaneConfig::new(2, 2).unwrap())
        .build()
        .unwrap();
    let flight = scheduler.singleflight::<u8, (), ()>();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut waiters = Vec::new();
    for key in [1, 2] {
        let flight = flight.clone();
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        waiters.push(tokio::spawn(async move {
            flight
                .run(key, move |_| async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(now, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
        }));
    }
    started.notified().await;
    started.notified().await;
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    release.notify_waiters();
    for waiter in waiters {
        assert!(matches!(
            &*waiter.await.unwrap(),
            TaskTerminal::Completed(())
        ));
    }
}

#[tokio::test]
async fn dropping_one_or_all_waiters_does_not_cancel_the_leader() {
    let scheduler = Scheduler::builder().build().unwrap();
    let flight = scheduler.singleflight::<&'static str, (), ()>();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let leader = {
        let flight = flight.clone();
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            flight
                .run("key", move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(())
                })
                .await
        })
    };
    started.notified().await;
    {
        let follower = flight.run("key", |_| async { panic!("follower work must not run") });
        tokio::pin!(follower);
        tokio::select! {
            biased;
            _ = follower.as_mut() => panic!("leader is still blocked"),
            _ = tokio::task::yield_now() => {}
        }
    }
    release.notify_one();

    assert!(matches!(
        &*leader.await.unwrap(),
        TaskTerminal::Completed(())
    ));
    assert_eq!(flight.in_flight_count(), 0);
}

#[tokio::test]
async fn completed_key_is_removed_and_can_start_a_fresh_execution() {
    let scheduler = Scheduler::builder().build().unwrap();
    let flight = scheduler.singleflight::<u8, usize, ()>();
    let calls = Arc::new(AtomicUsize::new(0));
    for expected in [1, 2] {
        let calls = Arc::clone(&calls);
        let result = flight
            .run(1, move |_| async move {
                Ok(calls.fetch_add(1, Ordering::SeqCst) + 1)
            })
            .await;
        assert!(matches!(&*result, TaskTerminal::Completed(value) if *value == expected));
        assert_eq!(flight.in_flight_count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_panic_wakes_every_waiter_with_the_same_terminal() {
    let scheduler = Scheduler::builder().build().unwrap();
    let flight = scheduler.singleflight::<u8, (), ()>();
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let leader = {
        let flight = flight.clone();
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            flight
                .run(1, move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    panic!("leader panic")
                })
                .await
        })
    };
    started.notified().await;
    let follower = flight.run(1, |_| pending::<Result<(), ()>>());
    tokio::pin!(follower);
    tokio::select! {
        biased;
        _ = follower.as_mut() => panic!("leader is still blocked"),
        _ = tokio::task::yield_now() => {}
    }
    release.notify_one();

    let first = leader.await.unwrap();
    let second = follower.await;
    assert!(matches!(&*first, TaskTerminal::Panicked(_)));
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn group_shutdown_cancels_singleflight_leader_and_wakes_followers() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("flight").unwrap();
    let flight: Singleflight<&'static str, (), ()> = group.singleflight();
    let started = Arc::new(Notify::new());
    let leader = {
        let flight = flight.clone();
        let started = Arc::clone(&started);
        tokio::spawn(async move {
            flight
                .run("key", move |context| async move {
                    started.notify_one();
                    context.cancelled().await;
                    Ok(())
                })
                .await
        })
    };
    started.notified().await;
    let follower = flight.run("key", |_| async { Ok(()) });
    tokio::pin!(follower);
    tokio::select! {
        biased;
        _ = follower.as_mut() => panic!("leader is still blocked"),
        _ = tokio::task::yield_now() => {}
    }

    assert!(group.shutdown().await.is_complete());
    let first = leader.await.unwrap();
    let second = follower.await;
    assert!(matches!(
        &*first,
        TaskTerminal::Cancelled {
            reason: CancelReason::GroupShutdown
        }
    ));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(group.active_task_count(), 0);
    assert_eq!(flight.in_flight_count(), 0);
}

#[tokio::test]
async fn scheduler_submission_failure_has_a_typed_reason() {
    let scheduler = Scheduler::builder().build().unwrap();
    let flight = scheduler.singleflight::<u8, (), ()>();
    assert!(scheduler.shutdown().await.is_complete());

    let terminal = flight.run(1, |_| async { Ok(()) }).await;

    assert!(matches!(
        &*terminal,
        TaskTerminal::SubmissionFailed {
            reason: SubmissionFailure::Closed
        }
    ));
    assert_eq!(flight.in_flight_count(), 0);
}

#[tokio::test]
async fn closed_group_singleflight_submission_has_a_typed_reason() {
    let scheduler = Scheduler::builder().build().unwrap();
    let group = scheduler.create_group("closed-flight").unwrap();
    let flight = group.singleflight::<u8, (), ()>();
    assert!(group.shutdown().await.is_complete());

    let terminal = flight.run(1, |_| async { Ok(()) }).await;

    assert!(matches!(
        &*terminal,
        TaskTerminal::SubmissionFailed {
            reason: SubmissionFailure::Closed
        }
    ));
    assert_eq!(flight.in_flight_count(), 0);
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn singleflight_handle_is_send_and_sync_when_key_and_result_are() {
    assert_send_sync::<Singleflight<String, usize, String>>();
}

#[test]
fn stopped_bound_runtime_wakes_singleflight_waiters_and_removes_key() {
    let bound = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let scheduler = Scheduler::builder()
        .runtime_handle(bound.handle().clone())
        .build()
        .unwrap();
    let flight = scheduler.singleflight::<String, (), ()>();
    let waiter_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (started, started_rx) = std::sync::mpsc::channel();
    let waiter = waiter_runtime.spawn({
        let flight = flight.clone();
        async move {
            flight
                .run("key".to_owned(), move |_| async move {
                    started.send(()).unwrap();
                    pending::<Result<(), ()>>().await
                })
                .await
        }
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("leader must start on the bound runtime");

    drop(bound);

    let terminal = waiter_runtime
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(250), waiter).await
        })
        .expect("runtime shutdown must wake singleflight waiters")
        .unwrap();
    assert!(matches!(&*terminal, TaskTerminal::ExecutorStopped));
    assert_eq!(flight.in_flight_count(), 0);
}
