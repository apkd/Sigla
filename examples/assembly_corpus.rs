//! validate each distinct referenced assembly once, without parsing source files.
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeSet, path::PathBuf, time::Instant};
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let entry = PathBuf::from(
        args.next()
            .context("Expected project path followed by allowed roots")?,
    );
    let policy = sigla::discovery::Policy::new(args.map(PathBuf::from).collect())?;
    let discovery = sigla::discovery::discover(&entry, &policy)?;
    let paths: BTreeSet<_> = discovery
        .projects
        .iter()
        .flat_map(|p| p.assemblies.iter().map(|a| &a.path))
        .collect();
    let start = Instant::now();
    let (mut loaded, mut missing, mut failed, mut members) = (0, 0, 0, 0);
    for path in paths {
        if !path.exists() {
            missing += 1;
            continue;
        }
        let path = if discovery.dependencies.contains(path) {
            path.clone()
        } else {
            policy.canonical(path)?
        };
        match sigla::metadata::extract(&path) {
            Ok(facts) => {
                loaded += 1;
                members += facts.members.len();
            }
            Err(error) => {
                failed += 1;
                eprintln!("{}: {error:#}", path.display());
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({"loaded":loaded,"missing":missing,"failed":failed,"members":members,"elapsed_ms":start.elapsed().as_millis()})
    );
    ensure!(failed == 0, "Some assemblies failed validation");
    Ok(())
}
