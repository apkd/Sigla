//! Independent capacity for transfers, imports, and active source preparation.
use anyhow::Result;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

static CAPACITIES: LazyLock<Mutex<HashMap<PathBuf, Weak<Capacity>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static STOPPED: AtomicBool = AtomicBool::new(false);

pub(crate) struct Capacity {
    transfers: Arc<Semaphore>,
    imports: Arc<Semaphore>,
    stages: Arc<Semaphore>,
}

/// Shared with blocking workers so cancellation cannot release their capacity.
/// Published read-only inventory views have cache lifetimes, not worker lifetimes.
pub(crate) struct Staging {
    pub capacity: Arc<Capacity>,
    _permit: OwnedSemaphorePermit,
}

impl Capacity {
    pub fn open(cache: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(cache)?;
        let key = cache.canonicalize()?;
        let mut capacities = CAPACITIES.lock().unwrap();
        if let Some(capacity) = capacities.get(&key).and_then(Weak::upgrade) {
            return Ok(capacity);
        }
        capacities.retain(|_, capacity| capacity.strong_count() != 0);
        let capacity = Arc::new(Self {
            transfers: Arc::new(Semaphore::new(2)),
            imports: Arc::new(Semaphore::new(2)),
            stages: Arc::new(Semaphore::new(4)),
        });
        if STOPPED.load(Ordering::Acquire) {
            capacity.close();
        }
        capacities.insert(key, Arc::downgrade(&capacity));
        Ok(capacity)
    }

    pub async fn transfer(
        &self,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<OwnedSemaphorePermit> {
        let _wait = telemetry.wait("repository_transfer_admission");
        Ok(self.transfers.clone().acquire_owned().await?)
    }

    pub async fn import(
        &self,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<OwnedSemaphorePermit> {
        let _wait = telemetry.wait("repository_import_admission");
        Ok(self.imports.clone().acquire_owned().await?)
    }

    pub async fn staging(
        self: &Arc<Self>,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<Arc<Staging>> {
        let _wait = telemetry.wait("repository_staging_admission");
        let permit = self.stages.clone().acquire_owned().await?;
        Ok(Arc::new(Staging {
            capacity: self.clone(),
            _permit: permit,
        }))
    }

    fn close(&self) {
        self.transfers.close();
        self.imports.close();
        self.stages.close();
    }
}

pub(crate) fn shutdown() {
    let capacities = CAPACITIES.lock().unwrap();
    STOPPED.store(true, Ordering::Release);
    for capacity in capacities.values().filter_map(Weak::upgrade) {
        capacity.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn imports_and_unpublished_trees_leave_transfers_available() {
        let root = tempfile::tempdir().unwrap();
        let capacity = Capacity::open(root.path()).unwrap();
        let telemetry = crate::telemetry::Operation::default();
        let imports = capacity
            .imports
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let stages = capacity.stages.clone().acquire_many_owned(4).await.unwrap();
        let transfer = capacity.transfer(&telemetry).await.unwrap();
        assert!(capacity.imports.try_acquire().is_err());
        assert!(capacity.stages.try_acquire().is_err());
        drop((imports, stages, transfer));
        assert!(capacity.imports.try_acquire().is_ok());
        assert!(capacity.stages.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn detached_worker_keeps_staging_capacity_until_it_stops() {
        let root = tempfile::tempdir().unwrap();
        let capacity = Capacity::open(root.path()).unwrap();
        let telemetry = crate::telemetry::Operation::default();
        let occupied = capacity.stages.clone().acquire_many_owned(3).await.unwrap();
        let stage = capacity.staging(&telemetry).await.unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let worker_stage = stage.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _stage = worker_stage;
            wait.recv().unwrap();
            drop(_stage);
            finished.send(()).unwrap();
        });
        drop((stage, worker));
        assert!(capacity.stages.try_acquire().is_err());
        release.send(()).unwrap();
        done.await.unwrap();
        assert!(capacity.stages.try_acquire().is_ok());
        drop(occupied);
    }

    #[tokio::test]
    async fn closing_capacity_wakes_waiters() {
        let root = tempfile::tempdir().unwrap();
        let capacity = Capacity::open(root.path()).unwrap();
        let occupied = capacity
            .transfers
            .clone()
            .acquire_many_owned(2)
            .await
            .unwrap();
        let telemetry = crate::telemetry::Operation::default();
        let waiting = capacity.transfer(&telemetry);
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiting.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        capacity.close();
        assert!(waiting.await.is_err());
        drop(occupied);
    }
}
