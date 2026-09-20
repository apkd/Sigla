use super::{bind::Binder, catalog::View};
use crate::{discovery::Policy, store::Store, workspace::Workspace};
use std::sync::Arc;

#[test]
fn body_only_refresh_reuses_binding_but_refreshes_target_location() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Test.csproj"), "<Project><ItemGroup><Compile Include=\"Item.cs\" /><Compile Include=\"Usage.cs\" /></ItemGroup></Project>").unwrap();
    let declaration = "class Item { public void Run() {} }";
    std::fs::write(root.path().join("Item.cs"), declaration).unwrap();
    let use_source = "class Usage { void Use(Item item) { item.Run(); } }";
    std::fs::write(root.path().join("Usage.cs"), use_source).unwrap();
    let mut workspace = Workspace::open(
        root.path().into(),
        cache.path(),
        Policy::new(vec![root.path().into()]).unwrap(),
        Arc::new(Store::open(&cache.path().join("assemblies")).unwrap()),
        Arc::new(crate::watch::Monitor::default()),
    )
    .unwrap();
    workspace.refresh().unwrap();
    let bind = |workspace: &Workspace| {
        let source_tx = workspace.store.read().unwrap();
        let assembly_tx = workspace.assemblies.read().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let view = View {
            source: &workspace.store,
            assemblies: &workspace.assemblies,
            source_tx: &source_tx,
            assembly_tx: &assembly_tx,
            manifest: &workspace.manifest,
            cancel: &cancel,
        };
        let (file, entry) = workspace
            .manifest
            .files
            .iter()
            .find(|(_, f)| f.path.ends_with("Usage.cs"))
            .unwrap();
        let mut binder = Binder::default();
        let bound = binder
            .resolve(
                &view,
                file,
                entry.memberships[0].project,
                use_source.find("Run()").unwrap(),
                false,
                "Run",
            )
            .unwrap();
        assert_eq!(bound.len(), 1);
        assert!(!bound[0].1);
        (binder.cache_hits, bound[0].0.declaration().name_span.start)
    };
    let (hits, original) = bind(&workspace);
    assert_eq!(hits, 0);
    let environment = workspace.manifest.environment;
    std::fs::write(root.path().join("Item.cs"), format!("\n\n{declaration}")).unwrap();
    workspace.refresh().unwrap();
    assert_eq!(environment, workspace.manifest.environment);
    let (hits, updated) = bind(&workspace);
    assert_eq!(hits, 1);
    assert!(updated > original);
    std::fs::write(
        root.path().join("Item.cs"),
        "class Item { public void Run(int value) {} }",
    )
    .unwrap();
    workspace.refresh().unwrap();
    assert_ne!(environment, workspace.manifest.environment);
}
