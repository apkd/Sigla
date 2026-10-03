//! Usage and dependency ownership. Only this small catalog is persisted separately.
use super::policy::Usage;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub const VIEW_KINDS: &[&str] = &[
    "local-jobs",
    "local-packages",
    "unity-assets",
    "unity-packages",
    "editors",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Owner {
    pub entry: PathBuf,
    pub repository: Option<PathBuf>,
    pub usage: Usage,
    pub dependencies: BTreeSet<PathBuf>,
}
pub struct Catalog {
    path: PathBuf,
    pub entries: BTreeMap<PathBuf, Owner>,
    dirty: bool,
}
impl Catalog {
    pub fn open(cache: &Path) -> Result<Self> {
        let path = cache.join("owners.json");
        let entries = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            entries,
            dirty: false,
        })
    }
    pub fn observe(
        &mut self,
        entry: &Path,
        repository: Option<&Path>,
        manifest: &crate::workspace::Manifest,
        used: Option<u64>,
    ) {
        let initial = used.unwrap_or_else(super::now);
        let owner = self
            .entries
            .entry(entry.to_owned())
            .or_insert_with(|| Owner {
                entry: entry.to_owned(),
                repository: repository.map(Path::to_owned),
                usage: Usage::new(initial),
                dependencies: BTreeSet::new(),
            });
        owner.repository = repository.map(Path::to_owned);
        let cache = self.path.parent().unwrap();
        let views: Vec<_> = VIEW_KINDS.iter().map(|kind| cache.join(kind)).collect();
        let legacy = cache.join("dotnet/packages");
        owner.dependencies = manifest
            .files
            .values()
            .chain(manifest.deferred.values())
            .map(|file| &file.path)
            .chain(manifest.metadata.keys())
            .chain(manifest.dependencies.iter())
            .filter_map(|path| {
                for view in &views {
                    if let Ok(relative) = path.strip_prefix(view)
                        && let Some(first) = relative.components().next()
                    {
                        return Some(view.join(first));
                    }
                }
                path.starts_with(&legacy).then(|| legacy.clone())
            })
            .collect();
        owner.dependencies.insert(
            cache.join("local-jobs").join(
                blake3::hash(entry.as_os_str().as_encoded_bytes())
                    .to_hex()
                    .as_str(),
            ),
        );
        owner.dependencies.insert(
            cache.join("unity-assets").join(
                blake3::hash(manifest.root.as_os_str().as_encoded_bytes())
                    .to_hex()
                    .as_str(),
            ),
        );
        if let Some(now) = used {
            owner.usage.record(now);
        }
        self.dirty = true;
    }
    pub fn remove(&mut self, entry: &Path) {
        self.entries.remove(entry);
        self.dirty = true;
    }
    pub fn seed(&mut self, entry: &Path, repository: Option<&Path>, at: u64) {
        if !self.entries.contains_key(entry) {
            self.entries.insert(
                entry.to_owned(),
                Owner {
                    entry: entry.to_owned(),
                    repository: repository.map(Path::to_owned),
                    usage: Usage::new(at),
                    dependencies: BTreeSet::new(),
                },
            );
            self.dirty = true;
        }
    }
    pub fn flush(&mut self) -> Result<()> {
        if self.dirty {
            crate::repository::manager::write_json(&self.path, &self.entries)?;
            self.dirty = false;
        }
        Ok(())
    }
    pub fn retains(&self, path: &Path) -> bool {
        self.entries.values().any(|owner| {
            owner
                .dependencies
                .iter()
                .any(|dependency| dependency.starts_with(path))
        })
    }
    pub fn view_roots(&self, owner: &Owner) -> Vec<PathBuf> {
        let cache = self.path.parent().unwrap();
        let mut roots = BTreeSet::new();
        for path in &owner.dependencies {
            for kind in VIEW_KINDS {
                if let Ok(relative) = path.strip_prefix(cache.join(kind))
                    && let Some(first) = relative.components().next()
                {
                    let root = cache.join(kind).join(first);
                    if root.is_dir() {
                        roots.insert(root);
                    }
                }
            }
        }
        roots.into_iter().collect()
    }
}
