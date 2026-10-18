//! Unity object analysis is separate from source-code indexing.
mod compose;
pub mod jobs;
mod native;
mod parse;
mod query;
mod records;
mod snapshot;
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ScriptType {
    pub name: String,
    pub assembly: String,
    pub ancestry: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub scripts: BTreeMap<String, std::sync::Arc<ScriptType>>,
    pub instances: BTreeMap<String, std::sync::Arc<compose::Composed>>,
    native_types: BTreeMap<String, BTreeMap<i32, native::Resolved>>,
    observed: BTreeMap<PathBuf, Stamp>,
    retained_bytes: usize,
}
impl Index {
    fn current(&self) -> bool {
        self.observed
            .iter()
            .all(|(path, stamp)| Stamp::read(path).as_ref().ok() == Some(stamp))
    }
    fn measure(&mut self) {
        // Count shared object/type allocations once across prefab instances.
        let mut objects = std::collections::HashSet::new();
        let mut types = std::collections::HashSet::new();
        let mut issues = std::collections::HashSet::new();
        let mut type_bytes = 0;
        let mut issue_bytes = 0;
        let mut count_shared =
            |ty: Option<&std::sync::Arc<ScriptType>>, issue: Option<&std::sync::Arc<str>>| {
                if let Some(ty) = ty
                    && types.insert(std::sync::Arc::as_ptr(ty))
                {
                    type_bytes += 2 * std::mem::size_of::<usize>()
                        + std::mem::size_of::<ScriptType>()
                        + ty.name.capacity()
                        + ty.assembly.capacity()
                        + ty.ancestry.capacity() * std::mem::size_of::<String>()
                        + ty.ancestry.iter().map(String::capacity).sum::<usize>();
                }
                if let Some(issue) = issue
                    && issues.insert(std::sync::Arc::as_ptr(issue))
                {
                    issue_bytes += 2 * std::mem::size_of::<usize>() + issue.len();
                }
            };
        let mut bytes = crate::memory::encoded_size(&(&self.guids, &self.observed)) * 3;
        for (key, ty) in &self.scripts {
            bytes += 128 + key.capacity();
            count_shared(Some(ty), None);
        }
        for (project, resolved) in &self.native_types {
            bytes += 128 + project.capacity();
            for native in resolved.values() {
                bytes += 64;
                count_shared(native.ty.as_ref(), native.issue.as_ref());
            }
        }
        for (key, asset) in &self.assets {
            bytes += 256 + key.len() + asset.project.len() + asset.path.len() + asset.guid.len();
            for object in &asset.objects {
                if objects.insert(std::sync::Arc::as_ptr(object)) {
                    bytes += crate::memory::encoded_size(object) * 3;
                }
            }
        }
        for (key, composed) in &self.instances {
            bytes += 128
                + key.len()
                + crate::memory::encoded_size(&(&composed.aliases, &composed.incomplete)) * 3;
            for instance in &composed.objects {
                bytes += 256
                    + crate::memory::encoded_size(&(
                        &instance.id,
                        &instance.definition,
                        &instance.name,
                        &instance.references,
                        &instance.evidence,
                    )) * 3;
                count_shared(instance.ty.as_ref(), instance.native_issue.as_ref());
                if objects.insert(std::sync::Arc::as_ptr(&instance.object)) {
                    bytes += crate::memory::encoded_size(&instance.object) * 3;
                }
            }
        }
        self.retained_bytes = bytes + type_bytes + issue_bytes;
        tracing::debug!(
            retained_bytes = self.retained_bytes,
            shared_types = types.len(),
            shared_issues = issues.len(),
            type_bytes,
            issue_bytes,
            "Unity asset allocations measured"
        );
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
    fn script(&self, asset: &Asset, object: &Object) -> Option<&std::sync::Arc<ScriptType>> {
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
    pub previous: Option<&'a Index>,
    pub records: Option<&'a crate::cache::artifacts::Store>,
    pub remote: bool,
    pub omitted: &'a BTreeMap<String, String>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

type ScriptTypes = BTreeMap<(PathBuf, PathBuf), Vec<ScriptType>>;
fn script_types(
    root: &Path,
    cache: &Path,
    projects: &[PathBuf],
    manifest: &Manifest,
    store: &Store,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ScriptTypes> {
    let files: Vec<_> = manifest
        .files
        .iter()
        .filter(|(_, entry)| {
            if entry.language != Language::CSharp || entry.metadata {
                return false;
            }
            let mut meta = entry.path.as_os_str().to_os_string();
            meta.push(".meta");
            Path::new(&meta).is_file()
        })
        .collect();
    let key = *blake3::hash(&postcard::to_allocvec(&(
        env!("SIGLA_ASSET_FINGERPRINT"),
        manifest.environment,
        projects,
        files.iter().map(|(key, _)| key).collect::<Vec<_>>(),
    ))?)
    .as_bytes();
    let path = cache.join("script-types.zst");
    if let Ok(file) = std::fs::File::open(&path)
        && let Ok(bytes) = zstd::stream::decode_all(file)
        && let Ok((saved, types)) = postcard::from_bytes::<([u8; 32], ScriptTypes)>(&bytes)
        && saved == key
    {
        return Ok(types);
    }
    let mut type_files = BTreeMap::<(PathBuf, PathBuf), Vec<ScriptType>>::new();
    let types_started = std::time::Instant::now();
    for (file, entry) in files {
        ensure!(
            !cancel.is_cancelled(),
            "Asset indexing cancelled or superseded"
        );
        // Each source file is an independent binding query with its own budget.
        let mut binder = crate::csharp::bind::Binder::default();
        let stem = entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let tx = store.read()?;
        let view = crate::csharp::catalog::View {
            store,
            tx: &tx,
            manifest,
            cancel,
        };
        for id in store.csharp_lookup(&tx, file, stem, crate::store::DeclarationLookup::Type)? {
            let facts = store
                .csharp_declaration(&tx, file, id)?
                .context("Missing script declaration")?;
            let declaration = &facts.declarations[0];
            if declaration.kind != "class" || declaration.name != stem {
                continue;
            }
            for membership in &entry.memberships {
                let ancestry = binder.ancestry(
                    &view,
                    &crate::csharp::catalog::Site {
                        file,
                        project: membership.project,
                        declaration,
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
    records::save(&path, &(key, &type_files))?;
    Ok(type_files)
}

pub fn build(
    root: &Path,
    cache: &Path,
    manifest: &Manifest,
    store: &Store,
    options: BuildOptions<'_>,
) -> Result<Index> {
    let BuildOptions {
        previous,
        records: shared_records,
        remote,
        omitted,
        cancel,
    } = options;
    let snapshot = store.snapshot()?;
    let store = &*snapshot;
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    std::fs::create_dir_all(cache)?;
    let mut projects = Vec::new();
    let mut observed = BTreeMap::new();
    roots(root, &mut projects, &mut observed, 0)?;
    let mut index = Index::default();
    let type_files = script_types(root, cache, &projects, manifest, store, cancel)?;
    let assets_started = std::time::Instant::now();
    for project in &projects {
        let project_name = project.strip_prefix(root)?.to_string_lossy().to_string();
        let project_name = if project_name.is_empty() {
            ".".into()
        } else {
            project_name
        };
        // Rewriting this file need not change its parent directory's stamp.
        let version_file = project.join("ProjectSettings/ProjectVersion.txt");
        observed.insert(version_file.clone(), Stamp::read(&version_file)?);
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
        let mut records = records::load_all(cache, shared_records, &inputs, cancel)?;
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
                    index
                        .scripts
                        .insert(asset_key, std::sync::Arc::new(types[0].clone()));
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
        let classes = index
            .assets
            .values()
            .filter(|asset| asset.project == project_name)
            .flat_map(|asset| asset.objects.iter().map(|object| object.class))
            .collect();
        let native_tx = store.read()?;
        index.native_types.insert(
            project_name,
            native::build(project, &classes, manifest, store, &native_tx, cancel)?,
        );
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
    if let Some(previous) = previous {
        compose::reuse(&mut index, previous);
    }
    compose::build(&mut index)?;
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    ensure!(
        index.current(),
        "Unity inputs changed during indexing; retry query"
    );
    index.measure();
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
        store.save_manifest(&Manifest::default()).unwrap();
        let build_at = |path: &Path| {
            build(
                path,
                &cache.path().join("assets"),
                &Manifest::default(),
                &store,
                BuildOptions {
                    previous: None,
                    records: None,
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
        store.save_manifest(&Manifest::default()).unwrap();
        let index = build(
            root.path(),
            &cache.path().join("assets"),
            &Manifest::default(),
            &store,
            BuildOptions {
                previous: None,
                records: None,
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
