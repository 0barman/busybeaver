use busybeaver::{
    work, Backoff, Beaver, BeaverError, DispatchQueueConfig, DispatchQueueConfigError,
    ExecutorErrorCode, PeriodicBuilder, RetryPolicyError, ValidationError, WorkResult,
};
use std::error::Error;

#[test]
fn public_configuration_errors_have_stable_codes() {
    let validation = Beaver::try_new("invalid", 0);
    let Err(validation) = validation else {
        std::panic::panic_any("zero capacity unexpectedly succeeded");
    };
    assert!(matches!(
        validation,
        ValidationError::InvalidCapacity { capacity: 0, .. }
    ));
    assert_eq!(validation.code(), "BB-VAL-008");

    let dispatch = DispatchQueueConfig::new(0, 1);
    assert!(matches!(
        dispatch,
        Err(DispatchQueueConfigError::ZeroPendingCapacity)
    ));
    let Err(dispatch) = dispatch else {
        std::panic::panic_any("zero pending capacity unexpectedly succeeded");
    };
    assert_eq!(dispatch.code(), "BB-DISPATCH-CONFIG-001");

    let retry = Backoff::sequence(Vec::new(), false);
    assert!(matches!(retry, Err(RetryPolicyError::EmptySequence)));
    let Err(retry) = retry else {
        std::panic::panic_any("empty retry sequence unexpectedly succeeded");
    };
    assert_eq!(retry.code(), "BB-RETRY-004");

    assert_eq!(
        ExecutorErrorCode::JobAlreadyConsumed.as_str(),
        "BB-EXEC-001"
    );
}

#[test]
fn strict_constructor_reports_missing_runtime_with_a_code() {
    let result = Beaver::try_new("outside-runtime", 1);
    let Err(error) = result else {
        std::panic::panic_any("constructor unexpectedly found a Tokio runtime");
    };
    assert!(matches!(error, ValidationError::RuntimeUnavailable));
    assert_eq!(error.code(), "BB-VAL-009");
}

#[tokio::test]
async fn invalid_named_lane_returns_configuration_error_without_leaving_a_lane(
) -> Result<(), Box<dyn Error>> {
    let beaver = Beaver::try_new("default", 1)?;
    let rejected_task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) })).build()?;
    let rejected = beaver
        .enqueue_on_new_thread(rejected_task, "named", 0, false)
        .await;
    let Err(error) = rejected else {
        return Err("invalid named lane unexpectedly succeeded".into());
    };
    assert_eq!(error.code(), "BB-VAL-008");
    assert!(matches!(
        error,
        BeaverError::InvalidConfiguration(ValidationError::InvalidCapacity { capacity: 0, .. })
    ));

    let accepted_task = PeriodicBuilder::new(work(|| async { WorkResult::Done(()) })).build()?;
    beaver
        .enqueue_on_new_thread(accepted_task, "named", 1, false)
        .await?;
    beaver.destroy().await?;
    Ok(())
}
