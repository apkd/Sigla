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
    "unity-metadata",
    "unity-metadata-packs",
    "editors",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Owner {
    pub entry: PathBuf,
    pub repository: Option<PathBuf>,
    pub usage: Usage,
    pub dependencies: BTreeSet<PathBuf>,
}
#[derive(Clone)]
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
        owner.dependencies = manifest
            .files
            .values()
            .chain(manifest.deferred.values())
            .map(|file| &file.path)
            .chain(manifest.metadata.keys())
            .chain(manifest.dependencies.iter())
            .filter_map(|path| {
                let mut components = path.strip_prefix(cache).ok()?.components();
                let kind = components.next()?;
                if VIEW_KINDS.iter().any(|view| kind.as_os_str() == *view) {
                    Some(cache.join(kind).join(components.next()?))
                } else if kind.as_os_str() == "dotnet"
                    && components.next()?.as_os_str() == "packages"
                {
                    Some(cache.join("dotnet/packages"))
                } else {
                    None
                }
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
    pub(crate) fn record_history(&mut self, entry: &Path, now: u64) {
        self.seed(entry, None, now);
        self.entries.get_mut(entry).unwrap().usage.record(now);
        self.dirty = true;
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
        if crate::repository::cache::history_owner(cache, &owner.entry) {
            roots.insert(owner.entry.parent().unwrap().to_owned());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{FileEntry, Manifest, Stamp};

    #[test]
    fn observe_tracks_view_roots_across_manifest_paths_and_component_boundaries() {
        let cache = tempfile::tempdir().unwrap();
        let source = tempfile::NamedTempFile::new().unwrap();
        let stamp = Stamp::read(source.path()).unwrap();
        let file = |path: PathBuf| FileEntry {
            display: path.to_string_lossy().into_owned(),
            path,
            stamp: stamp.clone(),
            language: crate::model::Language::Cpp,
            memberships: Vec::new(),
            modules: Vec::new(),
            metadata: false,
        };
        let entry = cache.path().join("workspace");
        let mut manifest = Manifest {
            root: entry.clone(),
            ..Manifest::default()
        };
        let mut catalog = Catalog::open(cache.path()).unwrap();
        catalog.observe(&entry, None, &manifest, None);
        let mut expected = catalog.entries[&entry].dependencies.clone();
        for (i, kind) in VIEW_KINDS.iter().enumerate() {
            let root = cache.path().join(kind).join("owned");
            let path = root.join("nested/source.cpp");
            match i % 4 {
                0 => {
                    manifest.files.insert(kind.to_string(), file(path));
                }
                1 => {
                    manifest.deferred.insert(kind.to_string(), file(path));
                }
                2 => {
                    manifest.metadata.insert(path, stamp.clone());
                }
                _ => {
                    manifest.dependencies.insert(path);
                }
            }
            expected.insert(root);
            manifest.dependencies.extend([
                cache.path().join(kind),
                cache.path().join(format!("{kind}-other/ignored")),
                cache
                    .path()
                    .with_extension("other")
                    .join(kind)
                    .join("ignored"),
            ]);
        }
        manifest.dependencies.extend([
            cache.path().join("dotnet/packages-other/ignored"),
            cache.path().join("dotnet-other/packages/ignored"),
        ]);
        let legacy = cache.path().join("dotnet/packages");
        expected.insert(legacy.clone());
        for path in [legacy.clone(), legacy.join("package/source.cpp")] {
            manifest.dependencies.insert(path.clone());
            catalog.observe(&entry, None, &manifest, None);
            assert_eq!(catalog.entries[&entry].dependencies, expected);
            assert!(expected.iter().all(|root| catalog.retains(root)));
            manifest.dependencies.remove(&path);
        }
    }

    #[test]
    fn observe_replaces_dependencies_without_changing_usage_and_persists_removal() {
        let cache = tempfile::tempdir().unwrap();
        let entry = cache.path().join("workspace");
        let repository = cache.path().join("repository");
        let mut manifest = Manifest {
            root: entry.clone(),
            ..Manifest::default()
        };
        let mut catalog = Catalog::open(cache.path()).unwrap();
        let used = 100;
        catalog.observe(&entry, None, &manifest, Some(used));
        let mut expected = catalog.entries[&entry].dependencies.clone();
        let old = cache.path().join(VIEW_KINDS[0]).join("old");
        manifest.dependencies.insert(old.join("source.cpp"));
        catalog.observe(&entry, None, &manifest, None);
        assert!(catalog.retains(&old));
        let new = cache.path().join(VIEW_KINDS[0]).join("new");
        manifest.dependencies = BTreeSet::from([new.join("source.cpp")]);
        catalog.observe(&entry, Some(&repository), &manifest, None);
        expected.insert(new.clone());
        assert_eq!(catalog.entries[&entry].dependencies, expected);
        assert!(!catalog.retains(&old));
        assert!(catalog.retains(&new));
        assert_eq!(catalog.entries[&entry].usage.last_use, used);
        assert_eq!(
            catalog.entries[&entry].repository.as_ref(),
            Some(&repository)
        );
        assert!(catalog.dirty);
        catalog.flush().unwrap();
        assert!(!catalog.dirty);
        let mut reopened = Catalog::open(cache.path()).unwrap();
        assert_eq!(
            reopened.entries[&entry].dependencies,
            catalog.entries[&entry].dependencies
        );
        reopened.remove(&entry);
        assert!(reopened.dirty);
        assert!(!reopened.retains(&new));
        reopened.flush().unwrap();
        assert!(Catalog::open(cache.path()).unwrap().entries.is_empty());
    }
}
