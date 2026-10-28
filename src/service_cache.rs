//! Cache ownership is coordinated at the service boundary, above storage drivers.
use super::*;
use crate::cache::{
    blobs,
    policy::{Candidate, Disk},
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
};

struct MaintenanceDiskSample {
    measured_at_ms: u64,
    allocation: blobs::AttributedAllocation,
}

fn publish_disk_sample(sample: MaintenanceDiskSample) {
    crate::telemetry::disk_sample_at(
        sample.measured_at_ms,
        sample.allocation.total_bytes,
        sample.allocation.repositories,
        sample.allocation.shared,
    );
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MaintenanceOutcome {
    pub pressure: bool,
    pub progress: bool,
    pub compaction_deferred: Option<CompactionDeferral>,
    pub git_completed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CompactionDeferral {
    Busy,
    Changed,
    InsufficientSpace,
    Ineligible,
}

/// Deadlines belong to the maintenance worker; requests never mutate them.
pub(super) struct MaintenanceSchedule {
    pub collection: tokio::time::Instant,
    pub compaction: Option<tokio::time::Instant>,
    interval: std::time::Duration,
    stalled: std::time::Duration,
}

impl MaintenanceSchedule {
    const RETRY: std::time::Duration = std::time::Duration::from_secs(30);

    pub fn new(now: tokio::time::Instant, interval: std::time::Duration) -> Self {
        Self {
            collection: now + interval,
            compaction: None,
            interval,
            stalled: Self::RETRY,
        }
    }

    pub fn deadline(&self) -> tokio::time::Instant {
        self.compaction
            .map_or(self.collection, |next| next.min(self.collection))
    }

    pub fn collected(&mut self, now: tokio::time::Instant, outcome: Option<MaintenanceOutcome>) {
        let delay = match outcome {
            Some(outcome) if !outcome.pressure => {
                self.stalled = Self::RETRY;
                self.interval
            }
            Some(outcome) if outcome.progress => {
                self.stalled = Self::RETRY;
                Self::RETRY.min(self.interval)
            }
            _ => {
                let delay = self.stalled.min(self.interval);
                self.stalled = self.stalled.saturating_mul(2).min(self.interval);
                delay
            }
        };
        self.collection = now + delay;
        self.compaction = outcome
            .filter(|outcome| outcome.compaction_deferred == Some(CompactionDeferral::Busy))
            .map(|_| now + Self::RETRY.min(self.interval));
    }

    pub fn compacted(&mut self, now: tokio::time::Instant, outcome: MaintenanceOutcome) {
        self.compaction = (outcome.compaction_deferred == Some(CompactionDeferral::Busy))
            .then_some(now + Self::RETRY.min(self.interval));
        if !outcome.pressure {
            self.collection = now + self.interval;
            self.stalled = Self::RETRY;
        } else if outcome.progress {
            self.collection = self.collection.min(now + Self::RETRY.min(self.interval));
            self.stalled = Self::RETRY;
        }
    }
}

pub(super) struct Retirement {
    pub done: bool,
    pub progress: bool,
}

/// One startup sweep, owned by the existing maintenance worker, not by requests.
/// Only scheduling hints live here; retirement and collection progress live in LMDB.
#[derive(Default)]
pub(super) struct StartupMaintenance {
    scopes: Option<VecDeque<([u8; 32], crate::store::shared::WorkspaceInfo)>>,
    cleaned: bool,
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::store::shared::{Database, Encoded, Names, Phase, Scope};

    fn install(scope: &Scope, file: &str, object: [u8; 32]) {
        scope
            .install(
                file,
                vec![1],
                object,
                1,
                || Ok(()),
                || {
                    let mut records = std::collections::BTreeMap::from([(vec![1], vec![42])]);
                    let names = crate::store::format::write(
                        &[],
                        None,
                        true,
                        &Names::default(),
                        &mut records,
                    )?;
                    Ok(Encoded { names, records })
                },
            )
            .unwrap();
    }

    fn make_app(root: &Path, cache: &Path) -> Arc<App> {
        Arc::new(App::new(Policy::new(vec![root.into()]).unwrap(), cache.into(), 1).unwrap())
    }

    #[tokio::test]
    async fn startup_retirement_resumes_after_restart_and_preserves_shared_records() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let retiring = database.workspace([1; 32], root.path(), None).unwrap();
        let live = database.workspace([2; 32], root.path(), None).unwrap();
        let object = [9; 32];
        for i in 0..1024 {
            install(&retiring, &format!("retired-{i}"), object);
        }
        install(&live, "live", object);
        retiring.mark_deleting().unwrap();
        let small = database.workspace([4; 32], root.path(), None).unwrap();
        small.mark_deleting().unwrap();
        let unused = database.workspace([3; 32], root.path(), None).unwrap();
        for i in 0u32..300 {
            install(
                &unused,
                &format!("unused-{i}"),
                *blake3::hash(&i.to_le_bytes()).as_bytes(),
            );
        }
        unused.mark_deleting().unwrap();
        while !unused.delete_batch(2, 256).unwrap() {}
        let app = make_app(root.path(), cache.path());
        let bindings = || {
            database
                .binding_count(&live.read().unwrap(), &object)
                .unwrap()
                .unwrap()
        };
        let before = bindings();
        let mut pending = StartupMaintenance::default();
        let active = app.activity.read().await;
        assert!(!app.maintain_startup(&mut pending).await.unwrap());
        assert_eq!(bindings(), before);
        assert!(
            app.activity.try_read().is_ok(),
            "Maintenance queued a writer"
        );
        drop(active);
        assert!(!app.maintain_startup(&mut pending).await.unwrap());
        let remaining = bindings();
        assert!(
            remaining > 1 && remaining < before,
            "Retirement was not incremental"
        );
        assert!(!app.maintain_startup(&mut pending).await.unwrap());
        assert!(database.existing([4; 32]).unwrap().is_none());
        assert!(database.namespace([1; 32], retiring.id).unwrap().is_some());
        let retired_id = retiring.id;
        app.shutdown().await;
        drop(app);
        drop(unused);
        drop(small);
        drop(retiring);
        drop(live);
        drop(database);

        // Discard all scheduling hints and database handles. Committed progress,
        // including unused-object collection, must survive reopening.
        let app = make_app(root.path(), cache.path());
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let live = database.existing([2; 32]).unwrap().unwrap();
        assert_eq!(
            database
                .binding_count(&live.read().unwrap(), &object)
                .unwrap(),
            Some(remaining)
        );
        let mut pending = StartupMaintenance::default();
        let mut done = false;
        for _ in 0..128 {
            done = app.maintain_startup(&mut pending).await.unwrap();
            if done {
                break;
            }
        }
        assert!(
            done,
            "Pending retirement or collection stopped making progress"
        );
        assert_eq!(database.workspaces().unwrap().len(), 1);
        let tx = live.read().unwrap();
        assert_eq!(database.binding_count(&tx, &object).unwrap(), Some(1));
        assert_eq!(database.object_count(&tx).unwrap(), 1);
        assert_eq!(live.record(&tx, "live", &[1]).unwrap(), Some(&[42][..]));
        drop(tx);
        let replacement = database.workspace([1; 32], root.path(), None).unwrap();
        assert_ne!(replacement.id, retired_id);
        app.shutdown().await;
    }

    #[tokio::test]
    async fn startup_sweep_never_retires_a_reused_fingerprint() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = make_app(root.path(), cache.path());
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let old = database.workspace([1; 32], root.path(), None).unwrap();
        old.mark_deleting().unwrap();
        let mut pending = StartupMaintenance::default();
        let active = app.activity.read().await;
        app.maintain_startup(&mut pending).await.unwrap();
        assert!(old.delete_batch(1, 128).unwrap());
        let replacement = database.workspace([1; 32], root.path(), None).unwrap();
        install(&replacement, "replacement", [2; 32]);
        drop(active);
        assert!(app.maintain_startup(&mut pending).await.unwrap());
        let workspaces = database.workspaces().unwrap();
        let info = &workspaces[0].1;
        assert_eq!(info.id, replacement.id);
        assert_ne!(info.phase, Phase::Deleting);
        assert!(
            replacement
                .record(&replacement.read().unwrap(), "replacement", &[1])
                .unwrap()
                .is_some()
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn pressure_drains_retired_namespace_while_replacement_is_active() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = make_app(root.path(), cache.path());
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let old = database.workspace([1; 32], root.path(), None).unwrap();
        for i in 0..300 {
            install(&old, &format!("old-{i}"), [7; 32]);
        }
        old.mark_deleting().unwrap();
        let replacement = database.workspace([1; 32], root.path(), None).unwrap();
        install(&replacement, "replacement", [7; 32]);
        let active = Arc::new(tokio::sync::Mutex::new(None));
        app.workspaces.lock().unwrap().insert(
            root.path().to_owned(),
            WorkspaceSlot {
                state: active.clone(),
                preparing: None,
                used: Instant::now(),
            },
        );
        assert!(app.maintain_analysis(true).unwrap());
        assert!(database.namespace([1; 32], old.id).unwrap().is_none());
        assert!(Arc::ptr_eq(
            &app.workspaces.lock().unwrap()[root.path()].state,
            &active
        ));
        let tx = replacement.read().unwrap();
        assert_eq!(database.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
        assert!(
            replacement
                .record(&tx, "replacement", &[1])
                .unwrap()
                .is_some()
        );
        drop(tx);
        app.shutdown().await;
    }

    #[tokio::test]
    async fn valid_search_does_not_wait_for_retirement_or_optional_disposal() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(root.path().join("lib.rs"), "pub struct Available;").unwrap();
        let analysis = cache.path().join("analysis");
        {
            let database = Database::open(&analysis).unwrap();
            database
                .workspace([1; 32], &root.path().join("missing"), None)
                .unwrap();
        }
        // Disposal failure is unrelated to the validity of the current database.
        fs::create_dir(analysis.join("data.mdb.compacting")).unwrap();
        let app = make_app(root.path(), cache.path());
        // Drive maintenance explicitly in this test, independently of the timer.
        let (_setup, complete) = tokio::sync::watch::channel(Some(Ok(())));
        *app.startup.lock().unwrap() = Some(complete);
        let active = app.activity.read().await;
        let result = app
            .search(root.path().to_str().unwrap(), "type:Available")
            .await
            .unwrap();
        assert!(result.contains("Available"), "{result}");
        assert!(
            Database::open(&analysis)
                .unwrap()
                .existing([1; 32])
                .unwrap()
                .is_some()
        );
        assert!(analysis.join("data.mdb.compacting").is_dir());
        drop(active);
        // A disposal error must not prevent an independently valid retirement batch.
        drop(app.activity.write().await);
        assert!(
            app.maintain_startup(&mut StartupMaintenance::default())
                .await
                .is_err()
        );
        assert!(
            Database::open(&analysis)
                .unwrap()
                .existing([1; 32])
                .unwrap()
                .is_none()
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn setup_services_pending_retirement_before_periodic_disk_maintenance() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database.workspace([1; 32], root.path(), None).unwrap();
        scope.mark_deleting().unwrap();
        let app = make_app(root.path(), cache.path());
        assert!(database.namespace([1; 32], scope.id).unwrap().is_some());
        app.start_setup();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while database.namespace([1; 32], scope.id).unwrap().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Setup left interrupted retirement waiting for periodic maintenance");
        app.shutdown().await;
    }

    #[tokio::test]
    async fn setup_is_singleton_and_shutdown_stops_maintenance() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = make_app(root.path(), cache.path());
        app.start_setup();
        let id = app.maintenance.lock().unwrap().as_ref().unwrap().id();
        app.start_setup();
        assert_eq!(app.maintenance.lock().unwrap().as_ref().unwrap().id(), id);
        tokio::time::timeout(std::time::Duration::from_secs(5), app.shutdown())
            .await
            .unwrap();
        assert!(app.maintenance.lock().unwrap().is_none());
        app.start_setup();
        assert!(app.maintenance.lock().unwrap().is_none());
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database.workspace([1; 32], root.path(), None).unwrap();
        scope.mark_deleting().unwrap();
        assert!(
            app.maintain_startup(&mut StartupMaintenance::default())
                .await
                .unwrap()
        );
        assert!(database.namespace([1; 32], scope.id).unwrap().is_some());
    }

    #[tokio::test]
    async fn shutdown_joins_an_in_flight_blocking_maintenance_batch() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = make_app(root.path(), cache.path());
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = app.clone();
        *app.maintenance.lock().unwrap() = Some(tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _activity = worker.activity.clone().try_write_owned().unwrap();
                started.send(()).unwrap();
                wait.recv().unwrap();
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
        let stopping = {
            let app = app.clone();
            tokio::spawn(async move { app.shutdown().await })
        };
        tokio::task::yield_now().await;
        assert!(!stopping.is_finished());
        assert!(app.activity.try_read().is_err());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), stopping)
            .await
            .unwrap()
            .unwrap();
        assert!(app.maintenance.lock().unwrap().is_none());
        assert!(app.activity.try_write().is_ok());
    }
}

