#![allow(dead_code)]

use busybeaver::{
    AttemptContext, Backoff, Beaver, CancelReason, Cancelled, Lane, LaneConfig, RetryBuilder,
    RetryFailure, ShutdownMode, ShutdownOptions, ShutdownOutcome, ShutdownTimeoutAction,
    SpawnError, TaskControlHandle, TaskExit, TaskFailure, TaskSpec,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

async fn identity_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let spec = TaskSpec::new(|ctx| async move {
        ctx.cancelled().await;
        Ok::<_, String>(())
    });
    let mut first = beaver.spawn(spec.clone())?;
    let mut second = beaver.spawn(spec)?;
    if first.execution_id() == second.execution_id() {
        return Err("two executions unexpectedly shared an id".into());
    }
    if first.task_spec_id() != second.task_spec_id() {
        return Err("the reused spec unexpectedly changed identity".into());
    }
    first.control().cancel(CancelReason::UserRequested);
    let first_exit = first.join().await?;
    if !matches!(first_exit, TaskExit::Cancelled { .. }) {
        return Err("first execution was expected to be cancelled".into());
    }
    second.control().cancel(CancelReason::UserRequested);
    let _second_exit = second.join().await?;
    beaver.destroy().await?;
    Ok(())
}

async fn handle_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let mut handle = beaver.spawn_future(async { Ok::<_, String>(42_u32) })?;
    let first_summary = handle.wait().await;
    let second_summary = handle.wait().await;
    if first_summary != second_summary {
        return Err("terminal summary changed".into());
    }
    match handle.join().await? {
        TaskExit::Completed(value) => println!("value={value}"),
        TaskExit::Failed(_) => eprintln!("business operation failed"),
        TaskExit::Cancelled { reason } => eprintln!("cancelled: {reason:?}"),
        TaskExit::Panicked { source, .. } => eprintln!("panicked at {source:?}"),
        TaskExit::ExecutorStopped { reason } => eprintln!("runtime stopped: {reason:?}"),
        _ => eprintln!("another non-success terminal state"),
    }
    let pending =
        beaver.spawn_future(async { std::future::pending::<Result<(), String>>().await })?;
    let control = pending.control();
    let cancel_guard = pending.cancel_on_drop(CancelReason::UserRequested);
    drop(cancel_guard);
    let _summary = control.wait().await;
    beaver.destroy().await?;
    Ok(())
}

async fn context_and_child_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let mut handle = beaver.spawn(TaskSpec::new(|ctx| async move {
        ctx.sleep(Duration::from_secs(60)).await?;
        Ok::<_, Cancelled>(())
    }))?;
    let _summary = handle.cancel_and_wait(CancelReason::UserRequested).await;
    let _exit = handle.join().await?;

    let spec = TaskSpec::new(|ctx| async move {
        let mut child = ctx.spawn_child(async { Ok::<_, String>(7_u32) })?;
        let value = match child.join().await? {
            TaskExit::Completed(value) => value,
            _ => {
                return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    "child did not complete".into(),
                )
            }
        };
        println!("child value={value}");
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let mut child_parent = beaver.spawn(spec)?;
    let _exit = child_parent.join().await?;
    beaver.destroy().await?;
    Ok(())
}

async fn lane_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let http = beaver.create_lane(LaneConfig::new("http").capacity(512).concurrency(4))?;
    let pull = beaver.create_lane(LaneConfig::new("message-pull").capacity(64).concurrency(1))?;
    let spec = TaskSpec::new(|_| async { Ok::<_, String>("ok") });
    let _handle = http.try_spawn(spec.clone())?;
    let _handle = pull.spawn(spec.clone()).await?;
    match http.spawn_timeout(spec, Duration::from_millis(200)).await {
        Ok(_handle) => {}
        Err(SpawnError::AdmissionTimedOut) => eprintln!("http lane overloaded"),
        Err(other) => return Err(other.into()),
    }
    http.close_and_cancel().await;
    pull.close_and_cancel().await;
    beaver.destroy().await?;
    Ok(())
}

#[derive(Debug)]
enum EngineError {
    Network,
    Unauthorized,
}

async fn request_once(attempt: AttemptContext) -> Result<String, EngineError> {
    println!("attempt={}", attempt.number());
    Err(EngineError::Network)
}

