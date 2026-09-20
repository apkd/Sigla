use sigla::{
    model::Facts,
    store::{FileData, Store},
};
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn reader_keeps_source_and_declarations_from_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let data = |source: &str| FileData {
        source: source.into(),
        facts: sigla::extract::extract(source, sigla::model::Language::CSharp, &[], "").unwrap(),
        assembly: None,
    };
    store.replace("file", &data("class Before {}"), &1).unwrap();
    let read = store.read().unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| store.replace("file", &data("class After {}"), &2).unwrap())
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
        Arc::new(Store::open(&cache.path().join("assemblies")).unwrap()),
        Arc::new(sigla::watch::Monitor::default()),
    )
    .unwrap();
    workspace.refresh().unwrap();
    let source = workspace.store.clone();
    let assemblies = workspace.assemblies.clone();
    let manifest = workspace.manifest.clone();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut reader = Search::new(&source, &assemblies, &manifest, &cancel).unwrap();
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
    let mut reader = Search::new(&source, &assemblies, &workspace.manifest, &cancel).unwrap();
    let updated = reader.run(&query).unwrap();
    assert!(updated.contains("string Read()"), "{updated}");
}

#[test]
fn semantic_revision_ignores_body_edits_and_locations_but_tracks_headers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let replace = |source: &str| {
        let data = FileData {
            source: source.into(),
            facts: sigla::extract::extract(source, sigla::model::Language::CSharp, &[], "")
                .unwrap(),
            assembly: None,
        };
        store.replace("file", &data, &source).unwrap();
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
    let store = Store::open(dir.path()).unwrap();
    let ready = Barrier::new(4);
    let builds = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                ready.wait();
                store
                    .ensure_revision("shared", &1u64, || {
                        builds.fetch_add(1, Ordering::SeqCst);
                        Ok(FileData {
                            source: " ".repeat(1024 * 1024),
                            facts: Facts::default(),
                            assembly: None,
                        })
                    })
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
