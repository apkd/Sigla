//! Maintenance tool: inspect a matching editor's Managed directory without
//! loading assemblies into a CLR or launching Unity. No native IDs are inferred.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Options {
    /// The editor's actual version, not the installed CLR assembly version.
    #[arg(long)]
    version: String,
    /// Editor/Data/Managed; do not include player or platform reference copies.
    #[arg(long)]
    managed_dir: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Definition {
    assembly: String,
    base: Option<String>,
}
#[derive(Serialize)]
struct Metadata {
    version: String,
    sources: Vec<String>,
    types: BTreeMap<String, Option<Definition>>,
}

fn inventory(root: &Path, files: &mut BTreeSet<PathBuf>, depth: usize) -> Result<()> {
    ensure!(depth < 32, "Managed directory nesting limit exceeded");
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            inventory(&entry.path(), files, depth + 1)?;
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "dll") {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("UnityEngine.") || name.starts_with("UnityEditor.") {
                files.insert(entry.path());
                ensure!(
                    files.len() <= 4096,
                    "Managed assembly inventory limit exceeded"
                );
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let options = Options::parse();
    let version: sigla::unity::UnityVersion = options.version.parse()?;
    ensure!(
        version.stable(),
        "Use a stable editor build for a release-branch catalog"
    );
    let mut files = BTreeSet::new();
    inventory(&options.managed_dir, &mut files, 0)?;
    ensure!(!files.is_empty(), "No Unity assemblies found");
    let mut metadata = Metadata {
        version: version.to_string(),
        sources: Vec::new(),
        types: BTreeMap::new(),
    };
    let mut assemblies = BTreeSet::new();
    for path in files {
        ensure!(
            std::fs::metadata(&path)?.len() <= 256 * 1024 * 1024,
            "Assembly too large"
        );
        let bytes = std::fs::read(&path)?;
        let hash = blake3::hash(&bytes);
        let facts = sigla::metadata::extract_bytes(
            bytes,
            path.file_stem()
                .and_then(|s| s.to_str())
                .context("Invalid DLL name")?,
        )?;
        ensure!(
            assemblies.insert(facts.name.clone()),
            "Duplicate assembly: {}",
            facts.name
        );
        metadata.sources.push(format!(
            "{} BLAKE3:{hash}",
            path.strip_prefix(&options.managed_dir)?.display()
        ));
        for member in facts.members {
            if member.kind != "class" || member.generic_count != 0 {
                continue;
            }
            // Compiler-generated and unrelated library types are not needed.
            if !member.qualified.starts_with("UnityEngine.")
                && !member.qualified.starts_with("UnityEditor.")
            {
                continue;
            }
            let definition = Definition {
                assembly: facts.name.clone(),
                // metadata::extract_bytes records Extends before interfaces.
                base: member.bases.first().cloned(),
            };
            match metadata.types.entry(member.qualified) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(Some(definition));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.insert(None);
                }
            }
        }
    }
    std::fs::write(options.output, serde_json::to_vec_pretty(&metadata)?)?;
    Ok(())
}
