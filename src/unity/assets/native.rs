//! Native identity and managed inheritance are independent, versioned evidence.
//! Catalogs are built offline; asset queries never download data or run Unity.
use super::ScriptType;
use crate::{
    csharp::types::WrittenType,
    store::{DeclarationLookup, Store},
    unity::UnityVersion,
    workspace::Manifest,
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

const OBJECT: &str = "UnityEngine.Object";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Definition {
    assembly: String,
    base: Option<String>,
}

#[derive(Deserialize)]
struct Snapshot {
    branch: String,
    sources: Vec<String>,
    // (persistent native ID, native name, audited managed identity).
    // None means no established counterpart, not an inferred managed base.
    classes: Vec<(i32, String, Option<String>)>,
    types: BTreeMap<String, Definition>,
}

fn snapshots() -> &'static [Snapshot] {
    static SNAPSHOTS: OnceLock<Vec<Snapshot>> = OnceLock::new();
    SNAPSHOTS.get_or_init(|| {
        [
            include_str!("native/2022.3.json"),
            include_str!("native/6000.0.json"),
        ]
        .into_iter()
        .map(|text| {
            let snapshot: Snapshot =
                serde_json::from_str(text).expect("checked-in Unity native catalog");
            assert!(
                !snapshot.sources.is_empty(),
                "Unity catalog provenance is required"
            );
            snapshot
        })
        .collect()
    })
}

fn snapshot(version: Option<UnityVersion>) -> Option<&'static Snapshot> {
    let version = version.filter(|v| v.stable())?;
    let branch = version.branch.to_string();
    snapshots().iter().find(|s| s.branch == branch)
}

