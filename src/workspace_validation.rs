//! Explicit opt-in replay of a deployed input manifest. The deployed LMDB is
//! opened read-only; all extraction and indexing happen in a temporary cache.
use super::*;

#[test]
#[ignore = "requires SIGLA_VALIDATION_INDEX pointing to the Highland-Keep deployed index"]
fn highland_keep_full_context() {
    let index = PathBuf::from(
        std::env::var_os("SIGLA_VALIDATION_INDEX").expect("set SIGLA_VALIDATION_INDEX"),
    );
    let manifest: Manifest = {
        let env = unsafe {
            heed::EnvOpenOptions::new()
                .max_dbs(16)
                .flags(heed::EnvFlags::READ_ONLY)
                .open(&index)
        }
        .unwrap();
        let tx = env.read_txn().unwrap();
        let metadata = env
            .open_database::<heed::types::Str, heed::types::Bytes>(&tx, Some("metadata"))
            .unwrap()
            .unwrap();
        assert_eq!(
            metadata.get(&tx, "manifest_revision").unwrap(),
            Some(b"6".as_slice()),
            "Unsupported snapshot schema"
        );
        postcard::from_bytes(metadata.get(&tx, "manifest").unwrap().unwrap()).unwrap()
    };
    let cache = tempfile::tempdir().unwrap();
    let root = manifest.root.clone();
    let mut roots = vec![root.clone()];
    roots.extend(
        manifest
            .inputs
            .iter()
            .filter_map(|i| i.path.parent().map(Path::to_path_buf)),
    );
    roots.extend(
        manifest
            .projects
            .iter()
            .flat_map(|p| &p.assemblies)
            .filter_map(|a| a.path.parent().map(Path::to_path_buf)),
    );
    roots.sort();
    roots.dedup();
    roots.retain(|p| p.is_dir());
    let mut workspace = Workspace::open(
        root.clone(),
        cache.path(),
        Policy::new(roots).unwrap(),
        Store::open(&cache.path().join("assemblies")).unwrap(),
        Default::default(),
    )
    .unwrap();
    workspace
        .apply(Preparation {
            discovery: discovery::Discovery {
                root: root.clone(),
                projects: manifest.projects,
                sources: manifest.inputs,
                metadata: manifest.metadata.into_keys().collect(),
                dependencies: manifest.dependencies,
                diagnostics: manifest.diagnostics,
            },
            fence: 0,
            started: std::time::Instant::now(),
            validation: std::time::Duration::ZERO,
            discovery_time: std::time::Duration::ZERO,
        })
        .unwrap();
    eprintln!(
        "Rebuilt {} files across {} compilation contexts",
        workspace.manifest.files.len(),
        workspace.manifest.projects.len()
    );
    let source_tx = workspace.store.read().unwrap();
    let assembly_tx = workspace.assemblies.read().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let view = crate::csharp::catalog::View {
        source: &workspace.store,
        assemblies: &workspace.assemblies,
        source_tx: &source_tx,
        assembly_tx: &assembly_tx,
        manifest: &workspace.manifest,
        cancel: &cancel,
    };
    let mut count = 0;
    for (key, file) in &workspace.manifest.files {
        if file.metadata
            || !file
                .path
                .strip_prefix(&root)
                .is_ok_and(|p| p.starts_with("Assets/Scripts"))
        {
            continue;
        }
        let data = workspace.store.load(&source_tx, key).unwrap().unwrap();
        for occurrence in data
            .facts
            .occurrences
            .iter()
            .filter(|o| o.call && o.name == "GetNameCached")
        {
            count += 1;
            let position = crate::model::position(&data.source, occurrence.span.start);
            for _ in 0..2 {
                let mut binder = crate::csharp::bind::Binder::default();
                let targets = binder
                    .resolve(
                        &view,
                        key,
                        file.memberships[0].project,
                        occurrence.span.start,
                        false,
                        &occurrence.name,
                    )
                    .unwrap();
                assert_eq!(
                    targets.len(),
                    1,
                    "{}:{position:?}: {} targets",
                    file.path.display(),
                    targets.len()
                );
                assert!(
                    !targets[0].1,
                    "{}:{position:?}: uncertain",
                    file.path.display()
                );
                assert_eq!(
                    targets[0].0.declaration().qualified,
                    "HK.ExtensionMethods.GetNameCached"
                );
            }
            eprintln!(
                "Resolved {}:{position:?} cold and warm",
                file.path.display()
            );
        }
    }
    assert_eq!(
        count, 11,
        "Reported fixture changed; review its source before changing this expectation"
    );
    drop(source_tx);
    drop(assembly_tx);
    let mut search = crate::search::Search::new(
        &workspace.store,
        &workspace.assemblies,
        &workspace.manifest,
        &cancel,
    )
    .unwrap();
    let incoming = search
        .run(
            &crate::query::Query::parse(
                "calls:HK.ExtensionMethods.GetNameCached path:Assets/Scripts/** limit:30",
            )
            .unwrap(),
        )
        .unwrap();
    let grouped = regex::Regex::new(r"\((\d+) occurrences\)").unwrap();
    let returned: usize = incoming
        .lines()
        .filter(|l| l.contains(" → "))
        .map(|line| {
            grouped
                .captures(line)
                .map_or(1, |c| c[1].parse::<usize>().unwrap())
        })
        .sum();
    assert_eq!(returned, count, "{incoming}");
    for selector in ["uses", "writes"] {
        let result = search.run(&crate::query::Query::parse(&format!("{selector}:Highland.GameplayRecorder.GameplayRecorder.CaptureSlot.PendingReadbacks path:Packages/com.highlandkeep.gameplay-recorder/** limit:30")).unwrap()).unwrap();
        assert!(
            result.contains("data.Slot.PendingReadbacks = 2"),
            "{result}"
        );
        assert!(result.contains("Interlocked.Decrement"), "{result}");
    }
    let native = search.run(&crate::query::Query::parse("writes:Highland.GameplayRecorder.GameplayRecorder.nativeStatus path:Packages/com.highlandkeep.gameplay-recorder/** limit:30").unwrap()).unwrap();
    assert!(native.contains("HKGR_Poll(out nativeStatus"), "{native}");
    let outgoing = search
        .run(&crate::query::Query::parse("calls:* in:HK.MathOperation.Summarize limit:40").unwrap())
        .unwrap();
    let returned: usize = outgoing
        .lines()
        .filter(|l| l.contains(" → "))
        .map(|line| {
            grouped
                .captures(line)
                .map_or(1, |c| c[1].parse::<usize>().unwrap())
        })
        .sum();
    assert_eq!(returned, 4, "{outgoing}");
    eprintln!("Full-context incoming, mutation, and four-call checks passed");
}
