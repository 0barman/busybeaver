use busybeaver::{
    Beaver, LaneConfig, ShutdownOptions, ShutdownWaitError, SpawnError, TaskExit, TaskFailure,
    TaskSpec,
};

#[test]
fn timerless_runtime_reports_typed_failure_and_lane_survives(
) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let beaver = Beaver::try_new_with_handle("timerless", 8, runtime.handle().clone())?;
    let lane = beaver.create_lane(LaneConfig::new("timerless-lane").capacity(4))?;

    let mut timed = lane.try_spawn(TaskSpec::new(|context| async move {
        context.sleep(std::time::Duration::from_millis(1)).await?;
        Ok::<_, busybeaver::Cancelled>(())
    }))?;
    let timed_exit = runtime.block_on(timed.join())?;
    assert!(matches!(
        timed_exit,
        TaskExit::Failed(TaskFailure::TimerUnavailable)
    ));

    let mut immediate = lane.try_spawn(TaskSpec::new(|_| async { Ok::<_, ()>(7_u8) }))?;
    assert!(matches!(
        runtime.block_on(immediate.join())?,
        TaskExit::Completed(7)
    ));

    assert!(matches!(
        runtime.block_on(lane.spawn_timeout(
            TaskSpec::new(|_| async { Ok::<_, ()>(()) }),
            std::time::Duration::from_secs(1),
        )),
        Err(SpawnError::TimerUnavailable)
    ));

    let shutdown = beaver.shutdown(ShutdownOptions::new())?;
    match runtime.block_on(shutdown.wait_grace_outcome()) {
        Err(ShutdownWaitError::TimerUnavailable { shutdown: retry }) => {
            assert_eq!(retry.id(), shutdown.id());
        }
        other => panic!("unexpected timerless shutdown outcome: {other:?}"),
    }
    let _report = runtime.block_on(shutdown.wait_final())?;
    Ok(())
}