// Preserve known native identities outside the audited release branches, but
// expose their unverified version coverage. Only facts shared by every catalog
// are retained; version-specific renames and bases remain unresolved.
fn compatibility() -> &'static Snapshot {
    static COMMON: OnceLock<Snapshot> = OnceLock::new();
    COMMON.get_or_init(|| {
        let catalogs = snapshots();
        let first = &catalogs[0];
        let classes = first
            .classes
            .iter()
            .map(|(id, native, name)| {
                let name = name
                    .as_ref()
                    .filter(|name| {
                        catalogs.iter().all(|catalog| {
                            catalog.classes.binary_search_by_key(id, |c| c.0).is_ok_and(
                                |position| catalog.classes[position].2.as_ref() == Some(name),
                            )
                        })
                    })
                    .cloned();
                (*id, native.clone(), name)
            })
            .collect();
        let types = first
            .types
            .iter()
            .filter_map(|(name, definition)| {
                let definitions: Option<Vec<_>> = catalogs
                    .iter()
                    .map(|catalog| catalog.types.get(name))
                    .collect();
                let definitions = definitions?;
                if definitions
                    .iter()
                    .any(|other| other.assembly != definition.assembly)
                {
                    return None;
                }
                Some((
                    name.clone(),
                    Definition {
                        assembly: definition.assembly.clone(),
                        base: definitions
                            .iter()
                            .all(|other| other.base == definition.base)
                            .then(|| definition.base.clone())
                            .flatten(),
                    },
                ))
            })
            .collect();
        Snapshot {
            branch: "shared baseline".into(),
            sources: first.sources.clone(),
            classes,
            types,
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct Resolved {
    pub ty: Option<Arc<ScriptType>>,
    pub issue: Option<Arc<str>>,
}
impl Resolved {
    fn unavailable(reason: impl Into<Arc<str>>) -> Self {
        Self {
            ty: None,
            issue: Some(reason.into()),
        }
    }
}

#[derive(Default)]
struct Metadata {
    // A present assembly with an absent class disproves that fallback definition.
    assemblies: BTreeSet<String>,
    // Multiple definitions remain ambiguous, even when their base names agree.
    types: BTreeMap<String, Option<Definition>>,
}

/// metadata::extract_bytes puts Extends before interfaces. Unsupported signatures
/// are not reinterpreted as System.Object or expanded using guessed namespaces.
fn type_name(ty: &WrittenType) -> Option<String> {
    match ty {
        WrittenType::External { ty, .. } => type_name(ty),
        WrittenType::Name { alias, parts }
            if alias.as_deref().is_none_or(|a| a == "global")
                && !parts.is_empty()
                && parts.iter().all(|p| p.arguments.is_empty()) =>
        {
            Some(
                parts
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
            )
        }
        _ => None,
    }
}

/// References selected for another editor, another Unity root, plugins, and
/// source declarations are not evidence for this project's native hierarchy.
fn references(root: &Path, version: UnityVersion, manifest: &Manifest) -> BTreeSet<PathBuf> {
    let assets = root.join("Assets");
    let version = version.to_string();
    let provenance = format!("Unity editor {version}");
    manifest
        .projects
        .iter()
        .filter(|p| {
            p.source_roots.iter().any(|r| r.physical == assets)
                && p.compiler_options.get("UnityVersion") == Some(&version)
                && p.compiler_options.get("DeclaredUnityVersion") == Some(&version)
        })
        .flat_map(|p| &p.assemblies)
        .filter(|r| r.provenance == provenance)
        .map(|r| r.path.clone())
        .collect()
}

fn metadata(
    paths: &BTreeSet<PathBuf>,
    mut pending: Vec<String>,
    manifest: &Manifest,
    store: &Store,
    tx: &heed::RoTxn<'_>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Metadata> {
    let mut result = Metadata::default();
    if paths.is_empty() || pending.is_empty() {
        return Ok(result);
    }
    let mut files = BTreeMap::new();
    for (file, entry) in &manifest.files {
        ensure!(
            !cancel.is_cancelled(),
            "Asset indexing cancelled or superseded"
        );
        if !entry.metadata || !paths.contains(&entry.path) {
            continue;
        }
        let Some(assembly) = store.assembly_name_in(tx, file)? else {
            continue;
        };
        if !["UnityEngine", "UnityEditor"].iter().any(|prefix| {
            assembly == *prefix
                || assembly
                    .strip_prefix(*prefix)
                    .is_some_and(|s| s.starts_with('.'))
        }) {
            continue;
        }
        result.assemblies.insert(assembly.clone());
        files.insert(file.clone(), assembly);
    }
    if files.is_empty() {
        return Ok(result);
    }
    // Read only required class records and their transitive bases, not entire
    // editor assemblies (which contain many unrelated member declarations).
    let mut visited = BTreeSet::new();
    while let Some(name) = pending.pop() {
        ensure!(
            !cancel.is_cancelled(),
            "Asset indexing cancelled or superseded"
        );
        if !visited.insert(name.clone()) {
            continue;
        }
        ensure!(
            visited.len() <= 10_000,
            "Native metadata type limit exceeded"
        );
        let leaf = name.rsplit('.').next().unwrap_or(&name);
        let mut found = None;
        let mut ambiguous = false;
        for file in store.candidates(tx, leaf, false, false)? {
            let Some(assembly) = files.get(&file) else {
                continue;
            };
            for index in store.csharp_lookup(tx, &file, leaf, DeclarationLookup::Type)? {
                let Some(facts) = store.csharp_declaration(tx, &file, index)? else {
                    continue;
                };
                let Some(position) = facts.headers.iter().position(|h| h.declaration == index)
                else {
                    continue;
                };
                let Some(declaration) = facts.declarations.get(position) else {
                    continue;
                };
                let header = &facts.headers[position];
                if declaration.kind != "class"
                    || declaration.qualified != name
                    || !header.generics.is_empty()
                {
                    continue;
                }
                let definition = Definition {
                    assembly: assembly.clone(),
                    base: header.bases.first().and_then(type_name),
                };
                ambiguous |= found.replace(definition).is_some();
            }
        }
        if let Some(definition) = found {
            if ambiguous {
                result.types.insert(name, None);
            } else {
                if let Some(base) = &definition.base
                    && base != OBJECT
                    && base != "System.Object"
                {
                    pending.push(base.clone());
                }
                result.types.insert(name, Some(definition));
            }
        }
    }
    Ok(result)
}

fn hierarchy<'a>(
    name: &str,
    assembly: &str,
    mut definition: impl FnMut(&str) -> Result<&'a Definition, &'static str>,
) -> Resolved {
    let mut ancestry = Vec::new();
    let mut current = name.to_owned();
    let issue = loop {
        if ancestry.len() == 64 || ancestry.contains(&current) {
            break Some(format!("Cyclic or excessive managed ancestry at {current}"));
        }
        // The name of an unavailable base is established by its child's TypeRef.
        // Only the unobserved portion of its ancestry remains unknown.
        ancestry.push(current.clone());
        if current == OBJECT {
            break None;
        }
        if current == "System.Object" {
            return Resolved::unavailable(format!(
                "{name} is not a UnityEngine.Object in matching metadata"
            ));
        }
        match definition(&current) {
            Ok(Definition {
                base: Some(base), ..
            }) => current = base.clone(),
            Ok(_) => break Some(format!("Managed base unavailable for {current}")),
            Err(reason) => break Some(format!("{reason}: {current}")),
        }
    };
    Resolved {
        ty: Some(Arc::new(ScriptType {
            name: name.into(),
            assembly: assembly.into(),
            ancestry,
        })),
        issue: issue.map(Into::into),
    }
}

fn resolve(catalog: &Snapshot, metadata: &Metadata, class: i32) -> Resolved {
    // 114 represents both scripted components and ScriptableObjects. Its only
    // resolution path is m_Script, including prefab overrides.
    if class == 114 {
        return Resolved::unavailable("Class 114 requires m_Script");
    }
    let Ok(position) = catalog.classes.binary_search_by_key(&class, |c| c.0) else {
        return Resolved::unavailable(format!(
            "Native class {class} is absent from the {} ID catalog",
            catalog.branch
        ));
    };
    let (_, native, Some(name)) = &catalog.classes[position] else {
        return Resolved::unavailable(format!(
            "Native class {class} has no established managed counterpart in {}",
            catalog.branch
        ));
    };
    if let Some(definition) = metadata.types.get(name) {
        let Some(definition) = definition else {
            return Resolved::unavailable(format!("Ambiguous matching editor metadata for {name}"));
        };
        return hierarchy(name, &definition.assembly, |name| {
            match metadata.types.get(name) {
                Some(Some(definition)) => Ok(definition),
                Some(None) => Err("Ambiguous matching editor metadata"),
                None => Err("Matching editor metadata unavailable"),
            }
        });
    }
    let Some(definition) = catalog.types.get(name) else {
        return Resolved::unavailable(format!(
            "No managed definition for {native} ({name}) in {}",
            catalog.branch
        ));
    };
    if metadata.assemblies.contains(&definition.assembly) {
        return Resolved::unavailable(format!(
            "{name} is absent from matching {} metadata",
            definition.assembly
        ));
    }
    // Fallback as a whole; never splice a baseline into an observed metadata chain.
    hierarchy(name, &definition.assembly, |name| {
        catalog
            .types
            .get(name)
            .ok_or("Managed snapshot base unavailable")
    })
}

fn version(path: &Path) -> Option<UnityVersion> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024 + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > 64 * 1024 {
        return None;
    }
    let mut versions = text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| line.strip_prefix("m_EditorVersion:"));
    let version = versions.next()?.trim().parse().ok()?;
    versions.next().is_none().then_some(version)
}

