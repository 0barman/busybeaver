use busybeaver::{
    Beaver, CancelReason, CancelRequestOutcome, PanicSource, TaskExit, TaskFailure, TaskSpec,
};
use std::sync::Arc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn business_error_remains_owned_by_typed_exit() -> TestResult {
    struct BusinessError(&'static str);

    let beaver = Beaver::new("business-error", 8)?;
    let mut handle = beaver.spawn_future(async { Err::<(), _>(BusinessError("secret")) })?;
    match handle.join().await? {
        TaskExit::Failed(TaskFailure::Operation { error }) => assert_eq!(error.0, "secret"),
        _ => panic!("expected typed operation failure"),
    }
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn factory_and_future_panics_have_distinct_sources_and_lane_survives() -> TestResult {
    let beaver = Beaver::new("panic-source", 8)?;
    let factory_spec: TaskSpec<(), ()> =
        TaskSpec::new(|_| -> std::future::Ready<Result<(), ()>> { panic!("factory panic") });
    let mut factory = beaver.spawn(factory_spec)?;
    assert!(matches!(
        factory.join().await?,
        TaskExit::Panicked {
            source: PanicSource::Factory,
            message
        } if message.contains("factory panic")
    ));

    let mut future = beaver.spawn_future(async {
        panic!("future panic");
        #[allow(unreachable_code)]
        Ok::<(), ()>(())
    })?;
    assert!(matches!(
        future.join().await?,
        TaskExit::Panicked {
            source: PanicSource::WorkFuture,
            message
        } if message.contains("future panic")
    ));

    let mut canary = beaver.spawn_future(async { Ok::<_, ()>(7_u8) })?;
    assert!(matches!(canary.join().await?, TaskExit::Completed(7)));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn cancel_and_completion_race_has_one_first_wins_terminal() -> TestResult {
    let beaver = Beaver::new("terminal-race", 8)?;
    let release = Arc::new(tokio::sync::Notify::new());
    let release_c = Arc::clone(&release);
    let mut cancelled = beaver.spawn_future(async move {
        release_c.notified().await;
        Ok::<_, &'static str>(99_u8)
    })?;
    assert_eq!(
        cancelled.control().cancel(CancelReason::UserRequested),
        CancelRequestOutcome::Requested
    );
    release.notify_one();
    assert!(matches!(
        cancelled.join().await?,
        TaskExit::Cancelled {
            reason: CancelReason::UserRequested
        }
    ));

    let mut completed = beaver.spawn_future(async { Ok::<_, &'static str>(1_u8) })?;
    assert!(matches!(completed.join().await?, TaskExit::Completed(1)));
    assert_eq!(
        completed.control().cancel(CancelReason::UserRequested),
        CancelRequestOutcome::AlreadyTerminal
    );
    beaver.destroy().await?;
    Ok(())
}
