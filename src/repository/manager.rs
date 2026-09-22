//! Branch ownership, shared acquisition, and fetch-failure preservation.
use super::{
    Identity, Repository, authorize, job,
    materialize::{Prepared, Request, Target},
};
use crate::config::RemoteOptions;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
type Outcome = std::result::Result<(), String>;
type Running = watch::Receiver<Option<Outcome>>;

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub repository: Identity,
    pub transport: String,
    pub branch: String,
    pub last_use: u64,
    pub refreshed: u64,
    pub store_created: u64,
    pub policy: String,
    pub prepared: Prepared,
    pub indexed_revision: Option<String>,
    pub repair: bool,
    #[serde(default)]
    pub additional: std::collections::BTreeSet<String>,
}

pub struct Branch {
    pub root: PathBuf,
    pub repository: Repository,
    pub name: String,
    pub state: Arc<AsyncMutex<Option<State>>>,
    pub last_use: AtomicU64,
    pub generation: AtomicU64,
    running: Mutex<Option<Running>>,
    retry_after: AtomicU64,
    acquiring: AsyncMutex<()>,
}

impl Branch {
    pub fn source(&self) -> PathBuf {
        self.root.join("source")
    }
    pub fn analysis_cache(&self) -> PathBuf {
        self.root.join("analysis")
    }
    pub fn persist(&self, state: &State) -> Result<()> {
        write_json(&self.root.join("state.json"), state)
    }
    pub fn ttl(&self, options: &RemoteOptions) -> Duration {
        if matches!(self.name.as_str(), "main" | "master") {
            options.repo_ttl
        } else {
            options.branch_ttl
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct DefaultBranch {
    repository: Identity,
    branch: String,
    refreshed: u64,
}
type DefaultOperation = (
    u64,
    watch::Receiver<Option<std::result::Result<DefaultBranch, String>>>,
);

pub struct Manager {
    cache: PathBuf,
    pub options: RemoteOptions,
    branches: Mutex<HashMap<String, Arc<Branch>>>,
    defaults: AsyncMutex<HashMap<String, DefaultBranch>>,
    acquisitions: Arc<Semaphore>,
    default_running: Mutex<HashMap<String, DefaultOperation>>,
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("Managed metadata parent is missing")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

impl Manager {
    pub fn new(cache: PathBuf, options: RemoteOptions) -> Result<Arc<Self>> {
        fs::create_dir_all(cache.join("repositories"))?;
        fs::create_dir_all(cache.join("defaults"))?;
        for entry in fs::read_dir(cache.join("repositories"))? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_dir()
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit()),
                "Invalid managed branch directory"
            );
            let path = entry.path().join("state.json");
            let expired = if path.is_file() {
                let state: State = serde_json::from_slice(&fs::read(path)?)?;
                let ttl = if matches!(state.branch.as_str(), "main" | "master") {
                    options.repo_ttl
                } else {
                    options.branch_ttl
                };
                now().saturating_sub(state.last_use) >= ttl.as_millis() as u64
            } else {
                true
            };
            if expired {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(Arc::new(Self {
            cache,
            options,
            branches: Mutex::new(HashMap::new()),
            defaults: AsyncMutex::new(HashMap::new()),
            acquisitions: Arc::new(Semaphore::new(2)),
            default_running: Mutex::new(HashMap::new()),
        }))
    }

    async fn default_branch(self: &Arc<Self>, repository: &Repository) -> Result<String> {
        let key = repository.identity.storage_key();
        let mut defaults = self.defaults.lock().await;
        if !defaults.contains_key(&key) {
            let path = self.cache.join("defaults").join(&key);
            if path.is_file() {
                let saved: DefaultBranch = serde_json::from_slice(&fs::read(path)?)?;
                ensure!(
                    saved.repository == repository.identity,
                    "Cached repository identity is invalid"
                );
                super::validate_branch(&saved.branch)?;
                defaults.insert(key.clone(), saved);
            }
        }
        let cached = defaults.get(&key).cloned();
        drop(defaults);
        if let Some(saved) = &cached
            && now().saturating_sub(saved.refreshed)
                < self.options.refresh_interval.as_millis() as u64
        {
            return Ok(saved.branch.clone());
        }
        let mut running = {
            let mut operations = self.default_running.lock().unwrap();
            let reuse = operations.get(&key).is_some_and(|(started, receive)| {
                receive.borrow().is_none() || now().saturating_sub(*started) < 30_000
            });
            if !reuse {
                let (send, receive) = watch::channel(None);
                operations.insert(key.clone(), (now(), receive));
                let manager = self.clone();
                let repository = repository.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    let result = async {
                        let request = Request {
                            repository: repository.transport.clone(),
                            target: Target::DefaultBranch,
                            store: PathBuf::new(),
                            staging: PathBuf::new(),
                            include: vec![],
                            exclude: vec![],
                            required: vec![],
                            additional: vec![],
                            previous: BTreeMap::new(),
                            subdirectory: None,
                        };
                        let cache = manager.cache.clone();
                        let _permit = manager.acquisitions.acquire().await?;
                        let prepared =
                            tokio::task::spawn_blocking(move || job::execute(&request, &cache))
                                .await??;
                        let saved = DefaultBranch {
                            repository: repository.identity,
                            branch: prepared.branch.context("Default branch is missing")?,
                            refreshed: now(),
                        };
                        write_json(&manager.cache.join("defaults").join(&key), &saved)?;
                        manager.defaults.lock().await.insert(key, saved.clone());
                        Ok::<_, anyhow::Error>(saved)
                    }
                    .await
                    .map_err(|e| e.to_string());
                    let _ = send.send(Some(result));
                });
            }
            operations[&key].1.clone()
        };
        if let Some(cached) = cached {
            return Ok(cached.branch);
        }
        loop {
            if let Some(result) = running.borrow().clone() {
                return result.map(|r| r.branch).map_err(anyhow::Error::msg);
            }
            running.changed().await?;
        }
    }

    /// Returns idle expired branches for the service to close before deleting their stores.
    pub async fn maintain(self: &Arc<Self>) -> Result<Vec<Arc<Branch>>> {
        let branches: Vec<_> = self.branches.lock().unwrap().values().cloned().collect();
        let mut expired = Vec::new();
        for branch in branches {
            if now().saturating_sub(branch.last_use.load(Ordering::Relaxed))
                >= branch.ttl(&self.options).as_millis() as u64
            {
                expired.push(branch);
                continue;
            }
            if let Ok(mut state) = branch.state.try_lock()
                && let Some(state) = state.as_mut()
            {
                let last_use = branch.last_use.load(Ordering::Relaxed);
                if state.last_use != last_use {
                    state.last_use = last_use;
                    branch.persist(state)?;
                }
                if now().saturating_sub(state.refreshed)
                    >= self.options.refresh_interval.as_millis() as u64
                {
                    self.schedule(branch.clone());
                }
            }
        }
        Ok(expired)
    }

    pub fn expired(&self) -> Vec<Arc<Branch>> {
        self.branches
            .lock()
            .unwrap()
            .values()
            .filter(|branch| {
                now().saturating_sub(branch.last_use.load(Ordering::Relaxed))
                    >= branch.ttl(&self.options).as_millis() as u64
            })
            .cloned()
            .collect()
    }

    pub async fn expire(&self, branch: &Arc<Branch>) -> Result<()> {
        let mut branches = self.branches.lock().unwrap();
        if now().saturating_sub(branch.last_use.load(Ordering::Relaxed))
            < branch.ttl(&self.options).as_millis() as u64
            || branch
                .running
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.borrow().is_none())
        {
            return Ok(());
        }
        fs::remove_dir_all(&branch.root)?;
        branches.retain(|_, value| !Arc::ptr_eq(value, branch));
        Ok(())
    }

