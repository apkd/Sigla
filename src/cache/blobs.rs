//! Immutable file storage. Only private, stopped job outputs may be imported.
use super::file_id::hasher as content_hasher;
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

struct OwnedFile {
    path: PathBuf,
    id: Id,
    new: bool,
}

/// Build a private immutable tree while bytes arrive. Publication is explicit,
/// so an archive reader can verify its complete transport before exposing links.
pub(crate) struct OwnedTree<'a> {
    store: &'a Store,
    root: PathBuf,
    files: Vec<OwnedFile>,
    aliases: Vec<(PathBuf, PathBuf)>,
    directories: BTreeSet<PathBuf>,
    stats: ImportStats,
    _lease: Lease,
}

impl OwnedTree<'_> {
    fn directory(&mut self, path: &Path) -> Result<()> {
        let relative = path
            .strip_prefix(&self.root)
            .context("Import path lies outside its private tree")?;
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
            "Invalid import file path"
        );
        let parent = path.parent().context("Cache input has no parent")?;
        if !self.directories.contains(parent) {
            fs::create_dir_all(parent)?;
            self.directories.extend(
                parent
                    .ancestors()
                    .take_while(|parent| parent.starts_with(&self.root))
                    .map(Path::to_owned),
            );
        }
        Ok(())
    }

    pub fn write(&mut self, path: &Path, mode: u32, mut reader: impl Read) -> Result<u64> {
        self.write_inner(path, mode, &mut reader, None)
    }

    /// The enclosing authenticated stream must succeed before publishing this tree.
    pub fn write_identified(
        &mut self,
        path: &Path,
        mode: u32,
        reader: impl Read,
        identity: (u32, Id),
    ) -> Result<u64> {
        self.write_inner(path, mode, reader, Some(identity))
    }

    fn write_inner(
        &mut self,
        path: &Path,
        mode: u32,
        mut reader: impl Read,
        identity: Option<(u32, Id)>,
    ) -> Result<u64> {
        self.directory(path)?;
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)?;
        let mode = output.metadata()?.mode() & 0o777;
        let supplied = identity
            .filter(|(expected, _)| *expected == mode)
            .map(|(_, id)| id);
        let mut hash = supplied.is_none().then(|| content_hasher(mode));
        let started = Instant::now();
        let mut size = 0;
        let mut bytes = [0; 128 * 1024];
        loop {
            let count = reader.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            output.write_all(&bytes[..count])?;
            if let Some(hash) = &mut hash {
                hash.update(&bytes[..count]);
            }
            size += count as u64;
        }
        let id = supplied.unwrap_or_else(|| *hash.unwrap().finalize().as_bytes());
        self.stats.files += 1;
        if supplied.is_none() {
            self.stats.hashed_bytes += size;
        }
        self.stats.read_hash += started.elapsed();
        let new = !self.store.path(&id).try_exists()?;
        if new {
            mark(&output, &id)?;
        }
        self.files.push(OwnedFile {
            path: path.to_owned(),
            id,
            new,
        });
        Ok(size)
    }

    pub fn alias(&mut self, source: &Path, destination: &Path) -> Result<()> {
        self.directory(source)?;
        self.directory(destination)?;
        self.aliases
            .push((source.to_owned(), destination.to_owned()));
        Ok(())
    }

    #[cfg(test)]
    pub fn publish(mut self) -> Result<ImportStats> {
        self.store
            .publish_tree(&self.root, &self.files, &mut self.stats)?;
        self.finish_publication()
    }

    fn finish_publication(mut self) -> Result<ImportStats> {
        let started = Instant::now();
        for (source, destination) in &self.aliases {
            fs::hard_link(source, destination)?;
        }
        self.stats.files += self.aliases.len() as u64;
        self.stats.reused += self.aliases.len() as u64;
        self.stats.publication += started.elapsed();
        let started = Instant::now();
        // Reverse path order places children before their parents.
        for directory in self.directories.iter().rev() {
            File::open(directory)?.sync_all()?;
        }
        self.stats.sync += started.elapsed();
        Ok(self.stats)
    }

    /// Flush stopped private trees together before publishing canonical links.
    /// Callers retain their stages and activate each destination independently.
    pub(crate) fn publish_together(trees: Vec<Self>) -> Result<Vec<Result<ImportStats>>> {
        Self::publish_together_with(trees, |directory| {
            ensure!(
                unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(directory)) } == 0,
                "Cannot flush cache import: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        })
    }

    fn publish_together_with(
        mut trees: Vec<Self>,
        flush: impl FnOnce(&File) -> Result<()>,
    ) -> Result<Vec<Result<ImportStats>>> {
        let Some(first) = trees.first() else {
            return Ok(Vec::new());
        };
        let root = &first.store.root;
        let device = fs::metadata(root)?.dev();
        let mut directory = None;
        for tree in &trees {
            ensure!(
                &tree.store.root == root,
                "Batch spans different blob stores"
            );
            if tree.files.iter().any(|file| file.new) {
                let input = File::open(&tree.root)?;
                ensure!(
                    input.metadata()?.dev() == device,
                    "Cache import must stay on one filesystem"
                );
                directory = Some(input);
            }
        }
        if let Some(directory) = directory {
            let started = Instant::now();
            flush(&directory)?;
            trees[0].stats.sync += started.elapsed();
        }
        Ok(trees
            .into_iter()
            .map(|mut tree| {
                tree.store
                    .publish_durable_files(&tree.files, &mut tree.stats)?;
                tree.finish_publication()
            })
            .collect())
    }
}

