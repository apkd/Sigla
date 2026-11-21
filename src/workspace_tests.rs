use super::*;

impl Workspace {
    pub(crate) fn benchmark_discovery(&mut self, discovery: discovery::Discovery) -> Result<()> {
        self.apply(Preparation {
            discovery,
            fence: 0,
            started: std::time::Instant::now(),
            validation: Default::default(),
            discovery_time: Default::default(),
        })
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn policy(root: &Path, cache: &Path) -> Policy {
    let source = root.join("source");
    let mut policy = Policy::new(vec![root.into()]).unwrap();
    policy.remote = Some(discovery::RemoteContext {
        workspace: source.clone(),
        writable: cache.join("generated"),
        shared: cache.into(),
        repositories: vec![],
        selection_identity: String::new(),
        tracked: Default::default(),
    });
    policy.materialized(source, [1; 32]);
    policy
}

fn open(root: &Path, cache: &Path, policy: Policy) -> Workspace {
    Workspace::open(
        root.join("source"),
        cache,
        policy,
        &cache.join("analysis"),
        None,
        Default::default(),
    )
    .unwrap()
}

fn has_symbol(workspace: &Workspace, symbol: &str) -> bool {
    !workspace
        .store
        .candidates(&workspace.store.read().unwrap(), symbol, false, false)
        .unwrap()
        .is_empty()
}

#[test]
fn idle_planning_releases_parser_admission_without_losing_the_source() {
    use std::time::Duration;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "source/lib.rs", "pub struct Planned;");
    let mut workspace = open(
        root.path(),
        cache.path(),
        Policy::new(vec![root.path().into()]).unwrap(),
    );
    let prepared = workspace.prepare().unwrap().unwrap();
    let demand = Arc::new(crate::demand::Demand::new(Duration::ZERO));
    let interest = demand.interest();
    let work = demand.work();
    let reservation = crate::memory::admit_file(u64::MAX, false, &()).unwrap();
    let (started, ready) = std::sync::mpsc::channel();
    let (send, receive) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        started.send(()).unwrap();
        let result = workspace.plan_cancellable(prepared, &work);
        send.send((workspace, result.map(|_| ()))).unwrap();
    });
    ready.recv().unwrap();
    // No plan can finish while the parser budget is held.
    assert!(receive.recv_timeout(Duration::from_millis(100)).is_err());
    drop(interest);
    let result = receive.recv_timeout(Duration::from_secs(10));
    drop(reservation);
    thread.join().unwrap();
    let (mut workspace, result) = result.unwrap();
    assert!(result.unwrap_err().is::<crate::demand::Paused>());
    workspace.refresh().unwrap();
    assert!(has_symbol(&workspace, "Planned"));
}

