//! Repository-owned Git pool. Call blocking operations on a blocking worker.
//! No repository operation lock survives into branch/source publication.
use super::{
    Identity, Repository,
    materialize::{Prepared, Request},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REBUILD_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const RETRY_MS: u64 = 60 * 60 * 1000;
static POOLS: LazyLock<Mutex<HashMap<PathBuf, Weak<Pool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Root {
    pub revision: String,
    pub selected: BTreeSet<String>,
}
impl Root {
    fn prepared(prepared: &Prepared) -> Self {
        Self {
            revision: prepared.revision.clone(),
            selected: prepared.selected.values().cloned().collect(),
        }
    }
}
/// A maintenance snapshot needs only the union, not one copy per branch/tag.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Retention {
    pub revisions: BTreeSet<String>,
    pub selected: BTreeSet<String>,
    /// Validated history objects; unlike snapshot roots, no tree closure is implied.
    #[serde(default)]
    pub history: BTreeSet<String>,
}
impl Retention {
    pub fn add(&mut self, revision: &str, selected: impl IntoIterator<Item = String>) {
        self.revisions.insert(revision.to_owned());
        self.selected.extend(selected);
    }
    fn is_empty(&self) -> bool {
        self.revisions.is_empty() && self.selected.is_empty() && self.history.is_empty()
    }
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    repository: Identity,
    replaced: u64,
    used: u64,
    retry_after: u64,
    #[serde(default)]
    retained: Option<String>,
}
struct Pool {
    root: PathBuf,
    identity: Identity,
    operation: Mutex<u64>,
    pending: Mutex<Vec<Weak<Root>>>,
    readers: Mutex<Vec<Weak<HistoryRoot>>>,
}

/// Hold this until parent-side publication completes or is abandoned.
/// Keeping Pool alive also keeps all acquisitions on the same operation mutex.
#[must_use = "retain the prepared-object pin until selector/package publication finishes"]
pub struct Pin {
    _root: Arc<Root>,
    _pool: Arc<Pool>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.is_dir() && !m.file_type().is_symlink(),
            "Invalid Git pool directory"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && !m.file_type().is_symlink() && m.len() <= max,
        "Invalid Git cache metadata"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn visit_selectors(cache: &Path, mut visit: impl FnMut(super::manager::State)) -> Result<()> {
    let directories = match fs::read_dir(cache.join("repositories")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in directories {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_hexdigit())),
            "Invalid selector cache directory"
        );
        let state = match read_json(&entry.path().join("state.json"), 256 * 1024 * 1024) {
            Ok(state) => state,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(e.context("Cannot establish Git retention roots")),
        };
        visit(state);
    }
    Ok(())
}

struct Snapshot {
    pool: Arc<Pool>,
    generation: u64,
    roots: Retention,
}
impl Snapshot {
    fn capture(pool: Arc<Pool>) -> Result<Option<Self>> {
        let operation = match pool.operation.try_lock() {
            Ok(operation) => operation,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(error) => return Err(anyhow::anyhow!("Git pool operation interrupted: {error}")),
        };
        let generation = *operation;
        let roots = pool.pending_roots()?;
        drop(operation);
        Ok(Some(Self {
            pool,
            generation,
            roots,
        }))
    }
}

