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

/// Safe admission waits also support blocking callers without a Tokio timer context.
pub(crate) async fn until<T>(
    cancel: &dyn Cancellation,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::pin!(future);
    loop {
        cancel.check()?;
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = futures_timer::Delay::new(std::time::Duration::from_millis(100)) => (),
        }
    }
}