#[test]
fn idle_indexing_never_publishes_a_partial_manifest() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "source/code.rs", "pub struct Before;");
    let mut workspace = open(
        root.path(),
        cache.path(),
        Policy::new(vec![root.path().into()]).unwrap(),
    );
    workspace.refresh().unwrap();
    write(root.path(), "source/code.rs", "pub struct Ready;");
    let preparation = workspace.prepare().unwrap().unwrap();
    let plan = workspace.plan(preparation).unwrap();
    let demand = Arc::new(crate::demand::Demand::new(Duration::ZERO));
    let interest = demand.interest();
    let work = demand.work();
    let store = workspace.store.clone();
    let original = workspace.manifest.clone();
    let reservation = crate::memory::admit_file(u64::MAX, false, &()).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker_plan = plan.clone();
    let thread = std::thread::spawn(move || {
        let result = workspace.apply_plan_cancellable(worker_plan, &work);
        send.send((workspace, result)).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.manifest_current().unwrap() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    drop(interest);
    let result = receive.recv_timeout(Duration::from_secs(10));
    drop(reservation);
    thread.join().unwrap();
    let (mut workspace, result) = result.unwrap();
    assert!(result.unwrap_err().is::<crate::demand::Paused>());
    assert!(Arc::ptr_eq(&workspace.manifest, &original));
    assert!(!workspace.published_current(&original).unwrap());
    let _interest = demand.interest();
    workspace
        .apply_plan_cancellable(plan, &demand.work())
        .unwrap();
    assert!(workspace.published_current(&workspace.manifest).unwrap());
    assert!(has_symbol(&workspace, "Ready"));
}

#[test]
fn scheduling_keeps_cold_work_parallel_and_bounds_warm_attachment_partitions() {
    let project = Project {
        identity: "fixture".into(),
        origin: None,
        name: "fixture".into(),
        defines: vec![],
        references: vec![],
        assemblies: vec![],
        edition: "2021".into(),
        compiler_options: Default::default(),
        source_roots: vec![],
    };
    let files: BTreeMap<_, _> = (0..128)
        .map(|index| {
            let path = format!("{index}.rs");
            (
                path.clone(),
                FileEntry {
                    path: path.clone().into(),
                    display: path,
                    stamp: Stamp {
                        size: 1024 * 1024,
                        revision: Revision::Immutable([index; 32]),
                    },
                    language: Language::Rust,
                    memberships: vec![Membership {
                        project: 0,
                        module: String::new(),
                    }],
                    modules: vec![],
                    metadata: false,
                },
            )
        })
        .collect();
    let inputs: Vec<_> = files
        .values()
        .map(|file| SourceInput {
            path: file.path.clone(),
            project: 0,
            module: String::new(),
            language: file.language,
            metadata: false,
        })
        .collect();
    let jobs: Vec<_> = files
        .iter()
        .zip(&inputs)
        .map(|((key, file), input)| (key.clone(), (file.stamp.clone(), input, &project)))
        .collect();
    let workers = index_workers();
    let batches = source_job_batches(&jobs, workers);
    // Large inputs must keep every available worker busy even below the count
    // limit. The same scheduling bounds cache-hit commits by worker count.
    assert_eq!(batches.len(), workers);
    assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), jobs.len());
    let native = native_batches(&files);
    assert_eq!(native.len(), workers);
    assert_eq!(native.iter().map(|b| b.len()).sum::<usize>(), files.len());
    assert!(batches.iter().all(|batch| batch.len() <= INDEX_BATCH_FILES));
}

#[test]
fn mounted_dependencies_survive_workspace_restart_and_missing_profiles_fall_back() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "source/Local.cs", "class Local {}");
    let conditional = "#if EDITOR\nclass Generated { string text = @\"\n#if GENERATED\nclass Output {}\n\"; }\n#endif\n";
    let files = [
        ("Dependency.cs", "class Dependency {}"),
        ("Conditional.cs", conditional),
    ];
    let archived = crate::metadata_archive::tests::cached_sources(
        cache.path(),
        &files,
        vec![vec!["EDITOR".into()]],
    );
    let policy = policy(root.path(), cache.path());
    let mut workspace = open(root.path(), cache.path(), policy.clone());
    let prepare = || Preparation {
        discovery: discovery::Discovery {
            leases: crate::cache::views::LeaseSet::new(cache.path()).unwrap(),
            root: root.path().join("source"),
            projects: vec![Project {
                identity: "fixture".into(),
                origin: None,
                name: "fixture".into(),
                defines: vec![],
                references: vec![],
                assemblies: vec![],
                edition: String::new(),
                compiler_options: Default::default(),
                source_roots: vec![],
            }],
            sources: [
                root.path().join("source/Local.cs"),
                archived.join("Dependency.cs"),
                archived.join("Conditional.cs"),
            ]
            .into_iter()
            .map(|path| SourceInput {
                path,
                project: 0,
                module: String::new(),
                language: Language::CSharp,
                metadata: false,
            })
            .collect(),
            metadata: BTreeSet::new(),
            dependencies: BTreeSet::from([archived.clone()]),
            diagnostics: vec![],
            dependency_retry: None,
        },
        fence: 0,
        started: std::time::Instant::now(),
        validation: Default::default(),
        discovery_time: Default::default(),
    };
    workspace.apply(prepare()).unwrap();
    assert!(has_symbol(&workspace, "Local") && has_symbol(&workspace, "Dependency"));
    assert_eq!(workspace.manifest.mounts.len(), 1);
    assert_eq!(workspace.manifest.mounts[0].files.len(), 1);
    assert!(
        workspace
            .manifest
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("Conditional.cs"))
    );
    let index = workspace.manifest.mounts[0].path.clone();
    let snapshot = workspace.store.snapshot().unwrap();
    drop(workspace);
    let mut restored = open(root.path(), cache.path(), policy.clone());
    assert!(has_symbol(&restored, "Dependency"));
    write(root.path(), "source/Local.cs", "class Changed {}");
    restored.apply(prepare()).unwrap();
    assert!(has_symbol(&restored, "Changed") && has_symbol(&restored, "Dependency"));
    drop(restored);
    std::fs::remove_file(&index).unwrap();
    let missing = open(root.path(), cache.path(), policy.clone());
    assert!(missing.manifest.files.is_empty());
    assert!(!missing.store.manifest_current().unwrap());
    assert!(
        !snapshot
            .candidates(&snapshot.query_read().unwrap(), "Dependency", false, false)
            .unwrap()
            .is_empty()
    );
    drop(missing);
    assert_eq!(
        crate::metadata_archive::tests::cached_sources(
            cache.path(),
            &files,
            vec![vec!["EDITOR".into()]]
        ),
        archived
    );
    let mut repaired = open(root.path(), cache.path(), policy);
    repaired.apply(prepare()).unwrap();
    assert!(has_symbol(&repaired, "Dependency"));
}