fn maintenance_stage<T>(
    operation: &crate::telemetry::Operation,
    name: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    operation.stage(name);
    let start = Instant::now();
    tracing::info!(stage = name, "Disk cache maintenance stage started");
    let result = run();
    tracing::info!(
        stage = name,
        elapsed_ms = start.elapsed().as_millis(),
        success = result.is_ok(),
        "Disk cache maintenance stage finished"
    );
    result
}

impl App {
    pub(super) async fn maintain_startup(
        self: &Arc<Self>,
        state: &mut StartupMaintenance,
    ) -> Result<bool> {
        let app = self.clone();
        let mut pending = std::mem::take(state);
        let (next, result) = tokio::task::spawn_blocking(move || {
            let result = app.maintain_startup_step(&mut pending);
            (pending, result)
        })
        .await?;
        // Keep the queue on ordinary I/O errors, including a failed batch commit.
        *state = next;
        result
    }

    fn maintain_startup_step(&self, state: &mut StartupMaintenance) -> Result<bool> {
        if self.inspection_shutdown.is_cancelled() {
            return Ok(true);
        }
        let analysis = self.cache.join("analysis");
        // Inventory and unlinking obsolete files do not replace the live database.
        // Use a shared gate: an O(namespaces) scan must not exclude navigation.
        let cleanup = {
            let Ok(_activity) = self.activity.clone().try_read_owned() else {
                return Ok(false);
            };
            let cleanup = if state.cleaned {
                Ok(())
            } else {
                let result = (|| {
                    crate::cache::compaction::cleanup(&self.cache)?;
                    for retired in crate::cache::views::Registry::open(&self.cache)?.pending()? {
                        retired.dispose()?;
                    }
                    Ok::<_, anyhow::Error>(())
                })();
                state.cleaned = result.is_ok();
                result
            };
            if state.scopes.is_none() {
                let mut scopes = VecDeque::new();
                if analysis.is_dir() {
                    for (key, info) in crate::store::Database::open(&analysis)?.workspaces()? {
                        if Self::retirement_candidate(&info) {
                            scopes.push_back((key, info));
                        }
                    }
                }
                state.scopes = Some(scopes);
            }
            cleanup
        };
        if self.inspection_shutdown.is_cancelled() {
            return Ok(true);
        }
        // Never queue an activity writer. One namespace batch and one GC batch
        // per turn; a startup backlog must not become a full disk-pressure pass.
        let Ok(_activity) = self.activity.clone().try_write_owned() else {
            return cleanup.map(|()| false);
        };
        if self.inspection_shutdown.is_cancelled() {
            return Ok(true);
        }
        if !analysis.is_dir() {
            return cleanup.map(|()| true);
        }
        let database = crate::store::Database::open(&analysis)?;
        let scopes = state.scopes.as_mut().unwrap();
        let now = crate::cache::now();
        if let Some((key, info)) = scopes.front() {
            if self.retire_analysis_scope(&database, *key, info, now)?.done {
                scopes.pop_front();
            } else {
                // Large or currently pinned namespaces cannot starve smaller ones.
                scopes.rotate_left(1);
            }
        }
        if self.inspection_shutdown.is_cancelled() {
            return Ok(true);
        }
        let collected = database.collect(now, 128)?;
        cleanup?;
        Ok(scopes.is_empty() && collected.visited < 128)
    }

    pub(super) async fn maintain_disk(self: &Arc<Self>) -> Result<MaintenanceOutcome> {
        self.maintain_disk_observed(|| {}).await
    }

