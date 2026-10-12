use sigla::{discovery::Policy, service::App, workspace::Workspace};
use std::{path::Path, sync::Arc};

#[path = "support/configuration_documents.rs"]
mod configuration_documents;

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn app(root: &Path, cache: &Path) -> Arc<App> {
    Arc::new(App::new(Policy::new(vec![root.into()]).unwrap(), cache.into(), 2).unwrap())
}

#[tokio::test]
async fn configuration_documents_are_delivered_with_and_without_build_projects() {
    for mixed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        for &(path, source) in configuration_documents::FILES {
            write(root.path(), path, source);
        }
        // Existing formats must remain searchable alongside the newly admitted inputs.
        for (path, source) in [
            (
                "configuration.nix",
                "{ enabled = true; } # ExistingMarker\n",
            ),
            ("scripts/task.fish", "echo ExistingMarker\n"),
            ("scripts/task.sh", "printf '%s\\n' ExistingMarker\n"),
            ("config/options.toml", "name = 'ExistingMarker'\n"),
        ] {
            write(root.path(), path, source);
        }
        if mixed {
            write(
                root.path(),
                "tools/Cargo.toml",
                "[package]\nname='coverage'\nversion='0.1.0'\nedition='2024'\n",
            );
            write(
                root.path(),
                "tools/src/lib.rs",
                "pub struct NativeControl;\n",
            );
            write(
                root.path(),
                "tools/settings.lua",
                "return 'ProjectMarker'\n",
            );
        }
        let app = app(root.path(), cache.path());
        let project = root.path().to_str().unwrap();
        let tree = app.browse(project, "").await.unwrap();
        for &(path, source) in configuration_documents::FILES {
            assert!(tree.contains(path.rsplit('/').next().unwrap()), "{tree}");
            let files = app.search(project, &format!("file:{path}")).await.unwrap();
            assert!(files.contains(path), "{path}: {files}");
            let text = app
                .search(project, &format!("text:\"CoverageMarker\" path:{path}"))
                .await
                .unwrap();
            let offset = source.find("CoverageMarker").unwrap();
            let line = source[..offset].bytes().filter(|&b| b == b'\n').count() + 1;
            assert!(text.contains(&format!("{path}:{line}")), "{text}");
            for mode in ["exact", "minified"] {
                let viewed = app.view(project, path, mode).await.unwrap();
                assert!(viewed.contains(source), "{path}: {viewed}");
                let slice = app
                    .view(project, &format!("{path}:{line}"), mode)
                    .await
                    .unwrap();
                assert!(slice.contains("CoverageMarker"), "{slice}");
            }
        }
        let existing = app
            .search(project, "text:\"ExistingMarker\"")
            .await
            .unwrap();
        for name in ["configuration.nix", "task.fish", "task.sh", "options.toml"] {
            assert!(existing.contains(name), "{existing}");
        }
        // Text navigation is not a claim of declarations, references, or calls.
        for query in [
            "coverage_lua",
            "function:coverage_lua",
            "uses:coverage_lua",
            "calls:coverage_lua",
            "function:coverage_python",
        ] {
            assert_eq!(app.search(project, query).await.unwrap(), "No matches.");
        }
        if mixed {
            assert!(
                app.search(project, "type:NativeControl")
                    .await
                    .unwrap()
                    .contains("tools/src/lib.rs")
            );
            let member = app
                .search(project, "text:\"ProjectMarker\" project:coverage")
                .await
                .unwrap();
            assert!(member.contains("tools/settings.lua"), "{member}");
            assert_eq!(member.matches("ProjectMarker").count(), 1, "{member}");
            for query in [
                "file:scripts/plugin.lua project:coverage",
                "text:\"CoverageMarker\" project:coverage",
                "text:\"ProjectMarker\" -project:coverage",
            ] {
                assert_eq!(app.search(project, query).await.unwrap(), "No matches.");
            }
        }
        app.shutdown().await;
    }
}

