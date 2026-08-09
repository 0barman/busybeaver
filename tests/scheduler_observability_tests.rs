use busybeaver::{
    FirstRun, Job, LaneConfig, MetricsHook, Schedule, ScheduledJob, Scheduler, SchedulerBuildError,
    ShutdownPolicy, TaskControl, TaskEvent, TaskEventKind, TaskState, TaskTerminal,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[tokio::test]
async fn terminal_event_follows_authoritative_snapshot_and_sequences_are_monotonic() {
    let scheduler = Scheduler::builder().build().unwrap();
    let mut events = scheduler.subscribe_events();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(7_u8) }))
        .await
        .unwrap();
    let observer = handle.observer();
    let mut prior_sequence = 0;

    loop {
        let event = events.recv().await.unwrap();
        assert!(event.sequence() > prior_sequence);
        prior_sequence = event.sequence();
        assert_eq!(event.run_id(), handle.id());
        if event.kind() == TaskEventKind::Terminal {
            assert_eq!(event.state(), TaskState::Completed);
            let snapshot = observer.snapshot();
            assert_eq!(snapshot.state(), TaskState::Completed);
            assert_eq!(snapshot.snapshot_version(), event.sequence());
            assert_eq!(snapshot.observed_at(), event.observed_at());
            break;
        }
    }

    assert!(matches!(handle.join().await, TaskTerminal::Completed(7)));
}

struct PanicHook(AtomicBool);

impl MetricsHook for PanicHook {
    fn on_event(&self, _: &TaskEvent) {
        if !self.0.swap(true, Ordering::AcqRel) {
            panic!("metrics hook panic must be isolated");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_hook_panic_is_isolated_and_does_not_change_task_terminal() {
    let scheduler = Scheduler::builder()
        .metrics_hook(Arc::new(PanicHook(AtomicBool::new(false))))
        .build()
        .unwrap();

    let first = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(1_u8) }))
        .await
        .unwrap();
    let second = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(2_u8) }))
        .await
        .unwrap();
    assert!(matches!(first.join().await, TaskTerminal::Completed(1)));
    assert!(matches!(second.join().await, TaskTerminal::Completed(2)));

    let report = scheduler.shutdown().await;
    assert!(report.is_complete());
    assert!(scheduler.snapshot().observation_failures() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_task_events_are_delivered_in_global_sequence_order() {
    const RUNS: usize = 24;
    let scheduler = Scheduler::builder()
        .event_capacity(512)
        .default_lane(LaneConfig::new(RUNS, RUNS).unwrap())
        .build()
        .unwrap();
    let mut events = scheduler.subscribe_events();
    let gate = Arc::new(tokio::sync::Barrier::new(RUNS + 1));
    let mut handles = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let gate = Arc::clone(&gate);
        handles.push(
            scheduler
                .submit(Job::once(move |_| async move {
                    gate.wait().await;
                    Ok::<_, ()>(())
                }))
                .await
                .unwrap(),
        );
    }
    gate.wait().await;
    for handle in handles {
        assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    }

    let mut terminals = 0;
    let mut previous = 0;
    while terminals < RUNS {
        let event = events.recv().await.unwrap();
        assert!(event.sequence() > previous);
        previous = event.sequence();
        if event.kind() == TaskEventKind::Terminal {
            terminals += 1;
        }
    }
}

struct BlockingHook {
    entered: AtomicBool,
    gate: (Mutex<bool>, Condvar),
}

impl BlockingHook {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
        }
    }

    fn release(&self) {
        *self
            .gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.gate.1.notify_all();
    }
}

impl MetricsHook for BlockingHook {
    fn on_event(&self, _: &TaskEvent) {
        if self.entered.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut released = self
            .gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = self
                .gate
                .1
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_metrics_hook_never_blocks_task_execution() {
    let hook = Arc::new(BlockingHook::new());
    let scheduler = Scheduler::builder()
        .event_capacity(16)
        .metrics_hook(hook.clone())
        .build()
        .unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();

    while !hook.entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    let terminal = tokio::time::timeout(Duration::from_secs(1), handle.join())
        .await
        .expect("task execution must not wait for the metrics hook");
    assert!(matches!(terminal, TaskTerminal::Completed(())));

    hook.release();
    assert!(scheduler.shutdown().await.is_complete());
}

#[tokio::test]
async fn lagging_event_receiver_is_bounded_and_counts_overwritten_deliveries() {
    let scheduler = Scheduler::builder().event_capacity(2).build().unwrap();
    let mut events = scheduler.subscribe_events();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));

    assert!(events.try_recv().unwrap().is_some());
    assert!(scheduler.snapshot().dropped_event_deliveries() > 0);
}

