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

fn remove_branch(root: &Path) -> Result<()> {
    crate::sandbox::remove_outputs(&root.join("generated"))?;
    fs::remove_dir_all(root).context("Cannot remove expired repository cache")
}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn options() -> RemoteOptions {
        RemoteOptions {
            rules: vec![],
            refresh_interval: None,
            repo_ttl: Duration::from_secs(7 * 86400),
            branch_ttl: Duration::from_secs(86400),
            unity_versions: vec![],
            selection: super::super::selection::Selection::new(&[], &[]).unwrap(),
        }
    }

    fn branch(target: Target, last_use: u64) -> Branch {
        Branch {
            root: PathBuf::new(),
            repository: Repository::parse("https://github.com/owner/repo")
                .unwrap()
                .unwrap(),
            name: "main".into(),
            target,
            state: Arc::new(AsyncMutex::new(None)),
            last_use: AtomicU64::new(last_use),
            generation: AtomicU64::new(0),
            running: Mutex::new(None),
            retry_after: AtomicU64::new(0),
            acquiring: AsyncMutex::new(()),
        }
    }

    #[test]
    fn deadlines_are_anchored_and_activity_is_per_selector() {
        let options = options();
        let refreshed = 12 * 3600 * 1000;
        let active = branch(Target::Branch("main".into()), 0);
        let idle_tag = branch(Target::Tag("release".into()), 0);
        let delay = options
            .refresh_delay(Duration::from_millis(refreshed))
            .as_millis() as u64;
        assert!(!active.refresh_due(&options, refreshed, refreshed + delay - 1, None));
        assert!(active.refresh_due(&options, refreshed, refreshed + delay + 1, None));
        // A request makes only its selector active, even before the old deadline.
        let request = refreshed + options.refresh_delay(Duration::ZERO).as_millis() as u64 + 1;
        active.last_use.store(request, Ordering::Relaxed);
        assert!(active.refresh_due(&options, refreshed, request, None));
        assert!(!idle_tag.refresh_due(&options, refreshed, request, None));
        // Successful publication starts a new interval; clock rollback cannot make it due.
        assert!(!active.refresh_due(&options, request, request, None));
        assert!(!active.refresh_due(&options, request, request - 1, None));
        assert!(idle_tag.refresh_due(&options, refreshed, refreshed + delay + 1, None));
    }

    #[test]
    fn commits_stay_pinned_and_fixed_intervals_ignore_activity() {
        let mut options = options();
        let pinned = branch(Target::Commit("a".repeat(40)), 0);
        assert!(!pinned.refresh_due(&options, 0, u64::MAX, None));
        let fixed = Duration::from_secs(23);
        options.refresh_interval = Some(fixed);
        let moving = branch(Target::Branch("main".into()), 0);
        let refreshed = 86400 * 1000;
        let deadline = refreshed + fixed.as_millis() as u64;
        for last_use in [0, deadline] {
            moving.last_use.store(last_use, Ordering::Relaxed);
            assert!(!moving.refresh_due(&options, refreshed, deadline - 1, None));
            assert!(moving.refresh_due(&options, refreshed, deadline, None));
        }
        assert!(!pinned.refresh_due(&options, 0, u64::MAX, None));
    }

    #[test]
    fn resolved_abbreviation_stays_pinned_without_expanding_display_selector() {
        let options = options();
        let mut short = branch(Target::Named("abcdef1".into()), 0);
        short.name = "abcdef1".into();
        let prepared = Prepared {
            omitted: Default::default(),
            transfer_bytes: 0,
            unavailable: Default::default(),
            transport: None,
            resolved_target: Some(Target::Commit("a".repeat(40))),
            branch: None,
            revision: "a".repeat(40),
            selected: Default::default(),
            tracked: Default::default(),
            directories: Default::default(),
        };
        let prepared: Prepared =
            serde_json::from_slice(&serde_json::to_vec(&prepared).unwrap()).unwrap();
        assert!(!short.refresh_due(&options, 0, u64::MAX, Some(&prepared)));
        assert!(matches!(
            short.acquisition_target(Some(&prepared)),
            Target::Commit(_)
        ));
        assert_eq!(short.selector(&prepared), "abcdef1");
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub repository: Identity,
    pub transport: String,
    pub branch: String,
    #[serde(default)]
    pub target: Option<Target>,
    pub last_use: u64,
    pub refreshed: u64,
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
    pub target: Target,
    pub state: Arc<AsyncMutex<Option<State>>>,
    pub last_use: AtomicU64,
    pub generation: AtomicU64,
    running: Mutex<Option<Running>>,
    retry_after: AtomicU64,
    acquiring: AsyncMutex<()>,
}