fn parallel_import<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T, &mut ImportStats) -> Result<R> + Sync,
) -> Result<(Vec<R>, ImportStats)> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8)
        .min(items.len());
    std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..workers)
            .map(|worker| {
                let work = &work;
                scope.spawn(move || -> Result<_> {
                    let mut stats = ImportStats::default();
                    let mut results = Vec::new();
                    for item in items.iter().skip(worker).step_by(workers) {
                        results.push(work(item, &mut stats)?);
                    }
                    Ok((results, stats))
                })
            })
            .collect();
        let mut stats = ImportStats::default();
        let mut results = Vec::new();
        for job in jobs {
            let (part, timing) = job
                .join()
                .map_err(|_| anyhow::anyhow!("Cache import worker panicked"))??;
            results.extend(part);
            stats.add(timing);
        }
        Ok((results, stats))
    })
}

fn mark(input: &File, id: &Id) -> Result<()> {
    ensure!(
        unsafe {
            libc::fsetxattr(
                std::os::fd::AsRawFd::as_raw_fd(input),
                ATTRIBUTE.as_ptr(),
                id.as_ptr().cast(),
                id.len(),
                0,
            )
        } == 0,
        "Cannot mark immutable cache input: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
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

fn replace_with_link(source: &Path, destination: &Path) -> Result<()> {
    tempfile::Builder::new()
        .prefix(".sigla-link-")
        .make_in(
            destination.parent().context("Cache input has no parent")?,
            |path| fs::hard_link(source, path),
        )?
        .into_temp_path()
        .persist(destination)?;
    Ok(())
}

impl Store {
    pub(crate) fn owned_tree(&self, root: &Path) -> Result<OwnedTree<'_>> {
        fs::create_dir_all(root)?;
        Ok(OwnedTree {
            store: self,
            root: root.to_owned(),
            files: Vec::new(),
            aliases: Vec::new(),
            directories: BTreeSet::from([root.to_owned()]),
            stats: ImportStats::default(),
            _lease: self.lease()?,
        })
    }

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
        let mut hash = content_hasher(mode);
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
        // The attribute is only trusted after checking the canonical blob's inode.
        mark(stage.as_file(), &id)?;
        stage.as_file().sync_all()?;
        match stage.persist_noclobber(&destination) {
            Ok(_) => {
                File::open(destination.parent().unwrap())?.sync_all()?;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        replace_with_link(&destination, path)?;
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
        let started = Instant::now();
        let _lease = self.lease()?;
        let mut files = Vec::new();
        let mut directories = Vec::new();
        Self::inventory(root, &mut files, &mut directories)?;
        let (prepared, mut stats) =
            parallel_import(&files, |path, stats| self.prepare_owned(path, stats, false))?;
        let prepared: Vec<_> = prepared.into_iter().flatten().collect();
        self.publish_tree(root, &prepared, &mut stats)?;
        let syncing = Instant::now();
        for directory in directories {
            File::open(directory)?.sync_all()?;
        }
        stats.sync += syncing.elapsed();
        tracing::info!(root = %root.display(), elapsed_ms = started.elapsed().as_millis(), files = stats.files,
            hashed_bytes = stats.hashed_bytes, adopted = stats.adopted, reused = stats.reused, fallback = stats.fallback,
            read_hash_worker_ms = stats.read_hash.as_millis(), publication_worker_ms = stats.publication.as_millis(),
            sync_work_ms = stats.sync.as_millis(), "Owned cache tree imported");
        Ok(stats)
    }

    fn publish_tree(
        &self,
        root: &Path,
        files: &[OwnedFile],
        stats: &mut ImportStats,
    ) -> Result<()> {
        if files.iter().any(|file| file.new) {
            let started = Instant::now();
            let directory = File::open(root)?;
            ensure!(
                directory.metadata()?.dev() == fs::metadata(&self.root)?.dev(),
                "Cache import must stay on one filesystem"
            );
            // All private writes and xattrs precede this barrier. No canonical
            // link can expose a new inode before its data is durable.
            ensure!(
                unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&directory)) } == 0,
                "Cannot flush cache import: {}",
                std::io::Error::last_os_error()
            );
            stats.sync += started.elapsed();
        }
        self.publish_durable_files(files, stats)
    }

    fn publish_durable_files(&self, files: &[OwnedFile], stats: &mut ImportStats) -> Result<()> {
        let shards = Mutex::new(BTreeSet::new());
        let (_, published) = parallel_import(files, |file, stats| {
            self.publish_owned(file, stats, &shards)
        })?;
        stats.add(published);
        self.sync_import(shards.into_inner().unwrap(), &[], stats)
    }

    fn sync_import(
        &self,
        shards: BTreeSet<PathBuf>,
        directories: &[PathBuf],
        stats: &mut ImportStats,
    ) -> Result<()> {
        let syncing = Instant::now();
        // Persist shard names once per import, including blobs reused from a
        // concurrent importer. That importer may not have reached this sync yet.
        File::open(&self.root)?.sync_all()?;
        for shard in shards {
            File::open(shard)?.sync_all()?;
        }
        for directory in directories {
            File::open(directory)?.sync_all()?;
        }
        stats.sync += syncing.elapsed();
        Ok(())
    }

    /// Adopt a stopped, private file. Hold a store lease through its publication.
    pub(crate) fn import_owned_file(&self, path: &Path) -> Result<ImportStats> {
        let shards = Mutex::new(BTreeSet::new());
        let mut stats = ImportStats::default();
        self.persist_owned(path, &mut stats, &shards)?;
        self.sync_import(
            shards.into_inner().unwrap(),
            &[path
                .parent()
                .context("Cache input has no parent")?
                .to_owned()],
            &mut stats,
        )?;
        Ok(stats)
    }

    fn persist_owned(
        &self,
        path: &Path,
        stats: &mut ImportStats,
        shards: &Mutex<BTreeSet<PathBuf>>,
    ) -> Result<()> {
        if let Some(file) = self.prepare_owned(path, stats, true)? {
            self.publish_owned(&file, stats, shards)?;
        }
        Ok(())
    }

    fn prepare_owned(
        &self,
        path: &Path,
        stats: &mut ImportStats,
        sync: bool,
    ) -> Result<Option<OwnedFile>> {
        let mut input = regular(path)?;
        let metadata = input.metadata()?;
        stats.files += 1;
        if self.identity(path, &metadata)?.is_some() {
            stats.reused += 1;
            return Ok(None);
        }
        // A shared inode can still be modified by its other owner.
        if metadata.nlink() != 1 {
            let started = Instant::now();
            self.import(path)?;
            stats.fallback += 1;
            stats.publication += started.elapsed();
            return Ok(None);
        }

        let started = Instant::now();
        let mode = metadata.mode() & 0o777;
        let mut hash = content_hasher(mode);
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
        let new = !self.path(&id).try_exists()?;
        if new {
            input.set_permissions(fs::Permissions::from_mode(mode))?;
            mark(&input, &id)?;
            if sync {
                let started = Instant::now();
                input.sync_all()?;
                stats.sync += started.elapsed();
            }
        }
        Ok(Some(OwnedFile {
            path: path.to_owned(),
            id,
            new,
        }))
    }

    fn publish_owned(
        &self,
        file: &OwnedFile,
        stats: &mut ImportStats,
        shards: &Mutex<BTreeSet<PathBuf>>,
    ) -> Result<()> {
        let started = Instant::now();
        let path = &file.path;
        let destination = self.path(&file.id);
        let parent = destination.parent().unwrap();
        {
            let mut ready = shards.lock().unwrap();
            if !ready.contains(parent) {
                fs::create_dir_all(parent)?;
                ready.insert(parent.to_owned());
            }
        }
        let mut adopted = false;
        if file.new {
            match fs::hard_link(path, &destination) {
                Ok(()) => adopted = true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        if adopted {
            stats.adopted += 1;
        } else {
            replace_with_link(&destination, path)?;
            stats.reused += 1;
        }
        stats.publication += started.elapsed();
        Ok(())
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
            let claims = downloads.join(".claims");
            if claims.is_dir() {
                // Active downloads hold a blob lease until their claim is closed.
                bytes += self.collect_directory(&lock, &claims, pressure, false)?;
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

#[cfg(test)]
thread_local! {
    pub(crate) static ALLOCATION_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Physical allocation, rather than the sum of each hardlinked view's apparent size.
pub fn allocated(root: &Path) -> Result<u64> {
    #[cfg(test)]
    ALLOCATION_WALKS.update(|count| count + 1);
    fn visit(
        root: &Path,
        metadata: std::io::Result<fs::Metadata>,
        seen: &mut BTreeSet<(u64, u64)>,
    ) -> Result<u64> {
        let metadata = match metadata {
            Ok(metadata) => metadata,
            // Background transfers can publish or remove staging files during a scan.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot measure {}", root.display()));
            }
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
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Cannot enumerate {}", root.display()));
                }
            };
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("Cannot enumerate {}", root.display()))?;
                bytes += visit(&entry.path(), entry.metadata(), seen)?;
            }
        }
        Ok(bytes)
    }
    visit(root, fs::symlink_metadata(root), &mut BTreeSet::new())
}

