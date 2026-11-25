//! Branch ownership, shared acquisition, and fetch-failure preservation.
use super::{
    Identity, Repository, authorize, job,
    materialize::{Prepared, Request, Target},
};
use crate::config::RemoteOptions;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock as AsyncRwLock, watch,
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
type Outcome = std::result::Result<(), Arc<anyhow::Error>>;
type Running = watch::Receiver<Option<Outcome>>;

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn options() -> RemoteOptions {
        RemoteOptions {
            rules: vec![],
            refresh_interval: None,
            repo_ttl: Duration::from_secs(7 * 86400),
            branch_ttl: Duration::from_secs(86400),
            unity_versions: vec![],
            selection: super::super::selection::Selection::new(&[], &[]).unwrap(),
        }
    }

    fn branch(target: Target, last_use: u64) -> Branch {
        Branch {
            root: PathBuf::new(),
            repository: Repository::parse("https://github.com/owner/repo")
                .unwrap()
                .unwrap(),
            name: "main".into(),
            target,
            state: Arc::new(AsyncRwLock::new(None)),
            analysis: Arc::new(AsyncRwLock::new(())),
            last_use: AtomicU64::new(last_use),
            persisted_use: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            running: Mutex::new(None),
            telemetry_id: Mutex::new(None),
            retry_after: AtomicU64::new(0),
            acquiring: Arc::new(AsyncMutex::new(())),
            sources: Mutex::new(None),
            state_bytes: Mutex::new(None),
            inventory: watch::channel(None).0,
            initialized: Arc::new(AsyncMutex::new(true)),
            demand: Arc::default(),
            inventory_demand: Arc::default(),
        }
    }

    #[tokio::test]
    async fn readiness_releases_state_before_waiting_for_publication() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let branch = Arc::new(branch(Target::Branch("main".into()), 0));
        let (sender, receiver) = watch::channel(None);
        *branch.running.lock().unwrap() = Some(receiver);
        let operation = crate::telemetry::Operation::default();
        let ready = manager.ready(&branch, &operation);
        tokio::pin!(ready);
        std::future::poll_fn(|cx| {
            assert!(ready.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let publication = branch
            .state
            .try_write()
            .expect("Readiness blocked publication");
        sender.send_replace(Some(Err(Arc::new(crate::diagnostics::Cancelled.into()))));
        drop(publication);
        assert!(ready.await.is_err());
    }

    #[tokio::test]
    async fn idle_required_input_admission_does_not_start_acquisition() {
        use std::future::Future;
        let cache = tempfile::tempdir().unwrap();
        let mut options = options();
        options
            .rules
            .push(crate::repository::Rule::parse_private("https://github.com/owner/*").unwrap());
        let manager = Manager::new(cache.path().into(), options).unwrap();
        let branch = Arc::new(branch(Target::Branch("main".into()), 0));
        let demand = Arc::new(crate::demand::Demand::new(Duration::ZERO));
        let caller = demand.interest();
        let work = Arc::new(demand.work());
        let _held = branch.acquiring.lock().await;
        let operation = crate::telemetry::Operation::default();
        let waiting =
            manager.require_inputs_cancellable(&branch, "revision", Vec::new(), &operation, work);
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(waiting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(caller);
        let error = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is::<crate::demand::Paused>());
    }

    #[tokio::test]
    async fn shutdown_joins_default_branch_lookups() {
        use std::future::Future;
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let (send, receive) = watch::channel(None);
        manager
            .default_running
            .lock()
            .unwrap()
            .insert("pending".into(), (0, receive, None));
        let stopping = manager.flush_usage();
        tokio::pin!(stopping);
        std::future::poll_fn(|cx| {
            assert!(stopping.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        send.send_replace(Some(Err(Arc::new(crate::diagnostics::Cancelled.into()))));
        tokio::time::timeout(Duration::from_secs(2), stopping)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn public_cached_acquisition_links_its_refresh_without_a_network_check() {
        let cache = tempfile::tempdir().unwrap();
        let mut options = options();
        options
            .rules
            .push(crate::repository::Rule::parse("https://github.com/owner/*").unwrap());
        let manager = Manager::new(cache.path().into(), options).unwrap();
        let repository = Repository::parse("https://github.com/owner/repo#refs/heads/main")
            .unwrap()
            .unwrap();
        manager.defaults.lock().await.insert(
            repository.identity.storage_key(),
            DefaultBranch {
                repository: repository.identity.clone(),
                branch: "main".into(),
                refreshed: now(),
                previous: None,
            },
        );
        let branch = Arc::new(branch(Target::Branch("main".into()), 0));
        let (sender, receiver) = watch::channel(None);
        *branch.running.lock().unwrap() = Some(receiver);
        *branch.telemetry_id.lock().unwrap() = Some("refresh-running".into());
        let key = blake3::hash(&crate::binary::encode(&(&repository.identity, "main")).unwrap())
            .to_hex()
            .to_string();
        manager.branches.lock().unwrap().insert(key, branch.clone());
        let (operation, recorder) = crate::telemetry::test_operation();
        let mut resolve = Box::pin(manager.resolve_monitored(repository, &operation));
        std::future::poll_fn(|cx| {
            assert!(resolve.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(resolve);
        operation.finish("cancelled");
        let records = recorder.completed();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].links, ["refresh-running"]);

        sender.send_replace(Some(Err(Arc::new(
            crate::diagnostics::RepositoryTransfer::new(std::io::Error::other("finished")).into(),
        ))));
        let (operation, recorder) = crate::telemetry::test_operation();
        let error = manager.ready(&branch, &operation).await.unwrap_err();
        assert_eq!(
            crate::diagnostics::monitoring(&error),
            Some(crate::telemetry::Diagnostic::RepositoryTransfer)
        );
        operation.finish("failure");
        assert!(recorder.completed()[0].links.is_empty());
    }

    #[tokio::test]
    async fn optional_publication_preserves_active_analysis_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let branch = Arc::new(Branch::for_test(root.path().into()));
        *branch.state.write().await = Some(Arc::new(state()));
        let analysis = branch.analyze().await;
        let old = analysis.as_ref().unwrap().prepared.deferred_assets.clone();
        {
            let mut publication =
                tokio::time::timeout(Duration::from_secs(1), branch.publication(true))
                    .await
                    .unwrap();
            publication
                .as_mut()
                .unwrap()
                .prepared
                .deferred_assets
                .clear();
        }
        assert_eq!(analysis.as_ref().unwrap().prepared.deferred_assets, old);
        assert!(
            branch
                .state
                .read()
                .await
                .as_ref()
                .unwrap()
                .prepared
                .deferred_assets
                .is_empty()
        );
        let mut publication = Box::pin(branch.publish());
        std::future::poll_fn(|cx| {
            assert!(publication.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(analysis);
        drop(
            tokio::time::timeout(Duration::from_secs(1), publication)
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn asset_acquisition_wait_does_not_hold_the_index_worker() {
        use crate::unity::assets::jobs::{Jobs, Request, generation, wait};

        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut options = options();
        options
            .rules
            .push(crate::repository::Rule::parse_private("https://github.com/owner/*").unwrap());
        let manager = Manager::new(cache.path().into(), options).unwrap();
        let branch = Arc::new(Branch::for_test(root.path().into()));
        let mut branch_state = state();
        branch_state.prepared.deferred_assets.clear();
        *branch.state.write().await = Some(Arc::new(branch_state));
        let held_state = branch.state.write().await;

        let mut workspace = crate::workspace::Workspace::open(
            root.path().into(),
            cache.path(),
            crate::discovery::Policy::new(vec![root.path().into()]).unwrap(),
            &cache.path().join("analysis"),
            None,
            Arc::new(crate::watch::Monitor::default()),
        )
        .unwrap();
        workspace.refresh().unwrap();
        let expected = generation(&workspace.manifest).unwrap();
        let assets = cache.path().join("unity-assets/worker");
        let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
        leases.retain(&assets).unwrap();
        let jobs = Jobs::default();
        let ticket = jobs.start(Request {
            activity: Arc::new(Arc::new(AsyncRwLock::new(())).read_owned().await),
            workspace: Arc::new(AsyncMutex::new(Some(workspace))),
            branch: Some(branch.clone()),
            remote: Some(manager),
            root: root.path().into(),
            cache: assets,
            records: Arc::new(crate::cache::artifacts::Store::open(cache.path()).unwrap()),
            expected,
            revision: Some("commit".into()),
            refresh: false,
            leases,
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while branch.acquiring.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Asset acquisition did not reach the held branch state");
        let permit = tokio::time::timeout(Duration::from_secs(5), jobs.pause())
            .await
            .expect("Asset acquisition held the index worker");
        drop(permit);
        drop(held_state);
        tokio::time::timeout(Duration::from_secs(5), wait(ticket))
            .await
            .expect("Asset indexing did not finish")
            .unwrap();
    }

    #[test]
    fn eviction_rechecks_branch_handles_and_identity_after_planning() {
        for existing in [false, true] {
            let cache = tempfile::tempdir().unwrap();
            let manager = Manager::new(cache.path().into(), options()).unwrap();
            let root = cache.path().join("repositories/candidate");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("source.cs"), "class Retained {}").unwrap();
            if existing {
                manager
                    .branches
                    .lock()
                    .unwrap()
                    .insert("candidate".into(), Arc::new(Branch::for_test(root.clone())));
            }
            let mut acquired = None;
            let removed = manager
                .evict(
                    &root,
                    true,
                    || {
                        // This lock also proves planning does not hold the registry.
                        let mut branches = manager.branches.lock().unwrap();
                        acquired = Some(
                            branches
                                .entry("candidate".into())
                                .or_insert_with(|| Arc::new(Branch::for_test(root.clone())))
                                .clone(),
                        );
                        Ok(())
                    },
                    |()| panic!("Retired newly acquired inputs"),
                )
                .unwrap();
            assert!(!removed);
            assert!(root.join("source.cs").exists());
            assert!(acquired.is_some());
        }
    }

    #[test]
    fn eviction_rejects_replaced_directory_after_planning() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let root = cache.path().join("repositories/candidate");
        fs::create_dir_all(&root).unwrap();
        assert!(
            !manager
                .evict(
                    &root,
                    true,
                    || {
                        fs::rename(&root, cache.path().join("old"))?;
                        fs::create_dir(&root)?;
                        fs::write(root.join("new.cs"), "class New {}")?;
                        Ok(())
                    },
                    |()| panic!("Retired replacement inputs")
                )
                .unwrap()
        );
        assert!(root.join("new.cs").exists());
    }

    #[test]
    fn deadlines_are_anchored_and_activity_is_per_selector() {
        let options = options();
        let refreshed = 12 * 3600 * 1000;
        let active = branch(Target::Branch("main".into()), 0);
        let idle_tag = branch(Target::Tag("release".into()), 0);
        let delay = options
            .refresh_delay(Duration::from_millis(refreshed))
            .as_millis() as u64;
        assert!(!active.refresh_due(&options, refreshed, refreshed + delay - 1, None));
        assert!(active.refresh_due(&options, refreshed, refreshed + delay + 1, None));
        // A request makes only its selector active, even before the old deadline.
        let request = refreshed + options.refresh_delay(Duration::ZERO).as_millis() as u64 + 1;
        active.last_use.store(request, Ordering::Relaxed);
        assert!(active.refresh_due(&options, refreshed, request, None));
        assert!(!idle_tag.refresh_due(&options, refreshed, request, None));
        // Successful publication starts a new interval; clock rollback cannot make it due.
        assert!(!active.refresh_due(&options, request, request, None));
        assert!(!active.refresh_due(&options, request, request - 1, None));
        assert!(idle_tag.refresh_due(&options, refreshed, refreshed + delay + 1, None));
    }

    #[test]
    fn commits_stay_pinned_and_fixed_intervals_ignore_activity() {
        let mut options = options();
        let pinned = branch(Target::Commit("a".repeat(40)), 0);
        assert!(!pinned.refresh_due(&options, 0, u64::MAX, None));
        let fixed = Duration::from_secs(23);
        options.refresh_interval = Some(fixed);
        let moving = branch(Target::Branch("main".into()), 0);
        let refreshed = 86400 * 1000;
        let deadline = refreshed + fixed.as_millis() as u64;
        for last_use in [0, deadline] {
            moving.last_use.store(last_use, Ordering::Relaxed);
            assert!(!moving.refresh_due(&options, refreshed, deadline - 1, None));
            assert!(moving.refresh_due(&options, refreshed, deadline, None));
        }
        assert!(!pinned.refresh_due(&options, 0, u64::MAX, None));
    }

    fn state() -> State {
        let repository = Repository::parse("https://github.com/owner/repo")
            .unwrap()
            .unwrap();
        State {
            repository: repository.identity,
            transport: repository.transport,
            branch: "main".into(),
            target: Target::Branch("main".into()),
            last_use: 0,
            refreshed: 0,
            policy: "selection".into(),
            prepared: Prepared {
                deferred_assets: ["Scene.asset".into()].into(),
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                branch: None,
                revision: "commit".into(),
                selected: [("lib.rs".into(), "source".into())].into(),
                tracked: Default::default(),
                directories: Default::default(),
            },
            indexed_revision: None,
            repair: false,
            additional: Default::default(),
        }
    }

    #[tokio::test]
    async fn indexing_updates_only_the_current_healthy_header() {
        let root = tempfile::tempdir().unwrap();
        let branch = Arc::new(Branch::for_test(root.path().into()));
        let initial = state();
        branch.persist(&initial).unwrap();
        *branch.state.write().await = Some(Arc::new(initial));
        let _interest = branch.demand.interest();
        let work = branch.demand.work();
        let operation = crate::telemetry::Operation::default();
        let mark = |complete| branch.mark_indexed(0, complete, &work, &operation);
        let saved = || {
            super::super::state_store::header(root.path())
                .unwrap()
                .indexed_revision
        };

        let reader = branch.analyze().await;
        tokio::time::timeout(Duration::from_secs(5), mark(true))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved().as_ref(),
            Some(&reader.as_ref().unwrap().prepared.revision)
        );
        assert!(reader.as_ref().unwrap().indexed_revision.is_none());
        let indexed = branch.state.read().await.clone().unwrap();
        mark(true).await.unwrap();
        assert!(Arc::ptr_eq(
            &indexed,
            branch.state.read().await.as_ref().unwrap()
        ));
        mark(false).await.unwrap();
        assert!(saved().is_none());

        branch.generation.store(1, Ordering::Release);
        mark(true).await.unwrap();
        assert!(saved().is_none());
        Arc::make_mut(branch.state.write().await.as_mut().unwrap()).repair = true;
        branch
            .mark_indexed(1, true, &work, &operation)
            .await
            .unwrap();
        assert!(saved().is_none());
    }

    #[tokio::test]
    async fn unchanged_ref_publication_preserves_indexing_and_rejects_stale_inputs() {
        let cache = tempfile::tempdir().unwrap();
        let mut options = options();
        options
            .rules
            .push(super::super::Rule::parse("https://github.com/owner/repo").unwrap());
        let manager = Manager::new(cache.path().into(), options).unwrap();
        let branch = Branch::for_test(cache.path().join("branch"));
        fs::create_dir_all(branch.source()).unwrap();
        fs::write(branch.source().join("lib.rs"), "fn source() {}").unwrap();
        let mut before = state();
        before.policy = manager.options.selection.identity.clone();
        super::super::state_store::write(&branch.root, &before).unwrap();
        let mut current = before.clone();
        current.indexed_revision = Some(current.prepared.revision.clone());
        *branch.state.write().await = Some(Arc::new(current));
        let telemetry = crate::telemetry::Operation::default();
        let held = super::super::cache::Acquisition::begin(
            cache.path(),
            &branch.repository.identity,
            &telemetry,
        )
        .await
        .unwrap();
        let generation = branch.generation.load(Ordering::Acquire);
        let resolved = (
            before.prepared.revision.clone(),
            before.prepared.branch.clone(),
        );
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                manager.refresh_unchanged(
                    &branch,
                    &Arc::new(before.clone()),
                    &telemetry,
                    &(),
                    move |_, _, _, _| Ok((resolved.0, resolved.1, None))
                )
            )
            .await
            .unwrap()
            .unwrap()
        );
        assert_eq!(
            branch
                .state
                .read()
                .await
                .as_ref()
                .unwrap()
                .indexed_revision
                .as_ref(),
            Some(&before.prepared.revision)
        );
        assert_eq!(branch.generation.load(Ordering::Acquire), generation);
        drop(held);
        for unavailable in [false, true] {
            let mut ineligible = before.clone();
            if unavailable {
                ineligible.prepared.unavailable.insert("input".into());
            } else {
                ineligible.policy.push_str("changed");
            }
            assert!(
                !manager
                    .refresh_unchanged(
                        &branch,
                        &Arc::new(ineligible),
                        &telemetry,
                        &(),
                        |_, _, _, _| panic!("Ineligible state must use normal acquisition")
                    )
                    .await
                    .unwrap()
            );
        }
        let mut obsolete = before.clone();
        obsolete.prepared.revision.push('0');
        assert!(
            !manager
                .publish_ref_refresh(&branch, &obsolete, None, &telemetry)
                .await
                .unwrap()
        );
        fs::remove_file(branch.source().join("lib.rs")).unwrap();
        assert!(
            !manager
                .publish_ref_refresh(&branch, &before, None, &telemetry)
                .await
                .unwrap()
        );
    }

    #[test]
    fn default_readiness_uses_state_headers_and_checks_index_status() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let root = cache.path().join("repositories");

        let mut ready = state();
        ready.policy = manager.options.selection.identity.clone();
        ready.prepared.branch = Some("main".into());
        ready.indexed_revision = Some(ready.prepared.revision.clone());
        ready.prepared.selected = (0..2048)
            .map(|n| (format!("asset-{n}"), "content-hash".into()))
            .collect();
        fs::create_dir_all(root.join("ready")).unwrap();
        super::super::state_store::write(&root.join("ready"), &ready).unwrap();

        let mut repairing = ready.clone();
        repairing.repair = true;
        fs::create_dir_all(root.join("repairing")).unwrap();
        super::super::state_store::write(&root.join("repairing"), &repairing).unwrap();

        assert!(manager.default_ready(&ready.repository, "main").unwrap());
        fs::remove_file(root.join("ready/state.bin")).unwrap();
        assert!(!manager.default_ready(&ready.repository, "main").unwrap());
        fs::remove_file(root.join("repairing/state.bin")).unwrap();

        let mut stale = ready.clone();
        stale.indexed_revision = Some("older-commit".into());
        fs::create_dir_all(root.join("stale")).unwrap();
        super::super::state_store::write(&root.join("stale"), &stale).unwrap();

        assert!(!manager.default_ready(&ready.repository, "main").unwrap());
        assert!(!manager.default_ready(&ready.repository, "develop").unwrap());
    }

    #[test]
    fn trimming_uninitialized_branches_preserves_persisted_inputs() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        for saved in [false, true] {
            let mut branch = branch(Target::Branch("main".into()), 0);
            branch.root = cache.path().join("repositories/pending");
            *branch.initialized.try_lock().unwrap() = false;
            if saved {
                fs::create_dir_all(branch.source()).unwrap();
                branch.persist(&state()).unwrap();
            }
            manager
                .branches
                .lock()
                .unwrap()
                .insert("pending".into(), Arc::new(branch));
            manager.trim_unreferenced(0, |_| false).unwrap();
            assert!(manager.branches.lock().unwrap().is_empty());
            assert_eq!(
                cache.path().join("repositories/pending/state.bin").exists(),
                saved
            );
        }
    }

    #[test]
    fn abandoned_inventory_only_selector_is_retired_from_disk() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let mut branch = branch(Target::Branch("main".into()), 0);
        branch.root = cache.path().join("repositories/inventory-only");
        fs::create_dir_all(branch.source()).unwrap();
        let path = branch.root.clone();
        manager
            .branches
            .lock()
            .unwrap()
            .insert("inventory-only".into(), Arc::new(branch));
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert!(manager.branches.lock().unwrap().is_empty());
        assert!(!path.exists());
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("usage.bin"), b"abandoned").unwrap();
        manager.collect_abandoned_inventories().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn repository_inventory_trimming_preserves_requests_workspaces_and_saved_state() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let mut branch = branch(Target::Branch("main".into()), 2);
        branch.root = cache.path().join("repositories/branch");
        fs::create_dir_all(branch.source()).unwrap();
        let source = branch.source().canonicalize().unwrap();
        let mut saved = state();
        saved.last_use = 1;
        branch.persist(&saved).unwrap();
        *branch.state.try_write().unwrap() = Some(Arc::new(saved.clone()));
        let branch = Arc::new(branch);
        manager
            .branches
            .lock()
            .unwrap()
            .insert("branch".into(), branch.clone());
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert_eq!(manager.branches.lock().unwrap().len(), 1);

        let reader = branch.state.clone().try_read_owned().unwrap();
        drop(branch);
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert_eq!(manager.branches.lock().unwrap().len(), 1);
        drop(reader);
        manager.trim_unreferenced(0, |path| path == source).unwrap();
        assert_eq!(manager.branches.lock().unwrap().len(), 1);

        manager.branches.lock().unwrap()["branch"]
            .retry_after
            .store(u64::MAX, Ordering::Relaxed);
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert_eq!(manager.branches.lock().unwrap().len(), 1);
        manager.branches.lock().unwrap()["branch"]
            .retry_after
            .store(0, Ordering::Relaxed);
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert!(manager.branches.lock().unwrap().is_empty());
        let restored =
            super::super::state_store::read(&cache.path().join("repositories/branch")).unwrap();
        assert_eq!(restored.last_use, 2);
        assert!(restored.same_inputs(&saved));
    }

    #[tokio::test]
    async fn cancelled_branch_load_stays_shared_without_blocking_other_branches() {
        use std::os::fd::AsRawFd;
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let mut pending = Branch::for_test(cache.path().join("repositories/pending"));
        pending.initialized = Arc::new(AsyncMutex::new(false));
        fs::create_dir_all(&pending.root).unwrap();
        super::super::state_store::write(&pending.root, &state()).unwrap();
        let locked = fs::File::open(&pending.root).unwrap();
        assert_eq!(unsafe { libc::flock(locked.as_raw_fd(), libc::LOCK_EX) }, 0);
        let pending = Arc::new(pending);
        manager
            .branches
            .lock()
            .unwrap()
            .insert("pending".into(), pending.clone());
        let loader = {
            let manager = manager.clone();
            let pending = pending.clone();
            tokio::spawn(async move {
                manager
                    .initialize_branch(&pending, &crate::telemetry::Operation::default())
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while pending.initialized.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        loader.abort();
        assert!(loader.await.unwrap_err().is_cancelled());

        let mut other = Branch::for_test(cache.path().join("repositories/other"));
        other.initialized = Arc::new(AsyncMutex::new(false));
        let other = Arc::new(other);
        tokio::time::timeout(
            Duration::from_secs(5),
            manager.initialize_branch(&other, &crate::telemetry::Operation::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(other.root.is_dir());
        manager.trim_unreferenced(0, |_| false).unwrap();
        assert!(manager.branches.lock().unwrap().contains_key("pending"));
        assert!(pending.initialized.try_lock().is_err());
        let flush = manager.flush_usage();
        tokio::pin!(flush);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(flush.as_mut().poll(cx).is_pending()))
                .await
        );
        assert_eq!(unsafe { libc::flock(locked.as_raw_fd(), libc::LOCK_UN) }, 0);
        tokio::time::timeout(Duration::from_secs(5), flush)
            .await
            .unwrap()
            .unwrap();
        manager
            .initialize_branch(&pending, &crate::telemetry::Operation::default())
            .await
            .unwrap();
        assert!(
            pending
                .state
                .read()
                .await
                .as_ref()
                .unwrap()
                .same_inputs(&state())
        );
    }

    #[tokio::test]
    async fn branch_load_failure_can_retry_and_pending_loads_allow_maintenance() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let mut pending = Branch::for_test(cache.path().join("repositories/pending"));
        pending.initialized = Arc::new(AsyncMutex::new(false));
        let pending = Arc::new(pending);
        manager
            .branches
            .lock()
            .unwrap()
            .insert("pending".into(), pending.clone());
        manager.maintain(false).await.unwrap();
        assert!(!pending.root.exists());
        fs::create_dir_all(&pending.root).unwrap();
        fs::write(pending.root.join("state.bin"), b"broken").unwrap();
        assert!(
            manager
                .initialize_branch(&pending, &crate::telemetry::Operation::default())
                .await
                .is_err()
        );
        assert!(!*pending.initialized.lock().await);
        fs::remove_file(pending.root.join("state.bin")).unwrap();
        super::super::state_store::write(&pending.root, &state()).unwrap();
        manager
            .initialize_branch(&pending, &crate::telemetry::Operation::default())
            .await
            .unwrap();
        assert!(pending.state.read().await.is_some());
    }

    #[test]
    fn unchanged_omissions_survive_refresh_but_changed_inputs_retry() {
        let mut before = state();
        before.prepared.tracked = [
            ("Same.asset".into(), "same blob".into()),
            ("Changed.asset".into(), "old blob".into()),
        ]
        .into();
        before.prepared.omitted = before
            .prepared
            .tracked
            .keys()
            .map(|path| (path.clone(), "Asset exceeds size limit".into()))
            .collect();
        let before: State = serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
        let mut prepared = before.prepared.clone();
        prepared.omitted.clear();
        prepared.deferred_assets = before.prepared.tracked.keys().cloned().collect();
        prepared
            .tracked
            .insert("Changed.asset".into(), "new blob".into());

        for retry in [false, true] {
            let mut prior = before.clone();
            if retry {
                prior.policy = "new selection rules".into();
            }
            let mut next = prepared.clone();
            prior.reuse_omissions(&mut next, &before.policy);
            assert_eq!(next.deferred_assets.contains("Same.asset"), retry);
            assert_eq!(next.omitted.contains_key("Same.asset"), !retry);
            assert!(next.deferred_assets.contains("Changed.asset"));
            assert!(!next.omitted.contains_key("Changed.asset"));
        }
        // Required inputs are selected immediately, even when previously omitted.
        prepared.deferred_assets.remove("Same.asset");
        prepared
            .selected
            .insert("Same.asset".into(), "same blob".into());
        before.reuse_omissions(&mut prepared, &before.policy);
        assert!(prepared.omitted.is_empty());
        assert!(prepared.selected.contains_key("Same.asset"));
    }

    #[test]
    fn refresh_metadata_does_not_change_published_inputs() {
        let before = state();
        let mut next = before.clone();
        next.last_use += 1;
        next.refreshed += 1;
        next.prepared.transfer_bytes += 1;
        next.prepared.transport = Some("a new transport for the same repository".into());
        assert!(before.same_inputs(&next));
        for change in [
            |s: &mut State| s.repair = true,
            |s: &mut State| s.policy.push_str("changed"),
            |s: &mut State| s.prepared.revision.push_str("changed"),
            |s: &mut State| s.prepared.branch = Some("refs/tags/main".into()),
            |s: &mut State| {
                s.prepared.selected.clear();
            },
            |s: &mut State| {
                s.prepared.deferred_assets.clear();
            },
            |s: &mut State| {
                s.prepared.unavailable.insert("lib.rs".into());
            },
            |s: &mut State| {
                s.additional.insert("Required.asset".into());
            },
        ] {
            let mut changed = before.clone();
            change(&mut changed);
            assert!(!changed.same_inputs(&next));
        }
    }

    #[test]
    fn discovery_identity_tracks_code_and_required_inputs_across_restart() {
        let mut state = state();
        let before = state.discovery_identity().unwrap();
        state
            .prepared
            .selected
            .insert("Scene.asset".into(), "asset".into());
        state.prepared.deferred_assets.clear();
        assert_eq!(state.discovery_identity().unwrap(), before);
        state.prepared.unavailable.insert("Scene.asset".into());
        assert_eq!(state.discovery_identity().unwrap(), before);
        let restored: State = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.discovery_identity().unwrap(), before);

        for path in [
            "new.rs",
            "Game.asmdef",
            "Game.asmdef.meta",
            "ProjectSettings/ProjectSettings.asset",
        ] {
            let mut changed = state.clone();
            changed
                .prepared
                .selected
                .insert(path.into(), "build input".into());
            assert_ne!(changed.discovery_identity().unwrap(), before);
        }
        let mut changed = state.clone();
        changed
            .prepared
            .selected
            .insert("lib.rs".into(), "edited".into());
        assert_ne!(changed.discovery_identity().unwrap(), before);
        changed = state.clone();
        changed.prepared.selected.remove("lib.rs");
        assert_ne!(changed.discovery_identity().unwrap(), before);
        changed = state.clone();
        changed.prepared.unavailable.insert("lib.rs".into());
        assert_ne!(changed.discovery_identity().unwrap(), before);
        changed = state.clone();
        changed.prepared.revision = "new commit with an unselected input".into();
        assert_ne!(changed.discovery_identity().unwrap(), before);

        state.additional.insert("Scene.asset".into());
        let required = state.discovery_identity().unwrap();
        assert_ne!(required, before);
        state.prepared.unavailable.remove("Scene.asset");
        assert_ne!(state.discovery_identity().unwrap(), required);
    }

    #[test]
    fn resolved_abbreviation_stays_pinned_without_expanding_display_selector() {
        let options = options();
        let mut short = branch(Target::Named("abcdef1".into()), 0);
        short.name = "abcdef1".into();
        let prepared = Prepared {
            deferred_assets: Default::default(),
            omitted: Default::default(),
            transfer_bytes: 0,
            unavailable: Default::default(),
            transport: None,
            resolved_target: Some(Target::Commit("a".repeat(40))),
            branch: None,
            revision: "a".repeat(40),
            selected: Default::default(),
            tracked: Default::default(),
            directories: Default::default(),
        };
        let prepared: Prepared =
            serde_json::from_slice(&serde_json::to_vec(&prepared).unwrap()).unwrap();
        assert!(!short.refresh_due(&options, 0, u64::MAX, Some(&prepared)));
        assert!(matches!(
            short.acquisition_target(Some(&prepared)),
            Target::Commit(_)
        ));
        assert_eq!(short.selector(&prepared), "abcdef1");
    }

    #[tokio::test]
    async fn expiry_uses_live_activity_and_loaded_state_before_retirement() {
        let cache = tempfile::tempdir().unwrap();
        let manager = Manager::new(cache.path().into(), options()).unwrap();
        let mut branch = branch(Target::Branch("main".into()), now());
        branch.root = cache.path().join("repositories/branch");
        // A branch still being acquired has no persisted inventory.
        assert!(!manager.branch_expired(&branch).unwrap());
        let state = State {
            repository: branch.repository.identity.clone(),
            transport: branch.repository.transport.clone(),
            branch: branch.name.clone(),
            target: branch.target.clone(),
            last_use: now(),
            refreshed: now(),
            policy: String::new(),
            prepared: Prepared {
                deferred_assets: Default::default(),
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                branch: Some(branch.name.clone()),
                revision: "a".repeat(40),
                selected: Default::default(),
                tracked: Default::default(),
                directories: Default::default(),
            },
            indexed_revision: None,
            repair: false,
            additional: Default::default(),
        };
        *branch.state.try_write().unwrap() = Some(Arc::new(state.clone()));
        branch.last_use.store(0, Ordering::Relaxed);
        assert!(manager.branch_expired(&branch).unwrap());
        branch.record_use();
        assert!(!manager.branch_expired(&branch).unwrap());

        fs::create_dir_all(&branch.root).unwrap();
        super::super::state_store::write(&branch.root, &state).unwrap();
        branch.last_use.store(0, Ordering::Relaxed);
        let _publication = branch.state.try_write().unwrap();
        assert!(manager.branch_expired(&branch).unwrap());
        drop(_publication);

        let branch = Arc::new(branch);
        manager
            .branches
            .lock()
            .unwrap()
            .insert(branch.name.clone(), branch.clone());
        assert!(manager.maintain(false).await.unwrap().is_empty());
        let expired = manager.maintain(true).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert!(Arc::ptr_eq(&expired[0], &branch));
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub repository: Identity,
    pub transport: String,
    pub branch: String,
    pub target: Target,
    pub last_use: u64,
    pub refreshed: u64,
    pub policy: String,
    pub prepared: Prepared,
    pub indexed_revision: Option<String>,
    pub repair: bool,
    pub additional: std::collections::BTreeSet<String>,
}

impl State {
    fn same_inputs(&self, next: &Self) -> bool {
        !self.repair
            && self.policy == next.policy
            && self.additional == next.additional
            && self.prepared.revision == next.prepared.revision
            && self.prepared.branch == next.prepared.branch
            && self.prepared.resolved_target == next.prepared.resolved_target
            && self.prepared.selected == next.prepared.selected
            && self.prepared.tracked == next.prepared.tracked
            && self.prepared.directories == next.prepared.directories
            && self.prepared.unavailable == next.prepared.unavailable
            && self.prepared.omitted == next.prepared.omitted
            && self.prepared.deferred_assets == next.prepared.deferred_assets
    }

    fn reuse_omissions(&self, prepared: &mut Prepared, policy: &str) {
        if self.repair || self.policy != policy {
            return;
        }
        // Only optional, newly deferred inputs qualify. Required inputs and
        // changed Git blobs must pass through acquisition again.
        prepared.deferred_assets.retain(|path| {
            if let Some(reason) = self.prepared.omitted.get(path)
                && prepared.tracked.get(path) == self.prepared.tracked.get(path)
            {
                prepared.omitted.insert(path.clone(), reason.clone());
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn discovery_identity(&self) -> Result<[u8; 32]> {
        let mut hash = blake3::Hasher::new();
        // Include the revision: newly tracked build inputs may not be selected yet.
        postcard::to_io(
            &(&self.prepared.revision, &self.policy, &self.additional),
            &mut hash,
        )?;
        for (path, object) in &self.prepared.selected {
            if !super::selection::optional_asset(Path::new(path)) || self.additional.contains(path)
            {
                postcard::to_io(
                    &(path, object, self.prepared.unavailable.contains(path)),
                    &mut hash,
                )?;
            }
        }
        Ok(*hash.finalize().as_bytes())
    }
}

#[derive(Clone)]
pub(crate) struct Inventory {
    pub(crate) prepared: Prepared,
    pub(crate) source: Option<Arc<tempfile::TempDir>>,
    _pin: Arc<super::cache::Pin>,
    refreshed: u64,
    heap_bytes: std::sync::OnceLock<u64>,
    _staging: Option<Arc<super::admission::Staging>>,
}

/// A temporary, pinned file view. Its disk reservation lasts through rendering.
pub(crate) struct FileView {
    pub prepared: Prepared,
    pub source: Arc<tempfile::TempDir>,
    _pin: super::cache::Pin,
    _staging: Arc<super::admission::Staging>,
}

pub(crate) struct AnalysisGuard {
    state: Option<Arc<State>>,
    _analysis: OwnedRwLockReadGuard<()>,
    _branch: Arc<Branch>,
}
impl AnalysisGuard {
    pub(crate) fn as_ref(&self) -> Option<&State> {
        self.state.as_deref()
    }
}

pub(crate) struct PublicationGuard {
    state: OwnedRwLockWriteGuard<Option<Arc<State>>>,
    _analysis: Option<OwnedRwLockWriteGuard<()>>,
}
#[cfg(test)]
impl PublicationGuard {
    pub(crate) fn as_mut(&mut self) -> Option<&mut State> {
        self.state.as_mut().map(Arc::make_mut)
    }
}
impl std::ops::Deref for PublicationGuard {
    type Target = Option<Arc<State>>;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
impl std::ops::DerefMut for PublicationGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

pub struct Branch {
    pub root: PathBuf,
    pub repository: Repository,
    pub name: String,
    pub target: Target,
    /// Short metadata reads. Long readers pin snapshots with analyze().
    pub state: Arc<AsyncRwLock<Option<Arc<State>>>>,
    analysis: Arc<AsyncRwLock<()>>,
    pub last_use: AtomicU64,
    persisted_use: AtomicU64,
    pub generation: AtomicU64,
    running: Mutex<Option<Running>>,
    telemetry_id: Mutex<Option<String>>,
    retry_after: AtomicU64,
    acquiring: Arc<AsyncMutex<()>>,
    sources: Mutex<Option<super::source_validation::Sources>>,
    state_bytes: Mutex<Option<(std::sync::Weak<State>, u64)>>,
    pub(crate) inventory: watch::Sender<Option<Arc<Inventory>>>,
    initialized: Arc<AsyncMutex<bool>>,
    pub(crate) demand: Arc<crate::demand::Demand>,
    inventory_demand: Arc<crate::demand::Demand>,
}

impl Branch {
    fn inventory_bytes(&self) -> u64 {
        let inventory = self.inventory.borrow();
        let inventory_bytes = inventory.as_ref().map_or(0, |inventory| {
            *inventory.heap_bytes.get_or_init(|| {
                crate::memory::encoded_size(&inventory.prepared).saturating_mul(3) as u64
            })
        });
        let mut cached = self.state_bytes.lock().unwrap();
        if let Ok(state) = self.state.try_read() {
            match state.as_ref() {
                Some(state)
                    if cached
                        .as_ref()
                        .is_none_or(|(saved, _)| !saved.ptr_eq(&Arc::downgrade(state))) =>
                {
                    *cached = Some((
                        Arc::downgrade(state),
                        crate::memory::encoded_size(state.as_ref()).saturating_mul(3) as u64,
                    ));
                }
                None => *cached = None,
                _ => (),
            }
        }
        inventory_bytes.saturating_add(cached.as_ref().map_or(0, |(_, bytes)| *bytes))
    }
    pub(crate) fn interest(&self) -> (crate::demand::Interest, crate::demand::Interest) {
        (self.demand.interest(), self.inventory_demand.interest())
    }
    // The acquisition guard serializes the per-revision transfer budget.
    fn record_transfer(&self, prepared: &Prepared) -> Result<()> {
        self.inventory.send_if_modified(|inventory| {
            if let Some(inventory) = inventory
                && inventory.prepared.revision == prepared.revision
                && inventory.prepared.transfer_bytes < prepared.transfer_bytes
            {
                Arc::make_mut(inventory).prepared.transfer_bytes = prepared.transfer_bytes;
                return true;
            }
            false
        });
        let mut state = self.state.blocking_write();
        if let Some(state) = state.as_mut()
            && state.prepared.revision == prepared.revision
            && state.prepared.transfer_bytes < prepared.transfer_bytes
        {
            let state = Arc::make_mut(state);
            state.prepared.transfer_bytes =
                state.prepared.transfer_bytes.max(prepared.transfer_bytes);
            self.persist_header(state)?;
        }
        Ok(())
    }
    pub(super) fn sources_current(&self, prepared: &Prepared) -> bool {
        self.sources
            .lock()
            .unwrap()
            .get_or_insert_with(|| super::source_validation::Sources::new(&self.source(), prepared))
            .current(prepared)
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf) -> Self {
        Self {
            root,
            repository: Repository::parse("https://github.com/owner/repo")
                .unwrap()
                .unwrap(),
            name: "main".into(),
            target: Target::Branch("main".into()),
            state: Arc::new(AsyncRwLock::new(None)),
            analysis: Arc::new(AsyncRwLock::new(())),
            last_use: AtomicU64::new(0),
            persisted_use: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            running: Mutex::new(None),
            telemetry_id: Mutex::new(None),
            retry_after: AtomicU64::new(0),
            acquiring: Arc::new(AsyncMutex::new(())),
            sources: Mutex::new(None),
            state_bytes: Mutex::new(None),
            inventory: watch::channel(None).0,
            initialized: Arc::new(AsyncMutex::new(true)),
            demand: Arc::default(),
            inventory_demand: Arc::default(),
        }
    }

    /// Publishers wait for long analyses before queuing a state writer. Short
    /// navigation readers can therefore use the stable files while analyses run.
    pub(crate) async fn analyze(self: &Arc<Self>) -> AnalysisGuard {
        let analysis = self.analysis.clone().read_owned().await;
        let state = self.state.read().await.clone();
        AnalysisGuard {
            state,
            _analysis: analysis,
            _branch: self.clone(),
        }
    }

    pub(crate) fn try_analyze(self: &Arc<Self>) -> Option<AnalysisGuard> {
        let analysis = self.analysis.clone().try_read_owned().ok()?;
        let state = self.state.try_read().ok()?.clone();
        Some(AnalysisGuard {
            state,
            _analysis: analysis,
            _branch: self.clone(),
        })
    }

    pub(crate) async fn publish(&self) -> PublicationGuard {
        self.publication(false).await
    }

    /// Completion changes the header only. Pinned analyses keep their source snapshot.
    pub(crate) async fn mark_indexed(
        self: &Arc<Self>,
        generation: u64,
        complete: bool,
        work: &crate::demand::Work,
        operation: &crate::telemetry::Operation,
    ) -> Result<()> {
        let mut gate = {
            let _wait = operation.wait("branch_state");
            work.until(async { Ok(self.state.clone().write_owned().await) })
                .await?
        };
        let state = gate.as_ref().unwrap();
        let revision = complete.then_some(state.prepared.revision.as_str());
        if state.repair
            || self.generation.load(Ordering::Acquire) != generation
            || state.indexed_revision.as_deref() == revision
        {
            return Ok(());
        }
        let indexed = revision.map(str::to_owned);
        let branch = self.clone();
        tokio::task::spawn_blocking(move || {
            let state = Arc::make_mut(gate.as_mut().unwrap());
            let previous = std::mem::replace(&mut state.indexed_revision, indexed);
            let used =
                std::mem::replace(&mut state.last_use, branch.last_use.load(Ordering::Relaxed));
            if let Err(error) = branch.persist_header(state) {
                state.indexed_revision = previous;
                state.last_use = used;
                return Err(error);
            }
            Ok(())
        })
        .await?
    }

    /// Optional assets only add deferred files; active code snapshots stay valid.
    async fn publication(&self, assets: bool) -> PublicationGuard {
        let analysis = if assets {
            None
        } else {
            Some(self.analysis.clone().write_owned().await)
        };
        let state = self.state.clone().write_owned().await;
        self.sources.lock().unwrap().take();
        PublicationGuard {
            state,
            _analysis: analysis,
        }
    }

    fn acquisition_target<'a>(&'a self, prepared: Option<&'a Prepared>) -> &'a Target {
        prepared
            .and_then(|prepared| prepared.resolved_target.as_ref())
            .unwrap_or(&self.target)
    }

    fn refresh_due(
        &self,
        options: &RemoteOptions,
        refreshed: u64,
        now: u64,
        prepared: Option<&Prepared>,
    ) -> bool {
        // Anchor the delay to the last successful refresh. A later access
        // shortens it to the active interval without moving the deadline forward.
        let idle =
            Duration::from_millis(refreshed.saturating_sub(self.last_use.load(Ordering::Relaxed)));
        !matches!(self.acquisition_target(prepared), Target::Commit(_))
            && Duration::from_millis(now.saturating_sub(refreshed)) >= options.refresh_delay(idle)
    }

    pub fn selector(&self, prepared: &Prepared) -> String {
        // Preserve the user's abbreviated selector in summaries; only refresh semantics expand it.
        if matches!(prepared.resolved_target.as_ref(), Some(Target::Commit(_))) {
            return self.name.clone();
        }
        if let Some(branch) = &prepared.branch {
            return format!("refs/heads/{branch}");
        }
        match &self.target {
            Target::Tag(name) | Target::Named(name) => format!("refs/tags/{name}"),
            Target::Commit(id) => id.clone(),
            _ => self.name.clone(),
        }
    }
    pub fn source(&self) -> PathBuf {
        self.root.join("source")
    }
    pub fn analysis_cache(&self) -> PathBuf {
        self.root.join("analysis")
    }
    pub fn persist(&self, state: &State) -> Result<()> {
        super::state_store::write(&self.root, state)?;
        self.persisted_use
            .fetch_max(state.last_use, Ordering::Relaxed);
        Ok(())
    }
    pub fn persist_header(&self, state: &State) -> Result<()> {
        super::state_store::update(&self.root, state)?;
        self.persisted_use
            .fetch_max(state.last_use, Ordering::Relaxed);
        Ok(())
    }
    fn publish_inventory(
        &self,
        state: &State,
        inventory: super::state_store::StagedInventory,
    ) -> Result<()> {
        super::state_store::publish(&self.root, state, inventory)?;
        self.persisted_use
            .fetch_max(state.last_use, Ordering::Relaxed);
        Ok(())
    }
    fn checkpoint_usage(&self, force: bool) -> Result<()> {
        let used = self.last_use.load(Ordering::Relaxed);
        let saved = self.persisted_use.load(Ordering::Relaxed);
        if used > saved
            && (force || used.saturating_sub(saved) >= super::state_store::USAGE_INTERVAL)
        {
            super::state_store::record_use(&self.root, used)?;
            self.persisted_use.fetch_max(used, Ordering::Relaxed);
        }
        Ok(())
    }
    pub fn record_use(&self) {
        self.last_use.fetch_max(now(), Ordering::Relaxed);
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DefaultBranch {
    pub(crate) repository: Identity,
    pub(crate) branch: String,
    pub(crate) refreshed: u64,
    pub(crate) previous: Option<String>,
}
type DefaultOperation = (
    u64,
    watch::Receiver<Option<std::result::Result<DefaultBranch, Arc<anyhow::Error>>>>,
    Option<String>,
);

pub struct Manager {
    cache: PathBuf,
    pub options: RemoteOptions,
    branches: Mutex<HashMap<String, Arc<Branch>>>,
    defaults: AsyncMutex<HashMap<String, DefaultBranch>>,
    capacity: Arc<super::admission::Capacity>,
    default_running: Mutex<HashMap<String, DefaultOperation>>,
}

impl Manager {
    async fn initialize_branch(
        &self,
        branch: &Arc<Branch>,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<()> {
        let mut initialized = branch.initialized.clone().lock_owned().await;
        if *initialized {
            return Ok(());
        }
        let permit = self.capacity.import(telemetry).await?;
        let branch = branch.clone();
        // The worker owns the gate and branch even if its original waiter leaves.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let state = if branch.root.join("state.bin").is_file() {
                Some(super::state_store::read(&branch.root)?)
            } else {
                None
            };
            if let Some(value) = &state {
                ensure!(
                    value.repository == branch.repository.identity
                        && value.branch == branch.name
                        && value.target == branch.target,
                    "Cached branch identity is invalid"
                );
            }
            fs::create_dir_all(&branch.root)?;
            branch.last_use.store(
                state.as_ref().map_or_else(now, |state| state.last_use),
                Ordering::Relaxed,
            );
            branch.persisted_use.store(
                state.as_ref().map_or(0, |state| state.last_use),
                Ordering::Relaxed,
            );
            *branch.state.blocking_write() = state.map(Arc::new);
            *initialized = true;
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    fn saved_default(&self, identity: &Identity) -> Result<Option<DefaultBranch>> {
        match crate::cache::records::read(
            &self.cache.join("defaults").join(identity.storage_key()),
            64 * 1024,
        ) {
            Ok(saved) => Ok(Some(saved)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    fn default_ready(&self, identity: &Identity, branch: &str) -> Result<bool> {
        // Named selectors and explicit refs can both resolve to the actual default.
        let entries = match fs::read_dir(self.cache.join("repositories")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            ensure!(entry.file_type()?.is_dir(), "Invalid selector directory");
            let state = match super::state_store::header(&entry.path()) {
                Ok(state) => state,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            if state.repository == *identity
                && state.policy == self.options.selection.identity
                && state.prepared.branch.as_deref() == Some(branch)
                && !state.repair
                && state.indexed_revision.as_deref() == Some(&state.prepared.revision)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn previous_default(&self, identity: &Identity, next: Option<&str>) -> Result<Option<String>> {
        let Some(saved) = self.saved_default(identity)? else {
            return Ok(None);
        };
        if Some(saved.branch.as_str()) == next {
            return Ok(saved.previous);
        }
        if self.default_ready(identity, &saved.branch)? {
            return Ok(Some(saved.branch));
        }
        Ok(saved.previous.or(Some(saved.branch)))
    }

    pub fn retention(&self, state: &State) -> Result<(bool, Duration)> {
        let (protection, ttl) = super::retention::Policy::from(&self.options).decision(
            &self.cache,
            state,
            |identity, branch| self.default_ready(identity, branch),
        )?;
        Ok((protection.is_some(), ttl))
    }

    fn state_expired(&self, state: &State, last_use: u64) -> Result<bool> {
        let (pinned, ttl) = self.retention(state)?;
        Ok(!pinned && now().saturating_sub(last_use) >= ttl.as_millis() as u64)
    }
    fn branch_expired(&self, branch: &Branch) -> Result<bool> {
        let last_use = branch.last_use.load(Ordering::Relaxed);
        // Most maintenance ticks cannot expire anything. Do not deserialize
        // entire file inventories just to discover that their TTL has not elapsed.
        let shortest_ttl = self.options.repo_ttl.min(self.options.branch_ttl);
        if now().saturating_sub(last_use) < shortest_ttl.as_millis() as u64 {
            return Ok(false);
        }
        if let Ok(state) = branch.state.try_read() {
            return state
                .as_ref()
                .map_or(Ok(false), |state| self.state_expired(state, last_use));
        }
        // Final retirement checks run with the publication lock held. Only
        // those candidates need the persisted snapshot as a fallback.
        let state = match super::state_store::header(&branch.root) {
            Ok(state) => state,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let (pinned, ttl) = super::retention::Policy::from(&self.options).projected_decision(
            &self.cache,
            &state,
            |identity, branch| self.default_ready(identity, branch),
        )?;
        Ok(pinned.is_none() && now().saturating_sub(last_use) >= ttl.as_millis() as u64)
    }

    pub fn cached(&self) -> Result<Vec<(PathBuf, State)>> {
        Ok(super::retention::cached(&self.cache)?.into_iter().collect())
    }
    pub(crate) fn active_entries(&self) -> std::collections::BTreeSet<PathBuf> {
        self.branches
            .lock()
            .unwrap()
            .values()
            .filter(|branch| {
                Arc::strong_count(branch) > 1
                    || branch
                        .running
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|job| job.borrow().is_none())
                    || branch.state.try_write().is_err()
            })
            .map(|branch| branch.source())
            .collect()
    }
    /// Recheck both protection and in-flight acquisitions at the point of removal.
    pub fn evict<T>(
        &self,
        root: &Path,
        pressure: bool,
        prepare: impl FnOnce() -> Result<T>,
        retire: impl FnOnce(T) -> Result<()>,
    ) -> Result<bool> {
        let branches = self.branches.lock().unwrap();
        let branch = branches.values().find(|b| b.root == root).cloned();
        let _gate = if let Some(branch) = &branch {
            if Arc::strong_count(branch) > 2
                || branch
                    .running
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|job| job.borrow().is_none())
            {
                return Ok(false);
            }
            let Ok(gate) = branch.state.clone().try_write_owned() else {
                return Ok(false);
            };
            Some(gate)
        } else {
            None
        };
        // Keep the per-branch writer, but allow unrelated branch acquisitions
        // while the caller enumerates analysis namespaces.
        use std::os::unix::fs::MetadataExt;
        let identity = fs::symlink_metadata(root)?;
        drop(branches);
        let plan = prepare()?;
        let mut branches = self.branches.lock().unwrap();
        let current = branches.values().find(|b| b.root == root);
        let unchanged = match (&branch, current) {
            (Some(before), Some(after)) => Arc::ptr_eq(before, after),
            (None, None) => true,
            _ => false,
        };
        if !unchanged
            || branch.as_ref().is_some_and(|branch| {
                Arc::strong_count(branch) > 2
                    || branch
                        .running
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|job| job.borrow().is_none())
            })
        {
            return Ok(false);
        }
        let current_identity = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if (identity.dev(), identity.ino()) != (current_identity.dev(), current_identity.ino()) {
            return Ok(false);
        }
        let mut state = super::state_store::read(root)?;
        if let Some(branch) = &branch {
            state.last_use = branch.last_use.load(Ordering::Relaxed);
        }
        if self.retention(&state)?.0 || !pressure && !self.state_expired(&state, state.last_use)? {
            return Ok(false);
        }
        retire(plan)?;
        let retired = crate::cache::views::retire(&self.cache, root)?;
        branches.retain(|_, branch| branch.root != root);
        drop(_gate);
        drop(branches);
        retired.dispose()?;
        Ok(true)
    }

    pub fn new(cache: PathBuf, options: RemoteOptions) -> Result<Arc<Self>> {
        let capacity = crate::cache::reset::with_space_recovery(&cache, || {
            fs::create_dir_all(cache.join("repositories"))?;
            fs::create_dir_all(cache.join("defaults"))?;
            super::admission::Capacity::open(&cache)
        })?;
        Ok(Arc::new(Self {
            capacity,
            cache,
            options,
            branches: Mutex::new(HashMap::new()),
            defaults: AsyncMutex::new(HashMap::new()),
            default_running: Mutex::new(HashMap::new()),
        }))
    }

    async fn default_branch(
        self: &Arc<Self>,
        repository: &Repository,
        wait: bool,
        parent: Option<&crate::telemetry::Operation>,
    ) -> Result<Option<String>> {
        let key = repository.identity.storage_key();
        let mut defaults = self.defaults.lock().await;
        if !defaults.contains_key(&key) {
            let path = self.cache.join("defaults").join(&key);
            if path.is_file() {
                let saved: DefaultBranch = crate::cache::records::read(&path, 64 * 1024)?;
                ensure!(
                    saved.repository == repository.identity,
                    "Cached repository identity is invalid"
                );
                super::validate_branch(&saved.branch)?;
                defaults.insert(key.clone(), saved);
            }
        }
        let cached = defaults.get(&key).cloned();
        drop(defaults);
        if let Some(saved) = &cached
            && now().saturating_sub(saved.refreshed)
                < self
                    .options
                    .refresh_interval
                    .unwrap_or(Duration::from_secs(5 * 60))
                    .as_millis() as u64
        {
            return Ok(Some(saved.branch.clone()));
        }
        let (mut running, telemetry_id) = {
            let mut operations = self.default_running.lock().unwrap();
            let reuse = operations.get(&key).is_some_and(|(started, receive, _)| {
                let outcome = receive.borrow();
                outcome.is_none()
                    || outcome.as_ref().is_some_and(Result::is_ok)
                        && now().saturating_sub(*started) < 30_000
            });
            if !reuse {
                let (send, receive) = watch::channel(None);
                let telemetry = crate::telemetry::operation(
                    "repository_default_lookup",
                    Some(&repository.identity.to_string()),
                    None,
                );
                let telemetry_id = telemetry.id();
                operations.insert(key.clone(), (now(), receive, telemetry_id));
                let manager = self.clone();
                let repository = repository.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    telemetry.stage("default_branch_lookup");
                    let result = async {
                        let request = Request {
                            contents: super::materialize::Contents::All,
                            transfer_used: 0,
                            unlimited_transfer: manager
                                .options
                                .rules
                                .iter()
                                .any(|r| r.exact_match(&repository)),
                            allow_private: authorize(&manager.options.rules, &repository)?,
                            repository: repository.transport.clone(),
                            preferred_transport: None,
                            target: Target::DefaultBranch,
                            store: PathBuf::new(),
                            staging: PathBuf::new(),
                            include: vec![],
                            exclude: vec![],
                            required: vec![],
                            additional: vec![],
                            previous: BTreeMap::new(),
                            subdirectory: None,
                        };
                        let cache = manager.cache.clone();
                        let permit = manager.capacity.transfer(&telemetry).await?;
                        telemetry.stage("default_branch_git");
                        let prepared = tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            job::execute(&request, &cache)
                        })
                        .await??;
                        let saved = DefaultBranch {
                            previous: manager.previous_default(
                                &repository.identity,
                                prepared.branch.as_deref(),
                            )?,
                            repository: repository.identity,
                            branch: prepared.branch.context("Default branch is missing")?,
                            refreshed: now(),
                        };
                        crate::cache::records::write(
                            &manager.cache.join("defaults").join(&key),
                            &saved,
                        )?;
                        manager.defaults.lock().await.insert(key, saved.clone());
                        Ok::<_, anyhow::Error>(saved)
                    }
                    .await;
                    if let Err(error) = &result {
                        tracing::warn!(error = %format!("{error:#}"), "Default branch lookup failed");
                        if let Some(diagnostic) = crate::diagnostics::monitoring(error) {
                            telemetry.diagnose(diagnostic);
                        }
                    }
                    telemetry.finish(if result.is_ok() { "success" } else { "failure" });
                    let _ = send.send(Some(result.map_err(Arc::new)));
                });
            }
            (operations[&key].1.clone(), operations[&key].2.clone())
        };
        if let (Some(parent), Some(id)) = (parent, telemetry_id.as_deref()) {
            parent.link(id);
        }
        if let Some(cached) = cached {
            return Ok(Some(cached.branch));
        }
        if !wait {
            return Ok(None);
        }
        loop {
            if let Some(result) = running.borrow().clone() {
                return result
                    .map(|r| Some(r.branch))
                    .map_err(crate::diagnostics::shared_error);
            }
            running.changed().await?;
        }
    }

    /// Refresh repositories; check expiry only on the slower cleanup cycle.
    pub async fn maintain(self: &Arc<Self>, collect: bool) -> Result<Vec<Arc<Branch>>> {
        if collect {
            let manager = self.clone();
            tokio::task::spawn_blocking(move || manager.collect_abandoned_inventories()).await??;
        }
        let branches: Vec<_> = self.branches.lock().unwrap().values().cloned().collect();
        let mut expired = Vec::new();
        for branch in branches {
            if collect && self.branch_expired(&branch)? {
                expired.push(branch);
                continue;
            }
            let checkpoint = branch.clone();
            tokio::task::spawn_blocking(move || checkpoint.checkpoint_usage(false)).await??;
            let inventory = branch.inventory.borrow().clone();
            let due = if let Some(inventory) = inventory {
                branch.refresh_due(
                    &self.options,
                    inventory.refreshed,
                    now(),
                    Some(&inventory.prepared),
                )
            } else {
                branch.state.try_read().is_ok_and(|state| {
                    state.as_ref().is_some_and(|state| {
                        branch.refresh_due(
                            &self.options,
                            state.refreshed,
                            now(),
                            Some(&state.prepared),
                        )
                    })
                })
            };
            if due {
                self.schedule(branch.clone(), false, branch.demand.idle());
            }
        }
        Ok(expired)
    }

    fn collect_abandoned_inventories(&self) -> Result<()> {
        // Headerless selectors contain only temporary inventory/view data. Live
        // callers publish their Branch in this registry before creating its root.
        let branches = self.branches.lock().unwrap();
        let mut retired = Vec::new();
        for entry in fs::read_dir(self.cache.join("repositories"))? {
            let entry = entry?;
            let root = entry.path();
            ensure!(entry.file_type()?.is_dir(), "Invalid selector directory");
            if !root.join("state.bin").try_exists()?
                && !branches.values().any(|branch| branch.root == root)
            {
                retired.push(crate::cache::views::retire(&self.cache, &root)?);
            }
        }
        drop(branches);
        for directory in retired {
            directory.dispose()?;
        }
        Ok(())
    }

    pub async fn flush_usage(&self) -> Result<()> {
        let defaults: Vec<_> = self
            .default_running
            .lock()
            .unwrap()
            .values()
            .map(|job| job.1.clone())
            .collect();
        for mut running in defaults {
            while running.borrow().is_none() {
                if running.changed().await.is_err() {
                    break;
                }
            }
        }
        let branches: Vec<_> = self.branches.lock().unwrap().values().cloned().collect();
        // Shutdown must also join loads whose request was cancelled.
        for branch in &branches {
            drop(branch.initialized.lock().await);
            let running = branch.running.lock().unwrap().clone();
            if let Some(mut running) = running {
                while running.borrow().is_none() {
                    if running.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
        tokio::task::spawn_blocking(move || {
            for branch in branches {
                branch.checkpoint_usage(true)?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    pub(crate) fn inventory_usage(&self) -> crate::memory::CacheUsage {
        let mut usage = crate::memory::CacheUsage::default();
        for branch in self.branches.lock().unwrap().values() {
            let bytes = branch.inventory_bytes();
            if bytes == 0 {
                continue;
            }
            usage.entries += 1;
            usage.retained_bytes += bytes;
            if Arc::strong_count(branch) > 1 {
                usage.active_entries += 1;
                usage.pinned_bytes += bytes;
            }
        }
        usage
    }

    /// Keep recent inventories within the unused workspace budget.
    /// The callback checks the current workspace registry while new branch lookups are excluded.
    pub(crate) fn trim_unreferenced(
        &self,
        budget: u64,
        retained: impl Fn(&Path) -> bool,
    ) -> Result<()> {
        let mut branches = self.branches.lock().unwrap();
        let mandatory: std::collections::HashSet<_> = branches
            .iter()
            .filter_map(|(key, branch)| {
                let source = branch.source();
                let source = source.canonicalize().unwrap_or(source);
                (Arc::strong_count(branch) != 1
                    || now() < branch.retry_after.load(Ordering::Relaxed)
                    || retained(&source)
                    || branch.state.try_write().is_err()
                    || branch
                        .running
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|job| job.borrow().is_none()))
                .then_some(key.clone())
            })
            .collect();
        let mut remaining = budget.saturating_sub(
            branches
                .iter()
                .filter(|(key, _)| mandatory.contains(*key))
                .map(|(_, branch)| branch.inventory_bytes())
                .sum(),
        );
        let mut candidates: Vec<_> = branches
            .iter()
            .filter_map(|(key, branch)| {
                let used = branch.last_use.load(Ordering::Relaxed);
                let bytes = branch.inventory_bytes();
                (!mandatory.contains(key)
                    && bytes > 0
                    && now().saturating_sub(used) < crate::demand::RETAIN_PAUSED.as_millis() as u64)
                    .then_some((key.clone(), used, bytes))
            })
            .collect();
        candidates.sort_by_key(|(_, used, _)| std::cmp::Reverse(*used));
        let keep: std::collections::HashSet<_> = candidates
            .into_iter()
            .take(256)
            .filter_map(|(key, _, bytes)| {
                if bytes > remaining {
                    return None;
                }
                remaining -= bytes;
                Some(key)
            })
            .collect();
        let mut retired = Vec::new();
        let mut directories = Vec::new();
        let mut failure = None;
        branches.retain(|key, branch| {
            if mandatory.contains(key) || keep.contains(key) {
                return true;
            }
            let Ok(mut state) = branch.state.try_write() else {
                return true;
            };
            if branch
                .running
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|job| job.borrow().is_none())
            {
                return true;
            }
            if let Some(state) = state.as_mut().map(Arc::make_mut) {
                let last_use = branch.last_use.load(Ordering::Relaxed);
                if state.last_use != last_use {
                    let previous = state.last_use;
                    state.last_use = last_use;
                    if let Err(error) = branch.checkpoint_usage(true) {
                        state.last_use = previous;
                        failure = Some(error);
                        return true;
                    }
                }
            } else if branch.root.is_dir() && !branch.root.join("state.bin").exists() {
                match crate::cache::views::retire(&self.cache, &branch.root) {
                    Ok(directory) => directories.push(directory),
                    Err(error) => {
                        failure = Some(error);
                        return true;
                    }
                }
            }
            retired.push(branch.clone());
            false
        });
        drop(branches);
        drop(retired);
        for directory in directories {
            directory.dispose()?;
        }
        failure.map_or(Ok(()), Err)
    }

    pub fn expire(&self, branch: &Arc<Branch>, retire: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut branches = self.branches.lock().unwrap();
        if Arc::strong_count(branch) > 2
            || !self.branch_expired(branch)?
            || branch
                .running
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.borrow().is_none())
        {
            return Ok(());
        }
        retire()?;
        let retired = crate::cache::views::retire(&self.cache, &branch.root)?;
        branches.retain(|_, value| !Arc::ptr_eq(value, branch));
        drop(branches);
        retired.dispose()?;
        Ok(())
    }

    pub async fn resolve(self: &Arc<Self>, repository: Repository) -> Result<Arc<Branch>> {
        self.resolve_with(repository, false, None).await
    }

    pub async fn resolve_monitored(
        self: &Arc<Self>,
        repository: Repository,
        operation: &crate::telemetry::Operation,
    ) -> Result<Arc<Branch>> {
        self.resolve_with(repository, false, Some(operation)).await
    }

    pub(crate) async fn resolve_inventory_monitored(
        self: &Arc<Self>,
        repository: Repository,
        operation: &crate::telemetry::Operation,
    ) -> Result<Arc<Branch>> {
        self.resolve_with(repository, true, Some(operation)).await
    }

    pub(crate) async fn ready(
        self: &Arc<Self>,
        branch: &Arc<Branch>,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<()> {
        loop {
            let ready = {
                let _wait = telemetry.wait("branch_state");
                branch
                    .state
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|state| self.usable(branch, state))
            };
            if ready {
                return Ok(());
            }
            let (mut running, mut job_id) = {
                let running = branch.running.lock().unwrap();
                (
                    running
                        .clone()
                        .context("Repository preparation was not scheduled")?,
                    branch.telemetry_id.lock().unwrap().clone(),
                )
            };
            loop {
                if let Some(outcome) = running.borrow().clone() {
                    if let Err(error) = outcome
                        && !error.is::<crate::demand::Paused>()
                    {
                        return Err(crate::diagnostics::shared_error(error));
                    }
                    break;
                }
                if let Some(job_id) = job_id.take() {
                    telemetry.link(&job_id);
                }
                let _wait = telemetry.wait("repository_refresh");
                running
                    .changed()
                    .await
                    .context("Repository preparation stopped")?;
            }
            let ready = branch
                .state
                .read()
                .await
                .as_ref()
                .is_some_and(|state| self.usable(branch, state));
            if !ready {
                self.schedule(branch.clone(), true, false);
            }
        }
    }

    pub(crate) fn usable(&self, branch: &Branch, state: &State) -> bool {
        !state.repair
            && branch.source().is_dir()
            && state.policy == self.options.selection.identity
            && branch.sources_current(&state.prepared)
            && branch
                .inventory
                .borrow()
                .as_ref()
                .is_none_or(|inventory| inventory.prepared.revision == state.prepared.revision)
    }

    /// Fetch one resolved source at the inventory's revision, including LFS fallback.
    pub(crate) async fn source_file(
        &self,
        branch: &Arc<Branch>,
        revision: &str,
        path: &str,
        activity: impl Send + 'static,
    ) -> Result<FileView> {
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let ownership = branch.acquiring.clone().lock_owned().await;
        let inventory = branch.inventory.borrow().clone();
        let state = branch.state.read().await.clone();
        let prepared = inventory
            .as_ref()
            .map(|inventory| &inventory.prepared)
            .or_else(|| {
                state
                    .as_ref()
                    .filter(|state| !state.repair)
                    .map(|state| &state.prepared)
            })
            .context("Repository inventory is unavailable")?;
        ensure!(
            prepared.revision == revision,
            crate::diagnostics::RefreshRequired("Repository changed before file acquisition")
        );
        ensure!(
            prepared.selected.contains_key(path) || prepared.deferred_assets.contains(path),
            "Source is not selected"
        );
        let path = path.to_owned();
        let mut paths = std::collections::BTreeSet::from([path.clone()]);
        if crate::unity::assets::is_asset(Path::new(&path)) {
            let metadata = format!("{path}.meta");
            if prepared.selected.contains_key(&metadata)
                || prepared.deferred_assets.contains(&metadata)
            {
                paths.insert(metadata);
            }
        }
        let operation = crate::telemetry::Operation::default();
        let mut acquisition =
            super::cache::Acquisition::begin(&self.cache, &branch.repository.identity, &operation)
                .await?;
        let staging = acquisition.staging.clone();
        let stage = Arc::new(
            tempfile::Builder::new()
                .prefix("view-")
                .tempdir_in(&branch.root)?,
        );
        let request = Request {
            transfer_used: prepared.transfer_bytes,
            contents: super::materialize::Contents::Only(paths.clone()),
            unlimited_transfer: self
                .options
                .rules
                .iter()
                .any(|rule| rule.exact_match(&branch.repository)),
            allow_private: authorize(&self.options.rules, &branch.repository)?,
            repository: branch.repository.transport.clone(),
            preferred_transport: None,
            target: Target::Commit(prepared.revision.clone()),
            store: PathBuf::new(),
            staging: stage.path().into(),
            include: self.options.selection.patterns.0.clone(),
            exclude: self.options.selection.patterns.1.clone(),
            required: paths.into_iter().collect(),
            additional: Vec::new(),
            previous: BTreeMap::new(),
            subdirectory: None,
        };
        let pinned = super::materialize::Reuse::Pinned(super::materialize::Inventory {
            repository: branch.repository.identity.clone(),
            policy: self.options.selection.identity.clone(),
            subdirectory: None,
            prepared: prepared.file_inventory(&request.required),
        });
        let cache = self.cache.clone();
        let branch = branch.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _ownership = ownership;
            let (acquired, pin) =
                acquisition.git_cancellable(&request, &cache, Some(pinned), &operation, &cancel)?;
            drop(acquisition);
            branch.record_transfer(&acquired.prepared)?;
            let acquired = super::cache::complete_cancellable(
                &request, &cache, acquired, &pin, &staging, &operation, &cancel,
            )?;
            branch.record_transfer(&acquired.prepared)?;
            Ok(FileView {
                prepared: acquired.prepared,
                source: stage,
                _pin: pin,
                _staging: staging,
            })
        })
        .await?;
        guard.disarm();
        result
    }

    async fn resolve_with(
        self: &Arc<Self>,
        repository: Repository,
        inventory_only: bool,
        operation: Option<&crate::telemetry::Operation>,
    ) -> Result<Arc<Branch>> {
        authorize(&self.options.rules, &repository)?;
        let default = self
            .default_branch(&repository, repository.selector.is_none(), operation)
            .await?;
        let target = match &repository.selector {
            Some(name) => Target::selector(name)?,
            None => Target::Branch(default.context("Default branch is missing")?),
        };
        let name = match &target {
            Target::Branch(name) | Target::Named(name) | Target::Commit(name) => name.clone(),
            Target::Tag(name) => format!("refs/tags/{name}"),
            Target::DefaultBranch => unreachable!(),
        };
        let identity = if matches!(target, Target::Branch(_)) {
            postcard::to_allocvec(&(&repository.identity, &name))?
        } else {
            postcard::to_allocvec(&(&repository.identity, &target))?
        };
        let key = blake3::hash(&identity).to_hex().to_string();
        let branch = {
            let mut branches = self.branches.lock().unwrap();
            if let Some(branch) = branches.get(&key) {
                branch.clone()
            } else {
                let root = self.cache.join("repositories").join(&key);
                let branch = Arc::new(Branch {
                    root,
                    repository: repository.clone(),
                    name,
                    target,
                    last_use: AtomicU64::new(0),
                    persisted_use: AtomicU64::new(0),
                    generation: AtomicU64::new(0),
                    state: Arc::new(AsyncRwLock::new(None)),
                    analysis: Arc::new(AsyncRwLock::new(())),
                    running: Mutex::new(None),
                    telemetry_id: Mutex::new(None),
                    retry_after: AtomicU64::new(0),
                    acquiring: Arc::new(AsyncMutex::new(())),
                    sources: Mutex::new(None),
                    state_bytes: Mutex::new(None),
                    inventory: watch::channel(None).0,
                    initialized: Arc::new(AsyncMutex::new(false)),
                    demand: Arc::default(),
                    inventory_demand: Arc::default(),
                });
                branches.insert(key, branch.clone());
                branch
            }
        };
        let _inventory_interest = branch.inventory_demand.interest();
        let _interest = (!inventory_only).then(|| branch.demand.interest());
        self.initialize_branch(
            &branch,
            operation.unwrap_or(&crate::telemetry::Operation::default()),
        )
        .await?;
        let state = branch.state.read().await;
        let usable = state
            .as_ref()
            .is_some_and(|state| self.usable(&branch, state));
        let due = state.as_ref().is_none_or(|s| {
            branch.refresh_due(&self.options, s.refreshed, now(), Some(&s.prepared))
        });
        drop(state);
        let inventory_current = || {
            branch.inventory.borrow().as_ref().is_some_and(|inventory| {
                !branch.refresh_due(
                    &self.options,
                    inventory.refreshed,
                    now(),
                    Some(&inventory.prepared),
                )
            })
        };
        let has_inventory = branch.inventory.borrow().is_some();
        if inventory_only && inventory_current() {
            return Ok(branch);
        }
        if !usable || due {
            self.schedule(
                branch.clone(),
                !usable && (!inventory_only || !has_inventory),
                inventory_only,
            );
        }
        if inventory_only && has_inventory {
            return Ok(branch);
        }
        if !inventory_only && !usable {
            self.ready(
                &branch,
                operation.unwrap_or(&crate::telemetry::Operation::default()),
            )
            .await?;
        } else if inventory_only && !usable {
            let mut inventory = branch.inventory.subscribe();
            let (mut running, mut job_id) = {
                let running = branch.running.lock().unwrap();
                (
                    running
                        .clone()
                        .context("Repository preparation was not scheduled")?,
                    branch.telemetry_id.lock().unwrap().clone(),
                )
            };
            loop {
                if inventory_current() {
                    break;
                }
                let outcome = running.borrow().clone();
                if let Some(outcome) = outcome {
                    if outcome
                        .as_ref()
                        .is_err_and(|error| error.is::<crate::demand::Paused>())
                    {
                        self.schedule(branch.clone(), true, true);
                        running = branch.running.lock().unwrap().as_ref().unwrap().clone();
                        job_id = branch.telemetry_id.lock().unwrap().clone();
                        continue;
                    }
                    outcome.map_err(crate::diagnostics::shared_error)?;
                    break;
                }
                if let (Some(operation), Some(job_id)) = (operation, job_id.take()) {
                    operation.link(&job_id);
                }
                tokio::select! {
                    result = running.changed() => result.context("Repository preparation stopped")?,
                    result = inventory.changed(), if inventory_only => result.context("Repository inventory stopped")?,
                }
            }
        }
        Ok(branch)
    }

    fn schedule(self: &Arc<Self>, branch: Arc<Branch>, requested: bool, inventory_only: bool) {
        let mut running = branch.running.lock().unwrap();
        if running.as_ref().is_some_and(|r| r.borrow().is_none())
            || !requested && now() < branch.retry_after.load(Ordering::Relaxed)
        {
            return;
        }
        let telemetry = crate::telemetry::operation(
            "repository_refresh",
            Some(&branch.repository.identity.to_string()),
            None,
        );
        telemetry.target(&branch.name);
        *branch.telemetry_id.lock().unwrap() = telemetry.id();
        let (send, receive) = watch::channel(None);
        *running = Some(receive);
        drop(running);
        let manager = self.clone();
        // Idle housekeeping only refreshes inventories. Recent source work uses
        // the remaining foreground grace period without extending it.
        let maintenance =
            (!requested && inventory_only).then(|| branch.inventory_demand.interest());
        let work = Arc::new(
            if inventory_only {
                &branch.inventory_demand
            } else {
                &branch.demand
            }
            .work(),
        );
        tokio::spawn(async move {
            let _maintenance = maintenance;
            telemetry.stage("repository_refresh");
            let result = manager
                .refresh(&branch, &telemetry, inventory_only, &work)
                .await;
            let paused = result
                .as_ref()
                .is_err_and(|error| error.is::<crate::demand::Paused>());
            if let Err(error) = &result
                && !paused
            {
                tracing::warn!(error = %format!("{error:#}"), "Repository refresh failed");
                if let Some(diagnostic) = crate::diagnostics::monitoring(error) {
                    telemetry.diagnose(diagnostic);
                }
            }
            telemetry.finish(if paused {
                "paused"
            } else if result.is_ok() {
                "success"
            } else {
                "failure"
            });
            if result.is_err() && !paused {
                branch.retry_after.store(now() + 30_000, Ordering::Relaxed);
            }
            let _ = send.send(Some(result.map_err(Arc::new)));
        });
    }

    /// Acquires discovery-requested inputs from the applied commit, outside the publication gate.
    pub async fn require_inputs(
        &self,
        branch: &Arc<Branch>,
        revision: &str,
        paths: Vec<String>,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<()> {
        self.extend_inputs(
            branch,
            revision,
            paths,
            super::materialize::Contents::Sources,
            telemetry,
            Arc::new(()),
        )
        .await
    }

    pub(crate) async fn require_inputs_cancellable(
        &self,
        branch: &Arc<Branch>,
        revision: &str,
        paths: Vec<String>,
        telemetry: &crate::telemetry::Operation,
        work: Arc<crate::demand::Work>,
    ) -> Result<()> {
        self.extend_inputs(
            branch,
            revision,
            paths,
            super::materialize::Contents::Sources,
            telemetry,
            work,
        )
        .await
    }

    pub(crate) async fn complete_assets(
        &self,
        branch: &Arc<Branch>,
        revision: &str,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<()> {
        self.extend_inputs(
            branch,
            revision,
            Vec::new(),
            super::materialize::Contents::All,
            telemetry,
            Arc::new(()),
        )
        .await
    }

    /// Materialize only the repository asset selected by a file-content view.
    pub(crate) async fn asset_file(
        self: &Arc<Self>,
        branch: &Arc<Branch>,
        revision: &str,
        path: &str,
        telemetry: &crate::telemetry::Operation,
        cancel: Arc<dyn crate::cancellation::Cancellation + Send>,
    ) -> Result<()> {
        let guard =
            crate::cancellation::until(cancel.as_ref(), async { Ok(branch.state.read().await) })
                .await?;
        let state = guard
            .as_ref()
            .context("Repository inputs are unavailable")?;
        ensure!(
            self.usable(branch, state) && state.prepared.revision == revision,
            crate::diagnostics::RefreshRequired(
                "Repository changed before asset view; retry query"
            )
        );
        if !state.prepared.deferred_assets.contains(path) {
            return Ok(());
        }
        let mut only = std::collections::BTreeSet::from([path.to_owned()]);
        let meta = format!("{path}.meta");
        if crate::unity::assets::is_asset(Path::new(path))
            && (state.prepared.selected.contains_key(&meta)
                || state.prepared.deferred_assets.contains(&meta))
        {
            only.insert(meta);
        }
        let revision = state.prepared.revision.clone();
        drop(guard);
        // Release the state reader before acquiring the publication gate.
        self.extend_inputs(
            branch,
            &revision,
            Vec::new(),
            super::materialize::Contents::Only(only),
            telemetry,
            cancel,
        )
        .await
    }

    async fn extend_inputs(
        &self,
        branch: &Arc<Branch>,
        revision: &str,
        paths: Vec<String>,
        contents: super::materialize::Contents,
        telemetry: &crate::telemetry::Operation,
        cancel: Arc<dyn crate::cancellation::Cancellation + Send>,
    ) -> Result<()> {
        let assets = matches!(
            contents,
            super::materialize::Contents::All | super::materialize::Contents::Only(_)
        );
        let only = match &contents {
            super::materialize::Contents::Only(paths) => Some(paths.clone()),
            _ => None,
        };
        authorize(&self.options.rules, &branch.repository)?;
        for path in &paths {
            self.options.selection.require(path)?;
        }
        let _operation = {
            let _wait = telemetry.wait("repository_admission");
            crate::cancellation::until(cancel.as_ref(), async {
                Ok(branch.acquiring.clone().lock_owned().await)
            })
            .await?
        };
        let before = {
            let _wait = telemetry.wait("branch_state");
            crate::cancellation::until(cancel.as_ref(), async {
                Ok(branch.state.read().await.clone())
            })
            .await?
        }
        .context("Repository inputs are unavailable")?;
        if assets {
            ensure!(
                before.prepared.revision == revision && !before.repair,
                crate::diagnostics::RefreshRequired(
                    "Repository changed before asset acquisition; retry query"
                )
            );
            if before.prepared.deferred_assets.is_empty()
                || only
                    .as_ref()
                    .is_some_and(|paths| paths.is_disjoint(&before.prepared.deferred_assets))
            {
                return Ok(());
            }
        }
        if before.prepared.revision != revision {
            return Ok(());
        }
        let mut acquisition = crate::cancellation::until(
            cancel.as_ref(),
            super::cache::Acquisition::begin(&self.cache, &branch.repository.identity, telemetry),
        )
        .await?;
        let staging = acquisition.staging.clone();
        let stage = Arc::new(
            tempfile::Builder::new()
                .prefix("required-")
                .tempdir_in(&branch.root)?,
        );
        let request = Request {
            transfer_used: before.prepared.transfer_bytes,
            contents,
            unlimited_transfer: self
                .options
                .rules
                .iter()
                .any(|r| r.exact_match(&branch.repository)),
            allow_private: authorize(&self.options.rules, &branch.repository)?,
            repository: branch.repository.transport.clone(),
            preferred_transport: None,
            target: Target::Commit(revision.to_owned()),
            store: PathBuf::new(),
            staging: stage.path().to_owned(),
            include: self.options.selection.patterns.0.clone(),
            exclude: self.options.selection.patterns.1.clone(),
            required: paths.clone(),
            additional: before.additional.iter().cloned().collect(),
            // Optional acquisition must leave the published code inputs alone,
            // including inputs already recorded as unavailable until refresh.
            previous: if assets {
                before.prepared.selected.clone()
            } else {
                before
                    .prepared
                    .selected
                    .iter()
                    .filter(|(p, _)| branch.source().join(p).is_file())
                    .map(|(p, id)| (p.clone(), id.clone()))
                    .collect()
            },
            subdirectory: None,
        };
        let cache = self.cache.clone();
        telemetry.stage("required_input_fetch");
        let staged = stage.clone();
        let worker_staging = staging.clone();
        let worker_telemetry = telemetry.clone();
        let pinned = super::materialize::Reuse::Pinned(super::materialize::Inventory {
            repository: before.repository.clone(),
            policy: before.policy.clone(),
            subdirectory: None,
            prepared: before.prepared.inventory(),
        });
        let worker_cancel = cancel.clone();
        let transfer_branch = branch.clone();
        let (prepared, _pool_pin, _operation) = tokio::task::spawn_blocking(move || {
            let operation = _operation;
            let _stage = staged;
            let (prepared, pin) = acquisition.git_cancellable(
                &request,
                &cache,
                Some(pinned),
                &worker_telemetry,
                worker_cancel.as_ref(),
            )?;
            drop(acquisition);
            transfer_branch.record_transfer(&prepared.prepared)?;
            let prepared = super::cache::complete_cancellable(
                &request,
                &cache,
                prepared,
                &pin,
                &worker_staging,
                &worker_telemetry,
                worker_cancel.as_ref(),
            )?;
            transfer_branch.record_transfer(&prepared.prepared)?;
            Ok::<_, anyhow::Error>((prepared, pin, operation))
        })
        .await??;
        let staged_files = prepared.staged;
        let mut prepared = prepared.prepared;
        prepared.branch = before.prepared.branch.clone();
        prepared.resolved_target = before.prepared.resolved_target.clone();
        before.reuse_omissions(&mut prepared, &self.options.selection.identity);
        if let Some(only) = &only {
            let mut selected = before.prepared.selected.clone();
            selected.extend(std::mem::take(&mut prepared.selected));
            prepared.selected = selected;
            prepared.deferred_assets = before
                .prepared
                .deferred_assets
                .difference(only)
                .cloned()
                .collect();
            let mut omitted = before.prepared.omitted.clone();
            omitted.extend(std::mem::take(&mut prepared.omitted));
            prepared.omitted = omitted;
        }
        if assets {
            prepared
                .unavailable
                .extend(before.prepared.unavailable.iter().cloned());
        }
        telemetry.stage("required_input_publication");
        let mut next = (*before).clone();
        next.additional.extend(paths);
        next.prepared = prepared;
        let unchanged = !assets
            && before.same_inputs(&next)
            && fs::read_dir(stage.path())?.next().transpose()?.is_none()
            && branch.source().is_dir()
            && next
                .prepared
                .directories
                .iter()
                .all(|path| branch.source().join(path).is_dir());
        let staged_inventory = if unchanged {
            None
        } else {
            let root = branch.root.clone();
            let (returned, inventory) = tokio::task::spawn_blocking(move || {
                let inventory = super::state_store::stage(&root, &next)?;
                Ok::<_, anyhow::Error>((next, inventory))
            })
            .await??;
            next = returned;
            Some(inventory)
        };
        let mut state = {
            let _wait = telemetry.wait("repository_publication");
            crate::cancellation::until(cancel.as_ref(), async {
                Ok(branch.publication(assets).await)
            })
            .await?
        };
        if assets {
            ensure!(
                state
                    .as_ref()
                    .is_some_and(|state| !state.repair && state.prepared.revision == revision),
                crate::diagnostics::RefreshRequired(
                    "Repository changed during asset acquisition; retry query"
                )
            );
        }
        next.indexed_revision = state
            .as_ref()
            .and_then(|state| state.indexed_revision.clone());
        if unchanged {
            branch.persist_header(&next)?;
            *state = Some(Arc::new(next));
            return Ok(());
        }
        let mut staged_inventory = staged_inventory;
        if !assets {
            next.repair = true;
            branch.publish_inventory(&next, staged_inventory.take().unwrap())?;
            *state = Some(Arc::new(next.clone()));
        }
        let changed: std::collections::BTreeSet<_> = staged_files
            .iter()
            .map(|file| &file.path)
            .chain(next.prepared.unavailable.iter())
            .chain(
                before
                    .prepared
                    .selected
                    .keys()
                    .filter(|path| !next.prepared.selected.contains_key(*path)),
            )
            .collect();
        for path in changed {
            let staged = stage.path().join(path);
            if next.prepared.unavailable.contains(path)
                || !next.prepared.selected.contains_key(path)
            {
                let target = branch.source().join(path);
                if target.is_file() {
                    fs::remove_file(target)?;
                }
                continue;
            }
            if staged.is_file() {
                let target = branch.source().join(path);
                fs::create_dir_all(target.parent().unwrap())?;
                fs::rename(staged, target)?;
            }
        }
        next.repair = false;
        if assets {
            branch.publish_inventory(&next, staged_inventory.unwrap())?;
        } else {
            branch.persist_header(&next)?;
        }
        *state = Some(Arc::new(next));
        if !assets {
            branch.generation.fetch_add(1, Ordering::Release);
        }
        Ok(())
    }

    async fn refresh(
        &self,
        branch: &Branch,
        telemetry: &crate::telemetry::Operation,
        inventory_only: bool,
        work: &Arc<crate::demand::Work>,
    ) -> Result<()> {
        authorize(&self.options.rules, &branch.repository)?;
        let _operation = {
            let _wait = telemetry.wait("repository_admission");
            work.until(async { Ok(branch.acquiring.lock().await) })
                .await?
        };
        let before = {
            let _wait = telemetry.wait("branch_state");
            branch.state.read().await.clone()
        };
        let previous_inventory = branch.inventory.borrow().clone();
        let cached_inventory = previous_inventory.clone().filter(|inventory| {
            !branch.refresh_due(
                &self.options,
                inventory.refreshed,
                now(),
                Some(&inventory.prepared),
            )
        });
        if cached_inventory.is_none()
            && let Some(before) = &before
            && self
                .refresh_unchanged(
                    branch,
                    before,
                    telemetry,
                    work.as_ref(),
                    super::job::reference,
                )
                .await?
        {
            branch.inventory.send_replace(None);
            return Ok(());
        }
        let mut acquisition = work
            .until(super::cache::Acquisition::begin(
                &self.cache,
                &branch.repository.identity,
                telemetry,
            ))
            .await?;
        let staging = acquisition.staging.clone();
        telemetry.stage("repository_inventory");
        let stage = Arc::new(
            tempfile::Builder::new()
                .prefix("incoming-")
                .tempdir_in(&branch.root)?,
        );
        let previous = before
            .as_ref()
            .filter(|s| !s.repair)
            .map(|s| {
                s.prepared
                    .selected
                    .iter()
                    .filter(|(p, _)| branch.source().join(p).is_file())
                    .map(|(p, i)| (p.clone(), i.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let resolved_target = before
            .as_ref()
            .and_then(|state| state.prepared.resolved_target.clone());
        let target =
            Target::clone(branch.acquisition_target(before.as_ref().map(|state| &state.prepared)));
        let mut request = Request {
            transfer_used: 0,
            contents: super::materialize::Contents::Sources,
            unlimited_transfer: self
                .options
                .rules
                .iter()
                .any(|r| r.exact_match(&branch.repository)),
            allow_private: authorize(&self.options.rules, &branch.repository)?,
            repository: branch.repository.transport.clone(),
            preferred_transport: None,
            target,
            store: PathBuf::new(),
            staging: stage.path().to_owned(),
            include: self.options.selection.patterns.0.clone(),
            exclude: self.options.selection.patterns.1.clone(),
            required: vec![],
            additional: before
                .as_ref()
                .map(|s| s.additional.iter().cloned().collect())
                .unwrap_or_default(),
            previous,
            subdirectory: None,
        };
        let cache = self.cache.clone();
        let inventory = if let Some(inventory) = cached_inventory {
            Some(inventory)
        } else if before.is_none() || inventory_only || previous_inventory.is_some() {
            telemetry.stage("repository_inventory_acquisition");
            let mut inventory_request = request.clone();
            inventory_request.contents = super::materialize::Contents::Inventory;
            let cache = cache.clone();
            let staged = stage.clone();
            let worker_telemetry = telemetry.clone();
            let worker_cancel = work.clone();
            let (returned, mut prepared, pin) = tokio::task::spawn_blocking(move || {
                let _stage = staged;
                let (prepared, pin) = acquisition.git_cancellable(
                    &inventory_request,
                    &cache,
                    None,
                    &worker_telemetry,
                    worker_cancel.as_ref(),
                )?;
                Ok::<_, anyhow::Error>((acquisition, prepared, pin))
            })
            .await??;
            acquisition = returned;
            let prior_bytes = previous_inventory
                .as_ref()
                .map(|inventory| &inventory.prepared)
                .into_iter()
                .chain(before.as_ref().map(|state| &state.prepared))
                .filter(|prior| prior.revision == prepared.prepared.revision)
                .map(|prior| prior.transfer_bytes)
                .max()
                .unwrap_or(0);
            prepared.prepared.transfer_bytes =
                prepared.prepared.transfer_bytes.saturating_add(prior_bytes);
            fs::create_dir_all(branch.source())?;
            let inventory = Arc::new(Inventory {
                prepared: prepared.prepared,
                source: None,
                _pin: Arc::new(pin),
                refreshed: now(),
                heap_bytes: Default::default(),
                _staging: None,
            });
            request.target = Target::Commit(inventory.prepared.revision.clone());
            request.transfer_used = inventory.prepared.transfer_bytes;
            branch.inventory.send_replace(Some(inventory.clone()));
            Some(inventory)
        } else {
            None
        };
        if inventory_only {
            return Ok(());
        }
        work.check()?;
        if let Some(inventory) = &inventory {
            request.target = Target::Commit(inventory.prepared.revision.clone());
            request.transfer_used = inventory.prepared.transfer_bytes;
        }
        // No publication lock is held while fetching or extracting objects.
        let mut transferred = request.transfer_used;
        let outcome = async {
            telemetry.stage("git_acquisition");
            let git_request = request.clone();
            let git_cache = cache.clone();
            let staged = stage.clone();
            let worker_telemetry = telemetry.clone();
            let worker_cancel = work.clone();
            let reuse = inventory
                .as_ref()
                .map(|inventory| {
                    super::materialize::Reuse::Pinned(super::materialize::Inventory {
                        repository: branch.repository.identity.clone(),
                        policy: self.options.selection.identity.clone(),
                        subdirectory: request.subdirectory.clone(),
                        prepared: inventory.prepared.inventory(),
                    })
                })
                .or_else(|| {
                    before
                        .as_ref()
                        .filter(|state| state.policy == self.options.selection.identity)
                        .map(|state| {
                            super::materialize::Reuse::Previous(super::materialize::Inventory {
                                repository: state.repository.clone(),
                                policy: state.policy.clone(),
                                subdirectory: request.subdirectory.clone(),
                                prepared: state.prepared.inventory(),
                            })
                        })
                });
            let (acquired, pin) = tokio::task::spawn_blocking(move || {
                let _stage = staged;
                acquisition.git_cancellable(
                    &git_request,
                    &git_cache,
                    reuse,
                    &worker_telemetry,
                    worker_cancel.as_ref(),
                )
            })
            .await??;
            let staged_files = acquired.staged;
            transferred = acquired.prepared.transfer_bytes;
            work.check()?;
            let mut prepared = acquired.prepared;
            let _pool_pin = Arc::new(pin);
            if let Some(inventory) = &inventory {
                prepared.branch = inventory.prepared.branch.clone();
                prepared.resolved_target = inventory.prepared.resolved_target.clone();
            }
            if resolved_target.is_some() {
                prepared.resolved_target = resolved_target;
            }
            if let Some(before) = &before {
                before.reuse_omissions(&mut prepared, &self.options.selection.identity);
            }
            if inventory.is_some() {
                branch.inventory.send_replace(Some(Arc::new(Inventory {
                    prepared: prepared.clone(),
                    source: Some(stage.clone()),
                    _pin: _pool_pin.clone(),
                    refreshed: now(),
                    heap_bytes: Default::default(),
                    _staging: Some(staging.clone()),
                })));
            }
            telemetry.stage("repository_materialization");
            let staged = stage.clone();
            let worker_staging = staging.clone();
            let worker_telemetry = telemetry.clone();
            let worker_pin = _pool_pin.clone();
            let worker_cancel = work.clone();
            prepared = tokio::task::spawn_blocking(move || {
                let _stage = staged;
                super::cache::complete_cancellable(
                    &request,
                    &cache,
                    super::materialize::Acquired {
                        prepared,
                        staged: staged_files,
                    },
                    &worker_pin,
                    &worker_staging,
                    &worker_telemetry,
                    worker_cancel.as_ref(),
                )
            })
            .await??
            .prepared;
            transferred = prepared.transfer_bytes;
            telemetry.stage("repository_publication");
            let mut next = State {
                repository: branch.repository.identity.clone(),
                transport: branch.repository.transport.clone(),
                branch: branch.name.clone(),
                target: branch.target.clone(),
                last_use: branch.last_use.load(Ordering::Relaxed),
                refreshed: now(),
                policy: self.options.selection.identity.clone(),
                prepared,
                indexed_revision: before.as_ref().and_then(|s| s.indexed_revision.clone()),
                repair: true,
                additional: before
                    .as_ref()
                    .map(|s| s.additional.clone())
                    .unwrap_or_default(),
            };
            let source = branch.source();
            let unchanged = before.as_ref().is_some_and(|s| s.same_inputs(&next))
                && fs::read_dir(stage.path())?.next().transpose()?.is_none()
                && source.is_dir()
                && next
                    .prepared
                    .directories
                    .iter()
                    .all(|p| source.join(p).is_dir());
            let staged_inventory = if unchanged {
                None
            } else {
                let root = branch.root.clone();
                let (returned, inventory) = tokio::task::spawn_blocking(move || {
                    let inventory = super::state_store::stage(&root, &next)?;
                    Ok::<_, anyhow::Error>((next, inventory))
                })
                .await??;
                next = returned;
                Some(inventory)
            };
            let mut state = {
                let _wait = telemetry.wait("repository_publication");
                work.until(async { Ok(branch.publish().await) }).await?
            };
            if unchanged {
                // Materialization already checked selected files. Keep the latest
                // indexing marker, since indexing can finish during acquisition.
                next.indexed_revision = state.as_ref().and_then(|s| s.indexed_revision.clone());
                next.repair = false;
                branch.persist_header(&next)?;
                *state = Some(Arc::new(next));
                branch.inventory.send_replace(None);
                return Ok(());
            }
            // Discovery may have completed or lost dependencies during acquisition.
            // Read its current completion marker while holding publication admission.
            let indexed = state.as_ref().is_some_and(|s| {
                !s.repair
                    && s.prepared.selected == next.prepared.selected
                    && s.prepared.unavailable == next.prepared.unavailable
                    && s.indexed_revision.as_ref() == Some(&s.prepared.revision)
            });
            next.indexed_revision = state.as_ref().and_then(|s| s.indexed_revision.clone());
            branch.publish_inventory(&next, staged_inventory.unwrap())?;
            *state = Some(Arc::new(next.clone()));
            fs::create_dir_all(&source)?;
            if let Some(before) = &before {
                for path in before
                    .prepared
                    .selected
                    .keys()
                    .filter(|p| !next.prepared.selected.contains_key(*p))
                {
                    let target = source.join(path);
                    if target.is_file() {
                        fs::remove_file(target)?;
                    }
                }
            }
            for path in next.prepared.selected.keys() {
                let staged = stage.path().join(path);
                if next.prepared.unavailable.contains(path) {
                    let target = source.join(path);
                    if target.is_file() {
                        fs::remove_file(target)?;
                    }
                    continue;
                }
                if staged.is_file() {
                    let target = source.join(path);
                    fs::create_dir_all(target.parent().unwrap())?;
                    if inventory.is_some() {
                        if target.is_file() {
                            fs::remove_file(&target)?;
                        }
                        fs::hard_link(staged, target)?;
                    } else {
                        fs::rename(staged, target)?;
                    }
                }
            }
            for directory in &next.prepared.directories {
                fs::create_dir_all(source.join(directory))?;
            }
            next.repair = false;
            if indexed {
                next.indexed_revision = Some(next.prepared.revision.clone());
            }
            branch.persist_header(&next)?;
            *state = Some(Arc::new(next));
            branch.generation.fetch_add(1, Ordering::Release);
            branch.inventory.send_replace(None);
            Ok(())
        }
        .await;
        if outcome.is_err() {
            let inventory = inventory.map(|mut inventory| {
                Arc::make_mut(&mut inventory).prepared.transfer_bytes = transferred;
                inventory
            });
            branch.inventory.send_replace(inventory);
        }
        outcome
    }

    async fn refresh_unchanged(
        &self,
        branch: &Branch,
        before: &Arc<State>,
        telemetry: &crate::telemetry::Operation,
        cancel: &dyn crate::cancellation::Cancellation,
        resolve: impl FnOnce(
            &Path,
            &Repository,
            &Target,
            bool,
        ) -> Result<(String, Option<String>, Option<String>)>
        + Send
        + 'static,
    ) -> Result<bool> {
        let target = branch.acquisition_target(Some(&before.prepared));
        if before.repair
            || before.policy != self.options.selection.identity
            || !before.prepared.unavailable.is_empty()
            || matches!(target, Target::Commit(_) | Target::DefaultBranch)
            || matches!(target, Target::Named(name) if super::revision::is_prefix(name))
            || !branch.sources_current(&before.prepared)
        {
            return Ok(false);
        }
        telemetry.stage("repository_ref_resolution");
        let repository = branch.repository.clone();
        let target = target.clone();
        let allow_private = authorize(&self.options.rules, &repository)?;
        let cache = self.cache.clone();
        let permit = crate::cancellation::until(cancel, self.capacity.transfer(telemetry)).await?;
        let (revision, name, transport) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            resolve(&cache, &repository, &target, allow_private)
        })
        .await??;
        if revision != before.prepared.revision || name != before.prepared.branch {
            return Ok(false);
        }
        crate::cancellation::until(
            cancel,
            self.publish_ref_refresh(branch, before, transport, telemetry),
        )
        .await
    }

    async fn publish_ref_refresh(
        &self,
        branch: &Branch,
        before: &State,
        transport: Option<String>,
        telemetry: &crate::telemetry::Operation,
    ) -> Result<bool> {
        let mut state = {
            let _wait = telemetry.wait("repository_publication");
            branch.state.write().await
        };
        let Some(current) = state.as_ref() else {
            return Ok(false);
        };
        if !current.same_inputs(before)
            || current.policy != self.options.selection.identity
            || !branch.sources_current(&current.prepared)
            || !branch.source().is_dir()
            || !current
                .prepared
                .directories
                .iter()
                .all(|path| branch.source().join(path).is_dir())
        {
            return Ok(false);
        }
        let mut next = (**current).clone();
        next.refreshed = now();
        next.last_use = branch.last_use.load(Ordering::Relaxed);
        next.prepared.transport = transport;
        next.prepared.transfer_bytes = 0;
        branch.persist_header(&next)?;
        *state = Some(Arc::new(next));
        Ok(true)
    }
}
