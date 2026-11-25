use super::*;
use std::{future::Future, task::Poll};

fn fixture(workers: usize) -> (tempfile::TempDir, tempfile::TempDir, App) {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let app = App::new(
        Policy::new(vec![root.path().into()]).unwrap(),
        cache.path().into(),
        workers,
    )
    .unwrap();
    (root, cache, app)
}

#[tokio::test]
async fn setup_deadline_covers_workspace_and_query_admission() {
    use std::time::Duration;

    let (root, _cache, mut app) = fixture(1);
    let source = root.path().join("code.rs");
    std::fs::write(&source, "pub struct Available;").unwrap();
    app.index_wait = Duration::from_millis(30);
    let app = Arc::new(app);
    let path = source.to_str().unwrap();
    app.search(path, "type:Available wait:complete")
        .await
        .unwrap();
    let workspace = app.workspaces.lock().unwrap()[&source].state.clone();
    for lock_workspace in [true, false] {
        let guard = if lock_workspace {
            Some(workspace.lock().await)
        } else {
            None
        };
        let permit = if lock_workspace {
            None
        } else {
            Some(app.workers.acquire().await.unwrap())
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            app.dispatch(path, Request::Search("type:Available".into())),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.structured_content.unwrap()["complete"], false);

        let mut waiting = Box::pin(app.search(path, "type:Available wait:complete"));
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut waiting)
                .await
                .is_err()
        );
        drop(guard);
        drop(permit);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap()
                .unwrap()
                .contains("Available")
        );
    }
    app.shutdown().await;
}

#[tokio::test]
async fn setup_deadline_covers_maintenance_admission() {
    use std::time::Duration;
    let (root, _cache, mut app) = fixture(1);
    std::fs::write(root.path().join("code.rs"), "pub struct Available;").unwrap();
    app.index_wait = Duration::from_millis(30);
    let app = Arc::new(app);
    let maintenance = app.activity.write().await;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        app.dispatch(
            root.path().to_str().unwrap(),
            Request::Search("type:Available".into()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.structured_content.unwrap()["complete"], false);
    let mut waiting = Box::pin(app.search(
        root.path().to_str().unwrap(),
        "type:Available wait:complete",
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(60), &mut waiting)
            .await
            .is_err()
    );
    drop(maintenance);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap()
            .contains("Available")
    );
    app.shutdown().await;
}

#[tokio::test]
async fn setup_deadline_covers_early_navigation_admission() {
    use std::time::Duration;
    let (root, _cache, mut app) = fixture(1);
    std::fs::write(root.path().join("code.rs"), "pub struct Available;").unwrap();
    app.index_wait = Duration::from_millis(30);
    let app = Arc::new(app);
    let project = root.path().to_str().unwrap();
    app.search(project, "type:Available wait:complete")
        .await
        .unwrap();
    let navigation = app
        .navigation_workers
        .clone()
        .acquire_many_owned(app.navigation_workers.available_permits() as u32)
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        app.dispatch(project, Request::Search("file:*.rs".into())),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.structured_content.unwrap()["complete"], false);
    drop(navigation);
    assert!(
        app.search(project, "file:*.rs")
            .await
            .unwrap()
            .contains("code.rs")
    );
    app.shutdown().await;
}

