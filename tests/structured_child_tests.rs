use busybeaver::{Beaver, CancelReason, SpawnChildError, TaskExit, TaskSpec};
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
async fn parent_does_not_finish_until_remaining_tracked_child_is_cancelled() -> TestResult {
    let beaver = Beaver::new("structured-child", 8);
    let child_dropped = Arc::new(AtomicBool::new(false));
    let child_dropped_c = Arc::clone(&child_dropped);

    let spec = TaskSpec::new(move |ctx| {
        let child_dropped = Arc::clone(&child_dropped_c);
        async move {
            let signal = DropSignal(child_dropped);
            let _child = ctx.spawn_child(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok::<_, &'static str>(())
            })?;
            Ok::<_, SpawnChildError>("parent")
        }
    });

    let mut parent = beaver.spawn(spec)?;
    assert!(matches!(
        parent.join().await?,
        TaskExit::Completed("parent")
    ));
    assert!(
        child_dropped.load(Ordering::SeqCst),
        "parent terminal must be published after tracked child cleanup"
    );
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn child_admission_is_closed_before_parent_terminal_is_published() -> TestResult {
    let beaver = Beaver::new("child-admission", 8);
    let context_slot = Arc::new(std::sync::Mutex::new(None));
    let context_slot_c = Arc::clone(&context_slot);
    let spec = TaskSpec::new(move |ctx| {
        *context_slot_c.lock().expect("context slot") = Some(ctx.clone());
        async move { Ok::<_, &'static str>(()) }
    });

    let mut parent = beaver.spawn(spec)?;
    assert!(matches!(parent.join().await?, TaskExit::Completed(())));
    let context = context_slot
        .lock()
        .expect("context slot")
        .take()
        .expect("factory ran");
    let child = context.spawn_child(async { Ok::<_, &'static str>(()) });
    assert!(matches!(child, Err(SpawnChildError::ParentClosing)));
    beaver.destroy().await?;
    Ok(())
}

#[tokio::test]
async fn parent_cancel_is_fanned_out_to_tracked_child() -> TestResult {
    let beaver = Beaver::new("child-cancel", 8);
    let child_dropped = Arc::new(AtomicBool::new(false));
    let child_dropped_c = Arc::clone(&child_dropped);
    let spec = TaskSpec::new(move |ctx| {
        let child_dropped = Arc::clone(&child_dropped_c);
        async move {
            let signal = DropSignal(child_dropped);
            let _child = ctx.spawn_child(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok::<_, &'static str>(())
            })?;
            ctx.cancelled().await;
            Ok::<_, SpawnChildError>(())
        }
    });

    let mut parent = beaver.spawn(spec)?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    parent.control().cancel(CancelReason::UserRequested);
    assert!(matches!(parent.join().await?, TaskExit::Cancelled { .. }));
    assert!(child_dropped.load(Ordering::SeqCst));
    beaver.destroy().await?;
    Ok(())
}
