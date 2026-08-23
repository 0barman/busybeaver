use super::{prepare_spec, ExecutionRegistry, StopCauseSummary, WorkContext};
use crate::{CancelReason, ResourceLimits, TaskSpec};
use std::error::Error;
use std::sync::Arc;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

#[tokio::test]
async fn direct_stop_cause_matches_snapshot_for_cancel_and_deadline() -> TestResult {
    let registry = ExecutionRegistry::new_with_limits(ResourceLimits::default());
    let cancel_prepared = prepare_spec(
        &registry,
        tokio::runtime::Handle::current(),
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
    )?;
    let cancel_work = WorkContext {
        core: Arc::clone(&cancel_prepared.start.core),
        attempt: 1,
    };
    let cancel_control = cancel_prepared.handle.control();
    cancel_control.cancel(CancelReason::Other("direct-stop-cause".to_string()));
    let cancel_direct = cancel_work.stop_cause();
    let cancel_snapshot = cancel_control.snapshot().stop_cause;
    drop(cancel_prepared.start);
    cancel_prepared.handle.wait().await;
    if cancel_direct != cancel_snapshot {
        return Err(test_error("direct cancel cause differed from snapshot"));
    }

    let deadline_prepared = prepare_spec(
        &registry,
        tokio::runtime::Handle::current(),
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
    )?;
    let deadline_work = WorkContext {
        core: Arc::clone(&deadline_prepared.start.core),
        attempt: 1,
    };
    let deadline_control = deadline_prepared.handle.control();
    deadline_control.mark_deadline();
    let deadline_direct = deadline_work.stop_cause();
    let deadline_snapshot = deadline_control.snapshot().stop_cause;
    drop(deadline_prepared.start);
    deadline_prepared.handle.wait().await;
    if deadline_direct != Some(StopCauseSummary::Deadline) || deadline_direct != deadline_snapshot {
        return Err(test_error("direct deadline cause differed from snapshot"));
    }
    Ok(())
}

#[cfg(not(feature = "tracing"))]
#[tokio::test]
async fn zero_subscriber_skips_execution_snapshot_construction() -> TestResult {
    let registry = ExecutionRegistry::new_with_limits(ResourceLimits::default());
    let prepared = prepare_spec(
        &registry,
        tokio::runtime::Handle::current(),
        TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
    )?;
    let core = Arc::clone(&prepared.start.core);
    prepared.start.register()?;
    prepared.start.start();
    prepared.handle.wait().await;
    let snapshot_calls = core.snapshot_calls_for_test();
    if snapshot_calls == 0 {
        Ok(())
    } else {
        Err(test_error(format!(
            "zero-subscriber execution constructed {snapshot_calls} snapshots"
        )))
    }
}