impl Pool {
    fn open(cache: &Path, identity: &Identity) -> Result<Arc<Self>> {
        private_dir(&cache.join("git"))?;
        let root = cache.join("git").join(identity.storage_key());
        private_dir(&root)?;
        let root = root.canonicalize()?;
        let mut pools = POOLS
            .lock()
            .map_err(|_| anyhow::anyhow!("Git pool registry poisoned"))?;
        if let Some(pool) = pools.get(&root).and_then(Weak::upgrade) {
            return Ok(pool);
        }
        pools.retain(|_, p| p.strong_count() != 0);
        let pool = Arc::new(Self {
            root: root.clone(),
            identity: identity.clone(),
            operation: Mutex::new(0),
            pending: Mutex::new(Vec::new()),
            readers: Mutex::new(Vec::new()),
        });
        pools.insert(root, Arc::downgrade(&pool));
        Ok(pool)
    }
    fn state(&self) -> Result<State> {
        let path = self.root.join("pool.json");
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let s: State = read_json(&path, 64 * 1024)?;
                ensure!(
                    s.version == 1 && s.repository == self.identity,
                    "Git pool identity mismatch"
                );
                Ok(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Persist the initial clock even when the following acquisition fails.
                // Otherwise an abandoned pool without pool.json never ages out.
                let time = now();
                let state = State {
                    version: 1,
                    repository: self.identity.clone(),
                    replaced: time,
                    used: time,
                    retry_after: 0,
                    retained: None,
                };
                self.persist(&state)?;
                Ok(state)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn persist(&self, state: &State) -> Result<()> {
        super::manager::write_json(&self.root.join("pool.json"), state)?;
        sync_dir(&self.root)
    }
    fn pending_roots(&self) -> Result<Retention> {
        let pending: Vec<Arc<Root>> = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("Git pin registry poisoned"))?;
            let held = pending.iter().filter_map(Weak::upgrade).collect();
            pending.retain(|p| p.strong_count() != 0);
            held
        };
        let mut roots = Retention::default();
        for root in &pending {
            roots.add(&root.revision, root.selected.iter().cloned());
        }
        Ok(roots)
    }
    /// Root capture MUST upgrade pending pins before reading persistent selectors.
    fn selector_roots(&self, cache: &Path) -> Result<Retention> {
        let mut roots = self.pending_roots()?;
        visit_selectors(cache, |state| {
            // Keep even expired-on-paper/repair states until retirement actually removes them.
            if state.repository == self.identity {
                roots.add(
                    &state.prepared.revision,
                    state.prepared.selected.values().cloned(),
                );
            }
        })?;
        Ok(roots)
    }
    fn roots(&self, cache: &Path) -> Result<Retention> {
        let mut roots = self.selector_roots(cache)?;
        self.retain_history(&mut roots)?;
        Ok(roots)
    }
    fn retain_history(&self, roots: &mut Retention) -> Result<()> {
        // Capture active pins before persisted coverage, as for snapshot publication.
        let readers: Vec<_> = {
            let mut readers = self.readers.lock().unwrap();
            readers.retain(|reader| reader.strong_count() != 0);
            readers.iter().filter_map(Weak::upgrade).collect()
        };
        for reader in readers {
            roots
                .history
                .extend(reader.objects.lock().unwrap().iter().cloned());
        }
        if let Some(coverage) = self.history_coverage()? {
            roots.history.extend(coverage.objects);
        }
        Ok(())
    }
    fn pin(self: &Arc<Self>, prepared: &Prepared) -> Result<Pin> {
        let root = Arc::new(Root::prepared(prepared));
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Git pin registry poisoned"))?;
        pending.retain(|p| p.strong_count() != 0);
        pending.push(Arc::downgrade(&root));
        Ok(Pin {
            _root: root,
            _pool: self.clone(),
        })
    }
    fn recover(&self) -> Result<()> {
        let current = self.root.join("current");
        let previous = self.root.join("previous");
        if !previous.try_exists()? {
            return Ok(());
        }
        private_dir(&previous)?;
        if current.try_exists()? {
            private_dir(&current)?;
            if !super::rebuild::completed(&current)? {
                let failed = tempfile::Builder::new()
                    .prefix("invalid-current-")
                    .tempdir_in(&self.root)?;
                fs::rename(&current, failed.path().join("git"))?;
                // Only the invalid candidate belongs to TempDir. previous survives any failure.
                fs::rename(&previous, &current)?;
                sync_dir(&self.root)?;
                return Ok(());
            }
            fs::remove_dir_all(previous)?;
        } else {
            fs::rename(previous, current)?;
        }
        sync_dir(&self.root)
    }
    fn replace(&self, staged: &Path) -> Result<()> {
        ensure!(
            super::rebuild::completed(staged)?,
            "Staged Git pool is not validated"
        );
        let current = self.root.join("current");
        let previous = self.root.join("previous");
        ensure!(!previous.try_exists()?, "Unrecovered Git backup exists");
        let had_current = current.try_exists()?;
        if had_current {
            fs::rename(&current, &previous)?;
            sync_dir(&self.root)?;
        }
        if let Err(error) = fs::rename(staged, &current) {
            if had_current && !current.try_exists()? {
                // previous is NOT owned by a TempDir: failed rollback must retain it.
                let _ = fs::rename(&previous, &current);
                let _ = sync_dir(&self.root);
            }
            return Err(error.into());
        }
        sync_dir(&self.root)?;
        if had_current {
            fs::remove_dir_all(previous)?;
            sync_dir(&self.root)?;
        }
        Ok(())
    }
    fn maybe_rebuild(
        &self,
        cache: &Path,
        state: &mut State,
        force: bool,
        captured: Option<&Retention>,
    ) -> Result<()> {
        let time = now();
        if !force && time.saturating_sub(state.replaced) < REBUILD_MS
            || time < state.retry_after
            || !self.root.join("current").try_exists()?
        {
            return Ok(());
        }
        let fresh;
        let roots = match captured {
            Some(roots) => roots,
            None => {
                fresh = self.roots(cache)?;
                &fresh
            }
        };
        let retained = blake3::hash(&serde_json::to_vec(roots)?)
            .to_hex()
            .to_string();
        if state.retained.as_ref() == Some(&retained) {
            return Ok(());
        }
        let coverage = self.history_coverage()?;
        let result = (|| -> Result<()> {
            let stage = tempfile::Builder::new()
                .prefix("rebuild-")
                .tempdir_in(&self.root)?;
            let target = stage.path().join("git");
            super::rebuild::execute(&self.root.join("current"), &target, roots, cache)?;
            self.replace(&target)?;
            if let Some(mut coverage) = coverage {
                coverage.generation = generation(&self.root.join("current"))?;
                super::manager::write_json(&self.root.join("history.json"), &coverage)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.replaced = time;
                state.retry_after = 0;
                state.retained = Some(retained);
            }
            Err(error) => {
                state.retry_after = time.saturating_add(RETRY_MS);
                tracing::warn!(%error, "Shared Git pool rebuild deferred; current pool retained");
                // A failure between renames must be recovered before the next acquisition.
                self.recover()?;
            }
        }
        self.persist(state)
    }
}

/// All current callers authorize before this call. The original job still resolves
/// branch/tag names and establishes its transport; a warm pool does not bypass it.
/// The worker may inspect `current` for a cached commit prefix while this operation
/// lock is held, so rebuild/replacement cannot race abbreviation resolution.
/// Return the pin alongside Prepared rather than dropping it inside this function.
pub fn acquire(request: &Request, cache: &Path) -> Result<(Prepared, Pin)> {
    let (prepared, pin) = acquire_git(request, cache)?;
    Ok((complete(request, cache, prepared)?, pin))
}

pub(super) fn acquire_git(request: &Request, cache: &Path) -> Result<(Prepared, Pin)> {
    ensure!(
        !matches!(request.target, super::materialize::Target::DefaultBranch),
        "Default-branch discovery must keep using the metadata-only job"
    );
    let repository =
        Repository::parse(&request.repository)?.context("Expected repository identity")?;
    let pool = Pool::open(cache, &repository.identity)?;
    let mut operation = pool
        .operation
        .lock()
        .map_err(|_| anyhow::anyhow!("Git pool operation interrupted"))?;
    // Even a failed acquisition can change the pool. Invalidate earlier root snapshots.
    *operation = operation
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Git acquisition generation exhausted"))?;
    private_dir(&pool.root)?; // A preceding idle-maintenance operation may have removed it.
    pool.recover()?;
    let mut state = pool.state()?;
    pool.maybe_rebuild(cache, &mut state, false, None)?;
    let mut request = request.clone();
    request.store = pool.root.join("current");
    // execute() is synchronous and does not return until its bounded child is stopped.
    let prepared = super::job::execute(&request, cache)?;
    state.used = now();
    state.retained = None;
    pool.persist(&state)?;
    let pin = pool.pin(&prepared)?; // Still under operation lock: no unprotected handoff.
    drop(operation);
    Ok((prepared, pin))
}

pub(super) fn complete(request: &Request, cache: &Path, prepared: Prepared) -> Result<Prepared> {
    // Hydration only uses staged pointers and the LFS cache. It must not hold
    // the Git pool gate while downloading large objects.
    let prepared = super::job::hydrate(request, cache, prepared)?;
    let started = std::time::Instant::now();
    if !matches!(request.contents, super::materialize::Contents::Inventory) {
        crate::cache::blobs::Store::open(cache)?.import_tree(&request.staging)?;
    }
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "Repository files imported"
    );
    Ok(prepared)
}

