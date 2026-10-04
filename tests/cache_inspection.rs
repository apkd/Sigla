use sigla::{
    cache::inspect::{Overrides, inspect},
    discovery::Policy,
    service::App,
};
use std::{fs, sync::Arc};

#[tokio::test]
async fn live_and_idle_inspection_preserve_analysis_and_report_its_live_pages() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.rs"), "pub struct Indexed;").unwrap();
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap(),
    );
    let server = app.start_inspection().unwrap();
    let (query, initial) = tokio::join!(
        app.search(root.path().to_str().unwrap(), "type:Indexed wait:complete"),
        inspect(cache.path(), Overrides::default()),
    );
    assert!(query.unwrap().contains("Indexed"));
    initial.unwrap();
    let report = inspect(cache.path(), Overrides::default()).await.unwrap();
    let analysis = report.analysis.unwrap();
    assert!(analysis.live > 0 && analysis.live <= analysis.allocated);
    assert!(report.workspaces.iter().any(|w| w.entry == root.path()));
    app.shutdown().await;
    server.await.unwrap();
    drop(app);
    let data = cache.path().join("analysis/data.mdb");
    let before = fs::read(&data).unwrap();
    let owners = fs::read(cache.path().join("owners.json")).unwrap();
    let offline = inspect(cache.path(), Overrides::default()).await.unwrap();
    assert!(!offline.live_service);
    assert_eq!(fs::read(&data).unwrap(), before);
    assert_eq!(fs::read(cache.path().join("owners.json")).unwrap(), owners);
    assert_eq!(offline.analysis.unwrap().live, analysis.live);
    assert!(offline.workspaces.iter().any(|w| w.entry == root.path()));
}
