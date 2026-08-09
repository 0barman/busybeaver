use busybeaver::{
    DispatchOptions, DispatchQueue, DispatchQueueConfig, DispatchQueueConfigError,
    DispatchSubmitError, EvictionPolicy, Job, LaneConfig, QueueOverflowPolicy, Scheduler,
    SubmissionFailure, TaskTerminal,
};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{Notify, Semaphore};

#[test]
fn dispatch_queue_configuration_rejects_zero_limits() {
    assert_eq!(
        DispatchQueueConfig::new(0, 1),
        Err(DispatchQueueConfigError::ZeroPendingCapacity)
    );
    assert_eq!(
        DispatchQueueConfig::new(1, 0),
        Err(DispatchQueueConfigError::ZeroConcurrency)
    );
}

#[test]
fn excessive_pending_capacity_is_rejected_without_panicking() {
    let result = std::panic::catch_unwind(|| DispatchQueueConfig::new(usize::MAX, 1));

    assert!(result.is_ok(), "configuration validation must not panic");
    assert_eq!(
        result.unwrap(),
        Err(DispatchQueueConfigError::PendingCapacityTooLarge {
            capacity: usize::MAX,
            maximum: DispatchQueueConfig::MAX_PENDING_CAPACITY,
        })
    );
}

#[tokio::test]
async fn pending_capacity_upper_boundary_is_valid_and_queue_creation_is_lazy() {
    let config = DispatchQueueConfig::new(DispatchQueueConfig::MAX_PENDING_CAPACITY, 1)
        .expect("the documented upper boundary must remain valid");
    assert_eq!(
        config.pending_capacity(),
        DispatchQueueConfig::MAX_PENDING_CAPACITY
    );

    let scheduler = Scheduler::builder().build().unwrap();
    let queue: DispatchQueue<(), ()> = scheduler.dispatch_queue(config);
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(queue.close(), 0);
    drop(queue);
    assert!(scheduler.shutdown().await.is_complete());
}

#[test]
fn pending_capacity_error_display_includes_value_and_maximum() {
    let rejected = DispatchQueueConfig::MAX_PENDING_CAPACITY + 1;
    let error = DispatchQueueConfig::new(rejected, 1).unwrap_err();

    assert_eq!(
        error,
        DispatchQueueConfigError::PendingCapacityTooLarge {
            capacity: rejected,
            maximum: DispatchQueueConfig::MAX_PENDING_CAPACITY,
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "dispatch pending capacity {rejected} exceeds maximum {}",
            DispatchQueueConfig::MAX_PENDING_CAPACITY
        )
    );
}

#[tokio::test]
async fn reject_newest_returns_the_unsubmitted_job() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(1, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::RejectNewest),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let blocker = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(0_u8)
                }
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    started.notified().await;
    let queued = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(1_u8) }),
            DispatchOptions::default(),
        )
        .unwrap();
    let rejected = queue.submit(
        Job::once(|_| async { Ok::<_, ()>(2_u8) }),
        DispatchOptions::default(),
    );
    let returned = match rejected {
        Err(DispatchSubmitError::Full(job)) => job,
        _ => panic!("newest job must be returned"),
    };
    assert_eq!(queue.pending_count(), 1);

    release.notify_one();
    assert!(matches!(blocker.join().await, TaskTerminal::Completed(0)));
    assert!(matches!(queued.join().await, TaskTerminal::Completed(1)));
    let returned = queue.submit(returned, DispatchOptions::default()).unwrap();
    assert!(matches!(returned.join().await, TaskTerminal::Completed(2)));
}

#[tokio::test]
async fn evict_oldest_publishes_exactly_once_evicted_terminal() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(1, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::EvictOldest),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let blocker = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(0_u8)
                }
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    started.notified().await;
    let oldest = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(1_u8) }),
            DispatchOptions::default(),
        )
        .unwrap();
    let newest = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(2_u8) }),
            DispatchOptions::default(),
        )
        .unwrap();

    assert!(matches!(
        oldest.join().await,
        TaskTerminal::Evicted {
            policy: EvictionPolicy::Oldest
        }
    ));
    release.notify_one();
    assert!(matches!(blocker.join().await, TaskTerminal::Completed(0)));
    assert!(matches!(newest.join().await, TaskTerminal::Completed(2)));
}