#[tokio::test]
async fn exact_asset_filename_does_not_wait_for_code_or_asset_indexing() {
    use std::time::Duration;
    let (root, _cache, app) = fixture(1);
    let source = root.path().join("code.rs");
    std::fs::write(&source, "pub struct Available;").unwrap();
    std::fs::create_dir(root.path().join("Assets")).unwrap();
    std::fs::create_dir(root.path().join("ProjectSettings")).unwrap();
    std::fs::write(
        root.path().join("ProjectSettings/ProjectVersion.txt"),
        "version",
    )
    .unwrap();
    std::fs::write(root.path().join("Assets/Known.asset"), "unparsed asset").unwrap();
    let app = Arc::new(app);
    let indexing = app.indexing_pause.acquire().await.unwrap();
    let assets = app.assets.pause().await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        app.dispatch(
            source.to_str().unwrap(),
            Request::Search("file:Assets/Known.asset -lang:cs".into()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result
            .structured_content
            .as_ref()
            .is_none_or(|v| v["complete"] != false)
    );
    assert!(result_text(result).unwrap().contains("Assets/Known.asset"));
    assert_eq!(app.assets.stats().entries, 0);
    drop(indexing);
    drop(assets);
    app.shutdown().await;
}

#[test]
fn ownership_lock_rejects_symlinks_and_non_regular_files() {
    use std::os::unix::ffi::OsStrExt;

    for fifo in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let lock = cache.path().join("ownership.lock");
        if fifo {
            let path = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        } else {
            std::os::unix::fs::symlink(outside.path(), &lock).unwrap();
        }
        let policy = Policy::new(vec![root.path().into()]).unwrap();
        assert!(App::new(policy, cache.path().into(), 1).is_err());
        assert!(outside.path().is_file());
    }
}

#[test]
fn workspace_registry_trims_after_active_pins_drain() {
    let (root, _cache, app) = fixture(1);
    let pins: Vec<_> = (0..WORKSPACE_CACHE_ENTRIES + 1)
        .map(|_| Arc::new(tokio::sync::Mutex::new(None)))
        .collect();
    {
        let mut registry = app.workspaces.lock().unwrap();
        for (index, state) in pins.iter().enumerate() {
            registry.insert(
                root.path().join(format!("workspace-{index}")),
                WorkspaceSlot {
                    state: state.clone(),
                    preparing: None,
                    used: Instant::now(),
                },
            );
        }
    }

    app.trim_idle();
    assert_eq!(
        app.workspaces.lock().unwrap().len(),
        WORKSPACE_CACHE_ENTRIES + 1
    );
    drop(pins);
    app.trim_idle();
    assert_eq!(
        app.workspaces.lock().unwrap().len(),
        WORKSPACE_CACHE_ENTRIES
    );
}

#[tokio::test]
async fn request_completion_trims_idle_workspace_registry() {
    let (root, _cache, app) = fixture(1);
    let app = Arc::new(app);
    {
        let mut registry = app.workspaces.lock().unwrap();
        for index in 0..WORKSPACE_CACHE_ENTRIES + 1 {
            registry.insert(
                root.path().join(format!("workspace-{index}")),
                WorkspaceSlot {
                    state: Arc::new(tokio::sync::Mutex::new(None)),
                    preparing: None,
                    used: Instant::now(),
                },
            );
        }
    }

    assert!(
        app.dispatch("", Request::Search("type:Anything".into()))
            .await
            .is_err()
    );
    assert!(app.workspaces.lock().unwrap().len() <= WORKSPACE_CACHE_ENTRIES);
}

#[tokio::test]
async fn active_query_pins_workspace_until_cancelled_worker_exits() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(1);
    let source = root.path().join("code.rs");
    std::fs::write(&source, "pub struct Available;").unwrap();
    let app = Arc::new(app);
    app.search(source.to_str().unwrap(), "type:Available wait:complete")
        .await
        .unwrap();
    app.query_snapshot_started.notified().await;
    let pause = app.query_snapshot_pause.lock().await;
    let query = tokio::spawn({
        let app = app.clone();
        let source = source.clone();
        async move {
            app.search(source.to_str().unwrap(), "type:Available wait:complete")
                .await
        }
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        app.query_snapshot_started.notified(),
    )
    .await
    .unwrap();
    {
        let mut registry = app.workspaces.lock().unwrap();
        assert!(registry[&source].state.try_lock().is_ok());
        for index in 0..WORKSPACE_CACHE_ENTRIES {
            registry.insert(
                root.path().join(format!("idle-{index}")),
                WorkspaceSlot {
                    state: Arc::new(tokio::sync::Mutex::new(None)),
                    preparing: None,
                    used: Instant::now(),
                },
            );
        }
    }
    app.trim_idle();
    assert!(app.workspaces.lock().unwrap().contains_key(&source));
    query.abort();
    assert!(query.await.unwrap_err().is_cancelled());
    assert_eq!(app.workspace_cache_usage().active_entries, 1);
    drop(pause);
    let drained = tokio::time::timeout(Duration::from_secs(5), app.activity.write())
        .await
        .unwrap();
    drop(drained);
    app.workspaces.lock().unwrap().insert(
        root.path().join("another-idle"),
        WorkspaceSlot {
            state: Arc::new(tokio::sync::Mutex::new(None)),
            preparing: None,
            used: Instant::now(),
        },
    );
    app.trim_idle();
    assert!(!app.workspaces.lock().unwrap().contains_key(&source));
    app.shutdown().await;
}

