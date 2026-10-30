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
    time::{Duration, Instant},
};

pub type Id = [u8; 32];

pub(crate) enum Linked {
    Missing,
    Oversized(u64),
    File(u64),
}

/// Match ordinary create_new file permissions without changing the process umask.
pub(crate) fn creation_mode(directory: &Path) -> Result<u32> {
    let temporary = tempfile::tempdir_in(directory)?;
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(temporary.path().join("mode"))?;
    Ok(file.metadata()?.mode() & 0o777)
}
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

#[derive(Default)]
pub(crate) struct ImportStats {
    pub files: u64,
    pub hashed_bytes: u64,
    pub adopted: u64,
    pub reused: u64,
    pub fallback: u64,
    pub read_hash: Duration,
    pub publication: Duration,
    pub sync: Duration,
}

impl ImportStats {
    fn add(&mut self, other: Self) {
        self.files += other.files;
        self.hashed_bytes += other.hashed_bytes;
        self.adopted += other.adopted;
        self.reused += other.reused;
        self.fallback += other.fallback;
        self.read_hash += other.read_hash;
        self.publication += other.publication;
        self.sync += other.sync;
    }
}

/// A physical cache snapshot split between repository owners and shared roots.
#[derive(Default, Debug)]
pub(crate) struct AttributedAllocation {
    pub total_bytes: u64,
    pub repositories: BTreeMap<String, u64>,
    pub shared: BTreeMap<String, u64>,
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