impl Branch {
    fn acquisition_target<'a>(&'a self, prepared: Option<&'a Prepared>) -> &'a Target {
        prepared
            .and_then(|prepared| prepared.resolved_target.as_ref())
            .unwrap_or(&self.target)
    }

    fn refresh_due(
        &self,
        options: &RemoteOptions,
        refreshed: u64,
        now: u64,
        prepared: Option<&Prepared>,
    ) -> bool {
        // Anchor the delay to the last successful refresh. A later access
        // shortens it to the active interval without moving the deadline forward.
        let idle =
            Duration::from_millis(refreshed.saturating_sub(self.last_use.load(Ordering::Relaxed)));
        !matches!(self.acquisition_target(prepared), Target::Commit(_))
            && Duration::from_millis(now.saturating_sub(refreshed)) >= options.refresh_delay(idle)
    }

    pub fn selector(&self, prepared: &Prepared) -> String {
        // Preserve the user's abbreviated selector in summaries; only refresh semantics expand it.
        if matches!(prepared.resolved_target.as_ref(), Some(Target::Commit(_))) {
            return self.name.clone();
        }
        if let Some(branch) = &prepared.branch {
            return format!("refs/heads/{branch}");
        }
        match &self.target {
            Target::Tag(name) | Target::Named(name) => format!("refs/tags/{name}"),
            Target::Commit(id) => id.clone(),
            _ => self.name.clone(),
        }
    }
    pub fn source(&self) -> PathBuf {
        self.root.join("source")
    }
    pub fn analysis_cache(&self) -> PathBuf {
        self.root.join("analysis")
    }
    pub fn persist(&self, state: &State) -> Result<()> {
        write_json(&self.root.join("state.json"), state)
    }
    pub fn record_use(&self) {
        self.last_use.fetch_max(now(), Ordering::Relaxed);
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct DefaultBranch {
    repository: Identity,
    branch: String,
    refreshed: u64,
    #[serde(default)]
    previous: Option<String>,
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
    fn saved_default(&self, identity: &Identity) -> Result<Option<DefaultBranch>> {
        match fs::read(self.cache.join("defaults").join(identity.storage_key())) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn default_ready(&self, identity: &Identity, branch: &str) -> Result<bool> {
        // Named selectors and explicit refs can both resolve to the actual default.
        Ok(self.cached()?.iter().any(|(_, state)| {
            &state.repository == identity
                && state.prepared.branch.as_deref() == Some(branch)
                && !state.repair
                && state.indexed_revision.as_ref() == Some(&state.prepared.revision)
        }))
    }
    fn previous_default(&self, identity: &Identity, next: Option<&str>) -> Result<Option<String>> {
        let Some(saved) = self.saved_default(identity)? else {
            return Ok(None);
        };
        if Some(saved.branch.as_str()) == next {
            return Ok(saved.previous);
        }
        if self.default_ready(identity, &saved.branch)? {
            return Ok(Some(saved.branch));
        }
        Ok(saved.previous.or(Some(saved.branch)))
    }

    pub fn retention(&self, state: &State) -> Result<(bool, Duration)> {
        let default = self.saved_default(&state.repository)?;
        let branch = state.prepared.branch.as_deref().or(match &state.target {
            Some(Target::Branch(name)) => Some(name.as_str()),
            None => Some(state.branch.as_str()),
            _ => None,
        });
        let is_default = default
            .as_ref()
            .is_some_and(|d| branch == Some(d.branch.as_str()));
        let mut protected = is_default;
        if let Some(default) = &default
            && default.previous.as_deref() == branch
            && branch.is_some()
        {
            let ready = self.default_ready(&state.repository, &default.branch)?;
            protected |= !ready;
        }
        let repository = Repository {
            identity: state.repository.clone(),
            transport: state.transport.clone(),
            selector: None,
        };
        // Until the remote's default is known, retain exact allowlist entries
        // conservatively. A failed metadata refresh must not evict their default.
        let pinned = (protected || default.is_none())
            && self
                .options
                .rules
                .iter()
                .any(|rule| rule.exact_match(&repository));
        Ok((
            pinned,
            if is_default {
                self.options.repo_ttl
            } else {
                self.options.branch_ttl
            },
        ))
    }

    fn state_expired(&self, state: &State) -> Result<bool> {
        let (pinned, ttl) = self.retention(state)?;
        Ok(!pinned && now().saturating_sub(state.last_use) >= ttl.as_millis() as u64)
    }
    fn branch_expired(&self, branch: &Branch) -> Result<bool> {
        let bytes = match fs::read(branch.root.join("state.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut state: State = serde_json::from_slice(&bytes)?;
        state.last_use = branch.last_use.load(Ordering::Relaxed);
        self.state_expired(&state)
    }

    pub fn cached(&self) -> Result<Vec<(PathBuf, State)>> {
        let mut result = Vec::new();
        for entry in fs::read_dir(self.cache.join("repositories"))? {
            let entry = entry?;
            ensure!(entry.file_type()?.is_dir(), "Invalid selector directory");
            match fs::read(entry.path().join("state.json")) {
                Ok(bytes) => result.push((entry.path(), serde_json::from_slice(&bytes)?)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(result)
    }
    pub fn migrate_inputs(&self, root: &Path, store: &crate::cache::blobs::Store) -> Result<bool> {
        let branches = self.branches.lock().unwrap();
        let branch = branches.values().find(|branch| branch.root == root);
        let _gate = if let Some(branch) = branch {
            if branch
                .running
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|job| job.borrow().is_none())
            {
                return Ok(false);
            }
            let Ok(gate) = branch.state.clone().try_lock_owned() else {
                return Ok(false);
            };
            Some(gate)
        } else {
            None
        };
        if root.join("source").is_dir() {
            store.import_tree(&root.join("source"))?;
        }
        if root.join("generated/packages").is_dir() {
            crate::cache::packages::View::open(&self.cache, &root.join("generated"))?;
        }
        Ok(true)
    }

    /// Recheck both protection and in-flight acquisitions at the point of removal.
    pub fn evict(
        &self,
        root: &Path,
        pressure: bool,
        retire: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        let mut branches = self.branches.lock().unwrap();
        let branch = branches.values().find(|b| b.root == root).cloned();
        let _gate = if let Some(branch) = &branch {
            if Arc::strong_count(branch) > 2
                || branch
                    .running
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|job| job.borrow().is_none())
            {
                return Ok(false);
            }
            let Ok(gate) = branch.state.clone().try_lock_owned() else {
                return Ok(false);
            };
            Some(gate)
        } else {
            None
        };
        let mut state: State = serde_json::from_slice(&fs::read(root.join("state.json"))?)?;
        if let Some(branch) = &branch {
            state.last_use = branch.last_use.load(Ordering::Relaxed);
        }
        if self.retention(&state)?.0 || !pressure && !self.state_expired(&state)? {
            return Ok(false);
        }
        retire()?;
        remove_branch(root)?;
        branches.retain(|_, branch| branch.root != root);
        Ok(true)
    }

    pub fn new(cache: PathBuf, options: RemoteOptions) -> Result<Arc<Self>> {
        fs::create_dir_all(cache.join("repositories"))?;
        fs::create_dir_all(cache.join("defaults"))?;
        Ok(Arc::new(Self {
            cache,
            options,
            branches: Mutex::new(HashMap::new()),
            defaults: AsyncMutex::new(HashMap::new()),
            acquisitions: Arc::new(Semaphore::new(2)),
            default_running: Mutex::new(HashMap::new()),
        }))
    }

    async fn default_branch(
        self: &Arc<Self>,
        repository: &Repository,
        wait: bool,
    ) -> Result<Option<String>> {
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
                < self
                    .options
                    .refresh_interval
                    .unwrap_or(Duration::from_secs(5 * 60))
                    .as_millis() as u64
        {
            return Ok(Some(saved.branch.clone()));
        }
        let mut running = {
            let mut operations = self.default_running.lock().unwrap();
            let reuse = operations.get(&key).is_some_and(|(started, receive)| {
                let outcome = receive.borrow();
                outcome.is_none()
                    || outcome.as_ref().is_some_and(Result::is_ok)
                        && now().saturating_sub(*started) < 30_000
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
                            transfer_used: 0,
                            unlimited_transfer: manager
                                .options
                                .rules
                                .iter()
                                .any(|r| r.exact_match(&repository)),
                            allow_private: authorize(&manager.options.rules, &repository)?,
                            repository: repository.transport.clone(),
                            preferred_transport: None,
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
                            previous: manager.previous_default(
                                &repository.identity,
                                prepared.branch.as_deref(),
                            )?,
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
            return Ok(Some(cached.branch));
        }
        if !wait {
            return Ok(None);
        }
        loop {
            if let Some(result) = running.borrow().clone() {
                return result.map(|r| Some(r.branch)).map_err(anyhow::Error::msg);
            }
            running.changed().await?;
        }
    }

    /// Returns idle expired branches for the service to close before deleting their stores.
    pub async fn maintain(self: &Arc<Self>) -> Result<Vec<Arc<Branch>>> {
        let branches: Vec<_> = self.branches.lock().unwrap().values().cloned().collect();
        let mut expired = Vec::new();
        for branch in branches {
            if self.branch_expired(&branch)? {
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
                if branch.refresh_due(&self.options, state.refreshed, now(), Some(&state.prepared))
                {
                    self.schedule(branch.clone(), false);
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
            .filter(|branch| self.branch_expired(branch).unwrap_or(false))
            .cloned()
            .collect()
    }

    pub async fn expire(
        &self,
        branch: &Arc<Branch>,
        retire: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut branches = self.branches.lock().unwrap();
        if Arc::strong_count(branch) > 2
            || !self.branch_expired(branch)?
            || branch
                .running
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|r| r.borrow().is_none())
        {
            return Ok(());
        }
        retire()?;
        remove_branch(&branch.root)?;
        branches.retain(|_, value| !Arc::ptr_eq(value, branch));
        Ok(())
    }

    pub async fn resolve(self: &Arc<Self>, repository: Repository) -> Result<Arc<Branch>> {
        if !authorize(&self.options.rules, &repository)? {
            let repository = repository.clone();
            tokio::task::spawn_blocking(move || super::transport::verify_public(&repository))
                .await??;
        }
        let default = self
            .default_branch(&repository, repository.selector.is_none())
            .await?;
        let target = match &repository.selector {
            Some(name) => Target::selector(name)?,
            None => Target::Branch(default.context("Default branch is missing")?),
        };
        let name = match &target {
            Target::Branch(name) | Target::Named(name) | Target::Commit(name) => name.clone(),
            Target::Tag(name) => format!("refs/tags/{name}"),
            Target::DefaultBranch => unreachable!(),
        };
        let identity = if matches!(target, Target::Branch(_)) {
            // Preserve existing default-branch caches across the selector upgrade.
            serde_json::to_vec(&(&repository.identity, &name))?
        } else {
            serde_json::to_vec(&(&repository.identity, &target))?
        };
        let key = blake3::hash(&identity).to_hex().to_string();
        let branch = {
            let mut branches = self.branches.lock().unwrap();
            if let Some(branch) = branches.get(&key) {
                branch.clone()
            } else {
                let root = self.cache.join("repositories").join(&key);
                let saved = root.join("state.json");
                let mut state: Option<State> = if saved.is_file() {
                    Some(serde_json::from_slice(&fs::read(saved)?)?)
                } else {
                    None
                };
                if let Some(value) = &mut state {
                    ensure!(
                        matches!(value.schema, 2..=4)
                            && value.repository == repository.identity
                            && value.branch == name
                            && value
                                .target
                                .clone()
                                .unwrap_or_else(|| Target::Branch(value.branch.clone()))
                                == target,
                        "Cached branch identity is invalid"
                    );
                    if value.schema < 3 {
                        value.repair = true;
                    }
                }
                fs::create_dir_all(&root)?;
                let branch = Arc::new(Branch {
                    root,
                    repository: repository.clone(),
                    name,
                    target,
                    last_use: AtomicU64::new(
                        state.as_ref().map_or_else(now, |state| state.last_use),
                    ),
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
        let state = branch.state.lock().await;
        let usable = state.as_ref().is_some_and(|s| {
            !s.repair
                && branch.source().is_dir()
                && s.policy == self.options.selection.identity
                && s.prepared.selected.keys().all(|p| {
                    s.prepared.unavailable.contains(p) || branch.source().join(p).is_file()
                })
        });
        let due = state.as_ref().is_none_or(|s| {
            branch.refresh_due(&self.options, s.refreshed, now(), Some(&s.prepared))
        });
        drop(state);
        if !usable || due {
            self.schedule(branch.clone(), !usable);
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

    fn schedule(self: &Arc<Self>, branch: Arc<Branch>, requested: bool) {
        let mut operation = branch.running.lock().unwrap();
        if operation.as_ref().is_some_and(|r| r.borrow().is_none())
            || !requested && now() < branch.retry_after.load(Ordering::Relaxed)
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
            transfer_used: before.prepared.transfer_bytes,
            unlimited_transfer: self
                .options
                .rules
                .iter()
                .any(|r| r.exact_match(&branch.repository)),
            allow_private: authorize(&self.options.rules, &branch.repository)?,
            repository: branch.repository.transport.clone(),
            preferred_transport: None,
            target: Target::Commit(revision.to_owned()),
            store: PathBuf::new(),
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
        let (mut prepared, _pool_pin) =
            tokio::task::spawn_blocking(move || super::cache::acquire(&request, &cache)).await??;
        prepared.branch = before.prepared.branch.clone();
        prepared.resolved_target = before.prepared.resolved_target.clone();
        let mut state = branch.state.lock().await;
        let mut next = before;
        next.additional.extend(paths);
        next.prepared = prepared;
        next.repair = true;
        branch.persist(&next)?;
        *state = Some(next.clone());
        for path in next.prepared.selected.keys() {
            let staged = stage.path().join(path);
            if next.prepared.unavailable.contains(path) {
                let target = branch.source().join(path);
                if target.is_file() {
                    fs::remove_file(target)?;
                }
                continue;
            }
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
        let resolved_target = before
            .as_ref()
            .and_then(|state| state.prepared.resolved_target.clone());
        let target =
            Target::clone(branch.acquisition_target(before.as_ref().map(|state| &state.prepared)));
        let request = Request {
            transfer_used: 0,
            unlimited_transfer: self
                .options
                .rules
                .iter()
                .any(|r| r.exact_match(&branch.repository)),
            allow_private: authorize(&self.options.rules, &branch.repository)?,
            repository: branch.repository.transport.clone(),
            preferred_transport: None,
            target,
            store: PathBuf::new(),
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
        let (mut prepared, _pool_pin) =
            tokio::task::spawn_blocking(move || super::cache::acquire(&request, &cache)).await??;
        if resolved_target.is_some() {
            prepared.resolved_target = resolved_target;
        }
        let mut state = branch.state.lock().await;
        let mut next = State {
            schema: 4,
            repository: branch.repository.identity.clone(),
            transport: branch.repository.transport.clone(),
            branch: branch.name.clone(),
            target: Some(branch.target.clone()),
            last_use: branch.last_use.load(Ordering::Relaxed),
            refreshed: now(),
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
            if next.prepared.unavailable.contains(path) {
                let target = source.join(path);
                if target.is_file() {
                    fs::remove_file(target)?;
                }
                continue;
            }
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
                && s.prepared.unavailable == next.prepared.unavailable
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