async fn retry_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let lane = beaver.create_lane(LaneConfig::new("http-retry").capacity(512).concurrency(4))?;
    let retry = RetryBuilder::new(request_once)
        .max_attempts(5)
        .backoff(Backoff::exponential(
            Duration::from_millis(500),
            2.0,
            Duration::from_secs(5),
        ))
        .retry_if(|decision| matches!(decision.error, EngineError::Network))
        .attempt_timeout(Duration::from_secs(8))
        .overall_timeout(Duration::from_secs(30))
        .tag("engine-http")
        .build()?;
    let mut handle = lane.spawn_retry(retry).await?;
    match handle.join().await? {
        TaskExit::Completed(response) => println!("response={response}"),
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::NonRetryable { error })) => {
            eprintln!("not retryable: {error:?}");
        }
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::Exhausted { last_error })) => {
            eprintln!("retry exhausted: {last_error:?}");
        }
        TaskExit::Failed(TaskFailure::Retry(RetryFailure::AttemptTimedOut {
            attempt,
            may_have_side_effects,
            ..
        })) => {
            eprintln!("attempt {attempt} timed out; side effects={may_have_side_effects}");
        }
        TaskExit::Failed(TaskFailure::DeadlineExceeded { last_error }) => {
            eprintln!(
                "overall deadline exceeded; had error={}",
                last_error.is_some()
            );
        }
        TaskExit::Cancelled { reason } => eprintln!("cancelled: {reason:?}"),
        _ => eprintln!("retry ended with another lifecycle result"),
    }
    beaver.destroy().await?;
    Ok(())
}

async fn shutdown_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("engine", 256);
    let shutdown = beaver.shutdown(
        ShutdownOptions::new()
            .mode(ShutdownMode::CancelAll)
            .grace_period(Duration::from_secs(2))
            .on_timeout(ShutdownTimeoutAction::ReportAndKeepTracked),
    )?;
    match shutdown.wait_grace_outcome().await? {
        ShutdownOutcome::Stopped(report) => println!("stopped in {:?}", report.elapsed),
        ShutdownOutcome::TimedOut {
            shutdown: continuing,
            ..
        } => {
            let _continuing = continuing;
        }
        _ => eprintln!("future shutdown outcome"),
    }
    Ok(())
}

#[derive(Default)]
struct SerialTaskSlot {
    replace_gate: Mutex<()>,
    current: Mutex<Option<TaskControlHandle>>,
}

impl SerialTaskSlot {
    async fn replace(&self, lane: &Lane, spec: TaskSpec<(), String>) -> Result<(), SpawnError> {
        let _replace = self.replace_gate.lock().await;
        if let Some(old) = self.current.lock().await.take() {
            let _old_exit = old.cancel_and_wait(CancelReason::Replaced).await;
        }
        let new_control = lane.spawn(spec).await?.detach();
        *self.current.lock().await = Some(new_control);
        Ok(())
    }
}

#[derive(Default)]
struct SessionTasks {
    controls: Mutex<Vec<TaskControlHandle>>,
}

impl SessionTasks {
    async fn register(&self, control: TaskControlHandle) {
        self.controls.lock().await.push(control);
    }

    async fn cancel_and_wait_all(&self) {
        let controls = std::mem::take(&mut *self.controls.lock().await);
        for control in &controls {
            control.cancel(CancelReason::ScopeCancelled);
        }
        for control in controls {
            let _summary = control.wait().await;
        }
    }
}

async fn dynamic_loop_example() -> Result<(), Box<dyn std::error::Error>> {
    let beaver = Beaver::new("loona-engine", 256);
    let delays = Arc::new([
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_millis(1500),
    ]);
    let spec = TaskSpec::new(move |ctx| {
        let delays = Arc::clone(&delays);
        async move {
            let mut tick = 0_usize;
            loop {
                let index = tick.min(delays.len() - 1);
                ctx.sleep(delays[index]).await?;
                tick = tick.saturating_add(1);
            }
            #[allow(unreachable_code)]
            Ok::<_, Cancelled>(())
        }
    });
    let control = beaver.spawn(spec)?.detach();
    let _summary = control.cancel_and_wait(CancelReason::Replaced).await;
    beaver.destroy().await?;
    Ok(())
}

#[test]
fn migration_examples_remain_type_checked() {
    let _ = identity_example;
    let _ = handle_example;
    let _ = context_and_child_example;
    let _ = lane_example;
    let _ = retry_example;
    let _ = shutdown_example;
    let _ = dynamic_loop_example;
}