    pub async fn resolve(self: &Arc<Self>, repository: Repository) -> Result<Arc<Branch>> {
        authorize(&self.options.rules, &repository)?;
        let name = match &repository.branch {
            Some(name) => name.clone(),
            None => self.default_branch(&repository).await?,
        };
        let key = blake3::hash(&serde_json::to_vec(&(&repository.identity, &name))?)
            .to_hex()
            .to_string();
        let branch = {
            let mut branches = self.branches.lock().unwrap();
            if let Some(branch) = branches.get(&key) {
                branch.last_use.store(now(), Ordering::Relaxed);
                branch.clone()
            } else {
                let root = self.cache.join("repositories").join(&key);
                let saved = root.join("state.json");
                let mut state: Option<State> = if saved.is_file() {
                    Some(serde_json::from_slice(&fs::read(saved)?)?)
                } else {
                    None
                };
                if let Some(value) = &state {
                    ensure!(
                        value.schema == 2
                            && value.repository == repository.identity
                            && value.branch == name,
                        "Cached branch identity is invalid"
                    );
                    let ttl = if matches!(name.as_str(), "main" | "master") {
                        self.options.repo_ttl
                    } else {
                        self.options.branch_ttl
                    };
                    if now().saturating_sub(value.last_use) >= ttl.as_millis() as u64 {
                        fs::remove_dir_all(&root)?;
                        state = None;
                    }
                }
                fs::create_dir_all(&root)?;
                let branch = Arc::new(Branch {
                    root,
                    repository: repository.clone(),
                    name,
                    last_use: AtomicU64::new(now()),
                    generation: AtomicU64::new(0),
                    state: Arc::new(AsyncMutex::new(state)),
                    running: Mutex::new(None),
                    retry_after: AtomicU64::new(0),
                    acquiring: AsyncMutex::new(()),
                });
                branches.insert(key, branch.clone());
                branch
            }
        };
        branch.last_use.store(now(), Ordering::Relaxed);
        let state = branch.state.lock().await;
        let usable = state.as_ref().is_some_and(|s| {
            !s.repair
                && branch.source().is_dir()
                && s.policy == self.options.selection.identity
                && s.prepared
                    .selected
                    .keys()
                    .all(|p| branch.source().join(p).is_file())
        });
        let due = state.as_ref().is_none_or(|s| {
            now().saturating_sub(s.refreshed) >= self.options.refresh_interval.as_millis() as u64
        });
        drop(state);
        if !usable || due {
            self.schedule(branch.clone());
        }
        if !usable {
            let mut running = branch
                .running
                .lock()
                .unwrap()
                .clone()
                .context("Repository preparation was not scheduled")?;
            loop {
                if let Some(outcome) = running.borrow().clone() {
                    outcome.map_err(anyhow::Error::msg)?;
                    break;
                }
                running
                    .changed()
                    .await
                    .context("Repository preparation stopped")?;
            }
        }
        Ok(branch)
    }

