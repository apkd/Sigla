use super::*;

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
