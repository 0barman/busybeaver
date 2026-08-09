use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// Cancellation state shared by every scheduler-managed wait for one legacy task.
pub(crate) struct RunControl {
    cancelled: AtomicBool,
    changed: watch::Sender<bool>,
}

impl RunControl {
    pub(crate) fn new() -> Self {
        let (changed, _) = watch::channel(false);
        Self {
            cancelled: AtomicBool::new(false),
            changed,
        }
    }

    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns true only for the first cancellation request.
    pub(crate) fn cancel(&self) -> bool {
        let first = !self.cancelled.swap(true, Ordering::AcqRel);
        if first {
            self.changed.send_replace(true);
        }
        first
    }

    async fn cancelled(&self) {
        let mut changed = self.changed.subscribe();
        if *changed.borrow_and_update() {
            return;
        }
        while changed.changed().await.is_ok() {
            if *changed.borrow_and_update() {
                return;
            }
        }
    }

    /// Waits for the duration or cancellation. Returns true when cancelled.
    pub(crate) async fn wait(&self, duration: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        tokio::select! {
            biased;
            _ = self.cancelled() => true,
            _ = tokio::time::sleep(duration) => self.is_cancelled(),
        }
    }
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new()
    }
}