pub(super) fn build(
    root: &Path,
    classes: &BTreeSet<i32>,
    manifest: &Manifest,
    store: &Store,
    tx: &heed::RoTxn<'_>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<BTreeMap<i32, Resolved>> {
    let version = version(&root.join("ProjectSettings/ProjectVersion.txt"));
    let exact = snapshot(version);
    let catalog = exact.unwrap_or_else(compatibility);
    let metadata = if let Some(version) = version {
        let names = catalog
            .classes
            .iter()
            .filter(|(class, _, _)| *class != 114 && classes.contains(class))
            .filter_map(|(_, _, name)| name.clone())
            .collect();
        metadata(
            &references(root, version, manifest),
            names,
            manifest,
            store,
            tx,
            cancel,
        )?
    } else {
        Metadata::default()
    };
    let mut result = BTreeMap::new();
    for &class in classes.iter().filter(|&&class| class != 114) {
        ensure!(
            !cancel.is_cancelled(),
            "Asset indexing cancelled or superseded"
        );
        let mut resolution = resolve(catalog, &metadata, class);
        if exact.is_none() {
            let mut issue = format!(
                "Native class ID coverage is unverified for Unity {}",
                version.map_or_else(|| "version unknown".into(), |v| v.to_string())
            );
            if let Some(detail) = &resolution.issue {
                issue.push_str("; ");
                issue.push_str(detail);
            }
            resolution.issue = Some(issue.into());
        }
        result.insert(class, resolution);
    }
    Ok(result)
}

#[cfg(test)]
#[path = "native/tests.rs"]
mod tests;