#[test]
fn assets_preserve_code_across_restart_and_external_inputs_stay_live() {
    for polling in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "source/Cargo.toml",
            "[package]\nname='main'\nversion='0.1.0'\n[dependencies]\ndep={path='../dependency'}\n",
        );
        write(root.path(), "source/src/lib.rs", "pub struct Main;");
        write(
            root.path(),
            "dependency/Cargo.toml",
            "[package]\nname='dep'\nversion='0.1.0'\n",
        );
        write(root.path(), "dependency/src/lib.rs", "pub struct Before;");
        let policy = policy(root.path(), cache.path());
        let mut workspace = open(root.path(), cache.path(), policy.clone());
        workspace.refresh().unwrap();
        if polling {
            // An unavailable watch switches the monitor to metadata polling.
            workspace.monitor.register(&root.path().join("absent"));
        }
        write(root.path(), "source/src/Scene.asset", "asset");
        assert!(workspace.prepare().unwrap().is_none());
        assert!(has_symbol(&workspace, "Main"));
        drop(workspace);

        // No running workspace has observed this publication before restart.
        write(root.path(), "source/Assets/New/Scene.prefab", "asset");
        let mut workspace = open(root.path(), cache.path(), policy);
        if polling {
            workspace.monitor.register(&root.path().join("absent"));
        }
        assert!(workspace.prepare().unwrap().is_none());
        assert!(has_symbol(&workspace, "Main"));

        write(root.path(), "source/Assets/Other.asset", "asset");
        write(root.path(), "dependency/src/lib.rs", "pub struct After;");
        workspace.refresh().unwrap();
        assert!(has_symbol(&workspace, "After"));
        assert!(!has_symbol(&workspace, "Before"));

        write(
            root.path(),
            "dependency/src/bin/new/main.rs",
            "pub struct NewTarget;",
        );
        workspace.refresh().unwrap();
        assert!(has_symbol(&workspace, "NewTarget"));
    }
}

