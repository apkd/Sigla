//! In-flight users pin managed views until durable ownership is published.
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::SystemTime,
};

const METADATA_KINDS: [&str; 2] = ["unity-metadata", "unity-metadata-packs"];
const LAST_USE: &str = "last-use";

pub(crate) fn is_metadata_root(cache: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(cache) else {
        return false;
    };
    let mut parts = relative.components();
    parts
        .next()
        .is_some_and(|kind| METADATA_KINDS.iter().any(|name| kind.as_os_str() == *name))
        && parts.next().is_some()
        && parts.next().is_none()
}

fn saved_metadata_usage(root: &Path) -> Result<Option<super::policy::Usage>> {
    match super::records::read(&root.join(LAST_USE), 1024 * 1024) {
        Ok(usage) => Ok(Some(usage)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn metadata_last_use(root: &Path) -> Result<u64> {
    Ok(metadata_usage_at(root)?.last_use)
}

fn metadata_usage_at(root: &Path) -> Result<super::policy::Usage> {
    if let Some(usage) = saved_metadata_usage(root)? {
        return Ok(usage);
    }
    let used = fs::metadata(root)?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_millis() as u64;
    Ok(super::policy::Usage::new(used))
}

pub(crate) fn metadata_complete(root: &Path) -> bool {
    if root
        .parent()
        .is_some_and(|parent| parent.ends_with("unity-metadata"))
    {
        root.join("contents/manifest.bin").is_file()
    } else {
        root.join("contents").is_dir()
    }
}

pub(crate) fn metadata_usage(cache: &Path) -> Result<BTreeMap<PathBuf, super::policy::Usage>> {
    let mut usage = BTreeMap::new();
    for kind in METADATA_KINDS {
        let entries = match fs::read_dir(cache.join(kind)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let root = entry?.path();
            if metadata_complete(&root) {
                match metadata_usage_at(&root) {
                    Ok(used) => {
                        usage.insert(root, used);
                    }
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(usage)
}

#[derive(Debug)]
pub struct Registry {
    cache: PathBuf,
    roots: Mutex<BTreeMap<PathBuf, usize>>,
    retirement: Mutex<()>,
    usage: Mutex<()>,
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

    /// Record successful reuse while retaining the view. Preserve frequency
    /// between maintenance passes without writing on every acquisition.
    pub(crate) fn record_metadata_use(&self, root: &Path) {
        if let Err(error) = self.record_metadata_use_at(root, super::now()) {
            tracing::warn!(root = %root.display(), %error, "Cannot record cached metadata use");
        }
    }

    fn record_metadata_use_at(&self, root: &Path, now: u64) -> Result<()> {
        let _usage = self.0.registry.usage.lock().unwrap();
        let mut usage = match saved_metadata_usage(root)? {
            Some(usage) if now.saturating_sub(usage.last_use) < 60_000 => return Ok(()),
            Some(usage) => usage,
            None => super::policy::Usage::new(now),
        };
        usage.record(now);
        super::records::write(&root.join(LAST_USE), &usage)?;
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
            usage: Mutex::new(()),
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
        self.try_retire_checked(root, || Ok(protected()))
    }

    pub(crate) fn try_retire_checked(
        &self,
        root: &Path,
        protected: impl FnOnce() -> Result<bool>,
    ) -> Result<Option<Retired>> {
        let root = normalized(root.parent().unwrap())?.join(root.file_name().unwrap());
        let roots = self.roots.lock().unwrap();
        if roots
            .keys()
            .any(|p| p.starts_with(&root) || root.starts_with(p))
            || protected()?
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
        Ok(super::cleanup::remove(&self.0)?)
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
    tracing::info!(root = %root.display(), "Retired managed cache view");
    Ok(Retired(slot))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_reuse_frequency_survives_between_ownership_scans() {
        let cache = tempfile::tempdir().unwrap();
        let leases = LeaseSet::new(cache.path()).unwrap();
        let repeated = cache.path().join("unity-metadata/repeated");
        let once = cache.path().join("unity-metadata/once");
        for root in [&repeated, &once] {
            leases.retain(root).unwrap();
            fs::create_dir_all(root.join("contents")).unwrap();
            fs::write(root.join("contents/manifest.bin"), b"cached").unwrap();
        }
        let hour = 60 * 60 * 1000;
        for at in [hour, 2 * hour, 3 * hour] {
            leases.record_metadata_use_at(&repeated, at).unwrap();
        }
        leases.record_metadata_use_at(&once, 3 * hour).unwrap();
        let usage = metadata_usage(cache.path()).unwrap();
        let mut owners = super::super::owners::Catalog::open(cache.path()).unwrap();
        owners.seed_metadata(&usage);
        owners.flush().unwrap();
        let owners = super::super::owners::Catalog::open(cache.path()).unwrap();
        assert_eq!(
            owners.entries[&repeated].usage.last_use,
            owners.entries[&once].usage.last_use
        );
        assert!(
            owners.entries[&repeated].usage.priority(3 * hour)
                > owners.entries[&once].usage.priority(3 * hour)
        );
    }
    #[test]
    fn metadata_usage_recovers_legacy_views_and_excludes_unfinished_downloads() {
        let cache = tempfile::tempdir().unwrap();
        let legacy = cache.path().join("unity-metadata/legacy");
        let pack = cache.path().join("unity-metadata-packs/pack");
        let partial = cache.path().join("unity-metadata/partial");
        fs::create_dir_all(legacy.join("contents")).unwrap();
        fs::write(legacy.join("contents/manifest.bin"), b"cached").unwrap();
        fs::create_dir_all(pack.join("contents")).unwrap();
        fs::create_dir_all(&partial).unwrap();
        fs::File::open(&legacy)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let usage = metadata_usage(cache.path()).unwrap();
        assert!(usage.contains_key(&legacy));
        assert!(usage.contains_key(&pack));
        assert!(!usage.contains_key(&partial));
        let leases = LeaseSet::new(cache.path()).unwrap();
        leases.retain(&legacy).unwrap();
        leases.record_metadata_use(&legacy);
        assert!(metadata_last_use(&legacy).unwrap() > usage[&legacy].last_use);
    }
    #[test]
    fn metadata_usage_is_read_without_a_write_and_updated_on_use() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("unity-metadata/editor");
        fs::create_dir_all(root.join("contents")).unwrap();
        fs::write(root.join("contents/manifest.bin"), b"cached").unwrap();
        let marker = root.join(LAST_USE);
        super::super::records::write(&marker, &super::super::policy::Usage::new(1)).unwrap();
        let before = fs::read(&marker).unwrap();
        assert_eq!(metadata_last_use(&root).unwrap(), 1);
        assert_eq!(fs::read(&marker).unwrap(), before);
        let leases = LeaseSet::new(cache.path()).unwrap();
        leases.retain(&root).unwrap();
        leases.record_metadata_use_at(&root, 120_000).unwrap();
        assert_eq!(metadata_last_use(&root).unwrap(), 120_000);
        assert_ne!(fs::read(&marker).unwrap(), before);
    }
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
        let work = path.join("generated/upper/custom/private");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("leftover"), b"scratch").unwrap();
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
