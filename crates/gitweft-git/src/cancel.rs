use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation shared between the UI and a background job.
///
/// gix loops poll [`CancelToken::flag`]; git child processes are killed by the runner.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// The raw flag, in the shape gix expects for `should_interrupt`.
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
}