#[tokio::test]
async fn events_do_not_expose_arbitrary_task_keys_or_terminal_payloads() {
    const PRIVATE_MARKER: &str = "private-key-marker-7f73";
    let scheduler = Scheduler::builder().build().unwrap();
    let mut events = scheduler.subscribe_events();
    let handle = scheduler
        .start_if_absent(
            PRIVATE_MARKER,
            Job::once(|_| async { Err::<(), _>("private-error-marker-91c2") }),
        )
        .unwrap();

    assert!(matches!(handle.join().await, TaskTerminal::Failed(_)));
    loop {
        let event = events.recv().await.unwrap();
        let rendered = format!("{event:?}");
        assert!(!rendered.contains(PRIVATE_MARKER));
        assert!(!rendered.contains("private-error-marker-91c2"));
        if event.kind() == TaskEventKind::Terminal {
            break;
        }
    }
}

#[tokio::test]
async fn scheduler_snapshot_reports_documented_resource_counts_and_versions() {
    let scheduler = Scheduler::builder().build().unwrap();
    let first = scheduler.snapshot();
    assert_eq!(first.active_tasks(), 0);
    assert_eq!(first.lanes(), 1);
    assert_eq!(first.groups(), 0);

    let group = scheduler.create_group("observed").unwrap();
    let handle = group
        .submit(Job::once(|context| async move {
            context.cancelled().await;
            Ok::<_, ()>(())
        }))
        .await
        .unwrap();
    while handle.observer().state() != TaskState::Running {
        tokio::task::yield_now().await;
    }
    let active = scheduler.snapshot();
    assert!(active.snapshot_version() > first.snapshot_version());
    assert_eq!(active.active_tasks(), 1);
    assert_eq!(active.running_tasks(), 1);
    assert_eq!(active.queued_tasks(), 0);
    assert_eq!(active.waiting_tasks(), 0);
    assert_eq!(active.cancel_requested_tasks(), 0);
    assert!(!active.is_shutting_down());
    assert_eq!(active.groups(), 1);
    assert_eq!(
        handle.observer().snapshot().group_generation(),
        Some(group.generation())
    );

    let report = scheduler.shutdown().await;
    assert!(report.is_complete());
    assert_eq!(scheduler.snapshot().active_tasks(), 0);
}

#[tokio::test(start_paused = true)]
async fn retry_and_schedule_waits_publish_their_next_monotonic_wake_time() {
    let scheduler = Scheduler::builder().build().unwrap();
    let schedule = Schedule::fixed_delay(
        Duration::from_secs(60),
        FirstRun::After(Duration::from_secs(15)),
    )
    .unwrap();
    let scheduled = ScheduledJob::new(schedule, |_| async {
        Ok::<_, ()>(TaskControl::Complete(()))
    });
    let handle = scheduler.submit(scheduled.instantiate()).await.unwrap();
    let observer = handle.observer();
    while observer.state() != TaskState::WaitingForSchedule {
        tokio::task::yield_now().await;
    }

    let snapshot = observer.snapshot();
    assert_eq!(
        snapshot.next_wake_at(),
        Some(snapshot.observed_at() + Duration::from_secs(15))
    );
    let aggregate = scheduler.snapshot();
    assert_eq!(aggregate.waiting_tasks(), 1);

    tokio::time::advance(Duration::from_secs(15)).await;
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
}

#[tokio::test]
async fn invalid_event_capacity_is_rejected_without_panicking() {
    assert!(matches!(
        Scheduler::builder().event_capacity(0).build(),
        Err(SchedulerBuildError::InvalidEventCapacity { capacity: 0, .. })
    ));
    assert!(matches!(
        Scheduler::builder().event_capacity(1_048_577).build(),
        Err(SchedulerBuildError::InvalidEventCapacity {
            capacity: 1_048_577,
            maximum: 1_048_576
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn event_drain_timeout_is_independent_from_task_grace_period() {
    let hook = Arc::new(BlockingHook::new());
    let scheduler = Scheduler::builder()
        .metrics_hook(hook.clone())
        .shutdown_policy(
            ShutdownPolicy::graceful(Duration::from_secs(30))
                .with_event_drain(Duration::from_secs(2)),
        )
        .build()
        .unwrap();
    let handle = scheduler
        .submit(Job::once(|_| async { Ok::<_, ()>(()) }))
        .await
        .unwrap();
    assert!(matches!(handle.join().await, TaskTerminal::Completed(())));
    while !hook.entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    let shutdown = scheduler.shutdown();
    tokio::pin!(shutdown);
    tokio::select! {
        _ = shutdown.as_mut() => panic!("event drain must honor its configured timeout"),
        _ = tokio::task::yield_now() => {}
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(shutdown.await.is_complete());
    hook.release();
}
