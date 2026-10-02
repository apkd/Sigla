use sigla::{
    model::Facts,
    store::{FileData, Store},
};
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn open(path: &std::path::Path) -> std::sync::Arc<Store> {
    Store::open_workspace(path, [0; 32], path, None).unwrap()
}

fn install(store: &Store, file: &str, data: FileData) {
    let id =
        sigla::store::source_id(&data.source, sigla::model::Language::CSharp, &[], "").unwrap();
    store
        .install(file, &id, id, || Ok(()), || Ok(data))
        .unwrap();
}

#[test]
fn legacy_analysis_is_rebuilt_without_changing_source_stamps() {
    use sigla::{
        discovery::Policy,
        workspace::{Stamp, Workspace},
    };
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Test.csproj"),
        "<Project><ItemGroup><Compile Include=\"Test.cs\" /></ItemGroup></Project>",
    )
    .unwrap();
    let path = root.path().join("Test.cs");
    let source = "struct Box { public static implicit operator int(Box value) => 0; }";
    std::fs::write(&path, source).unwrap();
    let stamp = Stamp::read(&path).unwrap();
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let open = |analysis: &std::path::Path| {
        Workspace::open(
            root.path().into(),
            cache.path(),
            policy.clone(),
            analysis,
            None,
            Default::default(),
        )
        .unwrap()
    };

    // Obtain a valid discovery manifest, then seed the old namespace with stale facts.
    let mut seed = open(&cache.path().join("seed"));
    seed.refresh().unwrap();
    let manifest = seed.manifest.clone();
    let (file, _) = manifest
        .files
        .iter()
        .find(|(_, entry)| entry.path == path)
        .unwrap();
    let analysis = cache.path().join("analysis");
    let legacy_key = *blake3::hash(
        &postcard::to_allocvec(&(root.path(), policy.unity_platform, policy.remote.is_some()))
            .unwrap(),
    )
    .as_bytes();
    let legacy = Store::open_workspace(&analysis, legacy_key, root.path(), None).unwrap();
    legacy.begin_refresh().unwrap();
    let tx = seed.store.read().unwrap();
    for (key, entry) in &manifest.files {
        let mut data = seed.store.load(&tx, key).unwrap().unwrap();
        if key == file {
            data.facts = Facts::default();
        }
        legacy
            .install(
                key,
                &entry.stamp,
                *blake3::hash(key.as_bytes()).as_bytes(),
                || Ok(()),
                || Ok(data),
            )
            .unwrap();
    }
    drop(tx);
    legacy.save_manifest(&manifest).unwrap();
    drop(legacy);
    drop(seed);

    let mut upgraded = open(&analysis);
    upgraded.refresh().unwrap();
    let declarations = upgraded.store.declarations(file).unwrap();
    assert!(declarations.iter().any(|d| d.kind == "operator"));
    assert_eq!(Stamp::read(&path).unwrap(), stamp);
    drop(upgraded);
    let mut reopened = open(&analysis);
    assert!(reopened.prepare().unwrap().is_none());
}

#[test]
fn compatible_cache_reopens_without_rebuilding_and_detects_offline_edits() {
    use sigla::{discovery::Policy, workspace::Workspace};
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let source = root.path().join("src/lib.rs");
    std::fs::write(&source, "pub struct Before;\n").unwrap();
    let open = || {
        Workspace::open(
            root.path().into(),
            cache.path(),
            Policy::new(vec![root.path().into()]).unwrap(),
            &cache.path().join("analysis"),
            None,
            Default::default(),
        )
        .unwrap()
    };
    let mut first = open();
    first.refresh().unwrap();
    let files = first.manifest.files.len();
    assert!(files > 0);
    drop(first);
    let mut reopened = open();
    assert!(reopened.prepare().unwrap().is_none());
    assert_eq!(reopened.manifest.files.len(), files);
    drop(reopened);
    std::fs::write(&source, "pub struct After;\n").unwrap();
    let mut changed = open();
    let update = changed
        .prepare()
        .unwrap()
        .expect("offline edit must invalidate the cache");
    changed.apply(update).unwrap();
    let read = changed.store.read().unwrap();
    let key = changed
        .manifest
        .files
        .iter()
        .find(|(_, file)| file.path == source)
        .unwrap()
        .0;
    assert!(
        changed
            .store
            .load(&read, key)
            .unwrap()
            .unwrap()
            .source
            .contains("After")
    );
}

#[test]
fn concurrent_opens_share_a_store_and_can_reopen_after_drop() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    open(dir.path())
                })
            })
            .collect();
        let stores: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for store in &stores {
            assert!(std::sync::Arc::ptr_eq(
                &stores[0].scope.database,
                &store.scope.database
            ));
        }
    });
    open(dir.path()).read().unwrap();
}