/// Invoke separately from selector expiration, on a blocking worker, without any
/// branch/workspace state lock. Retained state files and pending pins protect roots.
pub fn maintain(cache: &Path, identity: &Identity, idle_lifetime: Duration) -> Result<()> {
    maintain_pool(cache, identity, idle_lifetime, false, None)
}

fn maintain_pool(
    cache: &Path,
    identity: &Identity,
    idle_lifetime: Duration,
    pressure: bool,
    snapshot: Option<Snapshot>,
) -> Result<()> {
    if !cache
        .join("git")
        .join(identity.storage_key())
        .try_exists()?
    {
        return Ok(());
    }
    let pool = Pool::open(cache, identity)?;
    let operation = match pool.operation.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(anyhow::anyhow!("Git pool operation interrupted: {error}")),
    };
    private_dir(&pool.root)?; // A preceding idle-maintenance operation may have removed it.
    pool.recover()?;
    let mut state = pool.state()?;
    let active = pool
        .readers
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.strong_count() != 0);
    // Pressure eviction is ranked by the global owner policy. Periodic idle
    // maintenance also handles history-only pools, without preparing a selector.
    if !active
        && pool.root.join("history.json").is_file()
        && !pressure
        && now().saturating_sub(state.used)
            >= idle_lifetime.as_millis().try_into().unwrap_or(u64::MAX)
    {
        fs::remove_file(pool.root.join("history.json"))?;
    }
    let mut roots = match snapshot.filter(|snapshot| snapshot.generation == *operation) {
        Some(snapshot) => snapshot.roots,
        None => pool.selector_roots(cache)?,
    };
    // History can be published after the shared selector scan. Capture it under
    // the pool lock, after idle coverage has been retired.
    pool.retain_history(&mut roots)?;
    if !active
        && roots.is_empty()
        && (pressure
            || now().saturating_sub(state.used)
                >= idle_lifetime.as_millis().try_into().unwrap_or(u64::MAX))
    {
        fs::remove_dir_all(&pool.root)?;
        return Ok(());
    }
    pool.maybe_rebuild(cache, &mut state, pressure, Some(&roots))
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

