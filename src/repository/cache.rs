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
}
impl Retention {
    pub fn add(&mut self, revision: &str, selected: impl IntoIterator<Item = String>) {
        self.revisions.insert(revision.to_owned());
        self.selected.extend(selected);
    }
    fn is_empty(&self) -> bool {
        self.revisions.is_empty() && self.selected.is_empty()
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
    operation: Mutex<()>,
    pending: Mutex<Vec<Weak<Root>>>,
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
            operation: Mutex::new(()),
            pending: Mutex::new(Vec::new()),
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
    /// Root capture MUST upgrade pending pins before reading persistent selectors.
    fn roots(&self, cache: &Path) -> Result<Retention> {
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
        let directories = match fs::read_dir(cache.join("repositories")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(roots),
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
            let state_path = entry.path().join("state.json");
            let state: super::manager::State = match read_json(&state_path, 256 * 1024 * 1024) {
                Ok(state) => state,
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(e) => return Err(e.context("Cannot establish Git retention roots")),
            };
            // Keep even expired-on-paper/repair states until retirement actually removes them.
            if state.repository == self.identity {
                roots.add(
                    &state.prepared.revision,
                    state.prepared.selected.values().cloned(),
                );
            }
        }
        Ok(roots)
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
    fn maybe_rebuild(&self, cache: &Path, state: &mut State, force: bool) -> Result<()> {
        let time = now();
        if !force && time.saturating_sub(state.replaced) < REBUILD_MS
            || time < state.retry_after
            || !self.root.join("current").try_exists()?
        {
            return Ok(());
        }
        let roots = self.roots(cache)?;
        let retained = blake3::hash(&serde_json::to_vec(&roots)?)
            .to_hex()
            .to_string();
        if force && state.retained.as_ref() == Some(&retained) {
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let stage = tempfile::Builder::new()
                .prefix("rebuild-")
                .tempdir_in(&self.root)?;
            let target = stage.path().join("git");
            super::rebuild::execute(&self.root.join("current"), &target, &roots, cache)?;
            self.replace(&target)
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
    ensure!(
        !matches!(request.target, super::materialize::Target::DefaultBranch),
        "Default-branch discovery must keep using the metadata-only job"
    );
    let repository =
        Repository::parse(&request.repository)?.context("Expected repository identity")?;
    let pool = Pool::open(cache, &repository.identity)?;
    let _operation = pool
        .operation
        .lock()
        .map_err(|_| anyhow::anyhow!("Git pool operation interrupted"))?;
    private_dir(&pool.root)?; // A preceding idle-maintenance operation may have removed it.
    pool.recover()?;
    let mut state = pool.state()?;
    pool.maybe_rebuild(cache, &mut state, false)?;
    let mut request = request.clone();
    request.store = pool.root.join("current");
    // execute() is synchronous and does not return until its bounded child is stopped.
    let prepared = super::job::execute(&request, cache)?;
    crate::cache::blobs::Store::open(cache)?.import_tree(&request.staging)?;
    state.used = now();
    state.retained = None;
    pool.persist(&state)?;
    let pin = pool.pin(&prepared)?; // Still under operation lock: no unprotected handoff.
    Ok((prepared, pin))
}

/// Invoke separately from selector expiration, on a blocking worker, without any
/// branch/workspace state lock. Retained state files and pending pins protect roots.
pub fn maintain(cache: &Path, identity: &Identity, idle_lifetime: Duration) -> Result<()> {
    maintain_pool(cache, identity, idle_lifetime, false)
}

fn maintain_pool(
    cache: &Path,
    identity: &Identity,
    idle_lifetime: Duration,
    pressure: bool,
) -> Result<()> {
    if !cache
        .join("git")
        .join(identity.storage_key())
        .try_exists()?
    {
        return Ok(());
    }
    let pool = Pool::open(cache, identity)?;
    let _operation = match pool.operation.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
        Err(error) => return Err(anyhow::anyhow!("Git pool operation interrupted: {error}")),
    };
    private_dir(&pool.root)?; // A preceding idle-maintenance operation may have removed it.
    pool.recover()?;
    let mut state = pool.state()?;
    if pool.roots(cache)?.is_empty()
        && (pressure
            || now().saturating_sub(state.used)
                >= idle_lifetime.as_millis().try_into().unwrap_or(u64::MAX))
    {
        fs::remove_dir_all(&pool.root)?;
        return Ok(());
    }
    pool.maybe_rebuild(cache, &mut state, pressure)
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
        maintain_pool(cache, &state.repository, idle_lifetime, pressure)?;
    }
    Ok(())
}