/// Measure physical allocation and attribute each inode once. Repository roots
/// may overlap across branches; equal repository labels merge those branches.
/// Inodes visible to multiple repositories are divided evenly between them.
/// Inodes without a repository owner are divided between their cache categories.
pub(crate) fn allocated_attributed(
    root: &Path,
    repository_roots: &BTreeMap<PathBuf, String>,
) -> Result<AttributedAllocation> {
    allocated_with_owners(root, repository_roots, &[]).map(|(allocation, _)| allocation)
}

/// Collect eviction estimates in the same physical allocation walk.
pub(crate) fn allocated_with_owners(
    root: &Path,
    repository_roots: &BTreeMap<PathBuf, String>,
    owners: &[(PathBuf, Vec<PathBuf>)],
) -> Result<(AttributedAllocation, BTreeMap<PathBuf, u64>)> {
    #[cfg(test)]
    ALLOCATION_WALKS.update(|count| count + 1);
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

    trait Ownership: Default {
        fn insert(&mut self, owner: u32);
        fn share(&self, bytes: u64, totals: &mut [u64]);
    }
    impl Default for Tags {
        fn default() -> Self {
            Self::new()
        }
    }
    impl Ownership for Tags {
        fn insert(&mut self, owner: u32) {
            Tags::insert(self, owner);
        }
        fn share(&self, bytes: u64, totals: &mut [u64]) {
            let count = self.len() as u64;
            if count != 0 {
                self.for_each(|owner| totals[owner as usize] += bytes / count);
            }
        }
    }
    impl Ownership for () {
        fn insert(&mut self, _: u32) {}
        fn share(&self, _: u64, _: &mut [u64]) {}
    }

    struct Inode<T> {
        bytes: u64,
        tags: Tags,
        owners: T,
    }

    /// Mirror just the prefixes that can change inherited ownership.
    #[derive(Default)]
    struct Roots {
        repository: Option<u32>,
        owners: Vec<u32>,
        children: BTreeMap<std::ffi::OsString, Roots>,
    }
    impl Roots {
        fn at(&mut self, relative: &Path) -> &mut Self {
            let mut node = self;
            for part in relative {
                node = node.children.entry(part.to_owned()).or_default();
            }
            node
        }
    }

    fn record<T: Ownership>(
        metadata: &fs::Metadata,
        category: u32,
        repository: Option<u32>,
        inodes: &mut BTreeMap<(u64, u64), Inode<T>>,
        owners: &[u32],
    ) {
        let inode = inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| Inode {
                bytes: metadata.blocks() * 512,
                tags: Tags::new(),
                owners: T::default(),
            });
        for &owner in owners {
            inode.owners.insert(owner);
        }
        if let Some(repository) = repository {
            if inode.tags.first == NONE || inode.tags.first & CATEGORY != 0 {
                inode.tags.clear();
            }
            inode.tags.insert(repository);
        } else if inode.tags.first == NONE || inode.tags.first & CATEGORY != 0 {
            inode.tags.insert(category | CATEGORY);
        }
    }

    fn visit<T: Ownership>(
        path: &Path,
        metadata: std::io::Result<fs::Metadata>,
        category: u32,
        inherited_repository: Option<u32>,
        inodes: &mut BTreeMap<(u64, u64), Inode<T>>,
        roots: Option<&Roots>,
        inherited_owners: &[u32],
    ) -> Result<()> {
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot measure {}", path.display()));
            }
        };
        let repository = roots
            .and_then(|node| node.repository)
            .or(inherited_repository);
        let owners = match roots {
            Some(node) if !node.owners.is_empty() => {
                std::borrow::Cow::Owned([inherited_owners, &node.owners].concat())
            }
            _ => std::borrow::Cow::Borrowed(inherited_owners),
        };
        record(&metadata, category, repository, inodes, &owners);
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
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Cannot enumerate {}", path.display()));
                }
            };
            let children = roots.filter(|node| !node.children.is_empty());
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("Cannot enumerate {}", path.display()))?;
                visit(
                    &entry.path(),
                    entry.metadata(),
                    category,
                    repository,
                    inodes,
                    children.and_then(|node| node.children.get(&entry.file_name())),
                    &owners,
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

    fn measure<T: Ownership>(
        root: &Path,
        repository_roots: &BTreeMap<PathBuf, String>,
        owners: &[(PathBuf, Vec<PathBuf>)],
    ) -> Result<(AttributedAllocation, BTreeMap<PathBuf, u64>)> {
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
        let mut roots = Roots::default();
        for (path, name) in repository_roots {
            if let Ok(relative) = path.strip_prefix(root) {
                roots.at(relative).repository = Some(repository_ids[name]);
            }
        }
        ensure!(owners.len() < NONE as usize, "Too many allocation owners");
        for (index, (_, paths)) in owners.iter().enumerate() {
            for path in paths {
                let relative = path
                    .strip_prefix(root)
                    .context("Allocation owner lies outside the cache")?;
                roots.at(relative).owners.push(index as u32);
            }
        }
        let mut owner_bytes = vec![0; owners.len()];
        let root_metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((
                    AttributedAllocation::default(),
                    owners.iter().map(|(owner, _)| (owner.clone(), 0)).collect(),
                ));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot measure {}", root.display()));
            }
        };
        let mut category_names = Vec::new();
        let mut category_ids = BTreeMap::new();
        let other = intern("other".to_owned(), &mut category_names, &mut category_ids)?;
        let mut inodes = BTreeMap::<(u64, u64), Inode<T>>::new();
        record(
            &root_metadata,
            other,
            roots.repository,
            &mut inodes,
            &roots.owners,
        );
        let mut repo_bytes = vec![0u64; repository_names.len()];
        let mut shared_bytes = vec![0u64; category_names.len()];
        if root_metadata.is_dir() {
            let mut entries = match fs::read_dir(root) {
                Ok(entries) => entries
                    .collect::<std::io::Result<Vec<_>>>()
                    .with_context(|| format!("Cannot enumerate {}", root.display()))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && root.ends_with("work") =>
                {
                    Vec::new()
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Cannot enumerate {}", root.display()));
                }
            };
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry.file_name();
                let category_name = name.to_string_lossy().into_owned();
                let category = intern(category_name, &mut category_names, &mut category_ids)?;
                shared_bytes.resize(category_names.len(), 0);
                visit(
                    &entry.path(),
                    entry.metadata(),
                    category,
                    roots.repository,
                    &mut inodes,
                    roots.children.get(&name),
                    &roots.owners,
                )?;
            }
        }
        let mut allocation = AttributedAllocation::default();
        for inode in inodes.values() {
            allocation.total_bytes += inode.bytes;
            inode.owners.share(inode.bytes, &mut owner_bytes);
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
        Ok((
            allocation,
            owners
                .iter()
                .map(|(owner, _)| owner.clone())
                .zip(owner_bytes)
                .collect(),
        ))
    }
    // Ordinary monitoring keeps the original per-inode memory footprint.
    if owners.is_empty() {
        measure::<()>(root, repository_roots, owners)
    } else {
        measure::<Tags>(root, repository_roots, owners)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn grouped_publication_preserves_neighbors_after_one_tree_fails() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let mut trees = Vec::new();
        for index in 0..3 {
            let root = cache.path().join(format!("stage-{index}"));
            let mut tree = store.owned_tree(&root).unwrap();
            tree.write(&root.join("same"), 0o600, b"same bytes".as_slice())
                .unwrap();
            if index == 1 {
                tree.alias(&root.join("missing"), &root.join("alias"))
                    .unwrap();
            }
            trees.push(tree);
        }
        let id = trees[0].files[0].id;
        let results = OwnedTree::publish_together(trees).unwrap();
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
        let canonical = fs::metadata(store.path(&id)).unwrap().ino();
        for index in [0, 2] {
            let path = cache.path().join(format!("stage-{index}/same"));
            assert_eq!(fs::metadata(&path).unwrap().ino(), canonical);
            assert_eq!(fs::read(path).unwrap(), b"same bytes");
        }
        assert!(!cache.path().join("stage-1/alias").exists());
    }

    #[test]
    fn grouped_barrier_failure_exposes_no_canonical_files() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let mut trees = Vec::new();
        let mut ids = Vec::new();
        for index in 0..3 {
            let root = cache.path().join(format!("stage-{index}"));
            let mut tree = store.owned_tree(&root).unwrap();
            tree.write(&root.join("input"), 0o600, [index as u8].as_slice())
                .unwrap();
            ids.push(tree.files[0].id);
            trees.push(tree);
        }
        let result =
            OwnedTree::publish_together_with(trees, |_| anyhow::bail!("injected flush failure"));
        assert!(result.is_err());
        assert!(ids.iter().all(|id| !store.path(id).exists()));
    }

    #[test]
    fn authenticated_file_ids_skip_hashing_only_with_matching_modes() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let bytes = b"authenticated bytes";
        let root = cache.path().join("identified");
        let path = root.join("file");
        let mut hash = content_hasher(0o600);
        hash.update(bytes);
        let id = *hash.finalize().as_bytes();
        let mut tree = store.owned_tree(&root).unwrap();
        tree.write_identified(&path, 0o600, bytes.as_slice(), (0o600, id))
            .unwrap();
        let stats = tree.publish().unwrap();
        assert_eq!(stats.hashed_bytes, 0);
        assert_eq!(
            identity(&path, &fs::metadata(&path).unwrap()).unwrap(),
            Some(id)
        );
        let other = cache.path().join("fallback");
        let mut tree = store.owned_tree(&other).unwrap();
        let path = other.join("file");
        tree.write_identified(&path, 0o600, bytes.as_slice(), (0o644, [0; 32]))
            .unwrap();
        let stats = tree.publish().unwrap();
        assert_eq!(stats.hashed_bytes, bytes.len() as u64);
        assert_eq!(
            identity(&path, &fs::metadata(&path).unwrap()).unwrap(),
            Some(id)
        );
        assert_eq!(stats.reused, 1);
    }

    #[test]
    fn concurrent_streamed_trees_publish_the_same_durable_inode() {
        let cache = tempfile::tempdir().unwrap();
        let store = Store::open(cache.path()).unwrap();
        let roots = [cache.path().join("one"), cache.path().join("two")];
        let mut trees = Vec::new();
        for root in &roots {
            let mut tree = store.owned_tree(root).unwrap();
            tree.write(
                &root.join("nested/source"),
                0o666,
                b"shared data".as_slice(),
            )
            .unwrap();
            tree.alias(&root.join("nested/source"), &root.join("other/alias"))
                .unwrap();
            trees.push(tree);
        }
        std::thread::scope(|scope| {
            let jobs: Vec<_> = trees
                .into_iter()
                .map(|tree| scope.spawn(move || tree.publish().unwrap()))
                .collect();
            for job in jobs {
                job.join().unwrap();
            }
        });
        let expected = fs::metadata(roots[0].join("nested/source")).unwrap().ino();
        for root in &roots {
            for name in ["nested/source", "other/alias"] {
                let path = root.join(name);
                assert_eq!(fs::read(&path).unwrap(), b"shared data");
                let metadata = fs::metadata(&path).unwrap();
                assert_eq!(metadata.ino(), expected);
                assert!(identity(&path, &metadata).unwrap().is_some());
            }
        }
    }

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
    fn allocation_reports_inaccessible_paths_within_its_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        struct Restricted(PathBuf);
        impl Restricted {
            fn new(path: PathBuf) -> Self {
                fs::create_dir_all(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                Self(path)
            }
        }
        impl Drop for Restricted {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }

        fn assert_permission_error(error: anyhow::Error, path: &Path) {
            assert!(format!("{error:#}").contains(&path.display().to_string()));
            assert!(error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
            }));
        }

        let mount = tempfile::tempdir().unwrap();
        let cache = mount.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("data"), vec![42; 8192]).unwrap();
        let sibling = Restricted::new(mount.path().join("lost+found"));

        let bytes = allocated(&cache).unwrap();
        let allocation = allocated_attributed(&cache, &BTreeMap::new()).unwrap();
        assert!(bytes > 0);
        assert_eq!(allocation.total_bytes, bytes);
        assert_permission_error(allocated(mount.path()).unwrap_err(), &sibling.0);
        assert_permission_error(
            allocated_attributed(mount.path(), &BTreeMap::new()).unwrap_err(),
            &sibling.0,
        );

        let blocked = Restricted::new(cache.join("unreadable"));
        assert_permission_error(allocated(&cache).unwrap_err(), &blocked.0);
        assert_permission_error(
            allocated_attributed(&cache, &BTreeMap::new()).unwrap_err(),
            &blocked.0,
        );
    }

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
    fn combined_owner_costs_match_independent_attribution() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path();
        let repo_a = root.join("repositories/a");
        let repo_b = root.join("repositories/b");
        let nested = repo_a.join("nested");
        let shared = root.join("blobs");
        let unowned = root.join("other");
        for path in [&nested, &repo_b, &shared, &unowned] {
            fs::create_dir_all(path).unwrap();
        }
        let object = shared.join("object");
        fs::write(&object, vec![42; 8192]).unwrap();
        fs::hard_link(&object, nested.join("first")).unwrap();
        fs::hard_link(&object, repo_a.join("second")).unwrap();
        fs::hard_link(&object, repo_b.join("third")).unwrap();
        fs::hard_link(&object, unowned.join("alias")).unwrap();
        fs::write(nested.join("unique"), vec![7; 4096]).unwrap();
        fs::write(unowned.join("unowned"), vec![9; 4096]).unwrap();
        std::os::unix::fs::symlink(&repo_a, root.join("repo-link")).unwrap();

        let repositories = BTreeMap::from([
            (repo_a.clone(), "repository-a".into()),
            (repo_b.clone(), "repository-b".into()),
            (nested.join("unique"), "repository-file".into()),
        ]);
        let owners = vec![
            (root.join("owner-root"), vec![root.to_owned()]),
            (
                root.join("owner-a"),
                vec![repo_a.clone(), nested.clone(), repo_a.clone()],
            ),
            (root.join("owner-b"), vec![nested.clone(), repo_b.clone()]),
            (root.join("owner-c"), vec![repo_b.clone(), shared.clone()]),
            (root.join("owner-d"), vec![shared.clone()]),
            (root.join("owner-file"), vec![nested.join("unique")]),
            (root.join("owner-missing"), vec![root.join("absent")]),
        ];
        let expected = crate::cache::policy::attributed(&owners).unwrap();
        let original = allocated_attributed(root, &repositories).unwrap();
        let (combined, actual) = allocated_with_owners(root, &repositories, &owners).unwrap();

        assert_eq!(actual, expected);
        assert_eq!(combined.total_bytes, allocated(root).unwrap());
        assert_eq!(combined.total_bytes, original.total_bytes);
        assert_eq!(combined.repositories, original.repositories);
        assert_eq!(
            combined.repositories["repository-file"],
            fs::metadata(nested.join("unique")).unwrap().blocks() * 512
        );
        assert_eq!(combined.shared, original.shared);
        assert_eq!(actual[&root.join("owner-missing")], 0);
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
        let owners = vec![
            (cache.path().join("owner-a"), vec![source.clone()]),
            (cache.path().join("owner-b"), vec![source.clone()]),
        ];
        let mut baseline = Vec::new();
        let mut attributed = Vec::new();
        let mut separate = Vec::new();
        let mut combined = Vec::new();
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let total = allocated(cache.path()).unwrap();
            baseline.push(start.elapsed());

            let start = std::time::Instant::now();
            let sample = allocated_attributed(cache.path(), &roots).unwrap();
            attributed.push(start.elapsed());
            assert_eq!(sample.total_bytes, total);

            let start = std::time::Instant::now();
            let separate_allocation = allocated_attributed(cache.path(), &roots).unwrap();
            let separate_costs = crate::cache::policy::attributed(&owners).unwrap();
            separate.push(start.elapsed());

            let start = std::time::Instant::now();
            let (combined_allocation, combined_costs) =
                allocated_with_owners(cache.path(), &roots, &owners).unwrap();
            combined.push(start.elapsed());
            assert_eq!(
                combined_allocation.total_bytes,
                separate_allocation.total_bytes
            );
            assert_eq!(combined_costs, separate_costs);
        }
        baseline.sort();
        attributed.sort();
        separate.sort();
        combined.sort();
        eprintln!(
            "10k-file scan median: allocated={:?}, attributed={:?}, ratio={:.2}x, separate={:?}, combined={:?}",
            baseline[1],
            attributed[1],
            attributed[1].as_secs_f64() / baseline[1].as_secs_f64(),
            separate[1],
            combined[1],
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
    #[ignore = "measures owned imports using SIGLA_IMPORT_PROBE_SOURCE"]
    fn owned_import_real_tree_probe() {
        fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
            fs::create_dir(target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let path = entry.path();
                let target = target.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy_tree(&path, &target)?;
                } else {
                    assert!(entry.file_type()?.is_file());
                    fs::copy(path, target)?;
                }
            }
            Ok(())
        }

        let source = std::env::var_os("SIGLA_IMPORT_PROBE_SOURCE").unwrap();
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let cache = tempfile::Builder::new()
            .prefix("blob-real-import-probe-")
            .tempdir_in(target)
            .unwrap();
        let store = Store::open(cache.path()).unwrap();
        let first = cache.path().join("first");
        let second = cache.path().join("second");
        copy_tree(Path::new(&source), &first).unwrap();
        copy_tree(Path::new(&source), &second).unwrap();
        eprintln!(
            "phase,files,hashed_bytes,adopted,reused,fallback,wall_ms,read_hash_worker_ms,publication_worker_ms,sync_work_ms"
        );
        for (phase, stage) in [("cold", first), ("reuse", second)] {
            let started = Instant::now();
            let stats = store.import_owned_tree(&stage).unwrap();
            eprintln!(
                "{phase},{},{},{},{},{},{},{},{},{}",
                stats.files,
                stats.hashed_bytes,
                stats.adopted,
                stats.reused,
                stats.fallback,
                started.elapsed().as_millis(),
                stats.read_hash.as_millis(),
                stats.publication.as_millis(),
                stats.sync.as_millis()
            );
            assert_eq!(stats.adopted + stats.reused + stats.fallback, stats.files);
        }
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
    fn concurrent_owned_imports_keep_shared_files_after_reopen() {
        let cache = tempfile::tempdir().unwrap();
        let stages = [cache.path().join("first"), cache.path().join("second")];
        for stage in &stages {
            fs::create_dir(stage).unwrap();
            for index in 0..64 {
                fs::write(stage.join(index.to_string()), format!("contents {index}")).unwrap();
            }
        }
        {
            let store = Store::open(cache.path()).unwrap();
            let start = std::sync::Barrier::new(stages.len());
            std::thread::scope(|threads| {
                for stage in &stages {
                    let store = &store;
                    let start = &start;
                    threads.spawn(move || {
                        start.wait();
                        store.import_owned_tree(stage).unwrap();
                    });
                }
            });
        }
        let store = Store::open(cache.path()).unwrap();
        assert_eq!(store.collect().unwrap(), 0);
        for index in 0..64 {
            let first = stages[0].join(index.to_string());
            let second = stages[1].join(index.to_string());
            assert_eq!(
                fs::read_to_string(&first).unwrap(),
                format!("contents {index}")
            );
            assert_eq!(
                fs::metadata(&first).unwrap().ino(),
                fs::metadata(&second).unwrap().ino()
            );
            assert!(store.file_identity(&first).unwrap().is_some());
            assert!(store.file_identity(&second).unwrap().is_some());
        }
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
