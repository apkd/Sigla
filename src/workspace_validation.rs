//! Explicit opt-in replay of a deployed input manifest. The deployed LMDB is
//! opened read-only; all extraction and indexing happen in a temporary cache.
use super::*;

#[test]
#[ignore = "requires SIGLA_BENCH_ROOT; optional SIGLA_BENCH_INDEX replays a deployed snapshot"]
fn source_indexing_benchmark() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let path = |name| PathBuf::from(std::env::var_os(name).expect(name));
    let root = path("SIGLA_BENCH_ROOT");
    let parent = std::env::var_os("SIGLA_BENCH_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let cache = tempfile::tempdir_in(parent).unwrap();
    let manifest =
        if let Some(index) = std::env::var_os("SIGLA_BENCH_INDEX") {
            let bytes =
                crate::store::shared::snapshot_manifest(&PathBuf::from(index), &root, |bytes| {
                    Ok(crate::store::inspect_manifest(bytes)?
                        .is_some_and(|m| m.source_group.is_none()))
                })
                .unwrap();
            crate::store::inspect_manifest(&bytes).unwrap().unwrap()
        } else {
            let mut workspace = Workspace::open(
                root.clone(),
                cache.path(),
                Policy::new(vec![root.clone()]).unwrap(),
                &cache.path().join("discovery"),
                None,
                Default::default(),
            )
            .unwrap();
            let prepared = workspace.prepare().unwrap().unwrap();
            Arc::unwrap_or_clone(workspace.plan(prepared).unwrap().manifest)
        };
    let inputs: VecDeque<_> = manifest
        .files
        .values()
        .filter(|f| !f.metadata && f.language == Language::CSharp)
        .map(|f| SourceInput {
            path: f.path.clone(),
            project: f.memberships[0].project,
            module: f.memberships[0].module.clone(),
            language: f.language,
            metadata: false,
        })
        .collect();
    eprintln!(
        "manifest: files={} projects={} metadata={}",
        manifest.files.len(),
        manifest.projects.len(),
        manifest.metadata.len()
    );
    if std::env::var_os("SIGLA_BENCH_PLAN_ONLY").is_some() {
        eprintln!("C# inputs={}", inputs.len());
        return;
    }
    if std::env::var_os("SIGLA_BENCH_FULL").is_some() {
        benchmark_all_indexes(manifest, cache.path());
        return;
    }
    assert!(!inputs.is_empty(), "Benchmark has no C# inputs");
    let io = || {
        std::fs::read_to_string("/proc/self/io")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.to_owned(), v.trim().parse::<u64>().unwrap()))
            .collect::<BTreeMap<_, _>>()
    };
    for mode in ["individual", "batch"] {
        let store = Store::open_workspace(&cache.path().join(mode), [1; 32], &root, None).unwrap();
        let before = io();
        let started = std::time::Instant::now();
        let results = if mode == "batch" {
            index_sources(&store, &inputs, &manifest.projects).unwrap()
        } else {
            let jobs: Vec<_> = inputs.iter().collect();
            let next = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut results = BTreeMap::new();
                            while let Some(input) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                                let stamp = Stamp::read(&input.path).unwrap();
                                let project = &manifest.projects[input.project];
                                let key = input_key(input, project, &stamp);
                                let result = index_input(&store, &key, input, project, &stamp);
                                results.insert(key, (stamp, result));
                            }
                            results
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap())
                    .collect()
            })
        };
        let elapsed = started.elapsed();
        let after = io();
        assert_eq!(results.len(), inputs.len());
        let mut extraction = std::time::Duration::ZERO;
        let mut storage = std::time::Duration::ZERO;
        for (_, result) in results.values() {
            let indexed = result.as_ref().unwrap();
            extraction += indexed.extraction;
            storage += indexed.storage;
        }
        eprintln!(
            "{mode}: files={} elapsed_ms={} extraction_ms={} storage_ms={} write_bytes={}",
            results.len(),
            elapsed.as_millis(),
            extraction.as_millis(),
            storage.as_millis(),
            after
                .get("write_bytes")
                .unwrap_or(&0)
                .saturating_sub(*before.get("write_bytes").unwrap_or(&0))
        );
        store
            .scope
            .finish_refresh(b"benchmark", &results.keys().cloned().collect(), 0)
            .unwrap();
    }
}