    /// Resolve a weak content hint. The caller holds a lease through linking.
    pub(crate) fn cached_size(&self, id: &Id, mode: u32) -> Result<Option<u64>> {
        let path = self.path(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file()
                    && metadata.mode() & 0o777 == mode
                    && marked_id(&path)? == Some(*id) =>
            {
                Ok(Some(metadata.len()))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn link_cached(
        &self,
        id: &Id,
        mode: u32,
        limit: Option<u64>,
        destination: &Path,
    ) -> Result<Linked> {
        let Some(size) = self.cached_size(id, mode)? else {
            return Ok(Linked::Missing);
        };
        if limit.is_some_and(|limit| size > limit) {
            return Ok(Linked::Oversized(size));
        }
        fs::create_dir_all(destination.parent().context("Cache input has no parent")?)?;
        fs::hard_link(self.path(id), destination)?;
        Ok(Linked::File(size))
    }

    pub(crate) fn file_identity(&self, path: &Path) -> Result<Option<(Id, u32)>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(self
            .identity(path, &metadata)?
            .map(|id| (id, metadata.mode() & 0o777)))
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
        let mut files = Vec::new();
        let mut directories = Vec::new();
        Self::inventory(root, &mut files, &mut directories)?;
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
            .min(files.len());
        std::thread::scope(|scope| -> Result<()> {
            let jobs: Vec<_> = (0..workers)
                .map(|worker| {
                    let files = &files;
                    scope.spawn(move || -> Result<()> {
                        for path in files.iter().skip(worker).step_by(workers) {
                            self.import(path)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            for job in jobs {
                job.join()
                    .map_err(|_| anyhow::anyhow!("Cache import worker panicked"))??;
            }
            Ok(())
        })?;
        // All file replacements are durable before syncing directories, children first.
        for directory in directories {
            File::open(directory)?.sync_all()?;
        }
        Ok(())
    }

    /// Import a stopped, private stage. Later changes must replace files atomically.
    pub(crate) fn import_owned_tree(&self, root: &Path) -> Result<ImportStats> {
        let _lease = self.lease()?;
        let mut files = Vec::new();
        let mut directories = Vec::new();
        Self::inventory(root, &mut files, &mut directories)?;
        let shards = Mutex::new(BTreeSet::new());
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8)
            .min(files.len());
        let mut stats = std::thread::scope(|scope| -> Result<ImportStats> {
            let jobs: Vec<_> = (0..workers)
                .map(|worker| {
                    let files = &files;
                    let shards = &shards;
                    scope.spawn(move || -> Result<ImportStats> {
                        let mut stats = ImportStats::default();
                        for path in files.iter().skip(worker).step_by(workers) {
                            self.import_owned(path, &mut stats, shards)?;
                        }
                        Ok(stats)
                    })
                })
                .collect();
            let mut stats = ImportStats::default();
            for job in jobs {
                stats.add(
                    job.join()
                        .map_err(|_| anyhow::anyhow!("Cache import worker panicked"))??,
                );
            }
            Ok(stats)
        })?;
        let started = Instant::now();
        for shard in shards.into_inner().unwrap() {
            File::open(shard)?.sync_all()?;
        }
        for directory in directories {
            File::open(directory)?.sync_all()?;
        }
        stats.sync += started.elapsed();
        Ok(stats)
    }

    /// Adopt a stopped, private file. Hold a store lease through its publication.
    pub(crate) fn import_owned_file(&self, path: &Path) -> Result<ImportStats> {
        let shards = Mutex::new(BTreeSet::new());
        let mut stats = ImportStats::default();
        self.import_owned(path, &mut stats, &shards)?;
        let started = Instant::now();
        for shard in shards.into_inner().unwrap() {
            File::open(shard)?.sync_all()?;
        }
        File::open(path.parent().context("Cache input has no parent")?)?.sync_all()?;
        stats.sync += started.elapsed();
        Ok(stats)
    }

    fn import_owned(
        &self,
        path: &Path,
        stats: &mut ImportStats,
        shards: &Mutex<BTreeSet<PathBuf>>,
    ) -> Result<()> {
        let mut input = regular(path)?;
        let metadata = input.metadata()?;
        stats.files += 1;
        if self.identity(path, &metadata)?.is_some() {
            stats.reused += 1;
            return Ok(());
        }
        // A shared inode can still be modified by its other owner.
        if metadata.nlink() != 1 {
            let started = Instant::now();
            self.import(path)?;
            stats.fallback += 1;
            stats.publication += started.elapsed();
            return Ok(());
        }

        let started = Instant::now();
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
            stats.hashed_bytes += n as u64;
        }
        let id = *hash.finalize().as_bytes();
        stats.read_hash += started.elapsed();
        let started = Instant::now();
        let destination = self.path(&id);
        let parent = destination.parent().unwrap();
        let mut sync = Duration::ZERO;
        {
            let mut ready = shards.lock().unwrap();
            if !ready.contains(parent) {
                fs::create_dir_all(parent)?;
                let before = Instant::now();
                File::open(&self.root)?.sync_all()?;
                sync += before.elapsed();
                ready.insert(parent.to_owned());
            }
        }
        let mut adopted = false;
        if !destination.try_exists()? {
            input.set_permissions(fs::Permissions::from_mode(mode))?;
            ensure!(
                unsafe {
                    libc::fsetxattr(
                        std::os::fd::AsRawFd::as_raw_fd(&input),
                        ATTRIBUTE.as_ptr(),
                        id.as_ptr().cast(),
                        id.len(),
                        0,
                    )
                } == 0,
                "Cannot mark immutable cache input: {}",
                std::io::Error::last_os_error()
            );
            let before = Instant::now();
            input.sync_all()?;
            sync += before.elapsed();
            match fs::hard_link(path, &destination) {
                Ok(()) => adopted = true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        if adopted {
            stats.adopted += 1;
        } else {
            let parent = path.parent().context("Cache input has no parent")?;
            let temporary = tempfile::tempdir_in(parent)?;
            fs::hard_link(&destination, temporary.path().join("file"))?;
            fs::rename(temporary.path().join("file"), path)?;
            stats.reused += 1;
        }
        stats.sync += sync;
        stats.publication += started.elapsed().saturating_sub(sync);
        Ok(())
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

    fn inventory(
        root: &Path,
        files: &mut Vec<PathBuf>,
        directories: &mut Vec<PathBuf>,
    ) -> Result<()> {
        ensure!(
            fs::symlink_metadata(root)?.is_dir(),
            "Invalid managed input directory"
        );
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                Self::inventory(&entry.path(), files, directories)?;
            } else {
                ensure!(
                    kind.is_file(),
                    "Managed input is not a regular file: {}",
                    entry.path().display()
                );
                files.push(entry.path());
            }
        }
        directories.push(root.to_owned());
        Ok(())
    }

    fn identity(&self, path: &Path, metadata: &fs::Metadata) -> Result<Option<Id>> {
        if !metadata.is_file() || metadata.nlink() < 2 || !path.starts_with(self.cache()) {
            return Ok(None);
        }
        let Some(id) = marked_id(path)? else {
            return Ok(None);
        };
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
        let mut bytes = 0;
        let downloads = self.cache().join("lfs");
        if downloads.is_dir() {
            bytes += self.collect_directory(&lock, &downloads, pressure, true)?;
        }
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if shard.file_type()?.is_file()
                && shard
                    .file_name()
                    .to_string_lossy()
                    .starts_with("blob-pending-")
            {
                bytes += self.collect_batch(&lock, &[shard.path()], false, false)?;
                continue;
            }
            if !shard.file_type()?.is_dir() || shard.file_name().len() != 2 {
                continue;
            }
            bytes += self.collect_directory(&lock, &shard.path(), pressure, false)?;
        }
        Ok(bytes)
    }

    fn collect_directory(
        &self,
        lock: &File,
        directory: &Path,
        pressure: bool,
        downloads: bool,
    ) -> Result<u64> {
        let mut entries = fs::read_dir(directory)?;
        let mut bytes = 0;
        loop {
            let paths = entries
                .by_ref()
                .take(128)
                .map(|entry| Ok(entry?.path()))
                .collect::<Result<Vec<_>>>()?;
            if paths.is_empty() {
                return Ok(bytes);
            }
            bytes += self.collect_batch(lock, &paths, pressure, downloads)?;
            std::thread::yield_now();
        }
    }

    fn collect_batch(
        &self,
        lock: &File,
        paths: &[PathBuf],
        pressure: bool,
        downloads: bool,
    ) -> Result<u64> {
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let result = (|| {
            let mut bytes = 0;
            for path in paths {
                let metadata = match fs::symlink_metadata(path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let unused = metadata.nlink() == 1
                    || downloads
                        && metadata.nlink() == 2
                        && self.identity(path, &metadata)?.is_some();
                let eligible =
                    !downloads
                        || path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())
                        }) && (pressure
                            || metadata
                                .modified()?
                                .elapsed()
                                .is_ok_and(|age| age >= crate::config::DEFAULT_REPO_TTL));
                if metadata.is_file() && unused && eligible {
                    if metadata.nlink() == 1 {
                        bytes += metadata.blocks() * 512;
                    }
                    fs::remove_file(path)?;
                }
            }
            Ok(bytes)
        })();
        lock.unlock()?;
        result
    }
}

fn marked_id(path: &Path) -> Result<Option<Id>> {
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
    Ok((size == id.len() as isize).then_some(id))
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

/// Measure physical allocation and attribute each inode once. Repository roots
/// may overlap across branches; equal repository labels merge those branches.
/// Inodes visible to multiple repositories are divided evenly between them.
/// Inodes without a repository owner are divided between their cache categories.
pub(crate) fn allocated_attributed(
    root: &Path,
    repository_roots: &BTreeMap<PathBuf, String>,
) -> Result<AttributedAllocation> {
    const NONE: u32 = u32::MAX;

    struct Tags {
        first: u32,
        second: u32,
        rest: Option<Vec<u32>>,
    }
    impl Tags {
        fn new() -> Self {
            Self {
                first: NONE,
                second: NONE,
                rest: None,
            }
        }
        fn insert(&mut self, value: u32) {
            if self.first == NONE {
                self.first = value;
                return;
            }
            if value == self.first {
                return;
            }
            if self.second == NONE {
                if value < self.first {
                    self.second = self.first;
                    self.first = value;
                } else {
                    self.second = value;
                }
                return;
            }
            if value == self.second {
                return;
            }
            if value < self.first {
                let displaced = self.second;
                self.second = self.first;
                self.first = value;
                self.insert_rest(displaced);
            } else if value < self.second {
                let displaced = self.second;
                self.second = value;
                self.insert_rest(displaced);
            } else {
                self.insert_rest(value);
            }
        }
        fn insert_rest(&mut self, value: u32) {
            let rest = self.rest.get_or_insert_with(Vec::new);
            match rest.binary_search(&value) {
                Ok(_) => (),
                Err(index) => rest.insert(index, value),
            }
        }
        fn len(&self) -> usize {
            usize::from(self.first != NONE)
                + usize::from(self.second != NONE)
                + self.rest.as_ref().map_or(0, Vec::len)
        }
        fn clear(&mut self) {
            self.first = NONE;
            self.second = NONE;
            self.rest = None;
        }
        fn for_each(&self, mut visit: impl FnMut(u32)) {
            if self.first != NONE {
                visit(self.first);
            }
            if self.second != NONE {
                visit(self.second);
            }
            if let Some(rest) = &self.rest {
                for &value in rest {
                    visit(value);
                }
            }
        }
    }

    const CATEGORY: u32 = 1 << 31;

    struct Inode {
        bytes: u64,
        tags: Tags,
    }

    fn record(
        metadata: &fs::Metadata,
        category: u32,
        repository: Option<u32>,
        inodes: &mut BTreeMap<(u64, u64), Inode>,
    ) {
        let inode = inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| Inode {
                bytes: metadata.blocks() * 512,
                tags: Tags::new(),
            });
        if let Some(repository) = repository {
            if inode.tags.first == NONE || inode.tags.first & CATEGORY != 0 {
                inode.tags.clear();
            }
            inode.tags.insert(repository);
        } else if inode.tags.first == NONE || inode.tags.first & CATEGORY != 0 {
            inode.tags.insert(category | CATEGORY);
        }
    }

    fn visit(
        path: &Path,
        category: u32,
        inherited_repository: Option<u32>,
        repository_roots: &BTreeMap<PathBuf, u32>,
        inodes: &mut BTreeMap<(u64, u64), Inode>,
    ) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let repository = repository_roots.get(path).copied().or(inherited_repository);
        record(&metadata, category, repository, inodes);
        if metadata.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && path.ends_with("work") =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                visit(
                    &entry?.path(),
                    category,
                    repository,
                    repository_roots,
                    inodes,
                )?;
            }
        }
        Ok(())
    }