    fn schedule(self: &Arc<Self>, branch: Arc<Branch>) {
        let mut operation = branch.running.lock().unwrap();
        if operation.as_ref().is_some_and(|r| r.borrow().is_none())
            || now() < branch.retry_after.load(Ordering::Relaxed)
        {
            return;
        }
        let (send, receive) = watch::channel(None);
        *operation = Some(receive);
        drop(operation);
        let manager = self.clone();
        tokio::spawn(async move {
            let result = manager.refresh(&branch).await.map_err(|e| e.to_string());
            if result.is_err() {
                branch.retry_after.store(now() + 30_000, Ordering::Relaxed);
            }
            let _ = send.send(Some(result));
        });
    }

    /// Acquires discovery-requested inputs from the applied commit, outside the publication gate.
    pub async fn require_inputs(
        &self,
        branch: &Branch,
        revision: &str,
        paths: Vec<String>,
    ) -> Result<()> {
        authorize(&self.options.rules, &branch.repository)?;
        for path in &paths {
            self.options.selection.require(path)?;
        }
        let _operation = branch.acquiring.lock().await;
        let before = branch
            .state
            .lock()
            .await
            .clone()
            .context("Repository inputs are unavailable")?;
        if before.prepared.revision != revision {
            return Ok(());
        }
        let _permit = self.acquisitions.acquire().await?;
        let stage = tempfile::Builder::new()
            .prefix("required-")
            .tempdir_in(&branch.root)?;
        let request = Request {
            repository: branch.repository.transport.clone(),
            target: Target::Commit(revision.to_owned()),
            store: branch.root.join("git"),
            staging: stage.path().to_owned(),
            include: self.options.selection.patterns.0.clone(),
            exclude: self.options.selection.patterns.1.clone(),
            required: paths.clone(),
            additional: before.additional.iter().cloned().collect(),
            previous: before
                .prepared
                .selected
                .iter()
                .filter(|(p, _)| branch.source().join(p).is_file())
                .map(|(p, id)| (p.clone(), id.clone()))
                .collect(),
            subdirectory: None,
        };
        let cache = self.cache.clone();
        let mut prepared =
            tokio::task::spawn_blocking(move || job::execute(&request, &cache)).await??;
        prepared.branch = Some(branch.name.clone());
        let mut state = branch.state.lock().await;
        let mut next = before;
        next.additional.extend(paths);
        next.prepared = prepared;
        next.repair = true;
        branch.persist(&next)?;
        *state = Some(next.clone());
        for path in next.prepared.selected.keys() {
            let staged = stage.path().join(path);
            if staged.is_file() {
                let target = branch.source().join(path);
                fs::create_dir_all(target.parent().unwrap())?;
                fs::rename(staged, target)?;
            }
        }
        next.repair = false;
        branch.persist(&next)?;
        *state = Some(next);
        branch.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }

    async fn refresh(&self, branch: &Branch) -> Result<()> {
        authorize(&self.options.rules, &branch.repository)?;
        let _operation = branch.acquiring.lock().await;
        let _permit = self.acquisitions.acquire().await?;
        let before = branch.state.lock().await.clone();
        let stage = tempfile::Builder::new()
            .prefix("incoming-")
            .tempdir_in(&branch.root)?;
        let rebuild = before
            .as_ref()
            .is_some_and(|s| now().saturating_sub(s.store_created) >= 7 * 24 * 60 * 60 * 1000);
        let replacement = rebuild
            .then(|| {
                tempfile::Builder::new()
                    .prefix("git-rebuild-")
                    .tempdir_in(&branch.root)
            })
            .transpose()?;
        let previous = before
            .as_ref()
            .filter(|s| !s.repair)
            .map(|s| {
                s.prepared
                    .selected
                    .iter()
                    .filter(|(p, _)| branch.source().join(p).is_file())
                    .map(|(p, i)| (p.clone(), i.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let request = Request {
            repository: branch.repository.transport.clone(),
            target: Target::Branch(branch.name.clone()),
            store: replacement
                .as_ref()
                .map_or_else(|| branch.root.join("git"), |r| r.path().join("git")),
            staging: stage.path().to_owned(),
            include: self.options.selection.patterns.0.clone(),
            exclude: self.options.selection.patterns.1.clone(),
            required: vec![],
            additional: before
                .as_ref()
                .map(|s| s.additional.iter().cloned().collect())
                .unwrap_or_default(),
            previous,
            subdirectory: None,
        };
        let cache = self.cache.clone();
        // No publication lock is held while fetching or extracting objects.
        let prepared =
            tokio::task::spawn_blocking(move || job::execute(&request, &cache)).await??;
        let mut state = branch.state.lock().await;
        let mut next = State {
            schema: 2,
            repository: branch.repository.identity.clone(),
            transport: branch.repository.transport.clone(),
            branch: branch.name.clone(),
            last_use: branch.last_use.load(Ordering::Relaxed),
            refreshed: now(),
            store_created: before.as_ref().map_or(now(), |s| s.store_created),
            policy: self.options.selection.identity.clone(),
            prepared,
            indexed_revision: before.as_ref().and_then(|s| s.indexed_revision.clone()),
            repair: true,
            additional: before
                .as_ref()
                .map(|s| s.additional.clone())
                .unwrap_or_default(),
        };
        branch.persist(&next)?;
        *state = Some(next.clone());
        if let Some(replacement) = &replacement {
            let old = replacement.path().join("old");
            fs::rename(branch.root.join("git"), &old)?;
            if let Err(error) = fs::rename(replacement.path().join("git"), branch.root.join("git"))
            {
                fs::rename(old, branch.root.join("git"))?;
                return Err(error.into());
            }
            next.store_created = now();
        }
        let source = branch.source();
        fs::create_dir_all(&source)?;
        if let Some(before) = &before {
            for path in before
                .prepared
                .selected
                .keys()
                .filter(|p| !next.prepared.selected.contains_key(*p))
            {
                let target = source.join(path);
                if target.is_file() {
                    fs::remove_file(target)?;
                }
            }
        }
        for path in next.prepared.selected.keys() {
            let staged = stage.path().join(path);
            if staged.is_file() {
                let target = source.join(path);
                fs::create_dir_all(target.parent().unwrap())?;
                fs::rename(staged, target)?;
            }
        }
        for directory in &next.prepared.directories {
            fs::create_dir_all(source.join(directory))?;
        }
        next.repair = false;
        if before.as_ref().is_some_and(|s| {
            !s.repair
                && s.prepared.selected == next.prepared.selected
                && s.indexed_revision.as_ref() == Some(&s.prepared.revision)
        }) {
            next.indexed_revision = Some(next.prepared.revision.clone());
        }
        branch.persist(&next)?;
        *state = Some(next);
        branch.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }
}
