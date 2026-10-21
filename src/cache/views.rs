//! In-flight users pin managed views until durable ownership is published.
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Debug)]
pub struct Registry {
    cache: PathBuf,
    roots: Mutex<BTreeMap<PathBuf, usize>>,
    retirement: Mutex<()>,
}

#[derive(Clone, Debug)]
pub struct LeaseSet(Arc<Leases>);

#[derive(Debug)]
struct Leases {
    registry: Arc<Registry>,
    alias: PathBuf,
    roots: Mutex<BTreeSet<PathBuf>>,
}

impl LeaseSet {
    pub fn new(cache: &Path) -> Result<Self> {
        Ok(Self(Arc::new(Leases {
            registry: Registry::open(cache)?,
            alias: std::path::absolute(cache)?,
            roots: Mutex::new(BTreeSet::new()),
        })))
    }

    /// Call before probing or creating a view, including on cache hits.
    pub fn retain(&self, path: &Path) -> Result<()> {
        let path = if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            normalized(path)?
        } else {
            std::path::absolute(path)?
        };
        let Some(root) = managed_root(&self.0.registry.cache, &path)
            .or_else(|| managed_root(&self.0.alias, &path))
        else {
            return Ok(());
        };
        let root = normalized(root.parent().unwrap())?.join(root.file_name().unwrap());
        let mut retained = self.0.roots.lock().unwrap();
        if retained.insert(root.clone()) {
            *self
                .0
                .registry
                .roots
                .lock()
                .unwrap()
                .entry(root)
                .or_default() += 1;
        }
        Ok(())
    }
}

impl Drop for Leases {
    fn drop(&mut self) {
        let mut roots = self.registry.roots.lock().unwrap();
        for path in self.roots.get_mut().unwrap().iter() {
            let count = roots.get_mut(path).unwrap();
            *count -= 1;
            if *count == 0 {
                roots.remove(path);
            }
        }
    }
}

pub fn managed_root(cache: &Path, path: &Path) -> Option<PathBuf> {
    let mut components = path.strip_prefix(cache).ok()?.components();
    let kind = components.next()?;
    if super::owners::VIEW_KINDS
        .iter()
        .any(|view| kind.as_os_str() == *view)
    {
        Some(cache.join(kind).join(components.next()?))
    } else if kind.as_os_str() == "dotnet" && components.next()?.as_os_str() == "packages" {
        Some(cache.join("dotnet/packages"))
    } else {
        None
    }
}

fn normalized(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut suffix = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        match existing.canonicalize() {
            Ok(mut root) => {
                for component in suffix.into_iter().rev() {
                    root.push(component);
                }
                return Ok(root);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(existing.file_name().ok_or(error)?);
                existing = existing.parent().unwrap();
            }
            Err(error) => return Err(error.into()),
        }
    }
}

impl Registry {
    pub fn open(cache: &Path) -> Result<Arc<Self>> {
        static REGISTRIES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Registry>>>> = OnceLock::new();
        fs::create_dir_all(cache)?;
        let cache = cache.canonicalize()?;
        let mut registries = REGISTRIES.get_or_init(Default::default).lock().unwrap();
        registries.retain(|_, registry| registry.strong_count() > 0);
        if let Some(registry) = registries.get(&cache).and_then(Weak::upgrade) {
            return Ok(registry);
        }
        let registry = Arc::new(Self {
            cache: cache.clone(),
            roots: Mutex::new(BTreeMap::new()),
            retirement: Mutex::new(()),
        });
        registries.insert(cache, Arc::downgrade(&registry));
        Ok(registry)
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().unwrap().keys().cloned().collect()
    }