    fn intern(
        name: String,
        names: &mut Vec<String>,
        ids: &mut BTreeMap<String, u32>,
    ) -> Result<u32> {
        if let Some(&id) = ids.get(&name) {
            return Ok(id);
        }
        ensure!(
            names.len() < CATEGORY as usize,
            "Too many disk attribution labels"
        );
        let id = names.len() as u32;
        names.push(name.clone());
        ids.insert(name, id);
        Ok(id)
    }

    let mut repository_names: Vec<_> = repository_roots
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ensure!(
        repository_names.len() < CATEGORY as usize,
        "Too many disk attribution labels"
    );
    let repository_ids: BTreeMap<_, _> = repository_names
        .iter()
        .enumerate()
        .map(|(id, name)| (name.clone(), id as u32))
        .collect();
    let repository_roots: BTreeMap<_, _> = repository_roots
        .iter()
        .map(|(path, name)| (path.clone(), repository_ids[name]))
        .collect();

    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AttributedAllocation::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut category_names = Vec::new();
    let mut category_ids = BTreeMap::new();
    let other = intern("other".to_owned(), &mut category_names, &mut category_ids)?;
    let root_repository = repository_roots.get(root).copied();
    let mut inodes = BTreeMap::new();
    record(&root_metadata, other, root_repository, &mut inodes);
    let mut repo_bytes = vec![0u64; repository_names.len()];
    let mut shared_bytes = vec![0u64; category_names.len()];
    if root_metadata.is_dir() {
        let mut entries = match fs::read_dir(root) {
            Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && root.ends_with("work") =>
            {
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        };
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let category_name = entry.file_name().to_string_lossy().into_owned();
            let category = intern(category_name, &mut category_names, &mut category_ids)?;
            shared_bytes.resize(category_names.len(), 0);
            visit(
                &entry.path(),
                category,
                root_repository,
                &repository_roots,
                &mut inodes,
            )?;
        }
    }
    let mut allocation = AttributedAllocation::default();
    for inode in inodes.values() {
        allocation.total_bytes += inode.bytes;
        let bytes = if inode.tags.first & CATEGORY != 0 {
            &mut shared_bytes
        } else {
            &mut repo_bytes
        };
        let count = inode.tags.len() as u64;
        let mut remainder = inode.bytes % count;
        inode.tags.for_each(|tag| {
            bytes[(tag & !CATEGORY) as usize] += inode.bytes / count + u64::from(remainder > 0);
            remainder = remainder.saturating_sub(1);
        });
    }
    allocation.repositories = repository_names.drain(..).zip(repo_bytes).collect();
    allocation.shared = category_names.into_iter().zip(shared_bytes).collect();
    Ok(allocation)
}

