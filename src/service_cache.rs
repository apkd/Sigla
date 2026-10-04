//! Cache ownership is coordinated at the service boundary, above storage drivers.
use super::*;
use crate::cache::{
    blobs,
    policy::{Candidate, Disk},
};
use std::fs;

impl App {
    pub(super) async fn maintain_disk(self: &Arc<Self>) -> Result<()> {
        if self
            .startup
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.borrow().is_none())
        {
            return Ok(());
        }
        let gate = self.activity.clone().write_owned().await;
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            app.maintain_disk_exclusive()
        })
        .await?
    }

    fn seed_owners(&self) -> Result<bool> {
        let mut known = true;
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
        crate::cache::policy::candidates(
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
        self.maintain_analysis(true)?;
        if self.cache.join("analysis").is_dir() {
            crate::store::Database::open(&self.cache.join("analysis"))?
                .collect_unused(crate::cache::now(), pressure)?;
        }
        Ok(())
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
                let configured = kind == "editors"
                    && self.remote.as_ref().is_some_and(|remote| {
                        remote
                            .options
                            .unity_versions
                            .iter()
                            .any(|version| entry.file_name() == version.to_string().as_str())
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
        self.migrate_views()?;
        let known = self.seed_owners()?;
        let before = blobs::allocated(&self.cache)?;
        let disk = Disk::read(&self.cache)?;
        let pressure = self.cache_limits.pressured(before, disk);
        let mut candidates = self.candidates()?;
        for candidate in candidates.iter().filter(|candidate| candidate.expired) {
            self.evict_candidate(candidate, false)?;
        }
        self.collect_analysis(pressure)?;
        self.collect_views(known)?;
        if pressure {
            crate::repository::cache::reclaim(&self.cache)?;
        }
        self.blobs.collect_with_pressure(pressure)?;
        self.compact_analysis()?;
        candidates.retain(|candidate| !candidate.expired && !candidate.pinned);
        let mut remaining = self
            .cache_limits
            .reclaim(blobs::allocated(&self.cache)?, Disk::read(&self.cache)?);
        let mut candidates = candidates.into_iter();
        while remaining > 0 {
            let allocated = blobs::allocated(&self.cache)?;
            let mut estimated = 0;
            let mut evicted = false;
            for candidate in candidates.by_ref() {
                if self.evict_candidate(&candidate, true)? {
                    evicted = true;
                    estimated += candidate.bytes;
                }
                if estimated >= remaining {
                    break;
                }
            }
            if !evicted {
                break;
            }
            self.collect_analysis(true)?;
            self.collect_views(known)?;
            crate::repository::cache::reclaim(&self.cache)?;
            self.blobs.collect_with_pressure(true)?;
            self.compact_analysis()?;
            remaining =
                remaining.saturating_sub(allocated.saturating_sub(blobs::allocated(&self.cache)?));
        }
        self.owners.lock().unwrap().flush()?;
        let after = blobs::allocated(&self.cache)?;
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