    /// Lock order is registry, then the owner's catalog inside `protected`.
    pub fn try_retire(
        &self,
        root: &Path,
        protected: impl FnOnce() -> bool,
    ) -> Result<Option<Retired>> {
        let root = normalized(root.parent().unwrap())?.join(root.file_name().unwrap());
        let roots = self.roots.lock().unwrap();
        if roots
            .keys()
            .any(|p| p.starts_with(&root) || root.starts_with(p))
            || protected()
        {
            return Ok(None);
        }
        match retire(&self.cache, &root) {
            Ok(retired) => Ok(Some(retired)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn pending(&self) -> Result<Vec<Retired>> {
        let _publication = self.retirement.lock().unwrap();
        let directory = self.cache.join("retired-views");
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        entries.map(|entry| Ok(Retired(entry?.path()))).collect()
    }
}

#[derive(Debug)]
pub struct Retired(PathBuf);
impl Retired {
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn dispose(self) -> Result<()> {
        let generated = self.0.join("contents/generated");
        if fs::symlink_metadata(&generated).is_ok_and(|metadata| metadata.is_dir()) {
            crate::sandbox::remove_outputs(&generated)?;
        }
        crate::sandbox::remove_outputs(&self.0.join("contents"))?;
        crate::sandbox::remove_outputs(&self.0)
    }
}

/// Caller must exclude all acquisitions of `root` until this rename completes.
pub fn retire(cache: &Path, root: &Path) -> Result<Retired> {
    let registry = Registry::open(cache)?;
    let _publication = registry.retirement.lock().unwrap();
    anyhow::ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "Managed view is not a directory"
    );
    let directory = cache.join("retired-views");
    fs::create_dir_all(&directory)?;
    let slot = tempfile::Builder::new()
        .prefix("view-")
        .tempdir_in(&directory)?;
    let destination = slot.path().join("contents");
    fs::rename(root, &destination)?;
    let slot = slot.keep();
    fs::File::open(root.parent().unwrap())?.sync_all()?;
    fs::File::open(&slot)?.sync_all()?;
    fs::File::open(&directory)?.sync_all()?;
    Ok(Retired(slot))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leases_cover_clones_aliases_and_reacquisition() {
        let cache = tempfile::tempdir().unwrap();
        let aliases = tempfile::tempdir().unwrap();
        let alias = aliases.path().join("cache");
        std::os::unix::fs::symlink(cache.path(), &alias).unwrap();
        let path = cache.path().join("unity-packages/package");
        let leases = LeaseSet::new(&alias).unwrap();
        leases
            .retain(&alias.join("unity-packages/package/contents/file"))
            .unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("old"), b"old").unwrap();
        let registry = Registry::open(cache.path()).unwrap();
        let clone = leases.clone();
        drop(leases);
        assert!(registry.try_retire(&path, || false).unwrap().is_none());
        drop(clone);
        assert!(registry.try_retire(&path, || true).unwrap().is_none());
        let retired = registry.try_retire(&path, || false).unwrap().unwrap();
        let next = LeaseSet::new(cache.path()).unwrap();
        next.retain(&path).unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("new"), b"new").unwrap();
        retired.dispose().unwrap();
        assert!(path.join("new").is_file());
        assert!(!path.join("old").exists());
    }
    #[test]
    fn abandoned_retirement_is_discovered() {
        use std::os::unix::fs::PermissionsExt;
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("unity-metadata/old");
        let work = path.join("generated/package-work/work");
        fs::create_dir_all(&work).unwrap();
        fs::set_permissions(&work, fs::Permissions::from_mode(0o000)).unwrap();
        drop(retire(cache.path(), &path).unwrap());
        let registry = Registry::open(cache.path()).unwrap();
        for retired in registry.pending().unwrap() {
            retired.dispose().unwrap();
        }
        assert!(registry.pending().unwrap().is_empty());
    }

    #[test]
    fn acquisition_waits_until_retirement_has_detached_the_old_view() {
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("unity-assets/view");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("old"), b"old").unwrap();
        let registry = Registry::open(cache.path()).unwrap();
        let leases = LeaseSet::new(cache.path()).unwrap();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let retiring_path = &path;
            let retiring = scope.spawn(move || {
                registry
                    .try_retire(retiring_path, || {
                        locked_tx.send(()).unwrap();
                        started_rx.recv().unwrap();
                        false
                    })
                    .unwrap()
                    .unwrap()
            });
            locked_rx.recv().unwrap();
            started_tx.send(()).unwrap();
            leases.retain(&path).unwrap();
            assert!(!path.exists());
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("new"), b"new").unwrap();
            retiring.join().unwrap().dispose().unwrap();
            assert!(path.join("new").exists());
        });
    }

    #[test]
    fn concurrent_disposal_preserves_reacquired_view() {
        use std::os::unix::fs::PermissionsExt;
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("unity-packages/old");
        let work = path.join("generated/package-work/work");
        fs::create_dir_all(&work).unwrap();
        fs::set_permissions(&work, fs::Permissions::from_mode(0o000)).unwrap();
        let retired = retire(cache.path(), &path).unwrap();
        let registry = Registry::open(cache.path()).unwrap();
        let pending = registry.pending().unwrap().pop().unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("current.cs"), "class Current {}").unwrap();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                start.wait();
                retired.dispose()
            });
            start.wait();
            pending.dispose().unwrap();
            first.join().unwrap().unwrap();
        });
        assert!(registry.pending().unwrap().is_empty());
        assert!(path.join("current.cs").exists());
    }

    #[test]
    fn retirement_rejects_links_to_external_inputs() {
        let cache = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let path = cache.path().join("unity-packages/link");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(external.path(), &path).unwrap();
        let registry = Registry::open(cache.path()).unwrap();
        assert!(registry.try_retire(&path, || false).is_err());
        assert!(external.path().is_dir());
        assert!(path.is_symlink());
    }
}
