//! Foreground interest in shared work, independent of cached job handles.
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub(crate) const PAUSE_AFTER: Duration = Duration::from_secs(120);
pub(crate) const RETAIN_PAUSED: Duration = Duration::from_secs(600);

struct State {
    callers: usize,
    idle_since: Option<Instant>,
}

pub(crate) struct Demand {
    state: Mutex<State>,
    changed: Notify,
    grace: Duration,
}

pub(crate) struct Interest(Arc<Demand>);

/// A stopped attempt stays stopped even if a new caller requests a replacement.
pub(crate) struct Work {
    demand: Arc<Demand>,
    stopped: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct Paused;
impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Preparation paused while no requests need it")
    }
}
impl std::error::Error for Paused {}

impl Default for Demand {
    fn default() -> Self {
        Self::new(PAUSE_AFTER)
    }
}

impl Demand {
    pub fn new(grace: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                callers: 0,
                idle_since: None,
            }),
            changed: Notify::new(),
            grace,
        }
    }

    pub fn interest(self: &Arc<Self>) -> Interest {
        let mut state = self.state.lock().unwrap();
        state.callers += 1;
        state.idle_since = None;
        drop(state);
        self.changed.notify_waiters();
        Interest(self.clone())
    }

    pub fn work(self: &Arc<Self>) -> Work {
        Work {
            demand: self.clone(),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn idle(&self) -> bool {
        self.deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn deadline(&self) -> Option<Instant> {
        let state = self.state.lock().unwrap();
        (state.callers == 0).then(|| {
            state
                .idle_since
                .map_or_else(Instant::now, |since| since + self.grace)
        })
    }
}

impl Drop for Interest {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.callers -= 1;
        if state.callers == 0 {
            state.idle_since = Some(Instant::now());
        }
        drop(state);
        self.0.changed.notify_waiters();
    }
}

impl Work {
    pub fn check(&self) -> anyhow::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Paused.into());
        }
        let state = self.demand.state.lock().unwrap();
        if state.callers == 0
            && state
                .idle_since
                .is_none_or(|since| since.elapsed() >= self.demand.grace)
        {
            self.stopped.store(true, Ordering::Release);
            return Err(Paused.into());
        }
        Ok(())
    }

    /// Only wrap cancellable admission/readiness waits, never a blocking child join.
    pub async fn until<T>(
        &self,
        future: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        tokio::pin!(future);
        loop {
            let changed = self.demand.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            self.check()?;
            let deadline = self.demand.deadline();
            tokio::select! {
                biased;
                result = &mut future => return result,
                _ = changed => (),
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                        None => std::future::pending().await,
                    }
                } => (),
            }
        }
    }
}

impl crate::cancellation::Cancellation for Work {
    fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }

    fn check(&self) -> anyhow::Result<()> {
        Work::check(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_requested_work_has_no_foreground_grace() {
        let demand = Arc::new(Demand::default());
        assert!(demand.idle());
        assert!(demand.work().check().unwrap_err().is::<Paused>());
        let caller = demand.interest();
        assert!(!demand.idle());
        drop(caller);
        assert!(!demand.idle());
        assert!(demand.work().check().is_ok());
    }

    #[test]
    fn only_the_last_caller_starts_idle_and_new_attempts_can_resume() {
        let demand = Arc::new(Demand::new(Duration::ZERO));
        let first = demand.interest();
        let second = demand.interest();
        let work = demand.work();
        drop(first);
        assert!(work.check().is_ok());
        drop(second);
        assert!(work.check().unwrap_err().is::<Paused>());
        let _resumed = demand.interest();
        assert!(work.check().unwrap_err().is::<Paused>());
        assert!(demand.work().check().is_ok());
    }

    #[tokio::test]
    async fn abandoned_admission_wakes_without_counting_the_worker_as_demand() {
        let demand = Arc::new(Demand::new(Duration::ZERO));
        let caller = demand.interest();
        let work = demand.work();
        let waiting = work.until(std::future::pending::<anyhow::Result<()>>());
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(caller);
        assert!(waiting.await.unwrap_err().is::<Paused>());
    }
}
