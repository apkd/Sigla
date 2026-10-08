//! Unity object analysis is separate from source-code indexing.
mod compose;
pub mod jobs;
mod parse;
mod query;
mod records;
mod yaml;

use crate::{
    model::Language,
    store::Store,
    workspace::{Manifest, Stamp},
};
use anyhow::{Context, Result, ensure};
use parse::{Object, Parsed};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const LOCAL_LIMIT: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptType {
    pub name: String,
    pub assembly: String,
    pub ancestry: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Asset {
    pub project: String,
    pub path: String,
    pub guid: String,
    pub objects: Vec<std::sync::Arc<Object>>,
    pub content: Option<PathBuf>,
    pub unavailable: Option<String>,
}
#[derive(Default)]
pub struct Index {
    pub assets: BTreeMap<String, Asset>,
    pub guids: BTreeMap<(String, String), Vec<String>>,
    pub scripts: BTreeMap<String, ScriptType>,
    pub instances: BTreeMap<String, std::sync::Arc<compose::Composed>>,
    observed: BTreeMap<PathBuf, Stamp>,
}
impl Index {
    fn current(&self) -> bool {
        self.observed
            .iter()
            .all(|(path, stamp)| Stamp::read(path).as_ref().ok() == Some(stamp))
    }
}

pub fn is_asset(path: &Path) -> bool {
    crate::repository::selection::asset(path) || crate::repository::selection::visual_graph(path)
}
pub fn is_root(path: &Path) -> bool {
    path.join("Assets").is_dir() && path.join("ProjectSettings/ProjectVersion.txt").is_file()
}
fn ignored(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.starts_with('.')
            || n.ends_with('~')
            || matches!(
                n,
                "Library"
                    | "Temp"
                    | "Logs"
                    | "Build"
                    | "Builds"
                    | "obj"
                    | "bin"
                    | "target"
                    | "node_modules"
            )
    })
}
fn roots(
    path: &Path,
    found: &mut Vec<PathBuf>,
    observed: &mut BTreeMap<PathBuf, Stamp>,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth < 128,
        "Directory nesting exceeds asset discovery limit"
    );
    observed.insert(path.into(), Stamp::read(path)?);
    if is_root(path) {
        found.push(path.into());
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && !ignored(&entry.path())
            && !crate::discovery::linked_worktree(&entry.path())
        {
            roots(&entry.path(), found, observed, depth + 1)?;
        }
    }
    Ok(())
}
fn inventory(
    path: &Path,
    files: &mut BTreeSet<PathBuf>,
    directories: &mut BTreeMap<PathBuf, Stamp>,
    depth: usize,
    visual_graphs: bool,
) -> Result<()> {
    ensure!(
        depth < 128 && files.len() < 1_000_000,
        "Unity asset inventory limit exceeded"
    );
    directories.insert(path.into(), Stamp::read(path)?);
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if ignored(&path) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !is_root(&path) && !crate::discovery::linked_worktree(&path) {
                inventory(&path, files, directories, depth + 1, visual_graphs)?;
            }
        } else if kind.is_file()
            && (crate::repository::selection::asset(&path)
                || visual_graphs && crate::repository::selection::visual_graph(&path)
                || path.extension().is_some_and(|e| e == "meta"))
        {
            files.insert(path);
        }
    }
    Ok(())
}
fn metadata(path: &Path) -> Option<(Option<String>, bool)> {
    if std::fs::metadata(path).ok()?.len() > 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let guid = text.lines().find_map(|l| {
        let s = l.strip_prefix("guid:")?.trim();
        (s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
    });
    let folder = text.lines().any(|l| {
        l.strip_prefix("folderAsset:")
            .is_some_and(|v| v.trim() == "yes")
    });
    Some((guid, folder))
}
fn key(project: &str, path: &str) -> String {
    format!("{project}|{path}")
}
impl Index {
    fn resolve(&self, project: &str, guid: &str) -> Option<&str> {
        let paths = self.guids.get(&(project.into(), guid.into()))?;
        (paths.len() == 1).then(|| paths[0].as_str())
    }
    fn script(&self, asset: &Asset, object: &Object) -> Option<&ScriptType> {
        let script = object.pointer("m_Script");
        if script.null() {
            return None;
        }
        let target = self.resolve(&asset.project, &script.guid)?;
        // Imported DLL subobjects require importer metadata; do not guess their type.
        if !self.assets[target].path.ends_with(".cs") {
            return None;
        }
        self.scripts.get(target)
    }
}

pub struct BuildOptions<'a> {
    pub remote: bool,
    pub omitted: &'a BTreeMap<String, String>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

fn script_types(
    root: &Path,
    projects: &[PathBuf],
    manifest: &Manifest,
    store: &Store,
    tx: &heed::RoTxn<'_>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<BTreeMap<(PathBuf, PathBuf), Vec<ScriptType>>> {
    let mut type_files = BTreeMap::<(PathBuf, PathBuf), Vec<ScriptType>>::new();
    let view = crate::csharp::catalog::View {
        store,
        tx,
        manifest,
        cancel,
    };
    let types_started = std::time::Instant::now();
    for (file, entry) in &manifest.files {
        ensure!(
            !cancel.is_cancelled(),
            "Asset indexing cancelled or superseded"
        );
        if entry.language != Language::CSharp || entry.metadata {
            continue;
        }
        // Only a script's .meta entry can publish its type in index.scripts.
        // Avoid binding package/generated sources that have no such asset entry.
        let mut meta = entry.path.as_os_str().to_os_string();
        meta.push(".meta");
        if !Path::new(&meta).is_file() {
            continue;
        }
        // Each source file is an independent binding query with its own budget.
        let mut binder = crate::csharp::bind::Binder::default();
        let stem = entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        for declaration in store.declarations_in(tx, file)? {
            if declaration.kind != "class" || declaration.name != stem {
                continue;
            }
            for membership in &entry.memberships {
                let ancestry = binder.ancestry(
                    &view,
                    &crate::csharp::catalog::Site {
                        file,
                        project: membership.project,
                        declaration: &declaration,
                    },
                )?;
                if ancestry
                    .iter()
                    .any(|n| n == "UnityEngine.Component" || n == "UnityEngine.ScriptableObject")
                {
                    let compilation = &manifest.projects[membership.project];
                    let ty = ScriptType {
                        name: declaration.qualified.clone(),
                        assembly: compilation.name.clone(),
                        ancestry,
                    };
                    for root in projects.iter().filter(|r| {
                        entry.path.starts_with(r.join("Assets"))
                            || compilation
                                .source_roots
                                .iter()
                                .any(|s| s.physical == r.join("Assets"))
                    }) {
                        let types = type_files
                            .entry((root.clone(), entry.path.clone()))
                            .or_default();
                        if !types.contains(&ty) {
                            types.push(ty.clone());
                        }
                    }
                }
            }
        }
    }
    tracing::info!(workspace = %root.display(), scripts = type_files.len(), elapsed_ms = types_started.elapsed().as_millis(), "Unity script types resolved");
    Ok(type_files)
}

pub fn build(
    root: &Path,
    cache: &Path,
    manifest: &Manifest,
    store: &Store,
    tx: &heed::RoTxn<'_>,
    options: BuildOptions<'_>,
) -> Result<Index> {
    let BuildOptions {
        remote,
        omitted,
        cancel,
    } = options;
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    std::fs::create_dir_all(cache)?;
    let mut projects = Vec::new();
    let mut observed = BTreeMap::new();
    roots(root, &mut projects, &mut observed, 0)?;
    let mut index = Index::default();
    let type_files = script_types(root, &projects, manifest, store, tx, cancel)?;
    let assets_started = std::time::Instant::now();
    for project in &projects {
        let project_name = project.strip_prefix(root)?.to_string_lossy().to_string();
        let project_name = if project_name.is_empty() {
            ".".into()
        } else {
            project_name
        };
        let mut scopes: BTreeSet<_> = [project.join("Assets"), project.join("ProjectSettings")]
            .into_iter()
            .filter(|p| !crate::discovery::linked_worktree(p))
            .collect();
        let mut mapped = false;
        for p in &manifest.projects {
            if p.source_roots
                .iter()
                .any(|s| s.physical == project.join("Assets"))
            {
                mapped = true;
                let prefix = if project_name == "." {
                    "Packages/".into()
                } else {
                    format!("{project_name}/Packages/")
                };
                scopes.extend(
                    p.source_roots
                        .iter()
                        .filter(|s| s.logical.starts_with(&prefix))
                        .map(|s| s.physical.clone()),
                );
            }
        }
        if !mapped
            && let Ok(bytes) = std::fs::read(project.join("Packages/manifest.json"))
            && let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(dependencies) = manifest["dependencies"].as_object()
        {
            scopes.extend(
                dependencies
                    .keys()
                    .filter(|n| !n.contains('/') && !n.contains('\\') && !n.starts_with('.'))
                    .map(|n| project.join("Packages").join(n))
                    .filter(|p| !crate::discovery::crosses_worktree(project, p)),
            );
        }
        let inventory_started = std::time::Instant::now();
        let scope_count = scopes.len();
        let mut files = BTreeSet::new();
        for scope in scopes {
            if scope.is_dir() {
                inventory(&scope, &mut files, &mut observed, 0, remote)?;
            }
        }
        for file in &files {
            observed.insert(file.clone(), Stamp::read(file)?);
        }
        tracing::info!(workspace = %project.display(), scopes = scope_count, files = files.len(), elapsed_ms = inventory_started.elapsed().as_millis(), "Unity asset inventory ready");
        let limit = if remote {
            crate::repository::selection::ASSET_LIMIT
        } else {
            LOCAL_LIMIT
        };
        let inputs = files
            .iter()
            .filter(|file| file.extension().is_none_or(|e| e != "meta"))
            .filter(|file| observed[*file].size <= limit)
            .map(|file| (file.clone(), observed[file].clone()))
            .collect();
        let mut records = records::load_all(cache, &inputs, cancel)?;
        for file in files {
            ensure!(
                !cancel.is_cancelled(),
                "Asset indexing cancelled or superseded"
            );
            let is_meta = file.extension().is_some_and(|e| e == "meta");
            let physical = if is_meta {
                PathBuf::from(file.to_string_lossy().strip_suffix(".meta").unwrap())
            } else {
                file.clone()
            };
            let meta = is_meta.then(|| metadata(&file)).flatten();
            if physical.is_dir() || meta.as_ref().is_some_and(|(_, folder)| *folder) {
                continue;
            }
            let logical = physical
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| {
                    manifest
                        .projects
                        .iter()
                        .filter(|p| {
                            p.source_roots
                                .iter()
                                .any(|s| s.physical == project.join("Assets"))
                        })
                        .flat_map(|p| &p.source_roots)
                        .find_map(|s| {
                            physical
                                .strip_prefix(&s.physical)
                                .ok()
                                .map(|p| format!("{}/{}", s.logical, p.display()))
                        })
                        .unwrap_or_else(|| physical.to_string_lossy().into())
                });
            let asset_key = key(&project_name, &logical);
            let asset = index
                .assets
                .entry(asset_key.clone())
                .or_insert_with(|| Asset {
                    project: project_name.clone(),
                    path: logical,
                    guid: String::new(),
                    objects: Vec::new(),
                    content: None,
                    unavailable: None,
                });
            if is_meta {
                if let Some((Some(guid), _)) = meta {
                    asset.guid = guid;
                }
                if let Some(types) = type_files.get(&(project.clone(), physical.clone()))
                    && types.len() == 1
                {
                    index.scripts.insert(asset_key, types[0].clone());
                }
                if !physical.is_file()
                    || !is_asset(&physical) && physical.extension().is_none_or(|e| e != "cs")
                    || !remote && crate::repository::selection::visual_graph(&physical)
                {
                    asset.unavailable =
                        Some("Asset contents were not selected or are unavailable".into());
                }
                continue;
            }
            let stamp = Stamp::read(&file)?;
            ensure!(
                observed[&file] == stamp,
                "Asset changed while indexing; retry query"
            );
            let size = stamp.size;
            if size > limit {
                asset.unavailable = Some(format!("Asset exceeds size limit ({size} bytes)"));
                continue;
            }
            let record = records.remove(&file).unwrap();
            asset.objects = record.objects;
            asset.unavailable = record.unavailable;
            asset.content = record.content;
        }
    }
    let mut missing: BTreeMap<_, _> = omitted.iter().collect();
    for asset in index.assets.values_mut() {
        if let Some(reason) = omitted.get(&asset.path) {
            asset.unavailable = Some(reason.clone());
            missing.remove(&asset.path);
        }
    }
    for (path, reason) in missing {
        if let Some(project) = projects
            .iter()
            .filter(|p| root.join(path).starts_with(p))
            .max_by_key(|p| p.components().count())
        {
            let name = project.strip_prefix(root)?.to_string_lossy();
            let name = if name.is_empty() { "." } else { &name };
            index.assets.insert(
                key(name, path),
                Asset {
                    project: name.into(),
                    path: path.clone(),
                    guid: String::new(),
                    objects: Vec::new(),
                    content: None,
                    unavailable: Some(reason.clone()),
                },
            );
        }
    }
    for (key, asset) in &index.assets {
        if !asset.guid.is_empty() {
            index
                .guids
                .entry((asset.project.clone(), asset.guid.clone()))
                .or_default()
                .push(key.clone());
        }
    }
    index.observed = observed;
    tracing::info!(workspace = %root.display(), files = index.assets.len(), elapsed_ms = assets_started.elapsed().as_millis(), "Unity asset records ready");
    compose::build(&mut index)?;
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    ensure!(
        index.current(),
        "Unity inputs changed during indexing; retry query"
    );
    Ok(index)
}