#[test]
fn managed_input_changes_detect_additions_moves_deletions_and_edits() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "source/lib.rs", "pub struct Original;");
    let mut policy = policy(root.path(), cache.path());
    let mut workspace = open(root.path(), cache.path(), policy.clone());
    workspace.refresh().unwrap();
    let source = root.path().join("source");
    for revision in 2..=5 {
        match revision {
            2 => write(&source, "new.rs", "pub struct Added;"),
            3 => {
                std::fs::create_dir(source.join("nested")).unwrap();
                std::fs::rename(source.join("new.rs"), source.join("nested/moved.rs")).unwrap();
            }
            4 => std::fs::remove_file(source.join("lib.rs")).unwrap(),
            5 => write(&source, "nested/moved.rs", "pub struct Edited;"),
            _ => unreachable!(),
        }
        write(&source, &format!("Scene{revision}.asset"), "asset");
        policy.materialized(source.clone(), [revision; 32]);
        if revision == 3 {
            drop(workspace);
            workspace = open(root.path(), cache.path(), policy.clone());
        } else {
            workspace.update_policy(policy.clone());
        }
        workspace.refresh().unwrap();
        assert_eq!(has_symbol(&workspace, "Original"), revision < 4);
        assert_eq!(has_symbol(&workspace, "Added"), revision < 5);
        if revision >= 3 {
            assert!(
                workspace
                    .manifest
                    .files
                    .values()
                    .any(|f| f.path == source.join("nested/moved.rs"))
            );
            assert!(
                !workspace
                    .manifest
                    .files
                    .values()
                    .any(|f| f.path == source.join("new.rs"))
            );
        }
    }
    assert!(has_symbol(&workspace, "Edited"));
}

#[test]
fn remote_rust_requires_explicit_sources_even_with_asset_extensions() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "source/Cargo.toml",
        "[package]\nname='main'\nversion='0.1.0'\n[lib]\npath='nested/Root.asset'\n",
    );
    let mut policy = policy(root.path(), cache.path());
    policy.remote.as_mut().unwrap().tracked = Arc::new(
        ["Cargo.toml", "nested/Root.asset", "Module.asset"]
            .map(str::to_owned)
            .into_iter()
            .collect(),
    );
    let mut workspace = open(root.path(), cache.path(), policy);
    for (path, contents) in [
        (
            "nested/Root.asset",
            "#[path=\"../Module.asset\"] mod child; pub struct Root;",
        ),
        ("Module.asset", "pub struct Child;"),
    ] {
        let error = workspace.refresh().unwrap_err();
        assert!(
            error
                .downcast_ref::<discovery::RequiredInputs>()
                .unwrap()
                .0
                .contains(&path.to_owned())
        );
        write(&root.path().join("source"), path, contents);
    }
    workspace.refresh().unwrap();
    assert!(has_symbol(&workspace, "Root"));
    assert!(has_symbol(&workspace, "Child"));
}

#[test]
fn msbuild_asset_conditions_keep_directory_validation() {
    for restart in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "source/Project.csproj",
            r#"<Project><ItemGroup>
                <Compile Include="Always.cs" />
                <Compile Include="Conditional.cs" Condition="$([System.IO.File]::Exists('Flag.asset'))" />
            </ItemGroup></Project>"#,
        );
        write(root.path(), "source/Always.cs", "class Always {}");
        write(root.path(), "source/Conditional.cs", "class Conditional {}");
        let mut policy = policy(root.path(), cache.path());
        policy.remote.as_mut().unwrap().tracked = Arc::new(
            [
                "Project.csproj",
                "Always.cs",
                "Conditional.cs",
                "Flag.asset",
            ]
            .map(str::to_owned)
            .into_iter()
            .collect(),
        );
        let mut workspace = open(root.path(), cache.path(), policy.clone());
        workspace.refresh().unwrap();
        assert!(has_symbol(&workspace, "Always"));
        assert!(
            !has_symbol(&workspace, "Conditional"),
            "{:?}",
            workspace.manifest.diagnostics
        );
        write(root.path(), "source/Flag.asset", "asset");
        if restart {
            drop(workspace);
            workspace = open(root.path(), cache.path(), policy);
        }
        workspace.refresh().unwrap();
        assert!(has_symbol(&workspace, "Conditional"));
    }
}