#[cfg(test)]
mod tests {
    #[test]
    fn weak_links_validate_mode_size_and_survive_without_existing_views() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let source = cache.path().join("source");
        fs::write(&source, b"immutable bytes").unwrap();
        let mode = fs::metadata(&source).unwrap().mode() & 0o777;
        let lease = store.lease().unwrap();
        let id = store.import(&source).unwrap();
        let inode = fs::metadata(&source).unwrap().ino();
        fs::remove_file(&source).unwrap();
        let destination = cache.path().join("linked");
        assert!(matches!(
            store
                .link_cached(&id, mode ^ 0o100, None, &destination)
                .unwrap(),
            Linked::Missing
        ));
        assert!(matches!(
            store.link_cached(&id, mode, Some(0), &destination).unwrap(),
            Linked::Oversized(_)
        ));
        assert!(matches!(
            store.link_cached(&id, mode, None, &destination).unwrap(),
            Linked::File(_)
        ));
        assert_eq!(fs::metadata(&destination).unwrap().ino(), inode);
        drop(lease);
        assert_eq!(store.collect_with_pressure(true).unwrap(), 0);
        fs::remove_file(destination).unwrap();
        assert!(store.collect_with_pressure(true).unwrap() > 0);
        assert!(store.cached_size(&id, mode).unwrap().is_none());
    }

    use super::*;

    #[test]
    fn attributed_allocation_merges_branches_and_counts_hardlinks_once() {
        let cache = tempfile::tempdir().unwrap();
        let first_branch = cache.path().join("repositories/first");
        let second_branch = cache.path().join("repositories/second");
        let other_branch = cache.path().join("repositories/other");
        let first = first_branch.join("source/shared.bin");
        let second = second_branch.join("source/shared.bin");
        let other = other_branch.join("source/shared.bin");
        for path in [&first, &second, &other] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        fs::write(&first, vec![42; 8192]).unwrap();
        fs::hard_link(&first, &second).unwrap();
        fs::hard_link(&first, &other).unwrap();

        let shared = cache.path().join("analysis/data.mdb");
        let editor = cache.path().join("editors/shared.bin");
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::create_dir_all(editor.parent().unwrap()).unwrap();
        fs::write(&shared, vec![7; 8192]).unwrap();
        fs::hard_link(&shared, &editor).unwrap();

        let repositories = BTreeMap::from([
            (first_branch, "https://example.test/team/repo".to_owned()),
            (second_branch, "https://example.test/team/repo".to_owned()),
            (other_branch, "https://example.test/other/repo".to_owned()),
        ]);
        let allocation = allocated_attributed(cache.path(), &repositories).unwrap();

        assert_eq!(allocation.total_bytes, allocated(cache.path()).unwrap());
        assert_eq!(
            allocation.repositories.values().sum::<u64>() + allocation.shared.values().sum::<u64>(),
            allocation.total_bytes
        );
        assert!(
            allocation
                .repositories
                .contains_key("https://example.test/team/repo")
        );
        assert!(
            allocation
                .repositories
                .contains_key("https://example.test/other/repo")
        );
        assert!(
            !allocation
                .repositories
                .contains_key("https://example.test/team/repo#first")
        );
        assert!(allocation.shared.contains_key("analysis"));
        assert!(allocation.shared.contains_key("editors"));
    }

    #[test]
    fn attributed_hardlinks_split_between_unique_repositories_before_shared_categories() {
        let cache = tempfile::tempdir().unwrap();
        let blob = cache.path().join("blobs/shared");
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, vec![42; 8192]).unwrap();
        let bytes = fs::metadata(&blob).unwrap().blocks() * 512;
        let mut roots = BTreeMap::new();
        let mut expected = BTreeMap::<String, u64>::new();
        // Repeated branches must not increase their repository's share. More
        // than two owners exercises the overflow storage for inode labels.
        for (branch, repository) in ["e", "d", "c", "b", "a", "a"].into_iter().enumerate() {
            let root = cache.path().join(format!("repositories/{branch}"));
            fs::create_dir_all(&root).unwrap();
            fs::hard_link(&blob, root.join("shared")).unwrap();
            *expected.entry(repository.to_owned()).or_default() +=
                fs::metadata(&root).unwrap().blocks() * 512;
            roots.insert(root, repository.to_owned());
        }
        // Shared aliases occur both before and after repository views during
        // traversal. Neither may take bytes away from repository owners.
        let shared = cache.path().join("zzz-shared");
        fs::create_dir(&shared).unwrap();
        fs::hard_link(&blob, shared.join("alias")).unwrap();
        let count = expected.len() as u64;
        for (index, value) in expected.values_mut().enumerate() {
            *value += bytes / count + u64::from((index as u64) < bytes % count);
        }

        let allocation = allocated_attributed(cache.path(), &roots).unwrap();
        assert_eq!(allocation.repositories, expected);
        assert_eq!(allocation.total_bytes, allocated(cache.path()).unwrap());
        assert_eq!(
            allocation.repositories.values().sum::<u64>() + allocation.shared.values().sum::<u64>(),
            allocation.total_bytes
        );
    }

    #[test]
    #[ignore = "measures physical allocation traversal overhead"]
    fn attributed_allocation_performance_probe() {
        let cache = tempfile::tempdir().unwrap();
        let repository = cache.path().join("repositories/synthetic");
        let source = repository.join("source");
        fs::create_dir_all(&source).unwrap();
        for index in 0..10_000 {
            fs::write(source.join(format!("{index}.bin")), [42; 512]).unwrap();
        }
        let roots = BTreeMap::from([(repository, "synthetic".to_owned())]);
        let mut baseline = Vec::new();
        let mut attributed = Vec::new();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let total = allocated(cache.path()).unwrap();
            baseline.push(start.elapsed());

            let start = std::time::Instant::now();
            let sample = allocated_attributed(cache.path(), &roots).unwrap();
            attributed.push(start.elapsed());
            assert_eq!(sample.total_bytes, total);
        }
        baseline.sort();
        attributed.sort();
        eprintln!(
            "10k-file scan median: allocated={:?}, attributed={:?}, ratio={:.2}x",
            baseline[1],
            attributed[1],
            attributed[1].as_secs_f64() / baseline[1].as_secs_f64(),
        );
    }

    #[test]
    #[ignore = "measures cold imports and reuse of cached files"]
    fn owned_import_performance_probe() {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        fs::create_dir_all(&target).unwrap();
        let cache = tempfile::Builder::new()
            .prefix("blob-import-probe-")
            .tempdir_in(target)
            .unwrap();
        let store = Store::open(cache.path()).unwrap();
        let stages: Vec<_> = ["seed", "copy", "owned"]
            .map(|name| cache.path().join(name))
            .into_iter()
            .collect();
        for stage in &stages {
            fs::create_dir(stage).unwrap();
        }
        let count = 8_000u64;
        let mut bytes = [0u8; 8 * 1024];
        for index in 0..count {
            bytes[..8].copy_from_slice(&index.to_le_bytes());
            for stage in &stages {
                fs::write(stage.join(index.to_string()), bytes).unwrap();
            }
        }
        let start = Instant::now();
        let cold_stats = store.import_owned_tree(&stages[0]).unwrap();
        let cold = start.elapsed();
        let start = Instant::now();
        store.import_tree(&stages[1]).unwrap();
        let copy = start.elapsed();
        let start = Instant::now();
        let stats = store.import_owned_tree(&stages[2]).unwrap();
        let owned = start.elapsed();
        eprintln!(
            "{count} files, {} bytes: cold={cold:?}, cold_sync={:?}, copy={copy:?}, owned={owned:?}, reused={}",
            count * bytes.len() as u64,
            cold_stats.sync,
            stats.reused
        );
    }

    #[test]
    fn tree_import_preserves_files_and_shares_duplicates() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let root = cache.path().join("source");
        fs::create_dir_all(root.join("empty/nested")).unwrap();
        let mut expected = Vec::new();
        for directory in ["a", "b"] {
            fs::create_dir(root.join(directory)).unwrap();
            for index in 0..16 {
                let path = root.join(directory).join(index.to_string());
                let contents = format!("source {index}");
                fs::write(&path, &contents).unwrap();
                expected.push((path, contents));
            }
        }
        store.import_tree(&root).unwrap();
        for (path, contents) in expected {
            assert_eq!(fs::read_to_string(path).unwrap(), contents);
        }
        for index in 0..16 {
            assert_eq!(
                fs::metadata(root.join("a").join(index.to_string()))
                    .unwrap()
                    .ino(),
                fs::metadata(root.join("b").join(index.to_string()))
                    .unwrap()
                    .ino()
            );
        }
        assert!(root.join("empty/nested").is_dir());
        store.import_tree(&root).unwrap();
        assert_eq!(store.collect().unwrap(), 0);
    }

    #[test]
    fn owned_tree_adopts_files_and_reuses_existing_content_by_mode() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let root = cache.path().join("source");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("first");
        let duplicate = root.join("duplicate");
        let executable = root.join("executable");
        let special = root.join("special");
        for path in [&first, &duplicate, &executable, &special] {
            fs::write(path, b"same contents").unwrap();
        }
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&special, fs::Permissions::from_mode(0o4755)).unwrap();
        let original_plain = [
            fs::metadata(&first).unwrap().ino(),
            fs::metadata(&duplicate).unwrap().ino(),
        ];
        let original_executable = [
            fs::metadata(&executable).unwrap().ino(),
            fs::metadata(&special).unwrap().ino(),
        ];
        let stats = store.import_owned_tree(&root).unwrap();
        assert_eq!(stats.files, 4);
        assert_eq!(stats.adopted, 2);
        assert_eq!(stats.reused, 2);
        assert_eq!(stats.fallback, 0);
        assert_eq!(fs::read(&duplicate).unwrap(), b"same contents");
        assert_eq!(
            fs::metadata(&first).unwrap().ino(),
            fs::metadata(&duplicate).unwrap().ino()
        );
        assert_ne!(
            fs::metadata(&first).unwrap().ino(),
            fs::metadata(&executable).unwrap().ino()
        );
        assert!(original_plain.contains(&fs::metadata(&first).unwrap().ino()));
        assert!(original_executable.contains(&fs::metadata(&executable).unwrap().ino()));
        assert_eq!(fs::metadata(&special).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(store.collect().unwrap(), 0);

        let other = cache.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("copy"), b"same contents").unwrap();
        let stats = store.import_owned_tree(&other).unwrap();
        assert_eq!(stats.adopted, 0);
        assert_eq!(stats.reused, 1);
        assert_eq!(
            fs::metadata(other.join("copy")).unwrap().ino(),
            fs::metadata(&first).unwrap().ino()
        );
    }

    #[test]
    fn owned_file_adoption_and_reuse_do_not_copy_contents() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let _lease = store.lease().unwrap();
        let path = cache.path().join("download");
        let contents = vec![7; 8192];
        fs::write(&path, &contents).unwrap();
        let original = fs::metadata(&path).unwrap().ino();
        let stats = store.import_owned_file(&path).unwrap();
        assert_eq!(stats.hashed_bytes, contents.len() as u64);
        assert_eq!(stats.adopted, 1);
        assert_eq!(stats.fallback, 0);
        assert_eq!(fs::metadata(&path).unwrap().ino(), original);
        assert!(store.file_identity(&path).unwrap().is_some());
        let stats = store.import_owned_file(&path).unwrap();
        assert_eq!(stats.hashed_bytes, 0);
        assert_eq!(stats.reused, 1);
        assert_eq!(fs::metadata(&path).unwrap().ino(), original);
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn owned_tree_copies_shared_inputs_and_rejects_invalid_entries() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let external = cache.path().join("external");
        fs::write(&external, b"original").unwrap();
        let root = cache.path().join("source");
        fs::create_dir(&root).unwrap();
        let staged = root.join("file");
        fs::hard_link(&external, &staged).unwrap();
        let stats = store.import_owned_tree(&root).unwrap();
        assert_eq!(stats.fallback, 1);
        assert_ne!(
            fs::metadata(&external).unwrap().ino(),
            fs::metadata(&staged).unwrap().ino()
        );
        fs::write(&external, b"changed").unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"original");

        let invalid = root.join("invalid");
        std::os::unix::fs::symlink(&external, &invalid).unwrap();
        assert!(store.import_owned_tree(&root).is_err());
        assert_eq!(fs::read(&staged).unwrap(), b"original");
    }

    #[test]
    fn owned_tree_leaves_input_intact_if_publication_fails() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let root = cache.path().join("source");
        fs::create_dir(&root).unwrap();
        let staged = root.join("file");
        fs::write(&staged, b"original").unwrap();
        let seed = cache.path().join("seed");
        fs::write(&seed, b"original").unwrap();
        let _lease = store.lease().unwrap();
        let destination = store.path(&store.import(&seed).unwrap());
        fs::remove_file(&destination).unwrap();
        fs::create_dir_all(&destination).unwrap();
        assert!(store.import_owned_tree(&root).is_err());
        assert_eq!(fs::read(staged).unwrap(), b"original");
        assert!(destination.is_dir());
    }

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
