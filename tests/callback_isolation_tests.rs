use busybeaver::{
    listener_with_error, work, Beaver, BeaverResult, FixedCountBuilder, RuntimeError, WorkResult,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// F20: a panic in on_error must be contained by the execution cleanup
/// boundary and must not terminate the serial lane worker.
#[tokio::test]
async fn on_error_panic_does_not_kill_lane() -> BeaverResult<()> {
    let beaver = Beaver::new("on-error-panic", 8);
    let panicking = FixedCountBuilder::new(work(|| async { WorkResult::NeedRetry }))
        .count(1)
        .listener(listener_with_error(
            || {},
            || {},
            |_error: RuntimeError| panic!("on_error boom"),
        ))
        .build()?;
    beaver.enqueue(panicking).await?;

    let ran = Arc::new(AtomicBool::new(false));
    let ran_c = Arc::clone(&ran);
    let canary = FixedCountBuilder::new(work(move || {
        let ran = Arc::clone(&ran_c);
        async move {
            ran.store(true, Ordering::SeqCst);
            WorkResult::Done(())
        }
    }))
    .count(1)
    .build()?;
    beaver.enqueue(canary).await?;

    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        ran.load(Ordering::SeqCst),
        "lane must survive on_error panic"
    );
    beaver.destroy().await
}
