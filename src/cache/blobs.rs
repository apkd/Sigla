//! Immutable file storage. Only private, stopped job outputs may be imported.
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

pub type Id = [u8; 32];
const ATTRIBUTE: &std::ffi::CStr = c"user.sigla.blob";
static OPEN: LazyLock<Mutex<BTreeMap<PathBuf, Weak<Store>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

pub struct Store {
    root: PathBuf,
}

/// The shared filesystem lock also covers helper processes and incomplete views.
pub struct Lease {
    _file: File,
}

fn regular(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "Expected a regular cache input");
    Ok(file)
}

impl Store {
    pub fn open(cache: &Path) -> Result<Arc<Self>> {
        fs::create_dir_all(cache.join("blobs"))?;
        let root = cache.join("blobs").canonicalize()?;
        let mut opened = OPEN.lock().unwrap();
        opened.retain(|_, store| store.strong_count() != 0);
        if let Some(store) = opened.get(&root).and_then(Weak::upgrade) {
            return Ok(store);
        }
        let store = Arc::new(Self { root: root.clone() });
        opened.insert(root, Arc::downgrade(&store));
        Ok(store)
    }

    pub fn cache(&self) -> &Path {
        self.root.parent().unwrap()
    }

    pub fn lease(&self) -> Result<Lease> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join("ownership.lock"))?;
        file.lock_shared()?;
        Ok(Lease { _file: file })
    }

    fn path(&self, id: &Id) -> PathBuf {
        let name = blake3::Hash::from_bytes(*id).to_hex();
        self.root.join(&name[..2]).join(&name[2..])
    }

    /// Replace a private file with a link to the immutable copy. Hold a store lease.
    pub fn import(&self, path: &Path) -> Result<Id> {
        let mut input = regular(path)?;
        let metadata = input.metadata()?;
        if let Some(id) = self.identity(path, &metadata)? {
            return Ok(id);
        }
        let mut stage = tempfile::Builder::new()
            .prefix("blob-pending-")
            .tempfile_in(&self.root)?;
        let mode = metadata.mode() & 0o777;
        let mut hash = blake3::Hasher::new();
        hash.update(b"sigla-file-1\0");
        hash.update(&mode.to_le_bytes());
        let mut bytes = [0; 128 * 1024];
        loop {
            let n = input.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
            stage.write_all(&bytes[..n])?;
        }
        let id = *hash.finalize().as_bytes();
        let destination = self.path(&id);
        fs::create_dir_all(destination.parent().unwrap())?;
        stage
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
        let name = CString::new(stage.path().as_os_str().as_bytes())?;
        // The attribute is only trusted after checking the canonical blob's inode.
        ensure!(
            unsafe {
                libc::setxattr(
                    name.as_ptr(),
                    ATTRIBUTE.as_ptr(),
                    id.as_ptr().cast(),
                    id.len(),
                    0,
                )
            } == 0,
            "Cannot mark immutable cache input: {}",
            std::io::Error::last_os_error()
        );
        stage.as_file().sync_all()?;
        match stage.persist_noclobber(&destination) {
            Ok(_) => {
                File::open(destination.parent().unwrap())?.sync_all()?;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let parent = path.parent().context("Cache input has no parent")?;
        let temporary = tempfile::tempdir_in(parent)?;
        fs::hard_link(&destination, temporary.path().join("file"))?;
        fs::rename(temporary.path().join("file"), path)?;
        Ok(id)
    }

    pub fn import_tree(&self, root: &Path) -> Result<()> {
        let _lease = self.lease()?;
        self.visit(root)
    }
    pub fn link_tree(&self, source: &Path, destination: &Path) -> Result<()> {
        let _lease = self.lease()?;
        fn visit(source: &Path, destination: &Path) -> Result<()> {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let target = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    visit(&entry.path(), &target)?;
                } else {
                    ensure!(entry.file_type()?.is_file(), "Invalid immutable input");
                    if !target.exists() {
                        fs::hard_link(entry.path(), target)?;
                    }
                }
            }
            File::open(destination)?.sync_all()?;
            Ok(())
        }
        visit(source, destination)
    }

    fn visit(&self, root: &Path) -> Result<()> {
        ensure!(
            fs::symlink_metadata(root)?.is_dir(),
            "Invalid managed input directory"
        );
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                self.visit(&entry.path())?;
            } else {
                ensure!(
                    kind.is_file(),
                    "Managed input is not a regular file: {}",
                    entry.path().display()
                );
                self.import(&entry.path())?;
            }
        }
        File::open(root)?.sync_all()?;
        Ok(())
    }

    fn identity(&self, path: &Path, metadata: &fs::Metadata) -> Result<Option<Id>> {
        if !metadata.is_file() || metadata.nlink() < 2 || !path.starts_with(self.cache()) {
            return Ok(None);
        }
        let path = CString::new(path.as_os_str().as_bytes())?;
        let mut id = [0; 32];
        let size = unsafe {
            libc::getxattr(
                path.as_ptr(),
                ATTRIBUTE.as_ptr(),
                id.as_mut_ptr().cast(),
                id.len(),
            )
        };
        if size != id.len() as isize {
            return Ok(None);
        }
        match fs::symlink_metadata(self.path(&id)) {
            Ok(blob)
                if blob.is_file()
                    && blob.dev() == metadata.dev()
                    && blob.ino() == metadata.ino() =>
            {
                Ok(Some(id))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Canonical links with no retained views are disposable. Never race publication.
    pub fn collect(&self) -> Result<u64> {
        self.collect_with_pressure(false)
    }

    pub fn collect_with_pressure(&self, pressure: bool) -> Result<u64> {
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join("ownership.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let mut bytes = 0;
        let downloads = self.cache().join("lfs");
        if downloads.is_dir() {
            for entry in fs::read_dir(downloads)? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                let named = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
                let unused = metadata.nlink() == 1
                    || metadata.nlink() == 2 && self.identity(&entry.path(), &metadata)?.is_some();
                let old = metadata
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age >= crate::config::DEFAULT_REPO_TTL);
                if named && metadata.is_file() && unused && (pressure || old) {
                    if metadata.nlink() == 1 {
                        bytes += metadata.blocks() * 512;
                    }
                    fs::remove_file(entry.path())?;
                }
            }
        }
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if shard.file_type()?.is_file()
                && shard
                    .file_name()
                    .to_string_lossy()
                    .starts_with("blob-pending-")
            {
                fs::remove_file(shard.path())?;
                continue;
            }
            if !shard.file_type()?.is_dir() || shard.file_name().len() != 2 {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                if metadata.is_file() && metadata.nlink() == 1 {
                    bytes += metadata.blocks() * 512;
                    fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(bytes)
    }
}

pub fn identity(path: &Path, metadata: &fs::Metadata) -> Result<Option<Id>> {
    let stores: Vec<_> = OPEN
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    for store in stores {
        if let Some(id) = store.identity(path, metadata)? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Physical allocation, rather than the sum of each hardlinked view's apparent size.
pub fn allocated(root: &Path) -> Result<u64> {
    fn visit(root: &Path, seen: &mut BTreeSet<(u64, u64)>) -> Result<u64> {
        let metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            // Background transfers can publish or remove staging files during a scan.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        if !seen.insert((metadata.dev(), metadata.ino())) {
            return Ok(0);
        }
        let mut bytes = metadata.blocks() * 512;
        if metadata.is_dir() {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(bytes),
                // Overlayfs' empty private work directory is mode 000 after unmounting.
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && root.ends_with("work") =>
                {
                    return Ok(bytes);
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                bytes += visit(&entry?.path(), seen)?;
            }
        }
        Ok(bytes)
    }
    visit(root, &mut BTreeSet::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn views_share_bytes_and_collection_waits_for_the_last_view() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let a = cache.path().join("a");
        let b = cache.path().join("b");
        fs::write(&a, vec![7; 8192]).unwrap();
        fs::write(&b, vec![7; 8192]).unwrap();
        let lease = store.lease().unwrap();
        assert_eq!(store.import(&a).unwrap(), store.import(&b).unwrap());
        assert_eq!(
            fs::metadata(&a).unwrap().ino(),
            fs::metadata(&b).unwrap().ino()
        );
        drop(lease);
        assert_eq!(store.collect().unwrap(), 0);
        fs::remove_file(a).unwrap();
        assert_eq!(store.collect().unwrap(), 0);
        fs::remove_file(b).unwrap();
        assert!(store.collect().unwrap() > 0);
    }
    #[test]
    fn contents_and_permissions_define_identity() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let a = cache.path().join("a");
        let b = cache.path().join("b");
        fs::write(&a, b"first").unwrap();
        fs::write(&b, b"second").unwrap();
        let _lease = store.lease().unwrap();
        assert_ne!(store.import(&a).unwrap(), store.import(&b).unwrap());
        fs::remove_file(&b).unwrap();
        fs::write(&b, b"first").unwrap();
        fs::set_permissions(&b, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(store.import(&a).unwrap(), store.import(&b).unwrap());
    }
    #[test]
    fn changing_links_does_not_change_immutable_input_revisions() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let file = cache.path().join("source");
        fs::write(&file, b"class Example {}").unwrap();
        let lease = store.lease().unwrap();
        store.import(&file).unwrap();
        let before = crate::workspace::Stamp::read(&file).unwrap();
        let other = cache.path().join("other");
        fs::hard_link(&file, &other).unwrap();
        assert_eq!(crate::workspace::Stamp::read(&file).unwrap(), before);
        fs::remove_file(other).unwrap();
        drop(lease);
        store.collect().unwrap();
        assert_eq!(crate::workspace::Stamp::read(&file).unwrap(), before);
    }

    #[test]
    fn pressure_collects_lfs_only_after_download_and_source_leases_end() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        fs::create_dir(cache.path().join("lfs")).unwrap();
        let cached = cache.path().join("lfs").join("a".repeat(64));
        let source = cache.path().join("source");
        fs::write(&cached, vec![7; 8192]).unwrap();
        let lease = store.lease().unwrap();
        store.import(&cached).unwrap();
        assert_eq!(store.collect_with_pressure(true).unwrap(), 0);
        assert!(cached.exists());
        fs::hard_link(&cached, &source).unwrap();
        drop(lease);
        assert_eq!(store.collect_with_pressure(true).unwrap(), 0);
        assert!(cached.exists());
        fs::remove_file(source).unwrap();
        assert!(store.collect_with_pressure(true).unwrap() > 0);
        assert!(!cached.exists());
    }
}
