//! Cooperative cancellation shared by async jobs and blocking parsers.
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) trait Cancellation: Sync {
    fn is_cancelled(&self) -> bool;

    fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            Err(crate::diagnostics::Cancelled.into())
        } else {
            Ok(())
        }
    }
}

impl Cancellation for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Acquire)
    }
}

impl Cancellation for tokio_util::sync::CancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

impl Cancellation for () {
    fn is_cancelled(&self) -> bool {
        false
    }
}