/// Include orphaned pools, not only repositories still present in the selector registry.
pub fn maintain_all(cache: &Path, idle_lifetime: Duration) -> Result<()> {
    maintain_pools(cache, idle_lifetime, false)
}

pub fn reclaim(cache: &Path) -> Result<()> {
    maintain_pools(cache, Duration::ZERO, true)
}

fn maintain_pools(cache: &Path, idle_lifetime: Duration, pressure: bool) -> Result<()> {
    let entries = match fs::read_dir(cache.join("git")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut snapshots = HashMap::new();
    for entry in entries {
        let entry = entry?;
        ensure!(entry.file_type()?.is_dir(), "Invalid Git pool directory");
        let path = entry.path().join("pool.json");
        let state: State = match read_json(&path, 64 * 1024) {
            Ok(state) => state,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                // Recognize only an empty, old pool directory. A live opener
                // either owns the registry entry or will recreate it under its gate.
                let pools = POOLS
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Git pool registry poisoned"))?;
                let root = entry.path().canonicalize()?;
                let owned = pools
                    .get(&root)
                    .is_some_and(|pool| pool.strong_count() != 0);
                let named = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()));
                let old = entry
                    .metadata()?
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age >= idle_lifetime);
                if !owned && named && old && fs::read_dir(&root)?.next().is_none() {
                    fs::remove_dir(root)?;
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        ensure!(
            entry.file_name().to_str() == Some(state.repository.storage_key().as_str()),
            "Git pool directory identity mismatch"
        );
        if let Some(snapshot) = Snapshot::capture(Pool::open(cache, &state.repository)?)? {
            snapshots.insert(state.repository.storage_key(), snapshot);
        }
    }
    if snapshots.is_empty() {
        return Ok(());
    }
    // Capture every pending pin before reading any persistent selector. Acquisitions
    // after capture change the generation and force a fresh per-pool snapshot.
    visit_selectors(cache, |state| {
        if let Some(snapshot) = snapshots.get_mut(&state.repository.storage_key()) {
            snapshot.roots.add(
                &state.prepared.revision,
                state.prepared.selected.into_values(),
            );
        }
    })?;
    for snapshot in snapshots.into_values() {
        let identity = snapshot.pool.identity.clone();
        maintain_pool(cache, &identity, idle_lifetime, pressure, Some(snapshot))?;
    }
    Ok(())
}

