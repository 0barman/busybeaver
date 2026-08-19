use busybeaver::{Beaver, TaskExit, TaskSpec};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn dropping_typed_handle_before_completion_releases_later_result() -> TestResult {
    let beaver = Beaver::new("drop-result", 8);
    let released = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let released_c = Arc::clone(&released);
    let release_c = Arc::clone(&release);

    let handle = beaver.spawn_future(async move {
        release_c.notified().await;
        Ok::<_, &'static str>(DropSignal(released_c))
    })?;
    let control = handle.control();
    drop(handle);
    release.notify_one();
    assert!(control.wait().await.is_completed());
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        released.load(Ordering::SeqCst),
        "an unobserved typed result must not be retained by registry/control"
    );
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn long_lived_control_does_not_retain_operation_factory() -> TestResult {
    let beaver = Beaver::new("drop-factory", 8);
    let factory_released = Arc::new(AtomicBool::new(false));
    let signal = DropSignal(Arc::clone(&factory_released));
    let spec = TaskSpec::new(move |_ctx| {
        let _keep_factory_capture_alive = &signal;
        async move { Ok::<_, &'static str>(()) }
    });

    let mut handle = beaver.spawn(spec.clone())?;
    let control = handle.control();
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    drop(handle);
    drop(spec);
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(factory_released.load(Ordering::SeqCst));
    assert!(control.state().is_terminal());
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn future_holding_its_own_control_does_not_form_runner_cycle() -> TestResult {
    let beaver = Beaver::new("self-control", 8);
    let control_slot = Arc::new(std::sync::Mutex::new(None));
    let control_slot_c = Arc::clone(&control_slot);
    let spec = TaskSpec::new(move |ctx| {
        *control_slot_c.lock().expect("control slot") = Some(ctx.control());
        async move { Ok::<_, &'static str>(()) }
    });

    let mut handle = beaver.spawn(spec)?;
    assert!(matches!(handle.join().await?, TaskExit::Completed(())));
    let saved_control = control_slot
        .lock()
        .expect("control slot")
        .take()
        .expect("control saved");
    assert!(saved_control.state().is_terminal());
    assert!(beaver
        .execution_control(saved_control.execution_id())
        .is_none());
    beaver.destroy().await?;
    Ok(())
}