#[tokio::test]
async fn document_discovery_preserves_directory_and_symlink_exclusions() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "scripts/visible.LUA", "VisibleMarker\n");
    write(
        root.path(),
        ".agents/skills/review/SKILL.md",
        "VisibleMarker\n",
    );
    let excluded = [
        ".git/omitted.lua",
        ".hidden/omitted.py",
        ".mirror/omitted.json",
        "target/omitted.lua",
        "bin/omitted.py",
        "obj/omitted.json",
        "node_modules/omitted.lua",
        "Temp/omitted.jq",
        "Logs/omitted.logql",
        "UserSettings/omitted.patch",
        "Samples~/omitted.lua",
        ".agents/.cache/omitted.lua",
        "assets/omitted.png",
    ];
    for path in excluded {
        write(root.path(), path, "ExcludedMarker\n");
    }
    #[cfg(unix)]
    let outside = {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "omitted.lua", "ExcludedMarker\n");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("omitted.lua"),
            root.path().join("scripts/link.lua"),
        )
        .unwrap();
        outside
    };
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    let visible = app.search(project, "text:\"VisibleMarker\"").await.unwrap();
    assert!(visible.contains("visible.LUA"), "{visible}");
    assert!(
        visible.contains(".agents/skills/review/SKILL.md"),
        "{visible}"
    );
    assert_eq!(
        app.search(project, "text:\"ExcludedMarker\"")
            .await
            .unwrap(),
        "No matches."
    );
    for path in excluded {
        assert_eq!(
            app.search(project, &format!("file:{path}")).await.unwrap(),
            "No matches.",
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        for path in ["linked/omitted.lua", "scripts/link.lua"] {
            assert_eq!(
                app.search(project, &format!("file:{path}")).await.unwrap(),
                "No matches."
            );
        }
        drop(outside);
    }
    app.shutdown().await;
}

#[tokio::test]
async fn new_document_files_and_hidden_directories_refresh_without_restarting() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "configuration.nix", "{}\n");
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    assert_eq!(
        app.search(project, "file:*.lua").await.unwrap(),
        "No matches."
    );
    for (path, first, second) in [
        ("scripts/new.lua", "LuaBefore", "LuaAfter"),
        (".agents/skills/new/SKILL.md", "SkillBefore", "SkillAfter"),
        ("config/new.json", "JsonBefore", "JsonAfter"),
    ] {
        write(root.path(), path, first);
        assert!(
            app.search(project, &format!("text:\"{first}\""))
                .await
                .unwrap()
                .contains(path)
        );
        write(root.path(), path, second);
        assert!(
            app.view(project, path, "exact")
                .await
                .unwrap()
                .contains(second)
        );
        assert_eq!(
            app.search(project, &format!("text:\"{first}\""))
                .await
                .unwrap(),
            "No matches."
        );
        assert!(
            app.search(project, &format!("text:\"{second}\""))
                .await
                .unwrap()
                .contains(path)
        );
        std::fs::remove_file(root.path().join(path)).unwrap();
        assert_eq!(
            app.search(project, &format!("file:{path}")).await.unwrap(),
            "No matches."
        );
        assert_eq!(
            app.search(project, &format!("text:\"{second}\""))
                .await
                .unwrap(),
            "No matches."
        );
    }
    app.shutdown().await;
}

#[tokio::test]
async fn old_discovery_policy_rediscovers_unchanged_documents_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(root.path(), "configuration.nix", "{}\n");
    for &(path, source) in configuration_documents::FILES {
        write(root.path(), path, source);
    }
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let entry = root.path().canonicalize().unwrap();
    let mut workspace = Workspace::open(
        entry.clone(),
        cache.path(),
        policy.clone(),
        &cache.path().join("analysis"),
        None,
        Arc::new(sigla::watch::Monitor::default()),
    )
    .unwrap();
    workspace.refresh().unwrap();
    // Emulate the previous discovery policy without changing any source or directory stamp.
    let old_policy = *blake3::hash(
        &serde_json::to_vec(&(
            10u32,
            &policy.roots,
            policy.unity_platform,
            Option::<()>::None,
        ))
        .unwrap(),
    )
    .as_bytes();
    assert_ne!(policy.identity().unwrap(), old_policy);
    let mut stale = (*workspace.manifest).clone();
    stale.discovery_policy = old_policy;
    let retained = entry.join("configuration.nix");
    stale.files.retain(|_, file| file.path == retained);
    stale.inputs.retain(|input| input.path == retained);
    stale
        .metadata
        .retain(|path, _| !path.starts_with(entry.join(".agents")));
    workspace.store.save_manifest(&stale).unwrap();
    drop(workspace);
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    for &(path, source) in configuration_documents::FILES {
        let found = app
            .search(project, &format!("text:\"CoverageMarker\" path:{path}"))
            .await
            .unwrap();
        assert!(found.contains(path), "{found}");
        assert!(
            app.view(project, path, "exact")
                .await
                .unwrap()
                .contains(source)
        );
    }
    app.shutdown().await;
}