fn content(asset: &Asset) -> Result<Parsed> {
    records::content(
        asset
            .content
            .as_ref()
            .context("Asset contents unavailable")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn worktrees_are_excluded_from_project_and_asset_inventories() {
        let fixture = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = fixture.path().join("workspace");
        let repository = fixture.path().join("repository");
        let project_worktree = root.join("branches/feature");
        let asset_worktree = root.join("One/Assets/branches/feature");
        for worktree in [&project_worktree, &asset_worktree] {
            crate::discovery::tests::add_worktree(&repository, worktree);
        }
        for project in [root.join("One"), project_worktree.clone()] {
            write(&project, "ProjectSettings/ProjectVersion.txt", b"version\n");
            write(
                &project,
                "Assets/Object.asset",
                b"--- !u!1 &1\nGameObject:\n  m_Name: Example\n",
            );
            write(
                &project,
                "Assets/Object.asset.meta",
                b"guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            );
        }
        write(
            &asset_worktree,
            "Object.asset",
            b"--- !u!1 &1\nGameObject:\n  m_Name: Nested\n",
        );
        write(
            &asset_worktree,
            "Object.asset.meta",
            b"guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        let store =
            Store::open_workspace(&cache.path().join("store"), [0; 32], &root, None).unwrap();
        let tx = store.read().unwrap();
        let build_at = |path: &Path| {
            build(
                path,
                &cache.path().join("assets"),
                &Manifest::default(),
                &store,
                &tx,
                BuildOptions {
                    remote: false,
                    omitted: &Default::default(),
                    cancel: &tokio_util::sync::CancellationToken::new(),
                },
            )
            .unwrap()
        };
        let index = build_at(&root);
        assert_eq!(index.assets.len(), 1);
        assert!(index.assets.values().all(|a| !a.path.contains("branches")));
        for worktree in [&project_worktree, &asset_worktree] {
            assert!(!index.observed.keys().any(|p| p.starts_with(worktree)));
        }
        write(
            &asset_worktree,
            "Object.asset",
            b"Changed nested worktree asset",
        );
        assert!(index.current());
        let direct = build_at(&project_worktree);
        assert_eq!(direct.assets.len(), 1);
        assert!(direct.assets.values().any(|a| !a.objects.is_empty()));
    }

    #[test]
    fn independent_projects_binary_records_and_nested_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let names = ["One", "Two", "One/Assets/Inner"];
        for name in names {
            write(
                root.path(),
                &format!("{name}/ProjectSettings/ProjectVersion.txt"),
                b"version\n",
            );
            write(
                root.path(),
                &format!("{name}/Assets/Object.asset"),
                b"--- !u!1 &1\nGameObject:\n  m_Name: Example\n",
            );
            write(
                root.path(),
                &format!("{name}/Assets/Object.asset.meta"),
                b"guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            );
        }
        write(root.path(), "One/Assets/Binary.asset", b"\0\x01\x02");
        let omitted = BTreeMap::from([
            (
                "One/Assets/Binary.asset".into(),
                "Excluded binary input".into(),
            ),
            (
                "Two/Assets/Missing.asset".into(),
                "Unavailable download".into(),
            ),
        ]);
        let store =
            Store::open_workspace(&cache.path().join("store"), [0; 32], root.path(), None).unwrap();
        let tx = store.read().unwrap();
        let index = build(
            root.path(),
            &cache.path().join("assets"),
            &Manifest::default(),
            &store,
            &tx,
            BuildOptions {
                remote: false,
                omitted: &omitted,
                cancel: &tokio_util::sync::CancellationToken::new(),
            },
        )
        .unwrap();
        assert_ne!(
            index.resolve("One", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            index.resolve("Two", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(!index.assets.values().any(|a| a.path.contains("Inner")));
        let binary = &index.assets["One|One/Assets/Binary.asset"];
        assert!(binary.unavailable.is_some());
        assert!(binary.content.is_none());
        assert!(binary.objects.is_empty());
        for (path, reason) in &omitted {
            let records: Vec<_> = index.assets.values().filter(|a| &a.path == path).collect();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].unavailable.as_ref(), Some(reason));
        }
        assert!(index.current());
        write(
            root.path(),
            "Three/ProjectSettings/ProjectVersion.txt",
            b"version\n",
        );
        write(
            root.path(),
            "Three/Assets/New.asset",
            b"--- !u!1 &1\nGameObject:\n  m_Name: New\n",
        );
        assert!(
            !index.current(),
            "A new sibling project invalidates the inventory"
        );
    }
}