#[tokio::test]
async fn lowest_priority_eviction_selects_only_a_queued_task() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(2, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::EvictLowestPriority),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let running = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(0_u8)
                }
            }),
            DispatchOptions::default().priority(0),
        )
        .unwrap();
    started.notified().await;
    let low = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(1_u8) }),
            DispatchOptions::default().priority(1),
        )
        .unwrap();
    let medium = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(2_u8) }),
            DispatchOptions::default().priority(2),
        )
        .unwrap();
    let high = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(3_u8) }),
            DispatchOptions::default().priority(3),
        )
        .unwrap();

    assert!(matches!(
        low.join().await,
        TaskTerminal::Evicted {
            policy: EvictionPolicy::LowestPriority
        }
    ));
    release.notify_one();
    assert!(matches!(running.join().await, TaskTerminal::Completed(0)));
    assert!(matches!(high.join().await, TaskTerminal::Completed(3)));
    assert!(matches!(medium.join().await, TaskTerminal::Completed(2)));
}

#[tokio::test]
async fn lowest_priority_policy_rejects_an_incoming_non_improvement() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(1, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::EvictLowestPriority),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let running = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(())
                }
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    started.notified().await;
    let queued = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default().priority(5),
        )
        .unwrap();
    let rejected = queue.submit(
        Job::once(|_| async { Ok::<_, ()>(()) }),
        DispatchOptions::default().priority(5),
    );

    assert!(matches!(rejected, Err(DispatchSubmitError::Full(_))));
    release.notify_one();
    assert!(matches!(running.join().await, TaskTerminal::Completed(())));
    assert!(matches!(queued.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn coalesce_by_key_replaces_queued_but_never_running_work() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(2, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::CoalesceByKey),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let running = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(0_u8)
                }
            }),
            DispatchOptions::default().with_key("same"),
        )
        .unwrap();
    started.notified().await;
    let old = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(1_u8) }),
            DispatchOptions::default().with_key("same"),
        )
        .unwrap();
    let replacement = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(2_u8) }),
            DispatchOptions::default().with_key("same"),
        )
        .unwrap();

    assert!(matches!(
        old.join().await,
        TaskTerminal::Evicted {
            policy: EvictionPolicy::Coalesced
        }
    ));
    release.notify_one();
    assert!(matches!(running.join().await, TaskTerminal::Completed(0)));
    assert!(matches!(
        replacement.join().await,
        TaskTerminal::Completed(2)
    ));
}