#[test]
fn workspace_cache_usage_reports_active_pins() {
    let (root, _cache, app) = fixture(1);
    let state = Arc::new(tokio::sync::Mutex::new(None));
    app.workspaces.lock().unwrap().insert(
        root.path().into(),
        WorkspaceSlot {
            state: state.clone(),
            preparing: None,
            used: Instant::now(),
        },
    );

    let usage = app.workspace_cache_usage();
    assert_eq!(usage.entries, 1);
    assert_eq!(usage.active_entries, 1);
    assert_eq!(usage.retained_bytes, 0);
    assert_eq!(usage.pinned_bytes, 0);
    drop(state);
    assert_eq!(app.workspace_cache_usage().active_entries, 0);
}

#[tokio::test]
async fn preparation_lifecycle_bounds_plans_waiting_for_indexing() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(1);
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(root.path().join("first.rs"), "pub struct First;").unwrap();
    std::fs::write(nested.join("second.rs"), "pub struct Second;").unwrap();
    let app = Arc::new(app);
    let indexing_gate = app.indexing_pause.clone().acquire_owned().await.unwrap();
    let first = {
        let app = app.clone();
        let path = root.path().to_string_lossy().into_owned();
        tokio::spawn(async move { app.search(&path, "type:First").await })
    };
    tokio::time::timeout(Duration::from_secs(10), app.indexing_started.notified())
        .await
        .expect("first plan did not reach indexing");
    assert_eq!(app.preparation_lifecycle.available_permits(), 0);

    let second = {
        let app = app.clone();
        let path = nested.to_string_lossy().into_owned();
        tokio::spawn(async move { app.search(&path, "type:Second").await })
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        app.preparation_admission_started.notified(),
    )
    .await
    .expect("second preparation did not wait for lifecycle capacity");
    {
        let registry = app.workspaces.lock().unwrap();
        let slot = registry.get(&nested).unwrap();
        assert!(
            slot.preparing.as_ref().unwrap().discovering(),
            "queued preparation must remain before discovery"
        );
        assert!(slot.state.try_lock().unwrap().is_none());
    }

    drop(indexing_gate);
    let first = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.contains("First"), "{first}");
    assert!(second.contains("Second"), "{second}");
    assert_eq!(app.preparation_lifecycle.available_permits(), 1);
}

