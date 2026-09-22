//! Development-only validation of a real Unity archive; never executes its contents.
use anyhow::{Context, Result};
use std::path::Path;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let archive = args
        .first()
        .context("Usage: editor_archive ARCHIVE DESTINATION VERSION")?;
    let destination = args.get(1).context("Missing destination")?;
    let version = args.get(2).context("Missing editor version")?.parse()?;
    let inventory = sigla::unity::acquire::inspect_archive(
        Path::new(archive),
        Path::new(destination),
        version,
    )?;
    std::fs::write(
        Path::new(destination).join("inventory.json"),
        serde_json::to_vec_pretty(&inventory)?,
    )?;
    println!("Validated {} retained files", inventory.len());
    Ok(())
}