    async fn maintain_disk_observed(
        self: &Arc<Self>,
        started: impl FnOnce() + Send + 'static,
    ) -> Result<MaintenanceOutcome> {
        if self.inspection_shutdown.is_cancelled() {
            return Ok(MaintenanceOutcome::default());
        }
        if self
            .startup
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.borrow().is_none())
        {
            return Ok(MaintenanceOutcome::default());
        }
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            // Routine collection uses domain leases and database snapshots.
            // Only replacing the database needs an idle service.
            let Ok(gate) = app.activity.clone().try_read_owned() else {
                tracing::info!("Disk cache maintenance deferred during database replacement");
                return Ok(MaintenanceOutcome {
                    pressure: true,
                    ..Default::default()
                });
            };
            let _gate = gate;
            if app.inspection_shutdown.is_cancelled() {
                return Ok(MaintenanceOutcome::default());
            }
            let start = Instant::now();
            let operation = crate::telemetry::operation("cache_maintenance", None, None);
            let mut disk_sample: Option<MaintenanceDiskSample> = None;
            tracing::info!("Disk cache maintenance started");
            started();
            let result = app.maintain_disk_shared_with(&operation, &mut disk_sample);
            drop(_gate);
            let result = result.and_then(|mut outcome| {
                if outcome.pressure && !app.inspection_shutdown.is_cancelled() {
                    let compact = app.try_compaction_with(&operation, &mut disk_sample)?;
                    outcome.pressure = compact.pressure;
                    outcome.progress |= compact.progress;
                    outcome.compaction_deferred = compact.compaction_deferred;
                }
                Ok(outcome)
            });
            if let Some(sample) = disk_sample {
                publish_disk_sample(sample);
            }
            operation.finish(if result.is_ok() { "success" } else { "failure" });
            tracing::info!(
                elapsed_ms = start.elapsed().as_millis(),
                success = result.is_ok(),
                "Disk cache maintenance finished"
            );
            result
        })
        .await?
    }

    pub(super) async fn retry_compaction(self: &Arc<Self>) -> Result<MaintenanceOutcome> {
        let app = self.clone();
        // Await the blocking worker even during shutdown: it owns the cache.
        tokio::task::spawn_blocking(move || {
            let operation = crate::telemetry::operation("cache_maintenance", None, None);
            let mut disk_sample: Option<MaintenanceDiskSample> = None;
            let result = app.try_compaction_with(&operation, &mut disk_sample);
            if let Some(sample) = disk_sample {
                publish_disk_sample(sample);
            }
            let status = match &result {
                Ok(outcome) if !outcome.progress => "noop",
                Ok(_) => "success",
                Err(_) => "failure",
            };
            operation.finish(status);
            result
        })
        .await?
    }

    pub(super) async fn maintain_git(&self, outcome: Option<MaintenanceOutcome>) -> Result<()> {
        if self.inspection_shutdown.is_cancelled()
            || outcome.is_some_and(|outcome| outcome.git_completed)
        {
            return Ok(());
        }
        let cache = self.cache.clone();
        let ttl = self
            .remote
            .as_ref()
            .map_or(crate::config::DEFAULT_REPO_TTL, |remote| {
                remote.options.repo_ttl
            });
        // The worker awaits this job before releasing cache ownership.
        tokio::task::spawn_blocking(move || crate::repository::cache::maintain_all(&cache, ttl))
            .await?
    }

    fn try_compaction_with(
        &self,
        operation: &crate::telemetry::Operation,
        disk_sample: &mut Option<MaintenanceDiskSample>,
    ) -> Result<MaintenanceOutcome> {
        operation.stage("analysis compaction check");
        let mut outcome = MaintenanceOutcome {
            pressure: true,
            ..Default::default()
        };
        if self.inspection_shutdown.is_cancelled() {
            return Ok(outcome);
        }
        let Ok(read) = self.activity.clone().try_read_owned() else {
            outcome.compaction_deferred = Some(CompactionDeferral::Busy);
            return Ok(outcome);
        };
        if !self.cache.join("analysis").is_dir() {
            outcome.compaction_deferred = Some(CompactionDeferral::Ineligible);
            return Ok(outcome);
        }
        let database = crate::store::Database::open(&self.cache.join("analysis"))?;
        let stats = database.disk_stats()?;
        if !stats.eligible() {
            outcome.compaction_deferred = Some(CompactionDeferral::Ineligible);
            tracing::debug!(reason = ?outcome.compaction_deferred, "Analysis compaction deferred");
            return Ok(outcome);
        }
        if Disk::read(&self.cache)?.available < stats.temporary_bytes() {
            outcome.compaction_deferred = Some(CompactionDeferral::InsufficientSpace);
            tracing::info!(reason = ?outcome.compaction_deferred, temporary_bytes = stats.temporary_bytes(), "Analysis compaction deferred");
            return Ok(outcome);
        }
        let prepared = maintenance_stage(operation, "analysis compaction copy", || {
            database.prepare_compaction(&self.cache)
        })?;
        let candidate = match prepared {
            crate::cache::compaction::Preparation::Ready(candidate) => candidate,
            other => {
                outcome.compaction_deferred = Some(match other {
                    crate::cache::compaction::Preparation::InsufficientSpace => {
                        CompactionDeferral::InsufficientSpace
                    }
                    _ => CompactionDeferral::Changed,
                });
                return Ok(outcome);
            }
        };
        drop(read);
        if self.inspection_shutdown.is_cancelled() {
            return Ok(outcome);
        }
        let Ok(idle) = self.activity.clone().try_write_owned() else {
            // A full copy already ran. Use collection backoff rather than
            // repeatedly copying under sustained foreground activity.
            outcome.compaction_deferred = Some(CompactionDeferral::Changed);
            tracing::info!("Analysis compaction copy discarded while requests are active");
            return Ok(outcome);
        };
        if !database.compaction_current(&candidate)? {
            outcome.compaction_deferred = Some(CompactionDeferral::Changed);
            return Ok(outcome);
        }
        self.assets.trim_completed();
        self.sources.trim_completed();
        self.workspaces.lock().unwrap().clear();
        if Arc::strong_count(&database) != 1 {
            outcome.compaction_deferred = Some(CompactionDeferral::Changed);
            return Ok(outcome);
        }
        let start = Instant::now();
        let result = maintenance_stage(operation, "analysis compaction publish", || {
            database.publish_compaction(&self.cache, candidate)
        });
        if result.is_err() {
            crate::cache::compaction::recover(&self.cache)?;
        }
        tracing::info!(
            pause_ms = start.elapsed().as_millis(),
            "Analysis compaction publication finished"
        );
        drop(idle);
        let reclaimed = result?;
        tracing::info!(reclaimed_bytes = reclaimed, "Compacted analysis storage");
        // Unlink the old database after requests can resume.
        if let Ok(_read) = self.activity.clone().try_read_owned() {
            crate::cache::compaction::cleanup(&self.cache)?;
        }
        outcome.progress = reclaimed > 0;
        // Full filesystem accounting is needed only after an actual copy.
        if outcome.progress {
            let repository_roots =
                crate::telemetry::enabled().then(|| self.repository_disk_roots_for_telemetry());
            let (allocated, sample) = self.measure_allocated(repository_roots.as_ref())?;
            if sample.is_some() {
                *disk_sample = sample;
            }
            outcome.pressure = self
                .cache_limits
                .pressured(allocated, Disk::read(&self.cache)?);
        }
        Ok(outcome)
    }

    fn repository_disk_roots(&self) -> Result<BTreeMap<std::path::PathBuf, String>> {
        let mut roots = BTreeMap::new();
        for (root, state) in crate::repository::retention::cached(&self.cache)? {
            let identity = state.repository.to_string();
            roots.insert(root, identity.clone());
            roots.insert(
                self.cache.join("git").join(state.repository.storage_key()),
                identity,
            );
        }
        for (history, identity, _) in crate::repository::cache::history_owners(&self.cache)? {
            if let Some(root) = history.parent() {
                roots.insert(root.to_owned(), identity.to_string());
            }
        }
        Ok(roots)
    }

    fn repository_disk_roots_for_telemetry(&self) -> BTreeMap<std::path::PathBuf, String> {
        match self.repository_disk_roots() {
            Ok(roots) => roots,
            Err(error) => {
                tracing::warn!(%error, "Repository disk attribution unavailable; assigning cache usage to shared categories");
                BTreeMap::new()
            }
        }
    }

    fn measure_allocated(
        &self,
        repository_roots: Option<&BTreeMap<std::path::PathBuf, String>>,
    ) -> Result<(u64, Option<MaintenanceDiskSample>)> {
        if let Some(repository_roots) = repository_roots {
            match blobs::allocated_attributed(&self.cache, repository_roots) {
                Ok(allocation) => {
                    let measured_at_ms = crate::telemetry::now_ms();
                    Ok((
                        allocation.total_bytes,
                        Some(MaintenanceDiskSample {
                            measured_at_ms,
                            allocation,
                        }),
                    ))
                }
                Err(error) => {
                    tracing::warn!(%error, "Attributed disk measurement failed; continuing with total disk measurement");
                    Ok((blobs::allocated(&self.cache)?, None))
                }
            }
        } else {
            Ok((blobs::allocated(&self.cache)?, None))
        }
    }

    fn seed_owners(&self) -> Result<bool> {
        if self.inspection_shutdown.is_cancelled() {
            return Ok(false);
        }
        let mut known = true;
        for (owner, _, used) in crate::repository::cache::history_owners(&self.cache)? {
            self.owners.lock().unwrap().seed(&owner, None, used);
        }
        if let Some(remote) = &self.remote {
            for (root, state) in remote.cached()? {
                self.owners
                    .lock()
                    .unwrap()
                    .seed(&root.join("source"), Some(&root), state.last_use);
            }
        }
        if self.cache.join("analysis").is_dir() {
            let database = crate::store::Database::open(&self.cache.join("analysis"))?;
            for (key, info) in database.workspaces()? {
                if self.inspection_shutdown.is_cancelled() {
                    return Ok(false);
                }
                if info.phase == crate::store::shared::Phase::Deleting {
                    continue;
                }
                if let Some(scope) = database.existing(key)? {
                    let revision = self.owners.lock().unwrap().revision();
                    let store = crate::store::Store { scope };
                    let manifest: Option<crate::workspace::Manifest> = store.get_manifest()?;
                    if let Some(manifest) = manifest {
                        // The primary inventory owns dependencies for every source group.
                        if manifest.source_group.is_some() {
                            continue;
                        }
                        let mut owners = self.owners.lock().unwrap();
                        if owners.revision() != revision {
                            known = false;
                            continue;
                        }
                        owners.observe(&info.entry, info.owner.as_deref(), &manifest, None);
                    } else {
                        known = false;
                    }
                }
            }
        }
        let (revision, paths) = {
            let owners = self.owners.lock().unwrap();
            let paths: Vec<_> = owners
                .entries
                .values()
                .map(|owner| (owner.entry.clone(), owner.repository.clone()))
                .collect();
            (owners.revision(), paths)
        };
        let mut removed = Vec::new();
        for (entry, repository) in paths {
            if self.inspection_shutdown.is_cancelled() {
                return Ok(false);
            }
            if fs::symlink_metadata(repository.as_deref().unwrap_or(&entry))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                removed.push(entry);
            }
        }
        {
            let mut owners = self.owners.lock().unwrap();
            if owners.revision() == revision {
                for entry in removed {
                    owners.remove(&entry);
                }
            } else {
                known = false;
            }
        }
        self.flush_owners()?;
        Ok(known)
    }

    fn flush_owners(&self) -> Result<()> {
        let snapshot = self.owners.lock().unwrap().clone();
        if let Some(publication) = snapshot.stage()? {
            self.owners.lock().unwrap().publish(publication)?;
        }
        Ok(())
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        let owners = self.owners.lock().unwrap().clone();
        crate::cache::policy::retention_candidates(
            &self.cache,
            &owners,
            self.remote
                .as_ref()
                .map(|remote| crate::repository::retention::Policy::from(&remote.options))
                .as_ref(),
            crate::cache::now(),
        )
    }

    fn evict_candidate(&self, candidate: &Candidate, pressure: bool) -> Result<bool> {
        if candidate.pinned {
            return Ok(false);
        }
        if crate::repository::cache::history_owner(&self.cache, &candidate.owner.entry) {
            if !crate::repository::cache::evict_history(&candidate.owner.entry)? {
                return Ok(false);
            }
            self.owners.lock().unwrap().remove(&candidate.owner.entry);
            return Ok(true);
        }
        if let Some(root) = &candidate.owner.repository {
            if !self.remote.as_ref().unwrap().evict(
                root,
                pressure,
                || self.owner_scopes(root),
                |scopes| {
                    self.workspaces
                        .lock()
                        .unwrap()
                        .remove(&candidate.owner.entry);
                    self.retire_owner(scopes)?;
                    self.owners.lock().unwrap().remove(&candidate.owner.entry);
                    Ok(())
                },
            )? {
                return Ok(false);
            }
        } else {
            // Enumerating every retained fingerprint must not exclude workspace
            // lookup. Capture identities, then recheck them at each retirement.
            let mut scopes = Vec::new();
            if self.cache.join("analysis").is_dir() {
                let database = crate::store::Database::open(&self.cache.join("analysis"))?;
                for (key, info) in database.workspaces()? {
                    if info.entry == candidate.owner.entry
                        && let Some(scope) = database.existing(key)?
                        && scope.id == info.id
                    {
                        scopes.push(scope);
                    }
                }
            }
            let mut scopes = scopes.into_iter().peekable();
            let mut started = false;
            loop {
                let mut registry = self.workspaces.lock().unwrap();
                if registry
                    .get(&candidate.owner.entry)
                    .is_some_and(|slot| started || Arc::strong_count(&slot.state) != 1)
                    || self
                        .owners
                        .lock()
                        .unwrap()
                        .entries
                        .get(&candidate.owner.entry)
                        .is_some_and(|owner| owner.usage.last_use != candidate.owner.usage.last_use)
                {
                    return Ok(false);
                }
                // Keep leases until the catalog guard is released, including
                // when a storage operation fails below.
                let removed = registry.remove(&candidate.owner.entry);
                if !started {
                    self.sources.forget(&candidate.owner.entry);
                }
                started = true;
                if let Some(scope) = scopes.next()
                    && let Some(current) = scope.database.existing(scope.fingerprint)?
                    && current.id == scope.id
                {
                    current.mark_deleting()?;
                }
                let done = scopes.peek().is_none();
                if done {
                    self.owners.lock().unwrap().remove(&candidate.owner.entry);
                }
                drop(registry);
                drop(removed);
                if done {
                    return Ok(true);
                }
                std::thread::yield_now();
            }
        }
        tracing::info!(entry = %candidate.owner.entry.display(), pressure, "Evicted cached workspace");
        Ok(true)
    }

    fn collect_analysis(&self, pressure: bool) -> Result<bool> {
        // Ordinary passes retire and collect bounded batches. Disk pressure
        // drains all retired workspaces and unused objects.
        let mut progress = self.maintain_analysis(pressure)?;
        if self.cache.join("analysis").is_dir() {
            let database = crate::store::Database::open(&self.cache.join("analysis"))?;
            if pressure {
                while !self.inspection_shutdown.is_cancelled() {
                    let collected = database.collect_with_retention(
                        crate::cache::now(),
                        crate::store::shared::PRESSURE_BATCH,
                        0,
                    )?;
                    progress |= collected.objects > 0;
                    if collected.visited != crate::store::shared::PRESSURE_BATCH {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
            database.checkpoint()?;
        }
        Ok(progress)
    }

    fn analysis_savings(&self, disk: Disk) -> Result<u64> {
        // A compact copy cannot run while indexing owns a workspace or branch.
        // Do not credit those future savings instead of reclaiming idle files.
        let active_workspace = self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .any(|slot| Arc::strong_count(&slot.state) != 1);
        if active_workspace
            || self
                .remote
                .as_ref()
                .is_some_and(|remote| !remote.active_entries().is_empty())
        {
            return Ok(0);
        }
        if !self.cache.join("analysis").is_dir() {
            return Ok(0);
        }
        let stats = crate::store::Database::open(&self.cache.join("analysis"))?.disk_stats()?;
        Ok(
            if stats.eligible() && disk.available >= stats.temporary_bytes() {
                stats.reclaimable()
            } else {
                0
            },
        )
    }

    #[cfg(test)]
    pub(super) fn compact_analysis(&self) -> Result<u64> {
        if !self.cache.join("analysis").is_dir() {
            return Ok(0);
        }
        let database = crate::store::Database::open(&self.cache.join("analysis"))?;
        let stats = database.disk_stats()?;
        tracing::debug!(
            allocated = stats.allocated,
            live = stats.live,
            reclaimable = stats.reclaimable(),
            "Analysis allocation"
        );
        if !stats.eligible() {
            return Ok(0);
        }
        self.assets.trim_completed();
        self.sources.trim_completed();
        self.workspaces.lock().unwrap().clear();
        let start = Instant::now();
        match database.compact(&self.cache) {
            Ok(bytes) => {
                tracing::info!(
                    bytes,
                    pause_ms = start.elapsed().as_millis(),
                    "Compacted analysis storage"
                );
                Ok(bytes)
            }
            Err(error) => {
                tracing::warn!(%error, "Analysis compaction deferred; existing data retained");
                crate::cache::compaction::recover(&self.cache)?;
                Ok(0)
            }
        }
    }

    fn collect_views(&self, dependencies_known: bool) -> Result<bool> {
        self.collect_views_observed(dependencies_known, || {})
    }

    fn collect_views_observed(
        &self,
        dependencies_known: bool,
        prepared: impl FnOnce(),
    ) -> Result<bool> {
        if !dependencies_known {
            return Ok(false);
        }
        // This snapshot only skips known retained inputs. Candidates still check
        // current ownership and active leases at the retirement boundary.
        let owners = self.owners.lock().unwrap().clone();
        let retained: std::collections::BTreeSet<_> = owners
            .entries
            .values()
            .flat_map(|owner| &owner.dependencies)
            .filter_map(|path| crate::cache::views::managed_root(&self.cache, path))
            .collect();
        drop(owners);
        prepared();
        let mut progress = false;
        let views = crate::cache::views::Registry::open(&self.cache)?;
        for &kind in crate::cache::owners::VIEW_KINDS {
            let directory = self.cache.join(kind);
            if !directory.is_dir() {
                continue;
            }
            for entry in fs::read_dir(directory)? {
                if self.inspection_shutdown.is_cancelled() {
                    return Ok(progress);
                }
                let entry = entry?;
                let path = entry.path();
                ensure!(entry.file_type()?.is_dir(), "Invalid cache view directory");
                if retained.contains(&path) {
                    continue;
                }
                let configured = kind == "unity-metadata"
                    && self.remote.as_ref().is_some_and(|remote| {
                        crate::metadata_archive::configured(&path, &remote.options.unity_versions)
                    });
                if configured {
                    continue;
                }
                if let Some(retired) =
                    views.try_retire(&path, || self.owners.lock().unwrap().retains(&path))?
                {
                    retired.dispose()?;
                    progress = true;
                }
            }
        }
        let legacy = self.cache.join("dotnet/packages");
        if legacy.is_dir()
            && !retained.contains(&legacy)
            && let Some(retired) =
                views.try_retire(&legacy, || self.owners.lock().unwrap().retains(&legacy))?
        {
            retired.dispose()?;
            progress = true;
        }
        Ok(progress)
    }

    #[cfg(test)]
    fn maintain_disk_shared(&self) -> Result<MaintenanceOutcome> {
        let operation = crate::telemetry::operation("cache_maintenance", None, None);
        let mut disk_sample: Option<MaintenanceDiskSample> = None;
        let result = self.maintain_disk_shared_with(&operation, &mut disk_sample);
        if let Some(sample) = disk_sample {
            publish_disk_sample(sample);
        }
        operation.finish(if result.is_ok() { "success" } else { "failure" });
        result
    }

    fn maintain_disk_shared_with(
        &self,
        operation: &crate::telemetry::Operation,
        disk_sample: &mut Option<MaintenanceDiskSample>,
    ) -> Result<MaintenanceOutcome> {
        let mut outcome = MaintenanceOutcome::default();
        // Retry optional disposal independently of retirement and pressure relief.
        if let Err(error) = crate::cache::compaction::cleanup(&self.cache) {
            tracing::warn!(%error, "Cannot dispose of obsolete compaction files");
        }
        for retired in crate::cache::views::Registry::open(&self.cache)?.pending()? {
            if self.inspection_shutdown.is_cancelled() {
                return Ok(outcome);
            }
            retired.dispose()?;
            outcome.progress = true;
        }
        let known = maintenance_stage(operation, "ownership scan", || self.seed_owners())?;
        let repository_roots =
            crate::telemetry::enabled().then(|| self.repository_disk_roots_for_telemetry());
        let before = maintenance_stage(operation, "disk measurement", || {
            blobs::allocated(&self.cache)
        })?;
        let disk = Disk::read(&self.cache)?;
        let pressure = self.cache_limits.pressured(before, disk);
        let mut candidates =
            maintenance_stage(operation, "eviction planning", || self.candidates())?;
        for candidate in candidates.iter().filter(|candidate| candidate.expired) {
            if self.inspection_shutdown.is_cancelled() {
                return Ok(outcome);
            }
            outcome.progress |= self.evict_candidate(candidate, false)?;
        }
        outcome.progress |= maintenance_stage(operation, "analysis collection", || {
            self.collect_analysis(pressure)
        })?;
        outcome.progress |=
            maintenance_stage(operation, "view collection", || self.collect_views(known))?;
        maintenance_stage(operation, "blob collection", || {
            self.blobs.collect_with_pressure(pressure)
        })?;
        maintenance_stage(operation, "asset record collection", || {
            self.asset_records.collect(pressure)
        })?;
        let (mut after, sample) =
            maintenance_stage(operation, "post-collection disk measurement", || {
                self.measure_allocated(repository_roots.as_ref())
            })?;
        if sample.is_some() {
            *disk_sample = sample;
        }
        candidates.retain(|candidate| !candidate.expired);
        let mut remaining = self.cache_limits.reclaim(after, Disk::read(&self.cache)?);
        let mut savings = if remaining > 0 {
            self.analysis_savings(Disk::read(&self.cache)?)?
        } else {
            0
        };
        if remaining > savings {
            let owners = self.owners.lock().unwrap().clone();
            maintenance_stage(operation, "eviction ranking", || {
                crate::cache::policy::rank(&owners, &mut candidates, crate::cache::now())
            })?;
        }
        candidates.retain(|candidate| !candidate.pinned);
        let mut candidates = candidates.into_iter();
        while remaining > savings {
            if self.inspection_shutdown.is_cancelled() {
                return Ok(outcome);
            }
            // Credit an affordable compact copy, but postpone the copy until all
            // victims are retired. Git packing savings are deliberately not assumed.
            let needed = remaining - savings;
            let allocated = after;
            let mut estimated = 0;
            let mut evicted = false;
            for candidate in candidates.by_ref() {
                if self.evict_candidate(&candidate, true)? {
                    evicted = true;
                    outcome.progress = true;
                    estimated += candidate.bytes;
                }
                if estimated >= needed {
                    break;
                }
            }
            if !evicted {
                break;
            }
            outcome.progress |= maintenance_stage(operation, "analysis collection", || {
                self.collect_analysis(true)
            })?;
            outcome.progress |=
                maintenance_stage(operation, "view collection", || self.collect_views(known))?;
            maintenance_stage(operation, "blob collection", || {
                self.blobs.collect_with_pressure(true)
            })?;
            maintenance_stage(operation, "asset record collection", || {
                self.asset_records.collect(true)
            })?;
            let (measured, sample) =
                maintenance_stage(operation, "post-eviction disk measurement", || {
                    self.measure_allocated(repository_roots.as_ref())
                })?;
            after = measured;
            if sample.is_some() {
                *disk_sample = sample;
            }
            remaining = remaining.saturating_add(after).saturating_sub(allocated);
            savings = self.analysis_savings(Disk::read(&self.cache)?)?;
        }
        // Avoid repacking Git when the known compact-copy savings already meet
        // the target. Repacking is the fallback after idle victims are exhausted.
        if remaining > savings {
            maintenance_stage(operation, "Git reclamation", || {
                crate::repository::cache::reclaim(&self.cache)
            })?;
            outcome.git_completed = true;
            let (measured, sample) =
                maintenance_stage(operation, "post-Git disk measurement", || {
                    self.measure_allocated(repository_roots.as_ref())
                })?;
            after = measured;
            if sample.is_some() {
                *disk_sample = sample;
            }
        }
        self.flush_owners()?;
        let disk = Disk::read(&self.cache)?;
        tracing::info!(
            allocated = after,
            reclaimed = before.saturating_sub(after),
            available = disk.available,
            "Disk cache maintenance completed"
        );
        if self.cache_limits.pressured(after, disk) {
            tracing::warn!(
                allocated = after,
                available = disk.available,
                shortfall = self.cache_limits.reclaim(after, disk),
                dependencies_known = known,
                leased_views = crate::cache::views::Registry::open(&self.cache)?
                    .roots()
                    .len(),
                compact_copy_savings = savings,
                "Cache targets remain unmet; active leases, protected owners, and compaction may defer reclamation"
            );
        }
        outcome.progress |= after < before;
        outcome.pressure = self.cache_limits.pressured(after, disk);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{
        Repository, Rule,
        manager::{State, write_json},
        materialize::{Prepared, Target},
    };

    #[test]
    fn pressure_schedule_backs_off_stalls_and_resumes_after_progress() {
        let now = tokio::time::Instant::now();
        let interval = std::time::Duration::from_secs(15 * 60);
        let mut schedule = MaintenanceSchedule::new(now, interval);
        let stalled = MaintenanceOutcome {
            pressure: true,
            compaction_deferred: Some(CompactionDeferral::Busy),
            ..Default::default()
        };
        schedule.collected(now, Some(stalled));
        let fast = schedule.collection - now;
        let mut previous = fast;
        for _ in 0..16 {
            schedule.collected(now, Some(stalled));
            let delay = schedule.collection - now;
            assert!(delay >= previous && delay <= interval);
            previous = delay;
        }
        assert_eq!(previous, interval);
        assert!(schedule.compaction.unwrap() < schedule.collection);
        let full = schedule.collection;
        schedule.compacted(now, stalled);
        assert_eq!(
            schedule.collection, full,
            "Cheap retries must not pull full scans forward"
        );
        schedule.collected(
            now,
            Some(MaintenanceOutcome {
                progress: true,
                ..stalled
            }),
        );
        assert_eq!(schedule.collection - now, fast);
        schedule.compacted(
            now,
            MaintenanceOutcome {
                progress: true,
                ..Default::default()
            },
        );
        assert!(schedule.compaction.is_none());
        assert_eq!(schedule.collection - now, interval);
        schedule.collected(
            now,
            Some(MaintenanceOutcome {
                compaction_deferred: Some(CompactionDeferral::Changed),
                ..stalled
            }),
        );
        assert!(
            schedule.compaction.is_none(),
            "Discarded copies must use collection backoff"
        );
    }

    #[test]
    fn pressure_retries_recover_when_live_dependencies_are_released() {
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(cache.path(), vec![]);
        app.cache_limits.max_bytes = 1;
        let view = cache.path().join("unity-packages/active");
        fs::create_dir_all(&view).unwrap();
        fs::write(view.join("input.cs"), "class Active {}").unwrap();
        let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
        leases.retain(&view).unwrap();
        let protected = app.maintain_disk_shared().unwrap();
        assert!(protected.pressure && !protected.progress);
        assert!(
            protected.git_completed,
            "Pressure reclamation replaces the ordinary Git pass"
        );
        assert!(view.exists());
        drop(leases);
        let released = app.maintain_disk_shared().unwrap();
        assert!(released.progress);
        assert!(!view.exists());
    }

    #[test]
    fn view_collection_rechecks_ownership_published_after_snapshot() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let view = cache.path().join("unity-packages/newly-owned");
        fs::create_dir_all(&view).unwrap();
        let entry = cache.path().join("owner");
        let manifest = crate::workspace::Manifest {
            root: entry.clone(),
            dependencies: std::collections::BTreeSet::from([view.clone()]),
            ..Default::default()
        };
        assert!(
            !app.collect_views_observed(true, || {
                app.owners
                    .lock()
                    .unwrap()
                    .observe(&entry, None, &manifest, None);
            })
            .unwrap()
        );
        assert!(view.exists());
        app.owners.lock().unwrap().remove(&entry);
        assert!(app.collect_views(true).unwrap());
        assert!(!view.exists());
    }

    #[tokio::test]
    async fn ordinary_git_cleanup_runs_unless_pressure_already_completed_it() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let orphan = cache.path().join("git").join("a".repeat(64));
        let create = || {
            fs::create_dir_all(&orphan).unwrap();
            fs::File::open(&orphan)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
        };
        create();
        app.maintain_git(Some(MaintenanceOutcome {
            git_completed: true,
            ..Default::default()
        }))
        .await
        .unwrap();
        assert!(
            orphan.exists(),
            "A completed pressure pass must not scan Git again"
        );
        app.maintain_git(Some(MaintenanceOutcome::default()))
            .await
            .unwrap();
        assert!(
            !orphan.exists(),
            "Ordinary TTL cleanup must still run without pressure"
        );
        create();
        app.maintain_git(None).await.unwrap();
        assert!(
            !orphan.exists(),
            "A failed disk pass must not suppress Git cleanup"
        );
    }

    #[tokio::test]
    async fn successful_compaction_rechecks_pressure_after_busy_retry() {
        use crate::store::shared::{Database, Encoded, Names};
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(cache.path(), vec![]);
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database.workspace([1; 32], cache.path(), None).unwrap();
        let mut files = Vec::new();
        // Grow to the configured compaction threshold instead of asserting its value.
        while !(crate::cache::compaction::Stats {
            allocated: database
                .disk_stats()
                .unwrap()
                .allocated
                .saturating_sub(8 * 1024 * 1024),
            live: 0,
        })
        .eligible()
        {
            let file = format!("temporary-{}", files.len());
            scope
                .install(
                    &file,
                    vec![1],
                    *blake3::hash(file.as_bytes()).as_bytes(),
                    1,
                    || Ok(()),
                    || {
                        let mut records = std::collections::BTreeMap::from([(
                            vec![1],
                            vec![42; 8 * 1024 * 1024],
                        )]);
                        let names = crate::store::format::write(
                            &[],
                            None,
                            true,
                            &Names::default(),
                            &mut records,
                        )?;
                        Ok(Encoded { names, records })
                    },
                )
                .unwrap();
            files.push(file);
        }
        for file in files {
            scope.detach(&file, 2).unwrap();
        }
        database.collect_unused(3, true).unwrap();
        let stats = database.disk_stats().unwrap();
        assert!(stats.eligible());
        app.cache_limits.max_bytes =
            blobs::allocated(cache.path()).unwrap() - stats.reclaimable() / 2;
        app.cache_limits.headroom_percent = 0;
        drop(scope);
        drop(database);
        let app = Arc::new(app);
        let active = app.activity.read().await;
        let busy = app.retry_compaction().await.unwrap();
        assert_eq!(busy.compaction_deferred, Some(CompactionDeferral::Changed));
        assert!(!busy.progress && busy.pressure);
        assert!(app.activity.try_read().is_ok());
        assert!(!cache.path().join("analysis/data.mdb.compacting").exists());
        drop(active);
        let finished = app.retry_compaction().await.unwrap();
        assert!(finished.progress && !finished.pressure);
        assert!(finished.compaction_deferred.is_none());
        app.shutdown().await;
    }

    #[tokio::test]
    async fn compaction_retries_skip_collection_and_stop_after_shutdown() {
        let cache = tempfile::tempdir().unwrap();
        let app = Arc::new(app(cache.path(), vec![]));
        let view = cache.path().join("unity-packages/unused");
        fs::create_dir_all(&view).unwrap();
        let outcome = app.retry_compaction().await.unwrap();
        assert!(!outcome.progress && !outcome.git_completed);
        assert!(
            view.exists(),
            "Cheap compaction retries must not run collection"
        );
        app.inspection_shutdown.cancel();
        let outcome = app.retry_compaction().await.unwrap();
        assert!(outcome.compaction_deferred.is_none() && !outcome.progress);
        assert!(view.exists());
        app.shutdown().await;
    }

    #[test]
    fn background_manifests_preserve_all_workspace_dependencies() {
        use crate::{
            model::Language,
            native::Group,
            store::Store,
            workspace::{FileEntry, Manifest, Stamp},
        };
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let managed = cache.path().join("local-packages/managed/Library.dll");
        let native = cache.path().join("unity-packages/native/plugin.h");
        for path in [&managed, &native] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"retained").unwrap();
        }
        let mut primary = Manifest {
            root: root.path().into(),
            ..Default::default()
        };
        primary
            .metadata
            .insert(managed.clone(), Stamp::read(&managed).unwrap());
        primary.deferred.insert(
            "plugin.h".into(),
            FileEntry {
                path: native.clone(),
                display: "plugin.h".into(),
                stamp: Stamp::read(&native).unwrap(),
                language: Language::Header,
                memberships: vec![],
                modules: vec![],
                metadata: false,
            },
        );
        for (key, manifest) in [
            ([0; 32], primary),
            (
                [1; 32],
                Manifest {
                    source_group: Some(Group::Native),
                    ..Default::default()
                },
            ),
            (
                [2; 32],
                Manifest {
                    source_group: Some(Group::Shaders),
                    ..Default::default()
                },
            ),
        ] {
            Store::open_workspace(&cache.path().join("analysis"), key, root.path(), None)
                .unwrap()
                .save_manifest(&manifest)
                .unwrap();
        }
        assert!(app.seed_owners().unwrap());
        app.collect_views(true).unwrap();
        assert!(managed.is_file());
        assert!(native.is_file());
    }

    #[test]
    fn cancelled_ownership_scan_keeps_existing_owners() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let missing = cache.path().join("missing-workspace");
        app.owners.lock().unwrap().seed(&missing, None, 0);
        app.inspection_shutdown.cancel();
        assert!(!app.seed_owners().unwrap());
        assert!(app.owners.lock().unwrap().entries.contains_key(&missing));
    }

    fn app(cache: &Path, rules: Vec<Rule>) -> App {
        App::remote(
            Policy::new(vec![]).unwrap(),
            cache.into(),
            1,
            crate::config::RemoteOptions {
                rules,
                refresh_interval: None,
                repo_ttl: std::time::Duration::from_secs(7 * 86400),
                branch_ttl: std::time::Duration::from_secs(86400),
                unity_versions: vec![],
                selection: crate::repository::selection::Selection::new(&[], &[]).unwrap(),
            },
        )
        .unwrap()
        .with_cache_limits(crate::cache::policy::Limits {
            max_bytes: u64::MAX,
            headroom_percent: 0,
        })
    }
    fn seed(cache: &Path, name: &str, branch: &str, default: &str, last_use: u64) -> PathBuf {
        let repository = Repository::parse(&format!("https://github.com/example/{name}"))
            .unwrap()
            .unwrap();
        let key =
            blake3::hash(&serde_json::to_vec(&(&repository.identity, branch)).unwrap()).to_hex();
        let root = cache.join("repositories").join(key.as_str());
        fs::create_dir_all(root.join("source")).unwrap();
        fs::write(root.join("source/File.cs"), name.repeat(32 * 1024)).unwrap();
        write_json(&cache.join("defaults").join(repository.identity.storage_key()), &serde_json::json!({
            "repository": repository.identity, "branch": default, "refreshed": crate::cache::now(),
        })).unwrap();
        let revision = "a".repeat(40);
        let state = State {
            schema: 4,
            repository: repository.identity,
            transport: repository.transport,
            branch: branch.into(),
            target: Some(Target::Branch(branch.into())),
            last_use,
            refreshed: last_use,
            policy: String::new(),
            indexed_revision: Some(revision.clone()),
            repair: false,
            additional: Default::default(),
            prepared: Prepared {
                revision,
                branch: Some(branch.into()),
                selected: Default::default(),
                tracked: Default::default(),
                directories: Default::default(),
                omitted: Default::default(),
                unavailable: Default::default(),
                transfer_bytes: 0,
                transport: None,
                resolved_target: Some(Target::Branch(branch.into())),
            },
        };
        write_json(&root.join("state.json"), &state).unwrap();
        root
    }

    #[tokio::test]
    async fn maintenance_reclaims_idle_storage_during_activity() {
        let cache = tempfile::tempdir().unwrap();
        let app = Arc::new(app(
            cache.path(),
            vec![Rule::parse_private("https://github.com/example/*").unwrap()],
        ));
        let expired = seed(cache.path(), "expired", "trunk", "trunk", 1);
        let active = app.activity.read().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), app.maintain_disk())
            .await
            .expect("Maintenance must not wait behind active work")
            .unwrap();
        assert!(
            !expired.exists(),
            "Unrelated activity must not defer physical cleanup"
        );
        assert!(
            app.activity.try_read().is_ok(),
            "Maintenance must not queue ahead of new queries"
        );
        drop(active);
        app.maintain_disk().await.unwrap();
        assert!(!expired.exists(), "Retired storage must remain absent");
    }

    #[tokio::test]
    async fn queries_continue_during_a_slow_maintenance_stage() {
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("README.md"), "Available during collection").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = app.clone();
        let maintenance = tokio::spawn(async move {
            worker
                .maintain_disk_observed(move || {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                })
                .await
        });
        ready.await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            app.view(root.path().to_str().unwrap(), "README.md", "exact"),
        )
        .await;
        release.send(()).unwrap();
        maintenance.await.unwrap().unwrap();
        assert!(
            result
                .unwrap()
                .unwrap()
                .contains("Available during collection")
        );
        app.shutdown().await;
    }

    #[test]
    fn unfinished_discovery_protects_dependencies_while_idle_views_are_removed() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let live = cache.path().join("unity-packages/live");
        let idle = cache.path().join("unity-packages/idle");
        let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
        leases.retain(&live).unwrap();
        for path in [&live, &idle] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("input.cs"), "class Input {}").unwrap();
        }
        app.collect_views(true).unwrap();
        assert!(live.join("input.cs").is_file());
        assert!(!idle.exists());
        drop(leases);
        app.collect_views(true).unwrap();
        assert!(!live.exists());
    }

    #[tokio::test]
    async fn active_local_workspace_survives_pressure_eviction() {
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("lib.rs"), "pub struct Retained;").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        app.search(root.path().to_str().unwrap(), "type:Retained")
            .await
            .unwrap();
        let active = app
            .workspaces
            .lock()
            .unwrap()
            .get(root.path())
            .unwrap()
            .state
            .clone();
        let candidate = app
            .candidates()
            .unwrap()
            .into_iter()
            .find(|c| c.owner.entry == root.path())
            .unwrap();
        assert!(!app.evict_candidate(&candidate, true).unwrap());
        assert!(
            active
                .lock()
                .await
                .as_ref()
                .unwrap()
                .manifest
                .files
                .values()
                .any(|f| f.path.ends_with("lib.rs"))
        );
        drop(active);
        app.shutdown().await;
    }

    #[tokio::test]
    async fn requests_report_maintenance_and_cancellation_then_recover() {
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("README.md"), "Available after maintenance").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let gate = app.activity.write().await;
        let path = root.path().to_str().unwrap();
        let request = Request::View("README.md".into(), crate::navigation::Mode::Exact);
        let cancel = tokio_util::sync::CancellationToken::new();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            app.request_cancellable(path, request.clone(), &cancel, None),
        )
        .await
        .unwrap()
        .unwrap_err();
        let details = crate::diagnostics::details(&error, "maintenance");
        assert_eq!(details["error_code"], "UNAVAILABLE");
        assert_eq!(details["retryable"], true);
        cancel.cancel();
        let error = app
            .request_cancellable(path, request, &cancel, None)
            .await
            .unwrap_err();
        let details = crate::diagnostics::details(&error, "cancelled");
        assert_eq!(details["error_code"], "CANCELLED");
        assert_eq!(details["retryable"], false);
        drop(gate);
        assert!(
            app.view(path, "README.md", "exact")
                .await
                .unwrap()
                .contains("Available after maintenance")
        );
        app.shutdown().await;
    }

    #[test]
    fn ordinary_collection_makes_progress_and_pressure_finishes_retirement() {
        use crate::store::shared::{Database, Encoded, Names};
        let cache = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let app = app(cache.path(), vec![]);
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database.workspace([1; 32], root.path(), None).unwrap();
        let object = [1; 32];
        for i in 0..1024 {
            scope
                .install(
                    &format!("file-{i}"),
                    vec![1],
                    object,
                    1,
                    || Ok(()),
                    || {
                        let mut records = Default::default();
                        let names = crate::store::format::write(
                            &[],
                            None,
                            true,
                            &Names::default(),
                            &mut records,
                        )?;
                        Ok(Encoded { names, records })
                    },
                )
                .unwrap();
        }
        let bindings = || {
            let tx = scope.read().unwrap();
            database.binding_count(&tx, &object).unwrap().unwrap()
        };
        let before = bindings();
        scope.mark_deleting().unwrap();
        app.collect_analysis(false).unwrap();
        let remaining = bindings();
        assert!(remaining > 0 && remaining < before);
        app.collect_analysis(false).unwrap();
        assert!(
            bindings() < remaining,
            "Ordinary passes must resume deletion"
        );
        app.collect_analysis(true).unwrap();
        assert!(database.workspaces().unwrap().is_empty());
    }
    #[test]
    fn pressure_protects_exact_default_but_not_other_versions() {
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(
            cache.path(),
            vec![
                Rule::parse("https://github.com/example/*").unwrap(),
                Rule::parse_private("https://github.com/example/keep").unwrap(),
            ],
        );
        let kept = seed(cache.path(), "keep", "trunk", "trunk", 1);
        let other = seed(
            cache.path(),
            "keep",
            "feature",
            "trunk",
            crate::cache::now(),
        );
        let wildcard = seed(cache.path(), "other", "trunk", "trunk", crate::cache::now());
        app.cache_limits.max_bytes = 1;
        app.maintain_disk_shared().unwrap();
        assert!(kept.join("source/File.cs").is_file());
        assert!(!other.exists());
        assert!(!wildcard.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "release benchmark: retained view scans with foreground navigation"]
    async fn retained_views_with_foreground_queries_benchmark() {
        for count in [1_000, 10_000] {
            let cache = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            fs::write(
                root.path().join("README.md"),
                "Foreground remains available",
            )
            .unwrap();
            let app = Arc::new(
                App::new(
                    Policy::new(vec![root.path().into()]).unwrap(),
                    cache.path().into(),
                    1,
                )
                .unwrap(),
            );
            let selector = root.path().to_str().unwrap();
            app.view(selector, "README.md", "exact").await.unwrap();
            for index in 0..count {
                let view = cache
                    .path()
                    .join("unity-packages")
                    .join(format!("view-{index:05}"));
                fs::create_dir_all(&view).unwrap();
                fs::write(view.join("input.cs"), "class Input {}").unwrap();
                if index % 10 != 0 {
                    let entry = root.path().join(format!("owner-{index:05}"));
                    let manifest = crate::workspace::Manifest {
                        root: entry.clone(),
                        dependencies: std::collections::BTreeSet::from([view]),
                        ..Default::default()
                    };
                    app.owners
                        .lock()
                        .unwrap()
                        .observe(&entry, None, &manifest, None);
                }
            }
            let worker = app.clone();
            let (started, ready) = tokio::sync::oneshot::channel();
            let collection = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                started.send(()).unwrap();
                worker.collect_views(true).unwrap();
                start.elapsed()
            });
            ready.await.unwrap();
            let mut latencies = Vec::new();
            loop {
                let start = Instant::now();
                assert!(
                    app.view(selector, "README.md", "exact")
                        .await
                        .unwrap()
                        .contains("Foreground")
                );
                latencies.push(start.elapsed());
                if collection.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let elapsed = collection.await.unwrap();
            latencies.sort_unstable();
            eprintln!(
                "views={count} collection_ms={} queries={} query_p50_us={} query_p95_us={} query_max_us={}",
                elapsed.as_millis(),
                latencies.len(),
                latencies[latencies.len() / 2].as_micros(),
                latencies[(latencies.len() - 1) * 95 / 100].as_micros(),
                latencies.last().unwrap().as_micros()
            );
            assert_eq!(
                fs::read_dir(cache.path().join("unity-packages"))
                    .unwrap()
                    .count(),
                count * 9 / 10
            );
            app.shutdown().await;
        }
    }

    #[test]
    #[ignore = "manual disk-pressure replay with existing free pages and live workspaces"]
    fn pressure_cleanup_benchmark() {
        use crate::store::{
            Store,
            shared::{Encoded, Names, PRESSURE_BATCH},
        };
        use crate::workspace::{FileEntry, Manifest, Stamp};
        use std::collections::BTreeMap;
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(
            cache.path(),
            vec![
                Rule::parse("https://github.com/example/*").unwrap(),
                Rule::parse_private("https://github.com/example/keep").unwrap(),
            ],
        );
        for (name, count) in [
            ("keep", 16),
            ("cold-a", 384),
            ("cold-b", 384),
            ("unused", 1024),
        ] {
            let root = seed(cache.path(), name, "main", "main", crate::cache::now());
            let entry = root.join("source");
            let store = Store::open_workspace(
                &cache.path().join("analysis"),
                *blake3::hash(name.as_bytes()).as_bytes(),
                &entry,
                Some(&root),
            )
            .unwrap();
            let mut manifest = Manifest {
                root: entry.clone(),
                ..Default::default()
            };
            for start in (0..count).step_by(16) {
                let pending: Vec<_> = (start..(start + 16).min(count))
                    .map(|i| {
                        let file = format!("file-{i}");
                        manifest.files.insert(
                            file.clone(),
                            FileEntry {
                                path: entry.join("File.cs"),
                                display: file.clone(),
                                stamp: Stamp::read(&entry.join("File.cs")).unwrap(),
                                language: crate::model::Language::CSharp,
                                memberships: vec![],
                                modules: vec![],
                                metadata: false,
                            },
                        );
                        store
                            .scope
                            .prepare_install(
                                &file,
                                vec![1],
                                *blake3::hash(format!("{name}/{i}").as_bytes()).as_bytes(),
                                100,
                                || {
                                    let mut records =
                                        BTreeMap::from([(vec![1], vec![42; 512 * 1024])]);
                                    let names = crate::store::format::write(
                                        &[],
                                        None,
                                        true,
                                        &Names::default(),
                                        &mut records,
                                    )?;
                                    Ok(Encoded { names, records })
                                },
                            )
                            .unwrap()
                    })
                    .collect();
                store.scope.install_batch(&pending, |_| Ok(())).unwrap();
            }
            store.save_manifest(&manifest).unwrap();
            if name == "unused" {
                store.scope.mark_deleting().unwrap();
                while !store.scope.delete_batch(100, PRESSURE_BATCH).unwrap() {}
                store.scope.database.collect_unused(100, true).unwrap();
                fs::remove_dir_all(root).unwrap();
            }
        }
        app.cache_limits.max_bytes = 256 * 1024 * 1024;
        let before = blobs::allocated(cache.path()).unwrap();
        let start = Instant::now();
        app.maintain_disk_shared().unwrap();
        {
            let _idle = app.activity.try_write().unwrap();
            app.compact_analysis().unwrap();
        }
        let elapsed = start.elapsed();
        let after = blobs::allocated(cache.path()).unwrap();
        assert!(
            !app.cache_limits
                .pressured(after, Disk::read(cache.path()).unwrap())
        );
        let db = crate::store::Database::open(&cache.path().join("analysis")).unwrap();
        let scope = db
            .existing(*blake3::hash(b"keep").as_bytes())
            .unwrap()
            .unwrap();
        assert!(scope.manifest().unwrap().is_some());
        eprintln!("pressure cleanup: {elapsed:?}; allocation {before} -> {after}");
        drop(scope);
        drop(db);
        let start = Instant::now();
        app.maintain_disk_shared().unwrap();
        eprintln!("ordinary cleanup: {:?}", start.elapsed());
    }
    #[test]
    fn disk_pressure_evicts_the_less_frequently_used_workspace_first() {
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(
            cache.path(),
            vec![Rule::parse("https://github.com/example/*").unwrap()],
        );
        let time = crate::cache::now();
        let hot = seed(cache.path(), "hot", "trunk", "trunk", time - 60_000);
        let cold = seed(cache.path(), "cold", "trunk", "trunk", time - 60_000);
        app.maintain_disk_shared().unwrap();
        app.owners
            .lock()
            .unwrap()
            .entries
            .get_mut(&hot.join("source"))
            .unwrap()
            .usage
            .record(time);
        app.cache_limits.max_bytes = blobs::allocated(cache.path()).unwrap() - 4096;
        app.maintain_disk_shared().unwrap();
        assert!(hot.exists());
        assert!(!cold.exists());
    }

    #[tokio::test]
    async fn inspection_reports_pins_and_eviction_without_changing_live_or_idle_data() {
        use crate::cache::inspect::{Overrides, inspect};
        let cache = tempfile::tempdir().unwrap();
        let mut app = app(
            cache.path(),
            vec![
                Rule::parse("https://github.com/example/*").unwrap(),
                Rule::parse_private("https://github.com/example/keep").unwrap(),
            ],
        );
        let now = crate::cache::now();
        let pinned = seed(cache.path(), "keep", "trunk", "trunk", now);
        let cold = seed(cache.path(), "cold", "trunk", "trunk", now - 60_000);
        let hot = seed(cache.path(), "hot", "trunk", "trunk", now - 60_000);
        app.seed_owners().unwrap();
        app.owners.lock().unwrap().observe(
            &hot.join("source"),
            Some(&hot),
            &crate::workspace::Manifest {
                root: hot.join("source"),
                ..Default::default()
            },
            Some(now),
        );
        app.owners.lock().unwrap().flush().unwrap();
        app.cache_limits.max_bytes = 1;
        app.cache_limits.headroom_percent = 0;
        let app = Arc::new(app);
        let server = app.start_inspection().unwrap();
        let state_before = fs::read(cold.join("state.json")).unwrap();
        let owners_before = fs::read(cache.path().join("owners.json")).unwrap();
        let report = inspect(cache.path(), Overrides::default()).await.unwrap();
        assert!(report.live_service);
        assert_eq!(
            report.preview.eviction_order,
            vec![cold.join("source"), hot.join("source")]
        );
        assert!(
            report
                .workspaces
                .iter()
                .find(|w| w.entry == pinned.join("source"))
                .unwrap()
                .protection
                .is_some()
        );
        assert_eq!(fs::read(cold.join("state.json")).unwrap(), state_before);
        assert_eq!(
            fs::read(cache.path().join("owners.json")).unwrap(),
            owners_before
        );
        let roomy = inspect(
            cache.path(),
            Overrides {
                max_bytes: Some(u64::MAX),
                headroom_percent: Some(0),
            },
        )
        .await
        .unwrap();
        assert!(roomy.preview.eviction_order.is_empty());
        // Proposed limits must not alter the service's real settings.
        assert_eq!(app.cache_limits.max_bytes, report.limits.max_bytes);
        app.shutdown().await;
        server.await.unwrap();
        drop(app);
        let offline = inspect(cache.path(), Overrides::default()).await.unwrap();
        assert!(!offline.live_service);
        assert_eq!(
            offline.preview.eviction_order,
            report.preview.eviction_order
        );
        assert_eq!(fs::read(cold.join("state.json")).unwrap(), state_before);
        assert_eq!(
            fs::read(cache.path().join("owners.json")).unwrap(),
            owners_before
        );
    }

    #[test]
    fn default_branch_rename_keeps_the_previous_copy_until_the_new_one_is_ready() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(
            cache.path(),
            vec![Rule::parse_private("https://github.com/example/keep").unwrap()],
        );
        let old = seed(cache.path(), "keep", "old", "old", 1);
        let new = seed(cache.path(), "keep", "next", "next", 1);
        let manager = app.remote.as_ref().unwrap();
        let read = |root: &Path| -> State {
            serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap()
        };
        let old_state = read(&old);
        let mut new_state = read(&new);
        let defaults = cache
            .path()
            .join("defaults")
            .join(old_state.repository.storage_key());
        write_json(&defaults, &serde_json::json!({
            "repository": old_state.repository, "branch": "next", "previous": "old", "refreshed": crate::cache::now()
        })).unwrap();
        new_state.indexed_revision = None;
        write_json(&new.join("state.json"), &new_state).unwrap();
        assert!(manager.retention(&old_state).unwrap().0);
        assert!(manager.retention(&new_state).unwrap().0);
        new_state.indexed_revision = Some(new_state.prepared.revision.clone());
        write_json(&new.join("state.json"), &new_state).unwrap();
        assert!(!manager.retention(&old_state).unwrap().0);
        assert!(manager.retention(&new_state).unwrap().0);
        fs::remove_file(defaults).unwrap();
        assert!(
            manager.retention(&old_state).unwrap().0,
            "Unknown remote metadata must preserve the last usable copy"
        );
    }

    #[tokio::test]
    async fn active_branch_handles_prevent_pressure_eviction() {
        let cache = tempfile::tempdir().unwrap();
        let app = app(
            cache.path(),
            vec![Rule::parse_private("https://github.com/example/*").unwrap()],
        );
        let root = seed(
            cache.path(),
            "active",
            "trunk",
            "trunk",
            crate::cache::now(),
        );
        let manager = app.remote.as_ref().unwrap();
        let mut state: State =
            serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap();
        state.policy = manager.options.selection.identity.clone();
        write_json(&root.join("state.json"), &state).unwrap();
        let branch = manager
            .resolve(
                Repository::parse("https://github.com/example/active#refs/heads/trunk")
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !manager
                .evict(&root, true, || Ok(()), |()| panic!("Retired a live branch"))
                .unwrap()
        );
        drop(branch);
        assert!(manager.evict(&root, true, || Ok(()), |()| Ok(())).unwrap());
        assert!(!root.exists());
    }

    #[test]
    fn changing_allowlist_releases_old_pins_but_usage_survives_restart() {
        let cache = tempfile::tempdir().unwrap();
        let rules = || vec![Rule::parse_private("https://github.com/example/keep").unwrap()];
        let first = app(cache.path(), rules());
        // Model the descriptor inherited by a concurrently spawned child.
        let _inherited = first._ownership.0.try_clone().unwrap();
        let root = seed(cache.path(), "keep", "trunk", "trunk", 1);
        first.seed_owners().unwrap();
        {
            let mut owners = first.owners.lock().unwrap();
            owners.observe(
                &root.join("source"),
                Some(&root),
                &Default::default(),
                Some(crate::cache::now()),
            );
            owners.flush().unwrap();
        }
        drop(first);
        let second = app(
            cache.path(),
            vec![Rule::parse_private("https://github.com/example/*").unwrap()],
        );
        second.maintain_disk_shared().unwrap();
        assert!(root.exists(), "Restart must use persisted query recency");
        let candidates = second.candidates().unwrap();
        assert!(!candidates[0].pinned);
        assert!(second.evict_candidate(&candidates[0], true).unwrap());
        assert!(!root.exists());
    }
}