fn benchmark_all_indexes(manifest: Manifest, cache: &Path) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("sigla=info")
        .with_writer(std::io::stderr)
        .try_init();
    let started = std::time::Instant::now();
    let expected_files = manifest.files.len();
    let root = manifest.root.clone();
    let mut roots: Vec<_> = manifest
        .inputs
        .iter()
        .map(|i| i.path.parent().unwrap().to_owned())
        .chain(
            manifest
                .metadata
                .keys()
                .map(|p| p.parent().unwrap().to_owned()),
        )
        .chain(std::iter::once(root.clone()))
        .collect();
    roots.sort();
    roots.dedup();
    roots.retain(|p| p.is_dir());
    let analysis = cache.join("analysis");
    let mut workspace = Workspace::open(
        root.clone(),
        cache,
        Policy::new(roots).unwrap(),
        &analysis,
        None,
        Default::default(),
    )
    .unwrap();
    let plan = workspace
        .plan(Preparation {
            discovery: discovery::Discovery {
                root: root.clone(),
                projects: manifest.projects,
                sources: manifest.inputs,
                metadata: manifest.metadata.into_keys().collect(),
                dependencies: manifest.dependencies,
                diagnostics: manifest.diagnostics,
            },
            fence: 0,
            started,
            validation: std::time::Duration::ZERO,
            discovery_time: std::time::Duration::ZERO,
        })
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let activity = Arc::new(runtime.block_on(Arc::new(tokio::sync::RwLock::new(())).read_owned()));
    let asset_cache = cache.join("assets");
    workspace.apply_plan(plan).unwrap();
    assert_eq!(
        workspace.manifest.files.len(),
        expected_files,
        "Code replay lost inputs"
    );
    eprintln!(
        "code_and_metadata: files={} elapsed_ms={}",
        expected_files,
        started.elapsed().as_millis()
    );
    let manifest = workspace.manifest.clone();
    let workspace = Arc::new(tokio::sync::Mutex::new(Some(workspace)));
    runtime.block_on(async {
        let jobs = crate::native::jobs::Jobs::default();
        let mut tickets = Vec::new();
        for group in [crate::native::Group::Native, crate::native::Group::Shaders] {
            let expected = manifest
                .deferred
                .values()
                .filter(|f| crate::native::Group::of(f.language) == Some(group))
                .count();
            let ticket = jobs
                .start(crate::native::jobs::Request {
                    activity: activity.clone(),
                    workspace: workspace.clone(),
                    branch: None,
                    entry: root.clone(),
                    analysis: analysis.clone(),
                    manifest: manifest.clone(),
                    group,
                })
                .unwrap();
            tickets.push((group, expected, ticket));
        }
        let asset_jobs = crate::unity::assets::jobs::Jobs::default();
        let asset_started = std::time::Instant::now();
        let ticket = asset_jobs.start(crate::unity::assets::jobs::Request {
            activity,
            workspace: workspace.clone(),
            branch: None,
            root: root.clone(),
            cache: asset_cache,
            expected: crate::unity::assets::jobs::generation(&manifest).unwrap(),
            revision: None,
            refresh: false,
        });
        let assets = tokio::spawn(async move {
            let index = crate::unity::assets::jobs::wait(ticket).await.unwrap();
            eprintln!(
                "assets: files={} elapsed_ms={}",
                index.assets.len(),
                asset_started.elapsed().as_millis()
            );
        });
        for (group, expected, ticket) in tickets {
            let snapshot = ticket.wait().await.unwrap();
            assert_eq!(
                snapshot.manifest.files.len(),
                expected,
                "Source group replay lost inputs"
            );
            eprintln!(
                "{group:?}: files={} total_elapsed_ms={}",
                expected,
                started.elapsed().as_millis()
            );
        }
        assets.await.unwrap();
    });
    eprintln!("all_indexes: elapsed_ms={}", started.elapsed().as_millis());
}

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
        &cache.path().join("analysis"),
        None,
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
    let cancel = tokio_util::sync::CancellationToken::new();
    let view = crate::csharp::catalog::View {
        store: &workspace.store,
        tx: &source_tx,
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
    let mut search =
        crate::search::Search::new(&workspace.store, &workspace.manifest, &cancel).unwrap();
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