#[tokio::test]
async fn coalesce_by_key_rejects_a_job_without_a_key() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(1, 1)
            .unwrap()
            .overflow(QueueOverflowPolicy::CoalesceByKey),
    );
    let result = queue.submit(
        Job::once(|_| async { Ok::<_, ()>(()) }),
        DispatchOptions::default(),
    );

    assert!(matches!(result, Err(DispatchSubmitError::KeyRequired(_))));
    assert_eq!(queue.pending_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_concurrency_and_total_concurrency_are_both_bounded() {
    let scheduler = Scheduler::builder()
        .global_concurrency(3)
        .default_lane(LaneConfig::new(6, 3).unwrap())
        .build()
        .unwrap();
    let queue = scheduler.dispatch_queue(
        DispatchQueueConfig::new(6, 3)
            .unwrap()
            .key_concurrency(NonZeroUsize::new(1).unwrap()),
    );
    let same_active = Arc::new(AtomicUsize::new(0));
    let same_max = Arc::new(AtomicUsize::new(0));
    let total_active = Arc::new(AtomicUsize::new(0));
    let total_max = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let mut handles = Vec::new();
    for key in ["same", "same", "other"] {
        let is_same = key == "same";
        let same_active = Arc::clone(&same_active);
        let same_max = Arc::clone(&same_max);
        let total_active = Arc::clone(&total_active);
        let total_max = Arc::clone(&total_max);
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        handles.push(
            queue
                .submit(
                    Job::once(move |_| async move {
                        let total = total_active.fetch_add(1, Ordering::SeqCst) + 1;
                        total_max.fetch_max(total, Ordering::SeqCst);
                        if is_same {
                            let same = same_active.fetch_add(1, Ordering::SeqCst) + 1;
                            same_max.fetch_max(same, Ordering::SeqCst);
                        }
                        started.notify_one();
                        let permit = release.acquire().await.unwrap();
                        permit.forget();
                        if is_same {
                            same_active.fetch_sub(1, Ordering::SeqCst);
                        }
                        total_active.fetch_sub(1, Ordering::SeqCst);
                        Ok::<_, ()>(())
                    }),
                    DispatchOptions::default().with_key(key),
                )
                .unwrap(),
        );
    }
    started.notified().await;
    started.notified().await;
    assert_eq!(same_max.load(Ordering::SeqCst), 1);
    assert_eq!(total_max.load(Ordering::SeqCst), 2);
    release.add_permits(3);
    for handle in handles {
        assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    }
    assert_eq!(same_max.load(Ordering::SeqCst), 1);
    assert!(total_max.load(Ordering::SeqCst) <= 3);
}

#[tokio::test]
async fn higher_priority_dispatches_first_and_fifo_breaks_ties() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(DispatchQueueConfig::new(4, 1).unwrap());
    let blocker_started = Arc::new(Notify::new());
    let blocker_release = Arc::new(Notify::new());
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let blocker = queue
        .submit(
            Job::once({
                let started = Arc::clone(&blocker_started);
                let release = Arc::clone(&blocker_release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok::<_, ()>(())
                }
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    blocker_started.notified().await;
    let mut handles = Vec::new();
    for (value, priority) in [(1, 1), (2, 3), (3, 3)] {
        let order = Arc::clone(&order);
        handles.push(
            queue
                .submit(
                    Job::once(move |_| async move {
                        order.lock().unwrap().push(value);
                        Ok::<_, ()>(())
                    }),
                    DispatchOptions::default().priority(priority),
                )
                .unwrap(),
        );
    }
    blocker_release.notify_one();
    assert!(matches!(blocker.join().await, TaskTerminal::Completed(())));
    for handle in handles {
        assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    }
    assert_eq!(*order.lock().unwrap(), vec![2, 3, 1]);
}

#[tokio::test]
async fn dropping_last_queue_handle_stops_pending_without_detaching_it() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue: DispatchQueue<(), ()> =
        scheduler.dispatch_queue(DispatchQueueConfig::new(1, 1).unwrap());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let running = queue
        .submit(
            Job::once({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(())
                }
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    started.notified().await;
    let pending = queue
        .submit(Job::once(|_| async { Ok(()) }), DispatchOptions::default())
        .unwrap();
    drop(queue);

    assert!(matches!(
        pending.join().await,
        TaskTerminal::ExecutorStopped
    ));
    release.notify_one();
    assert!(matches!(running.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn scheduler_lane_failure_is_reported_with_typed_reason() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(DispatchQueueConfig::new(1, 1).unwrap());
    let handle = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(()) }).on_lane("missing"),
            DispatchOptions::default(),
        )
        .unwrap();

    assert!(matches!(
        handle.join().await,
        TaskTerminal::SubmissionFailed {
            reason: SubmissionFailure::LaneNotFound
        }
    ));
}

#[tokio::test]
async fn shutdown_winning_dispatch_handoff_has_a_typed_submission_failure() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(DispatchQueueConfig::new(1, 1).unwrap());
    let handle = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default(),
        )
        .unwrap();

    assert!(scheduler.shutdown().await.is_complete());
    assert!(matches!(
        handle.join().await,
        TaskTerminal::SubmissionFailed {
            reason: SubmissionFailure::ShuttingDown | SubmissionFailure::Closed
        }
    ));
}

#[tokio::test]
async fn queue_rejects_new_work_after_scheduler_shutdown() {
    let scheduler = Scheduler::builder().build().unwrap();
    let queue = scheduler.dispatch_queue(DispatchQueueConfig::new(1, 1).unwrap());
    assert!(scheduler.shutdown().await.is_complete());

    assert!(matches!(
        queue.submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default(),
        ),
        Err(DispatchSubmitError::Closed(_))
    ));
    assert_eq!(queue.pending_count(), 0);
}

#[test]
fn stopped_bound_runtime_closes_queue_and_releases_dispatch_accounting() {
    let bound = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let scheduler = Scheduler::builder()
        .runtime_handle(bound.handle().clone())
        .build()
        .unwrap();
    let queue = scheduler.dispatch_queue(DispatchQueueConfig::new(2, 1).unwrap());
    let (started, started_rx) = std::sync::mpsc::channel();
    let running = queue
        .submit(
            Job::once(move |_| async move {
                started.send(()).unwrap();
                std::future::pending::<Result<(), ()>>().await
            }),
            DispatchOptions::default(),
        )
        .unwrap();
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("job must start on the bound runtime");
    let pending = queue
        .submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default(),
        )
        .unwrap();

    drop(bound);

    let verifier = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    verifier.block_on(async {
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(250), running.join())
                .await
                .expect("running dispatch must settle"),
            TaskTerminal::ExecutorStopped
        ));
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(250), pending.join())
                .await
                .expect("pending dispatch must settle"),
            TaskTerminal::ExecutorStopped
        ));
    });
    assert_eq!(queue.active_count(), 0);
    assert_eq!(queue.pending_count(), 0);
    assert!(matches!(
        queue.submit(
            Job::once(|_| async { Ok::<_, ()>(()) }),
            DispatchOptions::default()
        ),
        Err(DispatchSubmitError::Closed(_))
    ));
}
