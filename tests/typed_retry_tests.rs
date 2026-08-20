use busybeaver::{
    AttemptContext, Backoff, Beaver, LaneConfig, PanicSource, RetryBuildError, RetryBuilder,
    RetryFailure, SpawnError, TaskExit, TaskFailure,
};
use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn test_lane(name: &str) -> busybeaver::Lane {
    Beaver::new(name, 8)
        .expect("valid test executor")
        .create_lane(LaneConfig::new(name).capacity(8).concurrency(1))
        .expect("valid test lane")
}

#[test]
fn retry_builder_rejects_ambiguous_or_unsafe_configuration() {
    let zero = RetryBuilder::new(|_: AttemptContext| async { Ok::<_, &'static str>(()) })
        .max_attempts(0)
        .build()
        .expect_err("zero attempts must be rejected");
    assert!(matches!(zero, RetryBuildError::InvalidAttemptCount));

    let implicit = RetryBuilder::new(|_: AttemptContext| async { Ok::<_, &'static str>(()) })
        .max_attempts(2)
        .build()
        .expect_err("multi-attempt work requires explicit retry authorization");
    assert!(matches!(implicit, RetryBuildError::RetryPolicyRequired));

    let mismatch = RetryBuilder::new(|_: AttemptContext| async { Ok::<_, &'static str>(()) })
        .max_attempts(3)
        .delays([Duration::from_secs(1)])
        .retry_all_errors()
        .build()
        .expect_err("there must be one between-attempt delay per retry");
    assert!(matches!(
        mismatch,
        RetryBuildError::DelayCountMismatch {
            expected: 2,
            actual: 1
        }
    ));

    let multiplier = RetryBuilder::new(|_: AttemptContext| async { Ok::<_, &'static str>(()) })
        .max_attempts(2)
        .backoff(Backoff::exponential(
            Duration::from_secs(2),
            f64::NAN,
            Duration::from_secs(4),
        ))
        .retry_all_errors()
        .build()
        .expect_err("non-finite multipliers must be rejected");
    assert!(matches!(
        multiplier,
        RetryBuildError::InvalidBackoffMultiplier
    ));
}

#[tokio::test(start_paused = true)]
async fn retry_attempts_are_one_based_and_backoff_is_between_failures() -> TestResult {
    let lane = test_lane("retry-attempts");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RetryBuilder::new(move |attempt: AttemptContext| {
        let attempts = Arc::clone(&attempts_c);
        async move {
            let observed = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(attempt.number() as usize, observed);
            if attempt.number() < 3 {
                Err("transient")
            } else {
                Ok(42_u32)
            }
        }
    })
    .max_attempts(3)
    .fixed_delay(Duration::from_secs(1))
    .retry_all_errors()
    .build()?;

    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;

    assert!(matches!(handle.join().await?, TaskExit::Completed(42)));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    Ok(())
}

#[derive(PartialEq)]
struct OwnedError(&'static str);

#[tokio::test]
async fn non_retryable_and_exhausted_exits_retain_the_owned_business_error() -> TestResult {
    let lane = test_lane("retry-owned-error");
    let non_retryable =
        RetryBuilder::new(|_: AttemptContext| async { Err::<(), _>(OwnedError("permanent")) })
            .max_attempts(3)
            .retry_if(|decision| decision.error.0 == "transient")
            .build()?;
    let mut handle = lane.spawn_retry(non_retryable).await?;
    match handle.join().await? {
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::NonRetryable { error })) => {
            assert!(error == OwnedError("permanent"));
        }
        _ => panic!("expected an owned non-retryable error"),
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_c = Arc::clone(&counter);
    let exhausted = RetryBuilder::new(move |_: AttemptContext| {
        let attempt = counter_c.fetch_add(1, Ordering::SeqCst) + 1;
        async move { Err::<(), _>(OwnedError(if attempt == 1 { "first" } else { "last" })) }
    })
    .max_attempts(2)
    .retry_all_errors()
    .build()?;
    let mut handle = lane.spawn_retry(exhausted).await?;
    match handle.join().await? {
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::Exhausted { last_error })) => {
            assert!(last_error == OwnedError("last"));
        }
        _ => panic!("expected the final owned error"),
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_backoff_prevents_the_next_attempt() -> TestResult {
    let lane = test_lane("retry-cancel-backoff");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RetryBuilder::new(move |_: AttemptContext| {
        attempts_c.fetch_add(1, Ordering::SeqCst);
        async { Err::<(), _>("retry") }
    })
    .max_attempts(2)
    .fixed_delay(Duration::from_secs(60))
    .retry_all_errors()
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    handle
        .control()
        .cancel(busybeaver::CancelReason::UserRequested);

    assert!(matches!(
        handle.join().await?,
        TaskExit::Cancelled {
            reason: busybeaver::CancelReason::UserRequested
        }
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn attempt_timeout_does_not_retry_without_explicit_acknowledgement() -> TestResult {
    let lane = test_lane("retry-attempt-timeout");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_c = Arc::clone(&attempts);
    let spec = RetryBuilder::new(move |_: AttemptContext| {
        let number = attempts_c.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if number == 1 {
                Err("first")
            } else {
                pending::<Result<(), &'static str>>().await
            }
        }
    })
    .max_attempts(3)
    .retry_all_errors()
    .attempt_timeout(Duration::from_secs(1))
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;

    match handle.join().await? {
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::AttemptTimedOut {
            attempt,
            previous_error,
            may_have_side_effects,
        })) => {
            assert_eq!(attempt, 2);
            assert_eq!(previous_error, Some("first"));
            assert!(may_have_side_effects);
        }
        _ => panic!("expected a structured in-flight timeout"),
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn overall_timeout_before_admission_creates_no_ghost_execution() -> TestResult {
    let beaver = Beaver::new("retry-admission-deadline", 8)?;
    let lane = beaver.create_lane(
        LaneConfig::new("retry-admission-deadline")
            .capacity(1)
            .concurrency(1),
    )?;
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker_release_c = Arc::clone(&blocker_release);
    let blocker = lane
        .try_spawn_future(async move {
            blocker_release_c.notified().await;
            Ok::<_, ()>(())
        })?
        .control();
    tokio::task::yield_now().await;
    let queued = lane
        .try_spawn_future(pending::<Result<(), ()>>())?
        .control();

    let ran = Arc::new(AtomicUsize::new(0));
    let ran_c = Arc::clone(&ran);
    let spec = RetryBuilder::new(move |_: AttemptContext| {
        ran_c.fetch_add(1, Ordering::SeqCst);
        async { Ok::<_, ()>(()) }
    })
    .overall_timeout(Duration::from_secs(1))
    .build()?;
    let admission = lane.spawn_retry(spec);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        admission.await,
        Err(SpawnError::AdmissionDeadlineExceeded)
    ));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(lane.stats().queued_live, 1);

    queued.cancel(busybeaver::CancelReason::UserRequested);
    blocker_release.notify_one();
    blocker.wait().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn deadline_after_admission_is_a_typed_failure_with_last_error() -> TestResult {
    let lane = test_lane("retry-running-deadline");
    let spec = RetryBuilder::new(|_: AttemptContext| async { Err::<(), _>("last") })
        .max_attempts(2)
        .fixed_delay(Duration::from_secs(60))
        .retry_all_errors()
        .overall_timeout(Duration::from_secs(1))
        .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;

    match handle.join().await? {
        TaskExit::Failed(TaskFailure::DeadlineExceeded { last_error }) => {
            assert_eq!(last_error, Some("last"));
        }
        _ => panic!("expected an execution deadline failure"),
    }
    Ok(())
}

#[tokio::test]
async fn retry_predicate_panic_preserves_last_business_error() -> TestResult {
    let lane = test_lane("retry-policy-panic");
    let spec = RetryBuilder::new(|_: AttemptContext| async { Err::<(), _>("sensitive") })
        .max_attempts(2)
        .retry_if(|_| panic!("predicate failed"))
        .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    match handle.join().await? {
        TaskExit::Failed(TaskFailure::PolicyPanicked { stage, last_error }) => {
            assert_eq!(stage, busybeaver::RetryPolicyStage::Predicate);
            assert_eq!(last_error, Some("sensitive"));
        }
        _ => panic!("expected a policy failure that retains the business error"),
    }
    Ok(())
}

#[tokio::test]
async fn retry_operation_future_panic_remains_an_outer_panic_exit() -> TestResult {
    let lane = test_lane("retry-operation-panic");
    let spec = RetryBuilder::new(|_: AttemptContext| async move {
        panic!("operation panic");
        #[allow(unreachable_code)]
        Ok::<(), &'static str>(())
    })
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Panicked {
            source: PanicSource::WorkFuture,
            ..
        }
    ));
    Ok(())
}

#[tokio::test]
async fn retry_operation_factory_panic_has_a_distinct_source() -> TestResult {
    let lane = test_lane("retry-factory-panic");
    let spec = RetryBuilder::new(|_: AttemptContext| {
        panic!("factory panic");
        #[allow(unreachable_code)]
        async {
            Ok::<(), &'static str>(())
        }
    })
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Panicked {
            source: PanicSource::Factory,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn overall_deadline_wins_when_attempt_timeout_is_the_same_instant() -> TestResult {
    let lane = test_lane("retry-equal-deadlines");
    let spec = RetryBuilder::new(|_: AttemptContext| async {
        pending::<Result<(), &'static str>>().await
    })
    .attempt_timeout(Duration::from_secs(1))
    .overall_timeout(Duration::from_secs(1))
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        handle.join().await?,
        TaskExit::Failed(TaskFailure::DeadlineExceeded { .. })
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn timed_out_attempt_children_finish_before_the_next_attempt() -> TestResult {
    struct ActiveGuard(Arc<AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    let lane = test_lane("retry-attempt-child-cleanup");
    let active = Arc::new(AtomicUsize::new(0));
    let active_c = Arc::clone(&active);
    let spec = RetryBuilder::new(move |attempt: AttemptContext| {
        let active = Arc::clone(&active_c);
        async move {
            if attempt.number() == 1 {
                let child_active = Arc::clone(&active);
                attempt.spawn_child(async move {
                    child_active.fetch_add(1, Ordering::SeqCst);
                    let _guard = ActiveGuard(child_active);
                    pending::<Result<(), ()>>().await
                })?;
                pending::<Result<u32, busybeaver::SpawnChildError>>().await
            } else {
                assert_eq!(active.load(Ordering::SeqCst), 0);
                Ok(7)
            }
        }
    })
    .max_attempts(2)
    .retry_all_errors()
    .attempt_timeout(Duration::from_secs(1))
    .retry_timed_out_attempts()
    .build()?;
    let mut handle = lane.spawn_retry(spec).await?;
    tokio::task::yield_now().await;
    assert_eq!(active.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;

    assert!(matches!(handle.join().await?, TaskExit::Completed(7)));
    assert_eq!(active.load(Ordering::SeqCst), 0);
    Ok(())
}