/// Coverage is published only after objects have been indexed and validated.
/// A generation mismatch invalidates claims, never triggers an implicit fetch.
#[derive(Default, Serialize, Deserialize)]
pub(super) struct HistoryCoverage {
    version: u32,
    generation: String,
    pub objects: BTreeSet<String>,
    #[serde(default)]
    pub complete: BTreeSet<String>,
    #[serde(default)]
    pub boundaries: BTreeSet<String>,
}
struct HistoryRoot {
    objects: Mutex<BTreeSet<String>>,
}

/// A Git control view over hardlinked immutable objects. Replacement can remove
/// current/previous while this view keeps the exact acquired pack generation alive.
/// Hardlinks are already deduplicated by the global inode-based disk accounting.
pub(super) struct ObjectLease {
    view: tempfile::TempDir,
    root: Arc<HistoryRoot>,
    pool: Arc<Pool>,
}

pub(super) fn generation(store: &Path) -> Result<String> {
    let path = store.join("sigla-generation");
    match fs::read_to_string(&path) {
        Ok(bytes) => {
            ensure!(
                bytes.len() == 64 && bytes.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid Git pool generation"
            );
            return Ok(bytes);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let unique = tempfile::NamedTempFile::new_in(store)?;
    let id = blake3::hash(unique.path().as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    fs::write(&path, &id)?;
    fs::File::open(&path)?.sync_all()?;
    sync_dir(store)?;
    Ok(id)
}

impl Pool {
    fn history_coverage(&self) -> Result<Option<HistoryCoverage>> {
        let path = self.root.join("history.json");
        let coverage: HistoryCoverage = match read_json(&path, 256 * 1024 * 1024) {
            Ok(coverage) => coverage,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let current = self.root.join("current");
        if coverage.version != 1
            || !current.is_dir()
            || fs::read_to_string(current.join("sigla-generation"))
                .ok()
                .as_ref()
                != Some(&coverage.generation)
        {
            return Ok(None);
        }
        Ok(Some(coverage))
    }
}

/// Copy only control metadata; share compressed objects physically. No alternates
/// point at a path that maintenance can replace, and no contents are decoded here.
fn link_objects(source: &Path, target: &Path) -> Result<()> {
    for entry in fs::read_dir(source.join("objects"))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("Invalid Git object directory")?;
        if name != "pack" && !(name.len() == 2 && name.bytes().all(|c| c.is_ascii_hexdigit())) {
            continue;
        }
        ensure!(entry.file_type()?.is_dir(), "Invalid Git object directory");
        let destination = target.join("objects").join(name);
        fs::create_dir_all(&destination)?;
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let filename = file.file_name();
            let filename = filename.to_str().context("Invalid Git object filename")?;
            let supported = if name == "pack" {
                filename.starts_with("pack-")
                    && [".pack", ".idx", ".promisor"]
                        .iter()
                        .any(|s| filename.ends_with(s))
            } else {
                matches!(filename.len(), 38 | 62) && filename.bytes().all(|c| c.is_ascii_hexdigit())
            };
            if !supported {
                continue;
            }
            ensure!(file.file_type()?.is_file(), "Invalid Git object file");
            let dest = destination.join(filename);
            if !dest.try_exists()? {
                fs::hard_link(file.path(), dest)?;
            }
        }
        sync_dir(&destination)?;
    }
    sync_dir(&target.join("objects"))
}

pub(super) fn object_lease(cache: &Path, identity: &Identity, format: &str) -> Result<ObjectLease> {
    let pool = Pool::open(cache, identity)?;
    let operation = pool.operation.lock().unwrap();
    private_dir(&pool.root)?;
    pool.recover()?;
    let mut state = pool.state()?;
    let current = pool.root.join("current");
    super::history::init(&current, format)?;
    generation(&current)?;
    // Orphan reader directories from a crashed process have no registry pins.
    if pool
        .readers
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.strong_count() == 0)
    {
        for entry in fs::read_dir(&pool.root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("history-reader-")
            {
                ensure!(
                    entry.file_type()?.is_dir(),
                    "Invalid history reader directory"
                );
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    let view = tempfile::Builder::new()
        .prefix("history-reader-")
        .tempdir_in(&pool.root)?;
    super::history::init(view.path(), format)?;
    link_objects(&current, view.path())?;
    if pool.history_coverage()?.is_none() {
        let coverage = HistoryCoverage {
            version: 1,
            generation: generation(&pool.root.join("current"))?,
            ..HistoryCoverage::default()
        };
        super::manager::write_json(&pool.root.join("history.json"), &coverage)?;
    }
    let root = Arc::new(HistoryRoot {
        objects: Mutex::new(BTreeSet::new()),
    });
    pool.readers.lock().unwrap().push(Arc::downgrade(&root));
    state.used = now();
    pool.persist(&state)?;
    drop(operation);
    Ok(ObjectLease { view, root, pool })
}

impl ObjectLease {
    pub fn path(&self) -> &Path {
        self.view.path()
    }
    pub fn owner(&self) -> PathBuf {
        self.pool.root.join("history.json")
    }
    pub fn retain(&self, id: &str) {
        self.root.objects.lock().unwrap().insert(id.to_owned());
    }

    /// Serialize publication with replacement, but never hold the gate while matching.
    pub fn acquire(&self, fetch: impl FnOnce(&Path, &Path) -> Result<u64>) -> Result<u64> {
        let _operation = self.pool.operation.lock().unwrap();
        self.pool.recover()?;
        let bytes = fetch(&self.pool.root.join("current"), self.view.path())?;
        link_objects(&self.pool.root.join("current"), self.view.path())?;
        let mut state = self.pool.state()?;
        state.used = now();
        state.retained = None;
        self.pool.persist(&state)?;
        Ok(bytes)
    }

    pub fn publish(
        &self,
        complete: &BTreeSet<String>,
        boundaries: &BTreeSet<String>,
    ) -> Result<()> {
        let _operation = self.pool.operation.lock().unwrap();
        let current = self.pool.root.join("current");
        // A replacement includes every object pinned before root capture. Readers
        // can also keep older objects: link these back before extending coverage.
        link_objects(self.view.path(), &current)?;
        let mut coverage = self.pool.history_coverage()?.unwrap_or_default();
        coverage.version = 1;
        coverage.generation = generation(&current)?;
        coverage
            .objects
            .extend(self.root.objects.lock().unwrap().iter().cloned());
        coverage.complete.extend(complete.iter().cloned());
        coverage.boundaries.extend(boundaries.iter().cloned());
        coverage
            .boundaries
            .retain(|id| !coverage.complete.contains(id));
        super::manager::write_json(&self.owner(), &coverage)?;
        sync_dir(&self.pool.root)?;
        let mut state = self.pool.state()?;
        state.used = now();
        state.retained = None;
        self.pool.persist(&state)
    }
}

pub(crate) fn history_owners(cache: &Path) -> Result<Vec<(PathBuf, Identity, u64)>> {
    let mut result = Vec::new();
    let entries = match fs::read_dir(cache.join("git")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.path().join("history.json").is_file() {
            let state: State = read_json(&entry.path().join("pool.json"), 64 * 1024)?;
            result.push((
                entry.path().join("history.json"),
                state.repository,
                state.used,
            ));
        }
    }
    Ok(result)
}

pub(crate) fn history_owner(cache: &Path, path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "history.json")
        && path.parent().is_some_and(|p| {
            p.parent() == Some(cache.join("git").as_path())
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

pub(crate) fn active_history(cache: &Path) -> BTreeSet<PathBuf> {
    let pools: Vec<_> = POOLS
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    pools
        .into_iter()
        .filter(|p| {
            p.root.starts_with(cache.join("git"))
                && p.readers
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.strong_count() != 0)
        })
        .map(|p| p.root.join("history.json"))
        .collect()
}

pub(crate) fn evict_history(owner: &Path) -> Result<bool> {
    let root = owner.parent().context("Invalid history owner")?;
    let cache = root
        .parent()
        .and_then(Path::parent)
        .context("Invalid history owner")?;
    ensure!(history_owner(cache, owner), "Invalid history owner");
    let state: State = read_json(&root.join("pool.json"), 64 * 1024)?;
    let pool = Pool::open(cache, &state.repository)?;
    let _operation = pool.operation.lock().unwrap();
    if pool
        .readers
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.strong_count() != 0)
    {
        return Ok(false);
    }
    match fs::remove_file(owner) {
        Ok(()) => {
            sync_dir(root)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
