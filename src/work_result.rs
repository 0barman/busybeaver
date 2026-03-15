/// Result of a work execution.
#[derive(Debug, Clone)]
pub enum WorkResult<T> {
    /// Retry is needed (current attempt failed or incomplete).
    NeedRetry,
    /// Completed successfully with the result; no more retries.
    Done(T),
}

impl<T> WorkResult<T> {
    /// Returns `true` if retry is needed.
    #[inline]
    pub fn need_retry(&self) -> bool {
        matches!(self, WorkResult::NeedRetry)
    }

    /// Returns `Some(&v)` if `Done(v)`, otherwise `None`.
    #[inline]
    pub fn as_done(&self) -> Option<&T> {
        match self {
            WorkResult::Done(v) => Some(v),
            _ => None,
        }
    }

    /// Returns `Some(v)` if `Done(v)` (consumes self), otherwise `None`.
    #[inline]
    pub fn into_done(self) -> Option<T> {
        match self {
            WorkResult::Done(v) => Some(v),
            _ => None,
        }
    }
}
