//! Measure local refresh with a retained disk cache and a synthetic Rust workspace.
use anyhow::{Result, ensure};
use sigla::{discovery::Policy, service::App};
use std::{fs, path::Path, sync::Arc, time::Instant};

fn app(root: &Path, cache: &Path) -> Result<Arc<App>> {
    Ok(Arc::new(App::new(
        Policy::new(vec![root.into()])?,
        cache.into(),
        2,
    )?))
}

async fn measure(app: &Arc<App>, root: &Path, operation: &str, round: usize) -> Result<()> {
    let start = Instant::now();
    let result = app
        .search(root.to_str().unwrap(), "type:Marker wait:complete")
        .await?;
    ensure!(
        result.contains("Marker"),
        "Probe declaration unavailable: {result}"
    );
    println!(
        "{operation},{round},{:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let files = std::env::args()
        .nth(1)
        .map_or(Ok(1000), |s| s.parse::<usize>())?;
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("project");
    let cache = fixture.path().join("cache");
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='refresh_probe'\nversion='0.1.0'\n",
    )?;
    let mut modules = String::from("pub struct Marker;\n");
    for i in 0..files {
        modules.push_str(&format!("mod item{i};\n"));
        fs::write(
            root.join(format!("src/item{i}.rs")),
            format!("pub struct Item{i};\n"),
        )?;
    }
    fs::write(root.join("src/lib.rs"), &modules)?;
    let service = app(&root, &cache)?;
    println!("operation,round,milliseconds");
    measure(&service, &root, "cold", 0).await?;
    let edited = root.join("src/item0.rs");
    for round in 0..5 {
        measure(&service, &root, "unchanged", round).await?;
        fs::write(&edited, format!("pub struct Item0; // edit {round}\n"))?;
        measure(&service, &root, "in_place", round).await?;
        fs::write(
            root.join("src/replacement.tmp"),
            format!("pub struct Item0; // atomic {round}\n"),
        )?;
        fs::rename(root.join("src/replacement.tmp"), &edited)?;
        measure(&service, &root, "atomic", round).await?;
        fs::write(root.join("src/temporary.tmp"), "temporary")?;
        measure(&service, &root, "temporary_create", round).await?;
        fs::remove_file(root.join("src/temporary.tmp"))?;
        measure(&service, &root, "temporary_remove", round).await?;
        fs::write(root.join("src/new.md"), "New indexed document")?;
        measure(&service, &root, "document_create", round).await?;
        ensure!(
            service
                .search(root.to_str().unwrap(), "file:new.md")
                .await?
                .contains("new.md")
        );
        fs::remove_file(root.join("src/new.md"))?;
        measure(&service, &root, "document_remove", round).await?;
    }
    service.shutdown().await;
    drop(service);
    let service = app(&root, &cache)?;
    measure(&service, &root, "reopen", 0).await?;
    service.shutdown().await;
    Ok(())
}
