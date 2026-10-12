//! Cache ownership is coordinated at the service boundary, above storage drivers.
use super::*;
use crate::cache::{
    blobs,
    policy::{Candidate, Disk},
};
use std::{collections::VecDeque, fs};

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
        assert!(database.existing([1; 32]).unwrap().is_some());
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
        assert!(database.existing([1; 32]).unwrap().is_some());
        app.start_setup();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while database.existing([1; 32]).unwrap().is_some() {
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
        assert!(database.existing([1; 32]).unwrap().is_some());
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

fn maintenance_stage<T>(name: &str, run: impl FnOnce() -> Result<T>) -> Result<T> {
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
                let result = crate::cache::compaction::cleanup(&self.cache);
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
            if self.retire_analysis_scope(&database, *key, info, now, false)? {
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

    pub(super) async fn maintain_disk(self: &Arc<Self>) -> Result<()> {
        if self.inspection_shutdown.is_cancelled() {
            return Ok(());
        }
        if self
            .startup
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.borrow().is_none())
        {
            return Ok(());
        }
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            // A queued writer blocks later readers, including navigation, while
            // existing indexing jobs finish. Maintenance must wait for idle time.
            let Ok(gate) = app.activity.clone().try_write_owned() else {
                tracing::info!(
                    "Disk cache maintenance deferred while queries or indexing are active"
                );
                return Ok(());
            };
            let _gate = gate;
            if app.inspection_shutdown.is_cancelled() {
                return Ok(());
            }
            let start = Instant::now();
            tracing::info!("Disk cache maintenance started");
            let result = app.maintain_disk_exclusive();
            tracing::info!(
                elapsed_ms = start.elapsed().as_millis(),
                success = result.is_ok(),
                "Disk cache maintenance finished"
            );
            result
        })
        .await?
    }

    fn seed_owners(&self) -> Result<bool> {
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
                if info.phase == crate::store::shared::Phase::Deleting {
                    continue;
                }
                if let Some(scope) = database.existing(key)? {
                    let store = crate::store::Store { scope };
                    let manifest: Option<crate::workspace::Manifest> = store.get_manifest()?;
                    if let Some(manifest) = manifest {
                        // The primary inventory owns dependencies for every source group.
                        if manifest.source_group.is_some() {
                            continue;
                        }
                        self.owners.lock().unwrap().observe(
                            &info.entry,
                            info.owner.as_deref(),
                            &manifest,
                            None,
                        );
                    } else {
                        known = false;
                    }
                }
            }
        }
        let mut owners = self.owners.lock().unwrap();
        let removed: Vec<_> = owners
            .entries
            .values()
            .filter(|owner| !owner.repository.as_deref().unwrap_or(&owner.entry).exists())
            .map(|owner| owner.entry.clone())
            .collect();
        for entry in removed {
            owners.remove(&entry);
        }
        owners.flush()?;
        Ok(known)
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        crate::cache::policy::retention_candidates(
            &self.cache,
            &self.owners.lock().unwrap(),
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
        self.workspaces
            .lock()
            .unwrap()
            .remove(&candidate.owner.entry);
        self.sources.forget(&candidate.owner.entry);
        if let Some(root) = &candidate.owner.repository {
            if !self
                .remote
                .as_ref()
                .unwrap()
                .evict(root, pressure, || self.retire_owner(root))?
            {
                return Ok(false);
            }
        } else if self.cache.join("analysis").is_dir() {
            let database = crate::store::Database::open(&self.cache.join("analysis"))?;
            for (key, info) in database.workspaces()? {
                if info.entry == candidate.owner.entry {
                    database.existing(key)?.unwrap().mark_deleting()?;
                }
            }
        }
        self.owners.lock().unwrap().remove(&candidate.owner.entry);
        tracing::info!(entry = %candidate.owner.entry.display(), pressure, "Evicted cached workspace");
        Ok(true)
    }

    fn collect_analysis(&self, pressure: bool) -> Result<()> {
        // Ordinary passes retire and collect bounded batches. Disk pressure
        // drains all retired workspaces and unused objects.
        self.maintain_analysis(pressure)?;
        if self.cache.join("analysis").is_dir() {
            let database = crate::store::Database::open(&self.cache.join("analysis"))?;
            if pressure {
                database.collect_unused(crate::cache::now(), true)?;
            }
            database.checkpoint()?;
        }
        Ok(())
    }

    fn analysis_savings(&self, disk: Disk) -> Result<u64> {
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

    fn collect_views(&self, dependencies_known: bool) -> Result<()> {
        if !dependencies_known {
            return Ok(());
        }
        let owners = self.owners.lock().unwrap();
        for &kind in crate::cache::owners::VIEW_KINDS {
            let directory = self.cache.join(kind);
            if !directory.is_dir() {
                continue;
            }
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                ensure!(entry.file_type()?.is_dir(), "Invalid cache view directory");
                let configured = kind == "unity-metadata"
                    && self.remote.as_ref().is_some_and(|remote| {
                        crate::metadata_archive::configured(&path, &remote.options.unity_versions)
                    });
                if configured || owners.retains(&path) {
                    continue;
                }
                // All project jobs are drained. remove_outputs also handles overlay work dirs.
                crate::sandbox::remove_outputs(&path)?;
            }
        }
        let legacy = self.cache.join("dotnet/packages");
        if legacy.is_dir() && !owners.retains(&legacy) {
            crate::sandbox::remove_outputs(&legacy)?;
        }
        Ok(())
    }

    fn migrate_views(&self) -> Result<()> {
        if let Some(remote) = &self.remote {
            for (root, _) in remote.cached()? {
                let marker = root.join("shared-files-v1");
                if marker.exists() {
                    continue;
                }
                // Refresh jobs may still be fetching. Import only under the publication gate.
                if remote.migrate_inputs(&root, &self.blobs)? {
                    fs::write(marker, b"1")?;
                }
            }
        }
        let directory = self.cache.join("local-jobs");
        if directory.is_dir() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    crate::cache::packages::View::open(&self.cache, &entry.path())?;
                }
            }
        }
        let directory = self.cache.join("unity-packages");
        if directory.is_dir() {
            for entry in fs::read_dir(directory)? {
                let root = entry?.path();
                let marker = root.join("shared-files-v1");
                if root.join("contents").is_dir() && !marker.exists() {
                    self.blobs.import_tree(&root.join("contents"))?;
                    fs::write(marker, b"1")?;
                }
            }
        }
        Ok(())
    }

    fn maintain_disk_exclusive(&self) -> Result<()> {
        // Retry optional disposal independently of retirement and pressure relief.
        if let Err(error) = crate::cache::compaction::cleanup(&self.cache) {
            tracing::warn!(%error, "Cannot dispose of obsolete compaction files");
        }
        maintenance_stage("view migration", || self.migrate_views())?;
        let known = maintenance_stage("ownership scan", || self.seed_owners())?;
        let before = maintenance_stage("disk measurement", || blobs::allocated(&self.cache))?;
        let disk = Disk::read(&self.cache)?;
        let pressure = self.cache_limits.pressured(before, disk);
        let mut candidates = maintenance_stage("eviction planning", || self.candidates())?;
        for candidate in candidates.iter().filter(|candidate| candidate.expired) {
            self.evict_candidate(candidate, false)?;
        }
        maintenance_stage("analysis collection", || self.collect_analysis(pressure))?;
        maintenance_stage("view collection", || self.collect_views(known))?;
        maintenance_stage("blob collection", || {
            self.blobs.collect_with_pressure(pressure)
        })?;
        maintenance_stage("asset record collection", || {
            self.asset_records.collect(pressure)
        })?;
        let mut after = blobs::allocated(&self.cache)?;
        candidates.retain(|candidate| !candidate.expired);
        let mut remaining = self.cache_limits.reclaim(after, Disk::read(&self.cache)?);
        let mut savings = if remaining > 0 {
            self.analysis_savings(Disk::read(&self.cache)?)?
        } else {
            0
        };
        if remaining > savings {
            maintenance_stage("eviction ranking", || {
                crate::cache::policy::rank(
                    &self.owners.lock().unwrap(),
                    &mut candidates,
                    crate::cache::now(),
                )
            })?;
        }
        candidates.retain(|candidate| !candidate.pinned);
        let mut candidates = candidates.into_iter();
        while remaining > savings {
            // Credit an affordable compact copy, but postpone the copy until all
            // victims are retired. Git packing savings are deliberately not assumed.
            let needed = remaining - savings;
            let allocated = after;
            let mut estimated = 0;
            let mut evicted = false;
            for candidate in candidates.by_ref() {
                if self.evict_candidate(&candidate, true)? {
                    evicted = true;
                    estimated += candidate.bytes;
                }
                if estimated >= needed {
                    break;
                }
            }
            if !evicted {
                break;
            }
            maintenance_stage("analysis collection", || self.collect_analysis(true))?;
            maintenance_stage("view collection", || self.collect_views(known))?;
            maintenance_stage("blob collection", || self.blobs.collect_with_pressure(true))?;
            maintenance_stage("asset record collection", || {
                self.asset_records.collect(true)
            })?;
            after = blobs::allocated(&self.cache)?;
            remaining = remaining.saturating_add(after).saturating_sub(allocated);
            savings = self.analysis_savings(Disk::read(&self.cache)?)?;
        }
        // Avoid repacking Git when the known compact-copy savings already meet
        // the target. Repacking is the fallback after idle victims are exhausted.
        if remaining > savings {
            let allocated = after;
            maintenance_stage("Git reclamation", || {
                crate::repository::cache::reclaim(&self.cache)
            })?;
            after = blobs::allocated(&self.cache)?;
            remaining = remaining.saturating_add(after).saturating_sub(allocated);
        }
        if remaining > 0
            && maintenance_stage("analysis compaction", || self.compact_analysis())? > 0
        {
            after = blobs::allocated(&self.cache)?;
        }
        self.owners.lock().unwrap().flush()?;
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
                "Cache targets cannot be met by evicting idle unpinned data"
            );
        }
        Ok(())
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
    async fn maintenance_yields_to_activity_and_retries_when_idle() {
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
        assert!(expired.exists());
        assert!(
            app.activity.try_read().is_ok(),
            "Maintenance must not queue ahead of new queries"
        );
        drop(active);
        app.maintain_disk().await.unwrap();
        assert!(
            !expired.exists(),
            "Deferred cleanup must run once the service is idle"
        );
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
        app.maintain_disk_exclusive().unwrap();
        assert!(kept.join("source/File.cs").is_file());
        assert!(!other.exists());
        assert!(!wildcard.exists());
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
        app.maintain_disk_exclusive().unwrap();
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
        app.maintain_disk_exclusive().unwrap();
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
        app.maintain_disk_exclusive().unwrap();
        app.owners
            .lock()
            .unwrap()
            .entries
            .get_mut(&hot.join("source"))
            .unwrap()
            .usage
            .record(time);
        app.cache_limits.max_bytes = blobs::allocated(cache.path()).unwrap() - 4096;
        app.maintain_disk_exclusive().unwrap();
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
                .evict(&root, true, || panic!("Retired a live branch"))
                .unwrap()
        );
        drop(branch);
        assert!(manager.evict(&root, true, || Ok(())).unwrap());
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
        second.maintain_disk_exclusive().unwrap();
        assert!(root.exists(), "Restart must use persisted query recency");
        let candidates = second.candidates().unwrap();
        assert!(!candidates[0].pinned);
        assert!(second.evict_candidate(&candidates[0], true).unwrap());
        assert!(!root.exists());
    }
}