#[test]
fn reader_keeps_source_and_declarations_from_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = |source: &str| FileData {
        source: source.into(),
        facts: sigla::extract::extract(source, sigla::model::Language::CSharp, &[], "").unwrap(),
        assembly: None,
    };
    install(&store, "file", data("class Before {}"));
    let read = store.read().unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| install(&store, "file", data("class After {}")))
            .join()
            .unwrap();
    });
    let source = store.load(&read, "file").unwrap().unwrap();
    let declarations = store.declarations_in(&read, "file").unwrap();
    assert!(source.source.contains(&declarations[0].name));
    assert_eq!(declarations[0].name, "Before");
    assert_eq!(
        store
            .csharp_headers(&read, "file")
            .unwrap()
            .unwrap()
            .declarations[0]
            .name,
        "Before"
    );
    drop(read);
    let read = store.read().unwrap();
    let source = store.load(&read, "file").unwrap().unwrap();
    let declarations = store.declarations_in(&read, "file").unwrap();
    assert!(source.source.contains(&declarations[0].name));
    assert_eq!(declarations[0].name, "After");
    assert_eq!(
        store
            .csharp_headers(&read, "file")
            .unwrap()
            .unwrap()
            .declarations[0]
            .name,
        "After"
    );
}

#[test]
fn running_search_keeps_its_revision_while_workspace_publishes_an_edit() {
    use sigla::{discovery::Policy, query::Query, search::Search, workspace::Workspace};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Test.csproj"),
        "<Project><ItemGroup><Compile Include=\"Source.cs\"/></ItemGroup></Project>",
    )
    .unwrap();
    let path = root.path().join("Source.cs");
    std::fs::write(&path, "class Item { public int Read() => 1; } class Usage { void Run(Item item) { item.Read(); } }").unwrap();
    let mut workspace = Workspace::open(
        root.path().into(),
        cache.path(),
        Policy::new(vec![root.path().into()]).unwrap(),
        &cache.path().join("analysis"),
        None,
        Arc::new(sigla::watch::Monitor::default()),
    )
    .unwrap();
    workspace.refresh().unwrap();
    let source = workspace.store.clone();
    let manifest = workspace.manifest.clone();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut reader = Search::new(&source, &manifest, &cancel).unwrap();
    let workspace = std::thread::spawn(move || {
        std::fs::write(path, "class Item { public string Read() => \"updated\"; } class Usage { void Run(Item item) { item.Read(); } }").unwrap();
        workspace.refresh().unwrap();
        workspace
    }).join().unwrap();
    let query = Query::parse("method:Read").unwrap();
    let old = reader.run(&query).unwrap();
    assert!(old.contains("int Read()"), "{old}");
    assert!(!old.contains("updated"), "{old}");
    drop(reader);
    let mut reader = Search::new(&source, &workspace.manifest, &cancel).unwrap();
    let updated = reader.run(&query).unwrap();
    assert!(updated.contains("string Read()"), "{updated}");
}

#[test]
fn semantic_revision_ignores_body_edits_and_locations_but_tracks_headers() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let replace = |source: &str| {
        let data = FileData {
            source: source.into(),
            facts: sigla::extract::extract(source, sigla::model::Language::CSharp, &[], "")
                .unwrap(),
            assembly: None,
        };
        install(&store, "file", data);
        store.declaration_revision("file").unwrap()
    };
    let before = replace("class C { private int Count(int value = 1) { return value; } }");
    let edited = replace(
        "\n\nclass C { private int Count(int value = 1) { int local = value + 2; return local; } }",
    );
    assert_eq!(before, edited);
    assert_eq!(
        before,
        replace(
            "class C { private int Count(int value = 1) { int Local(int input) => input; return Local(value); } }"
        )
    );
    assert_ne!(
        edited,
        replace("class C { private int Count(int value = 2) { return value; } }")
    );
    assert_ne!(
        edited,
        replace("class C { private long Count(int value = 1) { return value; } }")
    );
    assert_ne!(
        edited,
        replace("using Alias = C; class C { private int Count(int value = 1) { return value; } }")
    );
}

#[test]
fn concurrent_callers_share_a_cache_build() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let source = " ".repeat(1024 * 1024);
    let id = sigla::store::source_id(&source, sigla::model::Language::Text, &[], "").unwrap();
    let ready = Barrier::new(4);
    let builds = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                ready.wait();
                store
                    .install(
                        "shared",
                        &1u64,
                        id,
                        || Ok(()),
                        || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            Ok(FileData {
                                source: " ".repeat(1024 * 1024),
                                facts: Facts::default(),
                                assembly: None,
                            })
                        },
                    )
                    .unwrap();
            });
        }
    });
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(
        store
            .load(&store.read().unwrap(), "shared")
            .unwrap()
            .is_some()
    );
}