#[tokio::test]
async fn waiting_for_a_worker_releases_workspace_and_branch_locks() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let request = app.admit_query(workspace.clone(), Some(&branch), &operation);
    tokio::pin!(request);

    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    let workspace_guard = workspace
        .try_lock()
        .expect("workspace must remain available");
    let publication = tokio::time::timeout(std::time::Duration::from_secs(2), branch.publish())
        .await
        .expect("branch must remain available");
    drop(publication);
    drop(held);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(workspace_guard);

    let (permit, workspace_guard, analysis) = request.await.unwrap();
    assert!(analysis.is_some());
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn waiting_for_workspace_or_branch_does_not_reserve_a_worker() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let operation = crate::telemetry::Operation::default();

    let held = workspace.lock().await;
    let mut request = Box::pin(app.admit_query(workspace.clone(), Some(&branch), &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(held);
    drop(request);

    let publication = branch.publish().await;
    let request = app.admit_query(workspace.clone(), Some(&branch), &operation);
    tokio::pin!(request);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(publication);
    let (permit, workspace_guard, analysis) = request.await.unwrap();
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn branch_publication_after_worker_wait_returns_the_worker() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut request = Box::pin(app.admit_query(workspace.clone(), Some(&branch), &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());

    let publication = branch.publish().await;
    drop(held);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(publication);
    let (permit, guard, analysis) = request.await.unwrap();
    drop((permit, guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn cancelling_admission_releases_queue_and_locks() {
    let (_root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut request = Box::pin(app.admit_query(workspace.clone(), None, &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    drop(request);
    assert!(workspace.try_lock().is_ok());
    drop(held);
    assert_eq!(app.workers.available_permits(), 1);
    let (permit, workspace_guard, analysis) =
        app.admit_query(workspace, None, &operation).await.unwrap();
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn query_rechecks_workspace_generation_after_admission_wait() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(1);
    std::fs::write(root.path().join("lib.rs"), "pub struct Available;").unwrap();
    let app = Arc::new(app);
    let path = root.path().to_str().unwrap();
    assert!(
        app.search(path, "type:Available")
            .await
            .unwrap()
            .contains("Available")
    );
    let workspace = app
        .workspaces
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .state
        .clone();
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let request = app.request(
        path,
        Request::Search("type:Available".into()),
        crate::telemetry::Operation::default(),
        tokio::time::Instant::now() + app.index_wait,
    );
    tokio::pin!(request);
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = app.admission_started.notified() => {},
            result = &mut request => panic!("query finished before admission wait: {result:?}"),
        }
    })
    .await
    .unwrap();
    {
        let mut state = workspace.lock().await;
        let manifest = &mut state.as_mut().unwrap().manifest;
        Arc::make_mut(manifest).environment[0] ^= 1;
    }
    drop(held);
    let error = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<crate::diagnostics::RefreshRequired>())
    );
    app.shutdown().await;
}

#[tokio::test]
async fn queued_history_leaves_a_worker_for_interactive_queries() {
    let (root, _cache, app) = fixture(2);
    let app = Arc::new(app);
    let held = app.history_workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut history =
        Box::pin(app.history_request(root.path().to_str().unwrap(), "log", operation.clone()));
    let status = std::future::poll_fn(|cx| Poll::Ready(history.as_mut().poll(cx))).await;
    assert!(status.is_pending());

    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let (permit, guard, branch) = app.admit_query(workspace, None, &operation).await.unwrap();
    assert_eq!(app.workers.available_permits(), 1);
    drop((permit, guard, branch));
    drop(history);
    drop(held);
    assert_eq!(app.workers.available_permits(), 2);
}

#[tokio::test]
async fn cancelled_running_history_keeps_its_permits_until_work_exits() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(2);
    let app = Arc::new(app);
    let pause = app.history_pause.clone().acquire_owned().await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let request = {
        let app = app.clone();
        let cancel = cancel.clone();
        let path = root.path().to_string_lossy().into_owned();
        tokio::spawn(async move {
            app.request_cancellable(&path, Request::History("log".into()), &cancel, None)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), app.history_started.notified())
        .await
        .unwrap();
    assert_eq!(app.history_workers.available_permits(), 0);
    assert_eq!(app.workers.available_permits(), 1);

    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(app.history_workers.available_permits(), 0);
    assert_eq!(app.workers.available_permits(), 1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let operation = crate::telemetry::Operation::default();
    let (permit, guard, analysis) = app.admit_query(workspace, None, &operation).await.unwrap();
    drop((permit, guard, analysis));

    drop(pause);
    let history = tokio::time::timeout(
        Duration::from_secs(5),
        app.history_workers.clone().acquire_owned(),
    )
    .await
    .unwrap()
    .unwrap();
    let workers = tokio::time::timeout(
        Duration::from_secs(5),
        app.workers.clone().acquire_many_owned(2),
    )
    .await
    .unwrap()
    .unwrap();
    drop((history, workers));
    app.shutdown().await;
}

pub(super) async fn remote_preparation_fixture() -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<crate::repository::manager::Branch>,
    PathBuf,
) {
    use crate::repository::{
        Repository, Rule,
        manager::State,
        materialize::{Prepared, Target},
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let cache = tempfile::tempdir().unwrap();
    crate::cache::reset::prepare(cache.path()).unwrap();
    let repository = Repository::parse("https://github.com/fixture/repo#refs/heads/main")
        .unwrap()
        .unwrap();
    let key = blake3::hash(&postcard::to_allocvec(&(&repository.identity, "main")).unwrap())
        .to_hex()
        .to_string();
    let owner = cache.path().join("repositories").join(key);
    let source = owner.join("source");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::write(
        source.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(source.join("src/lib.rs"), "pub struct Before;").unwrap();
    let selection = crate::repository::selection::Selection::new(&[], &[]).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let files = std::collections::BTreeMap::from([
        ("Cargo.toml".into(), "b".into()),
        ("src/lib.rs".into(), "c".into()),
    ]);
    let state = State {
        repository: repository.identity.clone(),
        transport: repository.transport.clone(),
        branch: "main".into(),
        target: Target::Branch("main".into()),
        last_use: now,
        refreshed: now,
        policy: selection.identity.clone(),
        repair: false,
        indexed_revision: None,
        additional: Default::default(),
        prepared: Prepared {
            branch: Some("main".into()),
            revision: "a".repeat(40),
            selected: files.clone(),
            tracked: files,
            directories: ["src".into()].into(),
            deferred_assets: Default::default(),
            omitted: Default::default(),
            transfer_bytes: 0,
            unavailable: Default::default(),
            transport: None,
            resolved_target: None,
        },
    };
    crate::repository::state_store::write(&owner, &state).unwrap();
    let app = Arc::new(
        App::remote(
            Policy::new(vec![source.clone()]).unwrap(),
            cache.path().into(),
            1,
            crate::config::RemoteOptions {
                rules: vec![Rule::parse_private("https://github.com/fixture/*").unwrap()],
                refresh_interval: Some(Duration::from_secs(3600)),
                repo_ttl: Duration::from_secs(86400),
                branch_ttl: Duration::from_secs(86400),
                unity_versions: vec![],
                selection,
            },
        )
        .unwrap(),
    );
    let branch = app
        .remote
        .as_ref()
        .unwrap()
        .resolve(repository)
        .await
        .unwrap();
    (cache, app, branch, source)
}

#[tokio::test]
async fn exact_remote_asset_filename_uses_deferred_inventory_before_discovery() {
    use std::time::Duration;
    let (_cache, app, branch, source) = remote_preparation_fixture().await;
    {
        let mut publication = branch.publish().await;
        let mut state = (**publication.as_ref().unwrap()).clone();
        for path in ["Assets/Known.prefab", "ProjectSettings/ProjectVersion.txt"] {
            state.prepared.tracked.insert(path.into(), "object".into());
        }
        state
            .prepared
            .deferred_assets
            .insert("Assets/Known.prefab".into());
        state.prepared.directories.insert("Assets".into());
        *publication = Some(Arc::new(state));
    }
    assert!(!source.join("Assets/Known.prefab").exists());
    let discovery = app.discovery_pause.clone().acquire_owned().await.unwrap();
    let indexing = app.indexing_pause.clone().acquire_owned().await.unwrap();
    let assets = app.assets.pause().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        app.dispatch(
            "https://github.com/fixture/repo#refs/heads/main",
            Request::Search("file:Assets/Known.prefab".into()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        response
            .structured_content
            .as_ref()
            .is_none_or(|value| value["complete"] != false)
    );
    assert!(
        result_text(response)
            .unwrap()
            .contains("Assets/Known.prefab")
    );
    let filtered = tokio::time::timeout(
        Duration::from_secs(2),
        app.dispatch(
            "https://github.com/fixture/repo#refs/heads/main",
            Request::Search("file:Assets/Known.prefab -lang:cs".into()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        filtered.structured_content.as_ref().unwrap()["complete"],
        false
    );
    assert!(result_text(filtered).unwrap().contains("wait:complete"));
    assert!(app.workspaces.lock().unwrap().is_empty());
    assert_eq!(app.assets.stats().entries, 0);
    drop((discovery, indexing, assets));
    app.shutdown().await;
}

#[tokio::test]
async fn queued_remote_discovery_releases_locks_and_does_not_reserve_a_worker() {
    use std::time::Duration;

    let (_cache, app, branch, source) = remote_preparation_fixture().await;
    let held = app
        .preparation_workers
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let pause = app.discovery_pause.clone().acquire_owned().await.unwrap();
    let request = {
        let app = app.clone();
        tokio::spawn(async move {
            app.search(
                "https://github.com/fixture/repo#refs/heads/main",
                "type:Before",
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), app.admission_started.notified())
        .await
        .expect("discovery did not queue");
    let workspace = app
        .workspaces
        .lock()
        .unwrap()
        .get(&source)
        .unwrap()
        .state
        .clone();
    let workspace_guard = workspace
        .try_lock()
        .expect("queued discovery retained workspace");
    let publication = tokio::time::timeout(Duration::from_secs(2), branch.publish())
        .await
        .expect("queued discovery retained branch");
    drop(held);
    let available = tokio::time::timeout(
        Duration::from_secs(2),
        app.preparation_workers.clone().acquire_owned(),
    )
    .await
    .expect("discovery reserved a worker while locks were held")
    .unwrap();
    drop(available);
    drop(publication);
    drop(workspace_guard);
    tokio::time::timeout(Duration::from_secs(5), app.discovery_started.notified())
        .await
        .expect("discovery did not start");
    let mut publication = Box::pin(branch.publish());
    std::future::poll_fn(|cx| {
        assert!(
            publication.as_mut().poll(cx).is_pending(),
            "active discovery must protect source files"
        );
        Poll::Ready(())
    })
    .await;
    drop(publication);
    assert!(
        workspace.try_lock().is_err(),
        "active discovery must protect the workspace"
    );
    assert_eq!(app.preparation_workers.available_permits(), 0);
    drop(pause);
    let result = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(result.contains("Before"), "{result}");
    app.shutdown().await;
}

#[tokio::test]
async fn queued_remote_index_rebuilds_plan_after_branch_publication() {
    use std::time::Duration;

    let (_cache, app, branch, source) = remote_preparation_fixture().await;
    let held = app.indexing_workers.clone().acquire_owned().await.unwrap();
    let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
    let request = {
        let app = app.clone();
        tokio::spawn(async move {
            app.search(
                "https://github.com/fixture/repo#refs/heads/main",
                "type:After",
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), app.admission_started.notified())
        .await
        .expect("indexing did not queue");
    let workspace = app
        .workspaces
        .lock()
        .unwrap()
        .get(&source)
        .unwrap()
        .state
        .clone();
    let workspace_guard = workspace
        .try_lock()
        .expect("queued indexing retained workspace");
    let mut publication = tokio::time::timeout(Duration::from_secs(2), branch.publish())
        .await
        .expect("queued indexing retained branch");
    std::fs::write(source.join("src/lib.rs"), "pub struct After;").unwrap();
    let prepared = &mut publication.as_mut().unwrap().prepared;
    prepared
        .selected
        .insert("src/lib.rs".into(), "changed".into());
    prepared
        .tracked
        .insert("src/lib.rs".into(), "changed".into());
    branch
        .generation
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    drop(held);
    let available = tokio::time::timeout(
        Duration::from_secs(2),
        app.indexing_workers.clone().acquire_owned(),
    )
    .await
    .expect("indexing reserved a worker while locks were held")
    .unwrap();
    drop(available);
    drop(publication);
    drop(workspace_guard);

    tokio::time::timeout(Duration::from_secs(5), async {
        while app.indexing_workers.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rebuilt plan did not start indexing");
    let mut publication = Box::pin(branch.publish());
    std::future::poll_fn(|cx| {
        assert!(
            publication.as_mut().poll(cx).is_pending(),
            "active indexing must protect source files"
        );
        Poll::Ready(())
    })
    .await;
    drop(publication);
    assert!(
        workspace.try_lock().is_err(),
        "active indexing must protect the workspace"
    );
    drop(pause);
    let result = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(result.contains("After"), "{result}");
    let old = app
        .search(
            "https://github.com/fixture/repo#refs/heads/main",
            "type:Before",
        )
        .await
        .unwrap();
    assert!(!old.contains("Before"), "stale plan was indexed: {old}");
    app.shutdown().await;
}
