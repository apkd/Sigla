//! Measure structural parsing on a real Unity project without opening Unity.
use anyhow::{Context, Result};
fn main() -> Result<()> {
    let root = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .context("Expected Unity project path")?,
    )
    .canonicalize()?;
    let temp = tempfile::tempdir()?;
    let store =
        sigla::store::Store::open_workspace(&temp.path().join("analysis"), [0; 32], &root, None)?;
    let tx = store.read()?;
    let manifest = sigla::workspace::Manifest {
        root: root.clone(),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let index = sigla::unity::assets::build(
        &root,
        &temp.path().join("assets"),
        &manifest,
        &store,
        &tx,
        sigla::unity::assets::BuildOptions {
            remote: false,
            omitted: &Default::default(),
            cancel: &tokio_util::sync::CancellationToken::new(),
        },
    )?;
    let failures: Vec<_> = index
        .assets
        .values()
        .filter_map(|a| {
            a.unavailable
                .as_ref()
                .filter(|r| r.starts_with("Unsupported"))
                .map(|r| format!("{}: {r}", a.path))
        })
        .collect();
    let parsed = index
        .assets
        .values()
        .filter(|a| a.content.is_some())
        .count();
    let objects: usize = index.assets.values().map(|a| a.objects.len()).sum();
    let cache_bytes: u64 = std::fs::read_dir(temp.path().join("assets"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    println!(
        "{}",
        serde_json::json!({"seconds": started.elapsed().as_secs_f64(), "assets": index.assets.len(), "parsed": parsed, "objects": objects, "cache_bytes": cache_bytes, "unsupported": failures})
    );
    Ok(())
}
