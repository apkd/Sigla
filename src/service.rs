use crate::cache::Ownership;
use crate::{discovery::Policy, query::Query, search::Search, workspace::Workspace};
use anyhow::{Result, ensure};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
mod history;
mod indexes;
mod inspection;
mod preparation;
mod validation;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
#[path = "service_cache.rs"]
mod disk;

struct WorkspaceSlot {
    state: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    preparing: Option<preparation::Ticket>,
    materialized: Option<u64>,
    used: Instant,
}
#[derive(Clone, Debug)]
enum Request {
    Search(String),
    History(String),
    Browse(String),
    View(String, crate::navigation::Mode),
}
type Startup = tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>;
pub struct App {
    policy: Policy,
    cache: PathBuf,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceSlot>>,
    workers: Arc<tokio::sync::Semaphore>,
    preparation_workers: Arc<tokio::sync::Semaphore>,
    indexing_workers: Arc<tokio::sync::Semaphore>,
    navigation_workers: Arc<tokio::sync::Semaphore>,
    inspection_shutdown: tokio_util::sync::CancellationToken,
    assets: crate::unity::assets::jobs::Jobs,
    sources: crate::native::jobs::Jobs,
    history_cache: Arc<crate::history::Cache>,
    monitor: Arc<crate::watch::Monitor>,
    remote: Option<Arc<crate::repository::manager::Manager>>,
    upstream: Option<crate::upstream::Upstream>,
    startup: Mutex<Option<Startup>>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stateless_summaries: crate::summary::Stateless,
    activity: Arc<tokio::sync::RwLock<()>>,
    blobs: Arc<crate::cache::blobs::Store>,
    asset_records: Arc<crate::cache::artifacts::Store>,
    cache_limits: crate::cache::policy::Limits,
    owners: Mutex<crate::cache::owners::Catalog>,
    #[cfg(test)]
    preparation_started: tokio::sync::Notify,
    #[cfg(test)]
    discovery_started: tokio::sync::Notify,
    #[cfg(test)]
    discovery_pause: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    indexing_pause: Arc<tokio::sync::Semaphore>,
    _ownership: Ownership,
}
impl App {
    #[cfg(test)]
    async fn request_cancellable(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        cancel: &tokio_util::sync::CancellationToken,
        upstream: Option<&crate::upstream::Upstream>,
    ) -> Result<CallToolResult> {
        self.request_cancellable_monitored(
            path,
            request,
            cancel,
            upstream,
            crate::telemetry::Operation::default(),
        )
        .await
    }
    async fn request_cancellable_monitored(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        cancel: &tokio_util::sync::CancellationToken,
        upstream: Option<&crate::upstream::Upstream>,
        operation: crate::telemetry::Operation,
    ) -> Result<CallToolResult> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(crate::diagnostics::Cancelled.into()),
            result = self.dispatch_with_upstream(path, request, upstream, operation) => result,
        }
    }
    fn trim_idle(&self) {
        crate::memory::reclaim();
        let budget = crate::memory::cache_budget();
        self.assets.trim_to(budget / 2);
        self.sources.trim_to(budget / 4);
        let workspace_budget = (budget / 4).saturating_sub(self.history_cache.stats().bytes);
        loop {
            let retired = {
                let mut registry = self.workspaces.lock().unwrap();
                let bytes: usize = registry
                    .values()
                    .filter_map(|slot| {
                        slot.state.try_lock().ok().map(|state| {
                            state
                                .as_ref()
                                .map_or(0, |workspace| workspace.retained_bytes)
                        })
                    })
                    .sum();
                if bytes <= workspace_budget {
                    return;
                }
                let oldest = registry
                    .iter()
                    .filter(|(_, slot)| Arc::strong_count(&slot.state) == 1)
                    .min_by_key(|(_, slot)| slot.used)
                    .map(|(path, _)| path.clone());
                oldest.and_then(|path| registry.remove(&path))
            };
            if retired.is_none() {
                return;
            }
            // Drop cached heap outside the registry lock.
            drop(retired);
            crate::memory::reclaim();
        }
    }

    pub fn new(policy: Policy, cache: PathBuf, workers: usize) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
        ensure!(workers > 0, "At least one worker is required");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&cache)?;
        let metadata = std::fs::symlink_metadata(&cache)?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() },
            "Cache root must be a directory owned by the service account"
        );
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700))?;
        let cache = cache.canonicalize()?;
        let ownership = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(cache.join("ownership.lock"))?;
        ownership
            .try_lock()
            .map_err(|_| anyhow::anyhow!("Another Sigla process owns this cache root"))?;
        crate::cache::compaction::recover(&cache)?;
        crate::cache_migration::migrate(&cache)?;
        crate::memory::start_reclaimer()?;
        let app = Self {
            activity: Arc::new(tokio::sync::RwLock::new(())),
            blobs: crate::cache::blobs::Store::open(&cache)?,
            asset_records: Arc::new(crate::cache::artifacts::Store::open(&cache)?),
            cache_limits: Default::default(),
            owners: Mutex::new(crate::cache::owners::Catalog::open(&cache)?),
            _ownership: Ownership(ownership),
            policy,
            cache,
            workspaces: Mutex::new(HashMap::new()),
            workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            preparation_workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            indexing_workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            navigation_workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            inspection_shutdown: tokio_util::sync::CancellationToken::new(),
            assets: Default::default(),
            sources: Default::default(),
            history_cache: Arc::new(crate::history::Cache::new(
                crate::memory::cache_budget() / 4,
            )),
            monitor: Arc::new(crate::watch::Monitor::default()),
            remote: None,
            upstream: None,
            startup: Mutex::new(None),
            maintenance: Mutex::new(None),
            stateless_summaries: Default::default(),
            #[cfg(test)]
            preparation_started: tokio::sync::Notify::new(),
            #[cfg(test)]
            discovery_started: tokio::sync::Notify::new(),
            #[cfg(test)]
            discovery_pause: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            indexing_pause: Arc::new(tokio::sync::Semaphore::new(1)),
        };
        // Recovery above is mandatory. Retirement and space reclamation are
        // scheduled by start_setup, never part of synchronous construction.
        Ok(app)
    }
    pub fn with_cache_limits(mut self, limits: crate::cache::policy::Limits) -> Self {
        self.cache_limits = limits;
        self
    }

    pub fn remote(
        policy: Policy,
        cache: PathBuf,
        workers: usize,
        options: crate::config::RemoteOptions,
    ) -> Result<Self> {
        let mut app = Self::new(policy, cache.clone(), workers)?;
        let manager = crate::repository::manager::Manager::new(app.cache.clone(), options)?;
        app.remote = Some(manager);
        Ok(app)
    }

    pub fn hybrid(
        policy: Policy,
        cache: PathBuf,
        workers: usize,
        endpoint: &str,
        token_file: Option<&Path>,
    ) -> Result<Self> {
        let upstream = crate::upstream::Upstream::new(endpoint, token_file)?;
        let mut app = Self::new(policy, cache, workers)?;
        app.upstream = Some(upstream);
        Ok(app)
    }

    pub async fn shutdown(&self) {
        self.inspection_shutdown.cancel();
        let tickets: Vec<_> = self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .filter_map(|slot| slot.preparing.clone())
            .collect();
        for mut ticket in tickets {
            let _ = ticket.complete().await;
        }
        self.assets.shutdown();
        self.sources.shutdown();
        // Do not abort a spawn_blocking maintenance batch: join it while its
        // activity guard and cache ownership still protect the environment.
        let maintenance = {
            // Pair with start_setup's registration order, including a racing setup.
            let _startup = self.startup.lock().unwrap();
            self.maintenance.lock().unwrap().take()
        };
        if let Some(maintenance) = maintenance {
            let _ = maintenance.await;
        }
        let _idle = self.activity.write().await;
        if let Err(error) = self.owners.lock().unwrap().flush() {
            tracing::warn!(%error, "Cannot persist cache usage during shutdown");
        }
        if let Some(upstream) = &self.upstream {
            upstream.shutdown().await;
        }
    }

    pub fn start_setup(self: &Arc<Self>) {
        let mut startup = self.startup.lock().unwrap();
        if startup.is_some() || self.inspection_shutdown.is_cancelled() {
            return;
        }
        if let Err(error) = self.save_cache_configuration() {
            tracing::warn!(%error, "Cannot save cache inspection configuration");
        }
        let versions = self
            .remote
            .as_ref()
            .map(|remote| remote.options.unity_versions.clone())
            .unwrap_or_default();
        let (send, receive) = tokio::sync::watch::channel(None);
        *startup = Some(receive);
        {
            let weak = Arc::downgrade(self);
            let collection_interval = std::time::Duration::from_secs(15 * 60);
            let interval = self.remote.as_ref().map_or(collection_interval, |remote| {
                remote.options.maintenance_interval()
            });
            let shutdown = self.inspection_shutdown.clone();
            let maintenance = tokio::spawn(async move {
                let mut schedule = disk::MaintenanceSchedule::new(
                    tokio::time::Instant::now(),
                    collection_interval,
                );
                let mut regular = tokio::time::Instant::now() + interval;
                let retry = std::time::Duration::from_secs(1);
                let mut next_startup = tokio::time::Instant::now() + retry;
                let mut pending = Some(disk::StartupMaintenance::default());
                loop {
                    let deadline = regular.min(schedule.deadline());
                    let deadline = if pending.is_some() {
                        deadline.min(next_startup)
                    } else {
                        deadline
                    };
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep_until(deadline) => (),
                    }
                    let Some(app) = weak.upgrade() else { return };
                    if tokio::time::Instant::now() >= next_startup
                        && let Some(state) = pending.as_mut()
                    {
                        let maintenance =
                            crate::telemetry::operation("startup_maintenance", None, None);
                        maintenance.stage("retiring_cached_analysis");
                        let delay = match app.maintain_startup(state).await {
                            Ok(done) => {
                                maintenance.finish("success");
                                if done {
                                    pending = None;
                                }
                                retry
                            }
                            Err(error) => {
                                maintenance.finish("failure");
                                tracing::warn!(%error, "Startup cache maintenance deferred");
                                std::time::Duration::from_secs(30)
                            }
                        };
                        next_startup = tokio::time::Instant::now() + delay;
                    }
                    if shutdown.is_cancelled() {
                        return;
                    }
                    let now = tokio::time::Instant::now();
                    let collect = now >= schedule.collection;
                    if !collect && schedule.compaction.is_some_and(|deadline| now >= deadline) {
                        match app.retry_compaction().await {
                            Ok(outcome) => schedule.compacted(tokio::time::Instant::now(), outcome),
                            Err(error) => {
                                tracing::warn!(%error, "Analysis compaction retry failed");
                                schedule.compaction = None;
                            }
                        }
                    }
                    if now < regular && !collect {
                        continue;
                    }
                    if let Some(remote) = &app.remote {
                        // Fetch failures stay on the branch preparation state, never on valid query results.
                        match remote.maintain(collect).await {
                            Ok(expired) if !expired.is_empty() => {
                                if let Err(error) = app.expire_idle(expired).await {
                                    tracing::error!(%error, "Cannot expire repository storage");
                                }
                            }
                            Ok(_) => (),
                            Err(error) => {
                                tracing::error!(%error, "Cannot maintain repository ownership state");
                            }
                        }
                    }
                    if shutdown.is_cancelled() {
                        return;
                    }
                    if !collect {
                        regular = tokio::time::Instant::now() + interval;
                        continue;
                    }
                    let outcome = match app.maintain_disk().await {
                        Ok(outcome) => Some(outcome),
                        Err(error) => {
                            tracing::error!(%error, "Cannot maintain disk cache");
                            None
                        }
                    };
                    schedule.collected(tokio::time::Instant::now(), outcome);
                    if shutdown.is_cancelled() {
                        return;
                    }
                    if let Err(error) = app.maintain_git(outcome).await {
                        tracing::error!(%error, "Cannot maintain Git pools");
                    }
                    regular = tokio::time::Instant::now() + interval;
                }
            });
            *self.maintenance.lock().unwrap() = Some(maintenance);
        }
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                for version in versions {
                    let version_label = version.to_string();
                    crate::telemetry::record(
                        "editor_prefetch",
                        None,
                        Some(&version_label),
                        || crate::unity::acquire::prefetch(&cache, version),
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            if let Err(error) = &result {
                tracing::warn!(%error, "Unity editor prefetch failed");
            }
            let _ = send.send(Some(result));
        });
    }

    async fn expire_idle(
        self: &Arc<Self>,
        expired: Vec<Arc<crate::repository::manager::Branch>>,
    ) -> Result<()> {
        let activity = self.activity.clone().read_owned().await;
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let Some(remote) = &app.remote else {
                return Ok(());
            };
            for branch in expired {
                let Ok(_gate) = branch.state.try_write() else {
                    continue;
                };
                let retired = {
                    let mut registry = app.workspaces.lock().unwrap();
                    let path = branch.source();
                    if registry
                        .get(&path)
                        .is_some_and(|slot| Arc::strong_count(&slot.state) != 1)
                    {
                        continue;
                    }
                    registry.remove(&path)
                };
                drop(retired);
                let scopes = app.owner_scopes(&branch.root)?;
                remote.expire(&branch, || app.retire_owner(scopes))?;
            }
            Ok(())
        })
        .await?
    }

    fn owner_scopes(&self, owner: &Path) -> Result<Vec<(crate::store::shared::Scope, PathBuf)>> {
        let analysis = self.cache.join("analysis");
        if !analysis.try_exists()? {
            return Ok(Vec::new());
        }
        let database = crate::store::Database::open(&analysis)?;
        let mut scopes = Vec::new();
        for (key, info) in database.workspaces()? {
            if info.owner.as_deref() == Some(owner)
                && let Some(scope) = database.existing(key)?
                && scope.id == info.id
            {
                scopes.push((scope, info.entry));
            }
        }
        Ok(scopes)
    }

    fn retire_owner(&self, scopes: Vec<(crate::store::shared::Scope, PathBuf)>) -> Result<()> {
        for (scope, entry) in scopes {
            if let Some(current) = scope.database.existing(scope.fingerprint)?
                && current.id == scope.id
            {
                self.sources.forget(&entry);
                current.mark_deleting()?;
            }
        }
        Ok(())
    }

    fn maintain_analysis(&self, drain: bool) -> Result<bool> {
        let analysis = self.cache.join("analysis");
        if !analysis.try_exists()? {
            return Ok(false);
        }
        let database = crate::store::Database::open(&analysis)?;
        let now = crate::cache::now();
        let mut progress = false;
        let mut pending: std::collections::VecDeque<_> = database.workspaces()?.into();
        while let Some((key, info)) = pending.pop_front() {
            if self.inspection_shutdown.is_cancelled() {
                break;
            }
            let retired = self.retire_analysis_scope(&database, key, &info, now)?;
            progress |= retired.progress;
            if !retired.done
                && drain
                && self
                    .workspaces
                    .lock()
                    .unwrap()
                    .get(&info.entry)
                    .is_none_or(|slot| Arc::strong_count(&slot.state) == 1)
            {
                pending.push_back((key, info));
            }
            // Release the registry between batches and rotate large scopes.
            std::thread::yield_now();
        }
        if !drain {
            progress |= database.collect(now, 128)?.objects > 0;
        }
        Ok(progress)
    }

    fn retirement_candidate(info: &crate::store::shared::WorkspaceInfo) -> bool {
        info.phase == crate::store::shared::Phase::Deleting
            || std::fs::symlink_metadata(info.owner.as_deref().unwrap_or(&info.entry))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }

    fn retire_analysis_scope(
        &self,
        database: &Arc<crate::store::Database>,
        key: [u8; 32],
        info: &crate::store::shared::WorkspaceInfo,
        now: u64,
    ) -> Result<disk::Retirement> {
        let mut registry = self.workspaces.lock().unwrap();
        if registry
            .get(&info.entry)
            .is_some_and(|slot| Arc::strong_count(&slot.state) != 1)
        {
            return Ok(disk::Retirement {
                done: false,
                progress: false,
            });
        }
        if !Self::retirement_candidate(info) {
            return Ok(disk::Retirement {
                done: true,
                progress: false,
            });
        }
        let Some(scope) = database.existing(key)? else {
            return Ok(disk::Retirement {
                done: true,
                progress: false,
            });
        };
        // The startup sweep is a snapshot. A retired fingerprint may have been
        // recreated since it was queued; never delete the replacement namespace.
        if scope.id != info.id {
            return Ok(disk::Retirement {
                done: true,
                progress: false,
            });
        }
        registry.remove(&info.entry);
        self.sources.forget(&info.entry);
        scope.mark_deleting()?;
        // Preserve activity -> registry -> storage ordering and exclude opens
        // until this transaction commits. Deleting remains durable between turns.
        let done = scope.delete_batch(now, 128)?;
        Ok(disk::Retirement {
            done,
            progress: true,
        })
    }

    pub async fn search(self: &Arc<Self>, path: &str, query: &str) -> Result<String> {
        result_text(self.dispatch(path, Request::Search(query.into())).await?)
    }
    pub async fn history(self: &Arc<Self>, codebase: &str, query: &str) -> Result<String> {
        result_text(
            self.dispatch(codebase, Request::History(query.into()))
                .await?,
        )
    }
    pub async fn browse(self: &Arc<Self>, project: &str, path: &str) -> Result<String> {
        result_text(self.dispatch(project, Request::Browse(path.into())).await?)
    }
    pub async fn view(self: &Arc<Self>, project: &str, path: &str, mode: &str) -> Result<String> {
        result_text(
            self.dispatch(project, Request::View(path.into(), mode.parse()?))
                .await?,
        )
    }
    async fn dispatch(self: &Arc<Self>, path: &str, request: Request) -> Result<CallToolResult> {
        let result = self
            .dispatch_with_upstream(
                path,
                request,
                self.upstream.as_ref(),
                crate::telemetry::Operation::default(),
            )
            .await?;
        Ok(crate::summary::Session::default().present(result))
    }
    async fn dispatch_with_upstream(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        upstream: Option<&crate::upstream::Upstream>,
        operation: crate::telemetry::Operation,
    ) -> Result<CallToolResult> {
        ensure!(
            !path.trim().is_empty(),
            "Argument `codebase` must not be empty. Provide a local project path or repository URL"
        );
        validation::size("codebase", path, 4096)?;
        match &request {
            Request::Browse(path) | Request::View(path, _) => {
                validation::size("path", path, 4096)?;
            }
            Request::Search(query) | Request::History(query) => {
                validation::size("query", query, 16 * 1024)?;
            }
        }
        if let Some(upstream) = upstream
            && let Some(repository) = crate::repository::Repository::project(path, true)?
        {
            operation.repository(&repository.identity.to_string());
            if let Some(selector) = &repository.selector {
                operation.target(selector);
            }
            tracing::Span::current().record("stage", "upstream");
            operation.stage("upstream");
            let mut project = repository.transport;
            if let Some((_, selector)) = path.trim().split_once('#') {
                project.push('#');
                // Preserve encoding so the upstream decodes the selector exactly once.
                project.push_str(selector);
            }
            let (name, args) = match request {
                Request::History(query) => (
                    "history",
                    serde_json::json!({"codebase":project,"query":query}),
                ),
                Request::Search(query) => (
                    "search",
                    serde_json::json!({"codebase":project,"query":query}),
                ),
                Request::Browse(path) => (
                    "browse",
                    serde_json::json!({"codebase":project,"path":path}),
                ),
                Request::View(path, mode) => (
                    "view",
                    serde_json::json!({"codebase":project,"path":path,"mode":match mode {
                        crate::navigation::Mode::Exact => "exact",
                        crate::navigation::Mode::Minified => "minified",
                    }}),
                ),
            };
            let _wait = operation.wait("upstream");
            return upstream.call(name, args.as_object().unwrap().clone()).await;
        }
        self.request(path, request, operation).await
    }

    async fn request(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        operation: crate::telemetry::Operation,
    ) -> Result<CallToolResult> {
        if let Request::History(query) = &request {
            return self.history_request(path, query, operation).await;
        }
        tracing::Span::current().record("stage", "maintenance");
        operation.stage("maintenance_admission");
        let activity = {
            let _wait = operation.wait("maintenance_admission");
            Arc::new(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    self.activity.clone().read_owned(),
                )
                .await
                .map_err(|_| crate::diagnostics::MaintenanceBusy)?,
            )
        };
        let query = match &request {
            Request::Search(query) => {
                operation.stage("query_parse");
                Some(
                    Query::parse(query)
                        .map_err(|error| anyhow::anyhow!("Invalid search query: {error}"))?,
                )
            }
            _ => None,
        };
        self.start_setup();
        let repository = crate::repository::Repository::project(path, self.remote.is_some())?;
        if let Some(repository) = &repository {
            operation.repository(&repository.identity.to_string());
            if let Some(selector) = &repository.selector {
                operation.target(selector);
            }
        }
        tracing::Span::current().record("stage", "acquisition");
        operation.stage("repository_acquisition");
        if let Some(repository) = &repository {
            tracing::Span::current()
                .record("selector", tracing::field::debug(&repository.selector));
        }
        let branch = match repository {
            Some(repository) => {
                let remote = self.remote.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("Repository identifiers require --mode remote")
                })?;
                let _wait = operation.wait("repository_acquisition");
                let branch = if matches!(request, Request::Browse(_) | Request::View(..)) {
                    remote
                        .resolve_inventory_monitored(repository, &operation)
                        .await?
                } else {
                    remote.resolve_monitored(repository, &operation).await?
                };
                if let Some(job_id) = branch.telemetry_id() {
                    operation.link(&job_id);
                }
                Some(branch)
            }
            None => None,
        };
        let (entry, policy, cache) = if let Some(branch) = &branch {
            let source = branch.source().canonicalize()?;
            let mut policy = Policy::new(vec![source.clone()])?;
            policy.unity_platform = self.policy.unity_platform;
            policy.remote = Some(crate::discovery::RemoteContext {
                workspace: source.clone(),
                tracked: Default::default(),
                writable: branch.root.join("generated"),
                shared: self.cache.clone(),
                repositories: self.remote.as_ref().unwrap().options.rules.clone(),
                selection_identity: self
                    .remote
                    .as_ref()
                    .unwrap()
                    .options
                    .selection
                    .identity
                    .clone(),
            });
            (source, policy, branch.analysis_cache())
        } else {
            let entry = self.policy.canonical(Path::new(path))?;
            let mut policy = self.policy.clone();
            if let Some(remote) = &self.remote {
                let workspace = if entry.is_dir() {
                    entry.clone()
                } else {
                    entry.parent().unwrap().to_owned()
                };
                policy.remote = Some(crate::discovery::RemoteContext {
                    workspace,
                    tracked: Default::default(),
                    writable: self.cache.join("local-jobs").join(
                        blake3::hash(entry.as_os_str().as_encoded_bytes())
                            .to_hex()
                            .as_str(),
                    ),
                    shared: self.cache.clone(),
                    repositories: remote.options.rules.clone(),
                    selection_identity: remote.options.selection.identity.clone(),
                });
            }
            (entry, policy, self.cache.clone())
        };
        if branch.is_none() {
            let repository_path = if entry.is_dir() {
                entry.as_path()
            } else {
                entry.parent().unwrap_or(entry.as_path())
            };
            operation.repository(&repository_path.display().to_string());
        }
        let usage_entry = entry.clone();
        let usage_branch = branch.clone();
        tracing::Span::current().record("stage", "preparation");
        operation.stage("workspace_preparation");
        let (workspace, mut preparation, inventory) = loop {
            let (workspace, mut preparation, retired) = {
                let mut registry = self.workspaces.lock().unwrap();
                let mut retired = Vec::new();
                let slot = registry
                    .entry(entry.clone())
                    .or_insert_with(|| WorkspaceSlot {
                        state: Arc::new(tokio::sync::Mutex::new(None)),
                        preparing: None,
                        materialized: None,
                        used: Instant::now(),
                    });
                slot.used = Instant::now();
                let generation = branch
                    .as_ref()
                    .map(|branch| branch.generation.load(std::sync::atomic::Ordering::Acquire));
                if slot.materialized != generation
                    || slot
                        .preparing
                        .as_ref()
                        .is_none_or(|ticket| ticket.needs_refresh())
                {
                    slot.materialized = generation;
                    slot.preparing = Some(preparation::start(
                        self.clone(),
                        preparation::Request {
                            workspace: slot.state.clone(),
                            entry: entry.clone(),
                            policy: policy.clone(),
                            cache: cache.clone(),
                            branch: branch.clone(),
                            activity: activity.clone(),
                        },
                    ));
                }
                let state = slot.state.clone();
                let ticket = slot.preparing.as_ref().unwrap().clone();
                while registry.len() > 256 {
                    let oldest = registry
                        .iter()
                        .filter(|(_, slot)| Arc::strong_count(&slot.state) == 1)
                        .min_by_key(|(_, slot)| slot.used)
                        .map(|(path, _)| path.clone());
                    let Some(oldest) = oldest else {
                        break;
                    };
                    retired.push(registry.remove(&oldest));
                }
                (state, ticket, retired)
            };
            drop(retired);
            tracing::Span::current().record("shared_job", preparation.id());
            if let Some(job_id) = preparation.telemetry_id() {
                operation.link(&job_id);
            }
            if preparation.discovering()
                && let Some(branch) = &branch
                && (matches!(request, Request::Browse(_))
                    || matches!(request, Request::View(..)) && indexes::source_only(&request, None))
            {
                operation.stage("navigation");
                let _wait = operation.wait("navigation");
                if let Some(result) = self
                    .navigate_repository(branch.clone(), request.clone(), activity.clone())
                    .await?
                {
                    return Ok(result);
                }
            }
            tracing::Span::current().record("stage", "preparation");
            let inventory = {
                let _wait = operation.wait("workspace_preparation");
                preparation.inventory().await?
            };
            if let Some((_, selector, revision, _)) = &inventory.context {
                tracing::Span::current().record("selector", selector.as_str());
                tracing::Span::current().record("revision", revision.as_str());
                operation.target(revision);
            }
            operation.stage("inventory_validation");
            let current = {
                let _wait = operation.wait("inventory_validation");
                inventory.current().await?
            };
            if current {
                break (workspace, preparation, inventory);
            }
            // A shared job may predate this request's filesystem changes.
            // Drain it before preparing a fresh inventory, including when the
            // changes caused its semantic extraction to fail.
            let _ = preparation.complete().await;
        };
        if indexes::navigation(&request, query.as_ref())
            && (preparation.running()
                || preparation.retained_inventory()
                || indexes::source_only(&request, query.as_ref()))
        {
            operation.stage("navigation");
            return self
                .navigate_early(
                    inventory,
                    request,
                    query,
                    (usage_entry, usage_branch),
                    activity,
                    operation.clone(),
                )
                .await;
        }
        tracing::Span::current().record("stage", "source_indexing");
        operation.stage("source_indexing");
        {
            let _wait = operation.wait("workspace_preparation");
            preparation.complete().await?;
        }
        let asset_workspace = workspace.clone();
        let mut state = {
            let _wait = operation.wait("workspace_lock");
            workspace.lock_owned().await
        };
        let mut branch_state = match &branch {
            Some(branch) => {
                let _wait = operation.wait("branch_analysis");
                Some(branch.analyze().await)
            }
            None => None,
        };
        let context = branch.as_ref().map(|branch| {
            let prepared = &branch_state.as_ref().unwrap().as_ref().unwrap().prepared;
            (
                branch.repository.identity.clone(),
                branch.selector(prepared),
                prepared.revision.clone(),
                prepared.tracked.len(),
            )
        });
        let repository_root = branch
            .as_ref()
            .map(|branch| branch.source().canonicalize())
            .transpose()?;
        let allow_absolute = self.remote.is_none();
        let asset_request = match &request {
            Request::History(_) => unreachable!("history bypasses snapshot preparation"),
            Request::Search(_) => query.as_ref().is_some_and(|q| {
                matches!(
                    q.selector.as_str(),
                    "instance" | "references" | "dependencies"
                ) || matches!(q.selector.as_str(), "text" | "file")
                    && !q.filters.iter().any(|f| {
                        matches!(f.key.as_str(), "in" | "project" | "lang")
                            || f.key == "path"
                                && !f.negate
                                && matches!(
                                    std::path::Path::new(&f.value)
                                        .extension()
                                        .and_then(|e| e.to_str()),
                                    Some("cs" | "rs" | "md")
                                )
                    })
                    && !(q.selector == "file"
                        && crate::native::language(Path::new(&q.target.name)).is_some())
            }),
            Request::View(path, _) => {
                path.starts_with("unity@")
                    || crate::navigation::location(path).is_ok_and(|(p, _)| {
                        crate::unity::assets::is_asset(Path::new(p))
                            || crate::documents::language(Path::new(p)).is_none()
                                && !matches!(
                                    Path::new(p).extension().and_then(|s| s.to_str()),
                                    Some("cs" | "rs")
                                )
                                && crate::native::language(Path::new(p)).is_none()
                    })
            }
            Request::Browse(_) => true,
        };
        let asset_generation =
            crate::unity::assets::jobs::generation(&state.as_ref().unwrap().manifest)?;
        let ticket = self.start_assets(
            asset_workspace.clone(),
            branch.clone(),
            &state.as_ref().unwrap().manifest,
            context.as_ref().map(|(_, _, revision, _)| revision.clone()),
            asset_request && branch.is_none(),
            activity.clone(),
        )?;
        if let Some(job_id) = ticket.telemetry_id() {
            operation.link(&job_id);
        }
        let needed = indexes::groups(&request, query.as_ref(), &state.as_ref().unwrap().manifest);
        let mut source_tickets = std::collections::BTreeMap::new();
        for group in [crate::native::Group::Native, crate::native::Group::Shaders] {
            let code = state.as_ref().unwrap();
            source_tickets.insert(
                group,
                self.sources.start(crate::native::jobs::Request {
                    activity: activity.clone(),
                    workspace: asset_workspace.clone(),
                    branch: branch.clone(),
                    entry: code.entry.clone(),
                    analysis: self.cache.join("analysis"),
                    manifest: code.manifest.clone(),
                    group,
                })?,
            );
        }
        for ticket in source_tickets.values() {
            if let Some(job_id) = ticket.telemetry_id() {
                operation.link(&job_id);
            }
        }
        let wait_assets = asset_request && indexes::wait_assets(&request, query.as_ref());
        if wait_assets || needed.values().any(|wait| *wait) {
            // Waiting requests own neither workspace locks nor query workers.
            drop(state);
            drop(branch_state);
            for (group, wait) in &needed {
                if *wait {
                    let _wait = operation.wait("source_indexing");
                    source_tickets[group].clone().wait().await?;
                }
            }
            if wait_assets {
                tracing::Span::current().record("stage", "assets");
                operation.stage("assets");
                let _wait = operation.wait("assets");
                crate::unity::assets::jobs::wait(ticket.clone()).await?;
            }
            state = {
                let _wait = operation.wait("workspace_lock");
                asset_workspace.clone().lock_owned().await
            };
            branch_state = match &branch {
                Some(branch) => {
                    let _wait = operation.wait("branch_analysis");
                    Some(branch.analyze().await)
                }
                None => None,
            };
            ensure!(
                crate::unity::assets::jobs::generation(&state.as_ref().unwrap().manifest)?
                    == asset_generation,
                crate::diagnostics::RefreshRequired(
                    "Workspace changed during indexing; retry query"
                )
            );
            ensure!(
                branch_state
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .map(|s| &s.prepared.revision)
                    == context.as_ref().map(|(_, _, revision, _)| revision),
                crate::diagnostics::RefreshRequired(
                    "Repository changed during indexing; retry query"
                )
            );
        }
        tracing::Span::current().record("stage", "query_admission");
        operation.stage("query_admission");
        let permit = {
            let _wait = operation.wait("query_admission");
            self.workers.clone().acquire_owned().await?
        };
        tracing::Span::current().record("stage", "evaluation");
        operation.stage("evaluation");
        let app = self.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let rendering_activity = activity.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = rendering_activity;
            let _permit = permit;
            app.trim_idle();
            let result = (|| {
                let _branch_state = branch_state;
                let workspace = state.as_mut().unwrap();
                let store = workspace.store.clone();
                let mut manifest = workspace.manifest.as_ref().clone();
                let mut ready = Vec::new();
                let mut pending = std::collections::BTreeSet::new();
                let mut remaining = None;
                for (group, ticket) in &source_tickets {
                    match ticket.ready() {
                        Some(Ok(snapshot)) => {
                            manifest.files.extend(snapshot.manifest.files.clone());
                            ready.push((*group, snapshot));
                        }
                        _ if needed.contains_key(group) => {
                            pending.extend(
                                manifest
                                    .deferred
                                    .values()
                                    .filter(|f| {
                                        crate::native::Group::of(f.language) == Some(*group)
                                    })
                                    .map(|f| f.language.tag()),
                            );
                            if let Some(seconds) = ticket.remaining_seconds() {
                                remaining = Some(remaining.unwrap_or(0).max(seconds));
                            }
                        }
                        _ => (),
                    }
                }
                let assets = if asset_request {
                    ticket.ready_index()
                } else {
                    None
                };
                if asset_request && assets.is_none() {
                    pending.insert("Unity assets");
                }
                // Pin the shared LMDB snapshot under the publication gate. It
                // stays on this blocking thread through rendering and destruction.
                let mut search = Search::new(&store, &manifest, &cancel)?;
                for (group, snapshot) in ready {
                    search.with_source(group, snapshot.store.clone())?;
                }
                if let Some(assets) = &assets {
                    search.with_assets(assets);
                }
                drop(state);
                let root = repository_root.as_deref().unwrap_or(&manifest.root);
                let mut text = match request {
                    Request::History(_) => unreachable!("history bypasses snapshot preparation"),
                    Request::Search(_) => {
                        let query = query.as_ref().unwrap();
                        if matches!(
                            query.selector.as_str(),
                            "instance" | "references" | "dependencies"
                        ) {
                            assets.as_ref().unwrap().search(query)
                        } else if query.selector == "file" {
                            search.files(query, root)
                        } else {
                            search.run(query)
                        }
                    }
                    Request::Browse(path) => search.browse(root, &path, allow_absolute),
                    Request::View(path, mode) => search.view(root, &path, mode, allow_absolute),
                }?;
                if !pending.is_empty() {
                    let eta = remaining
                        .map_or(String::new(), |seconds| format!(" (~{seconds}s remaining)"));
                    text.push_str(&format!(
                        "\n\n> Index incomplete: {}{eta}",
                        pending.into_iter().collect::<Vec<_>>().join(", ")
                    ));
                }
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                if let Some((identity, branch, revision, tracked)) = context {
                    crate::summary::Summary::build(
                        &identity, &branch, &revision, tracked, root, &manifest,
                    )
                    .attach(&mut result);
                }
                app.record_usage(&usage_entry, usage_branch.as_deref(), &manifest);
                Ok(result)
            })();
            app.trim_idle();
            result
        })
        .await?;
        guard.disarm();
        result
    }
    fn record_usage(
        &self,
        entry: &Path,
        branch: Option<&crate::repository::manager::Branch>,
        manifest: &crate::workspace::Manifest,
    ) {
        if let Some(branch) = branch {
            branch.record_use();
        }
        self.owners.lock().unwrap().observe(
            entry,
            branch.map(|branch| branch.root.as_path()),
            manifest,
            Some(crate::cache::now()),
        );
    }
}

fn result_text(result: CallToolResult) -> Result<String> {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    ensure!(result.is_error != Some(true), "{text}");
    Ok(text)
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    /// Local project path or repository URL. Append # and a branch, tag, or commit SHA to select a revision.
    pub codebase: String,
    /// Query using the syntax and examples in the tool description.
    pub query: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowseArguments {
    /// Local project path or repository URL. Append # and a branch, tag, or commit SHA to select a revision.
    pub codebase: String,
    #[serde(default)]
    pub path: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewArguments {
    /// Local project path or repository URL. Append # and a branch, tag, or commit SHA to select a revision.
    pub codebase: String,
    pub path: String,
    #[serde(default)]
    pub mode: crate::navigation::Mode,
}

#[derive(Clone)]
pub struct Mcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
    heartbeat_period: std::time::Duration,
    session: Arc<crate::summary::Session>,
    upstream: Option<Arc<crate::upstream::Upstream>>,
}
impl Mcp {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            session: Arc::default(),
            upstream: app.upstream.as_ref().map(|u| Arc::new(u.session())),
            app,
            tool_router: Self::tool_router(),
            heartbeat_period: rmcp::transport::streamable_http_server::session::local::SessionConfig::DEFAULT_KEEP_ALIVE / 2,
        }
    }
}
#[tool_router]
impl Mcp {
    #[tool(
        name = "search",
        description = r#"Find symbols, follow references, and explore C#, Rust, C/C++, HLSL, GLSL, and ShaderLab code.

Native source and shaders
Bodies are indexed eagerly from source, without preprocessing or build configurations.
Native references are candidates; local variables and parameters can resolve by lexical scope.
Overloads and declaration sites remain distinct. Member signatures support const, &, and other written qualifiers.
derived: matches direct written bases; native impl: is unsupported.
Macro bodies contribute identifier references, not inferred calls or writes.
ShaderLab indexes embedded code and entry-point pragmas, not properties or material links.

Broad queries return ready results and one incomplete-index notice. Stale groups are hidden during refresh.
Queries narrowed to a background group wait for it; wait:complete waits for all relevant groups.

Unity assets
instance:TYPE finds saved and inherited Component/ScriptableObject instances, including derived types.
type-match:exact restricts instance searches to the requested type. unity-project: selects a Unity root.
references:ASSET finds incoming references; dependencies:ASSET finds outgoing references after prefab overrides.
ASSET can be a file path or a returned unity@ object identifier. Pass that identifier to view to inspect it.
Asset queries wait for background indexing; code queries remain available.

Declarations
Bare names find declarations. Qualified names and signatures narrow targets.
writes: includes out arguments and possible writes through ref.

Kinds
t: type: c: class: i: interface: struct: union: enum: delegate: m: method: function: operator: property: field: trait: module: macro:

Filters
project: path: namespace: access: attr: in: lang:
lang: accepts cs, rust, c, cpp, hlsl, glsl, shaderlab, native, or shaders.
Prefix filters with - to exclude matches.
File queries support project:, path:, and lang: filters.
project: matches build-project names.

Matching
match:exact (default)
match:loose broadens matching.

Limits
limit:N returns up to N complete matches (default 20).

Locations
1-based lines and Unicode columns.

Examples
method:Parser.Parse(string)
@src/Parser.cs:20:5
uses:Parser
calls:Parser.Parse
calls:@src/Parser.cs:20:5
writes:Player.health
derived:Base
impl:IParser
method:* in:Parser
calls:* in:Parser.Parse
text:"TODO" path:src/**
file:*.cs
file:src/**/*.cs"#,
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<Arguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        self.execute(&args.codebase, Ok(Request::Search(args.query)), context)
            .await
    }

    #[tool(
        name = "history",
        description = r#"Read Git history for code and files. Start with log to find a change, then use show with its commit SHA to read the diff.

Select a declaration, file, directory, or source location as it appears at the codebase revision. History follows renames and moves. diff compares two commits.

To continue a long result, repeat the query with the returned skip: or offset: value and keep the codebase revision fixed.

Examples
log method:Parser.Parse
show abc1234 method:Parser.Parse
log limit:5
log file:src/Parser.cs limit:5
log path:src/**
log @src/Parser.cs:20:5
log @docs/design.md:20-35
diff abc1234..def5678 file:src/Parser.cs"#,
        annotations(read_only_hint = true)
    )]
    async fn history(
        &self,
        Parameters(args): Parameters<Arguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        self.execute(&args.codebase, Ok(Request::History(args.query)), context)
            .await
    }

    #[tool(
        description = "Browse indexed files. Omit path for the repository root; pass a directory path to explore it.",
        annotations(read_only_hint = true)
    )]
    async fn browse(
        &self,
        Parameters(args): Parameters<BrowseArguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        self.execute(&args.codebase, Ok(Request::Browse(args.path)), context)
            .await
    }

    #[tool(
        description = "Read an indexed file by path. mode: minified (default) simplifies code for reading; exact preserves source text.\n\nExamples:\npath=\"src/Server.cs\"\npath=\"src/Server.cs:20-50\"",
        annotations(read_only_hint = true)
    )]
    async fn view(
        &self,
        Parameters(args): Parameters<ViewArguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        self.execute(
            &args.codebase,
            Ok(Request::View(args.path, args.mode)),
            context,
        )
        .await
    }
}
impl Mcp {
    #[tracing::instrument(skip(self, context), fields(request_id = ?context.id,
        stage = "validation", elapsed_ms = tracing::field::Empty,
        selector = tracing::field::Empty, revision = tracing::field::Empty,
        shared_job = tracing::field::Empty))]
    async fn execute(
        &self,
        codebase: &str,
        request: Result<Request>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let started = Instant::now();
        let operation = context
            .extensions
            .get::<crate::telemetry::Operation>()
            .cloned()
            .unwrap_or_default();
        operation.stage("validation");
        let search = async {
            self.app
                .request_cancellable_monitored(
                    codebase,
                    request?,
                    &context.ct,
                    self.upstream.as_deref(),
                    operation,
                )
                .await
        };
        let heartbeat = async {
            // The HTTP session manager counts transport activity, including while
            // a tool is preparing. Ping only during an active request so a long
            // acquisition survives, while abandoned sessions still expire.
            let period = self.heartbeat_period;
            loop {
                tokio::time::sleep(period).await;
                let ping = async {
                    context
                        .peer
                        .send_request_with_option(
                            rmcp::model::ServerRequest::PingRequest(Default::default()),
                            rmcp::service::PeerRequestOptions::with_timeout(period),
                        )
                        .await?
                        .await_response()
                        .await
                };
                if ping.await.is_err() {
                    return anyhow::anyhow!(
                        "Client stopped responding while waiting for the tool; shared preparation continues"
                    );
                }
            }
        };
        let result = tokio::select! {
            result = search => result,
            error = heartbeat => Err(error),
        };
        tracing::Span::current().record("elapsed_ms", started.elapsed().as_millis() as u64);
        match result {
            Ok(result)
                if context
                    .extensions
                    .get::<axum::http::request::Parts>()
                    .is_some_and(|parts| {
                        !parts.headers.contains_key("mcp-session-id")
                            || context.protocol_version().is_some_and(|version| {
                                version >= rmcp::model::ProtocolVersion::V_2026_07_28
                            })
                    }) =>
            {
                self.app.stateless_summaries.present(result)
            }
            Ok(result) => self.session.present(result),
            Err(e) => {
                if e.chain()
                    .any(|cause| cause.is::<crate::diagnostics::Cancelled>())
                {
                    tracing::info!(codebase, "Tool request cancelled");
                } else {
                    tracing::error!(codebase, error = %format!("{e:#}"), "Tool request failed");
                }
                let mut result =
                    CallToolResult::error(vec![ContentBlock::text(crate::render::error(&e))]);
                result.structured_content = Some(crate::diagnostics::details(
                    &e,
                    &format!("{:?}", context.id),
                ));
                result
            }
        }
    }
}
#[tool_handler(router = self.tool_router, name = "sigla", version = "0.1.0")]
impl ServerHandler for Mcp {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        mut context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let monitored = crate::telemetry::enabled();
        let codebase = monitored
            .then(|| {
                request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get("codebase"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        let query = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("query"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get("path"))
                    .and_then(serde_json::Value::as_str)
            });
        let operation = crate::telemetry::operation(&request.name, None, query);
        operation.stage("validation");
        if monitored {
            context.extensions.insert(operation.clone());
        }
        let result = async {
            let Some(tool) = self.tool_router.get(&request.name) else {
                return Err(rmcp::ErrorData::invalid_params(
                    format!(
                        "Unknown tool `{}`. Use `tools/list` to see available tools.",
                        request.name
                    ),
                    None,
                ));
            };
            if let Some(message) = validation::arguments(tool, request.arguments.as_ref()) {
                return Ok(CallToolResult::error(vec![ContentBlock::text(message)]).into());
            }
            self.tool_router
                .call(rmcp::handler::server::tool::ToolCallContext::new(
                    self, request, context,
                ))
                .await
        }
        .await;
        let status = match &result {
            Err(_) => "failure",
            Ok(rmcp::model::CallToolResponse::Complete(result))
                if result
                    .structured_content
                    .as_ref()
                    .and_then(|value| value.get("error_code"))
                    .and_then(serde_json::Value::as_str)
                    == Some("CANCELLED") =>
            {
                "cancelled"
            }
            Ok(rmcp::model::CallToolResponse::Complete(result))
                if result.is_error == Some(true) =>
            {
                "failure"
            }
            Ok(_) => "success",
        };
        if status != "success"
            && let Some(codebase) = codebase.as_deref()
        {
            match crate::repository::Repository::project(codebase, self.app.remote.is_some()) {
                Ok(Some(repository)) => {
                    operation.repository(&repository.identity.to_string());
                    if let Some(selector) = &repository.selector {
                        operation.target(selector);
                    }
                }
                Ok(None) => {
                    let (path, selector) = if Path::new(codebase).exists() {
                        (codebase, None)
                    } else {
                        codebase
                            .rsplit_once('#')
                            .map_or((codebase, None), |(path, selector)| (path, Some(selector)))
                    };
                    if let Ok(entry) = self.app.policy.canonical(Path::new(path)) {
                        let repository = if entry.is_dir() {
                            entry.as_path()
                        } else {
                            entry.parent().unwrap_or(entry.as_path())
                        };
                        operation.repository(&repository.display().to_string());
                        if let Some(selector) = selector {
                            operation.target(selector);
                        }
                    }
                }
                Err(_) => (),
            }
        }
        operation.finish(status);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mcp_errors_identify_inputs_and_preserve_response_types() {
        use rmcp::{ServiceExt, model::CallToolRequestParams};
        use serde_json::{Value, json};

        #[derive(Clone)]
        struct Logs(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Logs {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Logs(Arc::new(Mutex::new(Vec::new())));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("guide.md"), "First line\nSecond line\n").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let (server_io, client_io) = tokio::io::duplex(65536);
        let server = tokio::spawn(async move { Mcp::new(app).serve(server_io).await.unwrap() });
        let client = ().serve(client_io).await.unwrap();
        let server = server.await.unwrap();
        let project = root.path().to_str().unwrap();
        for (tool, args, field) in [
            ("search", json!({"query":"type:X"}), "codebase"),
            ("history", json!({"codebase":project}), "query"),
            ("view", json!({"codebase":project}), "path"),
            ("browse", json!({"codebase":4}), "codebase"),
            ("search", json!({"codebase":project,"query":null}), "query"),
            ("view", json!({"codebase":project,"path":[]}), "path"),
            ("browse", json!({"codebase":" "}), "codebase"),
            (
                "search",
                json!({"codebase":project,"query":"é".repeat(9000)}),
                "query",
            ),
        ] {
            let response = client
                .call_tool(
                    CallToolRequestParams::new(tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            let response = serde_json::to_value(response).unwrap();
            assert_eq!(response["isError"], true, "{response}");
            let text = response["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(field), "{text}");
            assert!(!text.contains("deserialize"), "{text}");
        }
        let unknown = "unregistered_tool";
        let error = client
            .call_tool(CallToolRequestParams::new(unknown))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(unknown));
        assert!(error.to_string().contains("tools/list"));

        // Defaults and valid tool dispatch still work after argument validation.
        for (tool, args, expected) in [
            ("browse", json!({"codebase":project}), "guide.md"),
            (
                "view",
                json!({"codebase":project,"path":"guide.md:1-1"}),
                "First line",
            ),
            (
                "view",
                json!({"codebase":project,"path":"absent.rs"}),
                "absent.rs",
            ),
        ] {
            let response = client
                .call_tool(
                    CallToolRequestParams::new(tool)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            let response: Value = serde_json::to_value(response).unwrap();
            assert_ne!(response["isError"], true, "{response}");
            assert!(
                response["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{response}"
            );
        }
        client.cancel().await.unwrap();
        server.cancel().await.unwrap();
        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let failure = captured
            .lines()
            .find(|line| line.contains("Tool request failed"))
            .unwrap();
        assert!(failure.contains("request_id="), "{failure}");
        assert!(failure.contains("stage="), "{failure}");
        assert!(failure.contains("elapsed_ms="), "{failure}");
    }

    #[test]
    fn advertised_tool_arguments_match_deserialization() {
        for tool in Mcp::tool_router().list_all() {
            let properties = tool
                .input_schema
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap();
            let arguments: serde_json::Map<String, serde_json::Value> = properties
                .keys()
                .filter(|key| key.as_str() != "mode")
                .map(|key| (key.clone(), serde_json::Value::String(String::new())))
                .collect();
            let value = serde_json::Value::Object(arguments);
            match tool.name.as_ref() {
                "search" | "history" => {
                    serde_json::from_value::<Arguments>(value).unwrap();
                }
                "browse" => {
                    serde_json::from_value::<BrowseArguments>(value).unwrap();
                }
                "view" => {
                    serde_json::from_value::<ViewArguments>(value).unwrap();
                }
                _ => panic!("Uncovered tool: {}", tool.name),
            }
        }
    }
    #[tokio::test]
    async fn failed_indexing_retains_source_navigation() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub struct Available;").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        // Closing the indexing gate fails preparation after publishing inventory.
        app.indexing_pause.close();
        let path = root.path().to_str().unwrap();
        assert!(app.search(path, "type:Available").await.is_err());
        assert!(app.browse(path, "").await.unwrap().contains("lib.rs"));
        assert!(app.search(path, "file:*").await.unwrap().contains("lib.rs"));
        assert!(
            app.view(path, "lib.rs", "exact")
                .await
                .unwrap()
                .contains("Available")
        );
        assert!(app.search(path, "file:* wait:complete").await.is_err());
    }
    #[tokio::test]
    async fn navigation_precedes_indexing_and_cancelled_waiters_do_not_stop_it() {
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            "mod child; pub struct Available;",
        )
        .unwrap();
        std::fs::write(root.path().join("child.rs"), "pub fn child_function() {}").unwrap();
        std::fs::write(root.path().join("invalid.rs"), [0xff]).unwrap();
        std::fs::write(root.path().join("Managed.cs"), "public class Managed {}").unwrap();
        std::fs::write(root.path().join("native.cpp"), "void native_function() {}").unwrap();
        std::fs::write(root.path().join("temporary.md"), "Temporary document").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
        let path = root.path().to_str().unwrap();
        let cancelled = tokio_util::sync::CancellationToken::new();
        let waiting = {
            let app = app.clone();
            let path = path.to_owned();
            let cancelled = cancelled.clone();
            tokio::spawn(async move {
                app.request_cancellable(
                    &path,
                    Request::Search("type:Available".into()),
                    &cancelled,
                    None,
                )
                .await
            })
        };
        app.preparation_started.notified().await;
        tokio::time::timeout(Duration::from_secs(5), app.maintain_disk())
            .await
            .expect("Maintenance must leave navigation available during indexing")
            .unwrap();
        for (query, expected) in [
            ("file:*.rs", "child.rs"),
            ("file:*.cpp", "native.cpp"),
            ("file:*.cs", "Managed.cs"),
        ] {
            let result = tokio::time::timeout(Duration::from_secs(5), app.search(path, query))
                .await
                .unwrap()
                .unwrap();
            assert!(result.contains(expected), "{result}");
            assert!(!result.contains("invalid.rs"), "{result}");
        }
        let tree = tokio::time::timeout(Duration::from_secs(5), app.browse(path, ""))
            .await
            .unwrap()
            .unwrap();
        assert!(tree.contains("lib.rs") && tree.contains("child.rs"));
        let source =
            tokio::time::timeout(Duration::from_secs(5), app.view(path, "child.rs", "exact"))
                .await
                .unwrap()
                .unwrap();
        assert!(source.contains("pub fn child_function() {}"));
        assert!(app.view(path, "../outside.rs", "exact").await.is_err());
        assert!(!waiting.is_finished());
        let mut other = Box::pin(app.search(path, "type:Available"));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                other.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        cancelled.cancel();
        let error = waiting.await.unwrap().unwrap_err();
        let details = crate::diagnostics::details(&error, "waiter");
        assert_eq!(details["error_code"], "CANCELLED");
        assert_eq!(details["retryable"], false);
        drop(pause);
        assert!(other.await.unwrap().contains("Available"));
        assert!(
            app.search(path, "type:Available")
                .await
                .unwrap()
                .contains("Available")
        );
        // The next inventory must reflect edits while the new index is pending.
        let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
        std::fs::write(root.path().join("child.rs"), "pub fn replacement() {}").unwrap();
        let source =
            tokio::time::timeout(Duration::from_secs(5), app.view(path, "child.rs", "exact"))
                .await
                .unwrap()
                .unwrap();
        assert!(source.contains("replacement") && !source.contains("child_function"));
        std::fs::remove_file(root.path().join("temporary.md")).unwrap();
        std::fs::write(root.path().join("added.md"), "Added during indexing").unwrap();
        let files = app.search(path, "file:*.md");
        tokio::pin!(files);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut files)
                .await
                .is_err(),
            "A changed inventory must wait for a fresh preparation"
        );
        drop(pause);
        let files = tokio::time::timeout(Duration::from_secs(5), files)
            .await
            .unwrap()
            .unwrap();
        assert!(files.contains("added.md") && !files.contains("temporary.md"));
        assert!(
            app.search(path, "function:replacement")
                .await
                .unwrap()
                .contains("replacement")
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn source_groups_are_independent_and_waits_leave_code_available() {
        use crate::native::Group;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("code.rs"), "pub struct Available;").unwrap();
        std::fs::write(root.path().join("native.cpp"), "void native_ready() {}").unwrap();
        std::fs::write(
            root.path().join("shader.hlsl"),
            "float shader_ready() { return 0; }",
        )
        .unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let native = app.sources.pause(Group::Native).await;
        let shader = app.sources.pause(Group::Shaders).await;
        let path = root.path().to_str().unwrap();
        let broad = app.search(path, "symbol:*").await.unwrap();
        assert!(
            broad.contains("Available")
                && !broad.contains("native_ready")
                && !broad.contains("shader_ready"),
            "{broad}"
        );
        assert_eq!(broad.matches("Index incomplete:").count(), 1);
        let waiting = {
            let app = app.clone();
            let path = path.to_owned();
            tokio::spawn(async move { app.search(&path, "symbol:* wait:complete").await })
        };
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            app.search(path, "type:Available lang:rust"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(code.contains("Available") && !code.contains("Index incomplete:"));
        drop(shader);
        let shader = tokio::time::timeout(
            Duration::from_secs(5),
            app.search(path, "function:shader_ready lang:hlsl"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(shader.contains("shader_ready"));
        assert!(!waiting.is_finished());
        waiting.abort();
        drop(native);
        assert!(
            app.search(path, "function:native_ready lang:cpp")
                .await
                .unwrap()
                .contains("native_ready")
        );

        let pause = app.sources.pause(Group::Native).await;
        std::fs::write(root.path().join("native.cpp"), "void intermediate() {}").unwrap();
        let broad = app.search(path, "symbol:*").await.unwrap();
        assert!(
            !broad.contains("native_ready") && broad.contains("Available"),
            "{broad}"
        );
        std::fs::write(root.path().join("native.cpp"), "void newest() {}").unwrap();
        let broad = app.search(path, "symbol:*").await.unwrap();
        assert!(!broad.contains("intermediate"), "{broad}");
        drop(pause);
        let current = app.search(path, "function:* lang:cpp").await.unwrap();
        assert!(
            current.contains("newest")
                && !current.contains("intermediate")
                && !current.contains("native_ready"),
            "{current}"
        );
        let database = crate::store::Database::open(&cache.path().join("analysis")).unwrap();
        let fingerprint = database.workspaces().unwrap()[0].0;
        let scope = database.existing(fingerprint).unwrap().unwrap();
        let before = scope.read().unwrap().id();
        app.sources.trim_completed();
        app.workspaces.lock().unwrap().clear();
        let reopened = app
            .search(path, "function:* lang:cpp wait:complete")
            .await
            .unwrap();
        assert!(reopened.contains("newest") && !reopened.contains("intermediate"));
        assert_eq!(
            scope.read().unwrap().id(),
            before,
            "Reopening completed groups must not write analysis"
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn asset_wait_does_not_hold_code_workspace_or_query_worker() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("code.rs"), "pub struct Available;").unwrap();
        std::fs::create_dir(root.path().join("Assets")).unwrap();
        std::fs::create_dir(root.path().join("ProjectSettings")).unwrap();
        std::fs::write(
            root.path().join("ProjectSettings/ProjectVersion.txt"),
            "version",
        )
        .unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let pause = app.assets.pause().await;
        let waiting = {
            let app = app.clone();
            let project = root.path().join("code.rs").to_string_lossy().into_owned();
            tokio::spawn(async move { app.search(&project, "instance:*").await })
        };
        app.preparation_started.notified().await;
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.search(
                root.path().join("code.rs").to_str().unwrap(),
                "type:Available",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(code.contains("Available"));
        assert!(!waiting.is_finished());
        drop(pause);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        app.shutdown().await;
    }
    #[tokio::test]
    async fn remote_navigation_does_not_wait_for_indexing_or_retain_queued_branch_guards() {
        use crate::repository::{Repository, Rule, materialize::Target};
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let cache = tempfile::tempdir().unwrap();
        let project = "https://github.com/fixture/repo#refs/heads/main";
        let repository = Repository::parse(project).unwrap().unwrap();
        let key = blake3::hash(&serde_json::to_vec(&(&repository.identity, "main")).unwrap())
            .to_hex()
            .to_string();
        let owner = cache.path().join("repositories").join(key);
        let source = owner.join("source");
        std::fs::create_dir_all(source.join("src")).unwrap();
        std::fs::write(
            source.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(source.join("src/lib.rs"), "pub struct Prepared;").unwrap();
        let selection = crate::repository::selection::Selection::new(&[], &[]).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let saved = serde_json::json!({
            "schema": 4, "repository": repository.identity, "transport": repository.transport,
            "branch": "main", "target": Target::Branch("main".into()),
            "last_use": now, "refreshed": now, "store_created": now,
            "policy": selection.identity, "repair": false, "indexed_revision": null,
            "prepared": {"branch": "main", "revision": "a".repeat(40),
                "selected": {"Cargo.toml": "b", "src/lib.rs": "c"}, "tracked": {"Cargo.toml": "b", "src/lib.rs": "c"}, "directories": ["src"]}
        });
        std::fs::write(
            owner.join("state.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        let app = Arc::new(
            App::remote(
                Policy::new(vec![source.clone()]).unwrap(),
                cache.path().into(),
                1,
                crate::config::RemoteOptions {
                    rules: vec![Rule::parse_private("https://github.com/fixture/*").unwrap()],
                    refresh_interval: Some(Duration::from_secs(3600)),
                    repo_ttl: Duration::from_secs(86400),
                    branch_ttl: Duration::from_secs(86400),
                    unity_versions: vec![],
                    selection,
                },
            )
            .unwrap(),
        );
        let branch = app
            .remote
            .as_ref()
            .unwrap()
            .resolve(repository)
            .await
            .unwrap();
        let workspace = Arc::new(tokio::sync::Mutex::new(None));
        app.workspaces.lock().unwrap().insert(
            source,
            WorkspaceSlot {
                state: workspace.clone(),
                preparing: None,
                materialized: None,
                used: Instant::now(),
            },
        );
        // Stand in for A's workspace guard while it releases branch state to
        // handle RequiredInputs. B must not retain branch state while queued.
        let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
        let held_workspace = workspace.lock().await;
        let discovery_pause = app.discovery_pause.clone().acquire_owned().await.unwrap();
        let request = {
            let app = app.clone();
            tokio::spawn(async move { app.search(project, "Prepared").await })
        };
        tokio::time::timeout(Duration::from_secs(5), app.preparation_started.notified())
            .await
            .unwrap();
        let reacquired = branch.state.try_write().is_ok();
        let tree = tokio::time::timeout(Duration::from_secs(5), app.browse(project, "src"))
            .await
            .unwrap()
            .unwrap();
        assert!(tree.contains("lib.rs"), "{tree}");
        let source = tokio::time::timeout(
            Duration::from_secs(5),
            app.view(project, "src/lib.rs", "exact"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(source.contains("pub struct Prepared;"), "{source}");
        assert!(!request.is_finished());
        drop(held_workspace);
        tokio::time::timeout(Duration::from_secs(5), app.discovery_started.notified())
            .await
            .unwrap();
        // Discovery owns the workspace and branch analysis guards here.
        let tree = tokio::time::timeout(Duration::from_secs(5), app.browse(project, "src"))
            .await
            .unwrap()
            .unwrap();
        assert!(tree.contains("lib.rs"));
        let source = tokio::time::timeout(
            Duration::from_secs(5),
            app.view(project, "src/lib.rs", "exact"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(source.contains("pub struct Prepared;"));
        assert!(!request.is_finished());
        drop(discovery_pause);
        let mut ticket = app
            .workspaces
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .preparing
            .clone()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), ticket.inventory())
            .await
            .unwrap()
            .unwrap();
        let mut publication = Box::pin(branch.publish());
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                publication.as_mut(),
                cx
            )))
            .await
            .is_pending()
        );
        let tree = tokio::time::timeout(Duration::from_secs(5), app.browse(project, "src"))
            .await
            .unwrap()
            .unwrap();
        assert!(tree.contains("lib.rs"), "{tree}");
        let source = tokio::time::timeout(
            Duration::from_secs(5),
            app.view(project, "src/lib.rs", "exact"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(source.contains("pub struct Prepared;"), "{source}");
        assert!(
            branch.state.try_write().is_err(),
            "Indexing must protect its source generation from publication"
        );
        drop(pause);
        drop(
            tokio::time::timeout(Duration::from_secs(5), publication)
                .await
                .unwrap(),
        );
        let result = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(reacquired, "Queued preparation retained branch state");
        assert!(result.contains("Prepared"), "{result}");
        branch
            .last_use
            .store(0, std::sync::atomic::Ordering::Relaxed);
        drop(branch);
        drop(workspace);
        let expired = app.remote.as_ref().unwrap().maintain(true).await.unwrap();
        app.expire_idle(expired).await.unwrap();
        assert!(!owner.exists());
        app.maintain_analysis(false).unwrap();
        assert!(
            crate::store::Database::open(&cache.path().join("analysis"))
                .unwrap()
                .workspaces()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deleted_local_workspaces_retire_and_can_be_created_again() {
        let directory = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("README.md"), "Before").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![directory.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        app.view(project.to_str().unwrap(), "README.md", "exact")
            .await
            .unwrap();
        // Navigation may finish while indexing still owns the workspace.
        // Production maintenance drains those jobs before retiring namespaces.
        let gate = app.activity.write().await;
        let database = crate::store::Database::open(&cache.path().join("analysis")).unwrap();
        let before = database.workspaces().unwrap()[0].1.id;
        app.maintain_analysis(false).unwrap();
        assert_eq!(database.workspaces().unwrap()[0].1.id, before);
        std::fs::remove_file(project.join("README.md")).unwrap();
        std::fs::remove_dir(&project).unwrap();
        app.maintain_analysis(false).unwrap();
        assert!(database.workspaces().unwrap().is_empty());
        drop(gate);
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("README.md"), "After").unwrap();
        assert!(
            app.view(project.to_str().unwrap(), "README.md", "exact")
                .await
                .unwrap()
                .contains("After")
        );
        assert!(database.workspaces().unwrap()[0].1.id > before);
        app.shutdown().await;
    }

    #[test]
    fn startup_defers_interrupted_namespace_retirement() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let database = crate::store::Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database.workspace([1; 32], root.path(), None).unwrap();
        scope.mark_deleting().unwrap();
        drop(scope);
        drop(database);
        let app = App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap();
        let database = crate::store::Database::open(&app.cache.join("analysis")).unwrap();
        let workspaces = database.workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].1.phase, crate::store::shared::Phase::Deleting);
        assert!(database.workspace([1; 32], root.path(), None).is_err());
    }

    #[tokio::test]
    async fn editor_prefetch_does_not_gate_unrelated_requests() {
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='prefetch'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub struct Available;").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let (prefetch, pending) = tokio::sync::watch::channel(None);
        *app.startup.lock().unwrap() = Some(pending);
        let path = root.path().to_str().unwrap();
        let tree = tokio::time::timeout(Duration::from_secs(5), app.browse(path, "src"))
            .await
            .unwrap()
            .unwrap();
        assert!(tree.contains("lib.rs"), "{tree}");
        let source = tokio::time::timeout(
            Duration::from_secs(5),
            app.view(path, "src/lib.rs", "exact"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(source.contains("pub struct Available;"), "{source}");
        prefetch
            .send(Some(Err("Editor download unavailable".into())))
            .unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(5), app.search(path, "type:Available"))
                .await
                .unwrap()
                .unwrap();
        assert!(result.contains("Available"), "{result}");
    }

    #[tokio::test]
    async fn preparation_survives_idle_timeout_but_idle_sessions_still_expire() {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };
        use serde_json::{Value, json};
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='waiting'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub struct Prepared;").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(Duration::from_millis(500));
        let ct = tokio_util::sync::CancellationToken::new();
        let service = StreamableHttpService::new(
            move || {
                let mut mcp = Mcp::new(app.clone());
                mcp.heartbeat_period = Duration::from_millis(100);
                Ok(mcp)
            },
            Arc::new(manager),
            StreamableHttpServerConfig::default()
                .with_json_response(true)
                .with_cancellation_token(ct.clone()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .tls_certs_only([])
            .build()
            .unwrap();
        let initialized = client.post(&url).header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"waiting","version":"1"}}})).send().await.unwrap();
        let session = initialized.headers()["mcp-session-id"].clone();
        initialized.text().await.unwrap();
        let post = || {
            client
                .post(&url)
                .header("Accept", "application/json, text/event-stream")
                .header("mcp-session-id", &session)
        };
        post()
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await
            .unwrap();
        let (pings, mut received) = tokio::sync::mpsc::channel(16);
        let request = post().json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"codebase":root.path(),"query":"type:Prepared"}}}));
        let search = {
            let client = client.clone();
            let url = url.clone();
            let session = session.clone();
            tokio::spawn(async move {
                let mut events = request.send().await.unwrap().error_for_status().unwrap();
                let mut pending = String::new();
                while let Some(chunk) = events.chunk().await.unwrap() {
                    pending.push_str(std::str::from_utf8(&chunk).unwrap());
                    while let Some(end) = pending.find('\n') {
                        let line: String = pending.drain(..=end).collect();
                        let Some(data) = line.trim().strip_prefix("data: ") else {
                            continue;
                        };
                        let Ok(message) = serde_json::from_str::<Value>(data) else {
                            continue;
                        };
                        if message["method"] == "ping" {
                            client
                                .post(&url)
                                .header("Accept", "application/json, text/event-stream")
                                .header("mcp-session-id", &session)
                                .json(&json!({"jsonrpc":"2.0","id":message["id"],"result":{}}))
                                .send()
                                .await
                                .unwrap()
                                .error_for_status()
                                .unwrap();
                            pings.send(()).await.unwrap();
                        } else if message["id"] == 2 && message.get("method").is_none() {
                            return message;
                        }
                    }
                }
                panic!("MCP stream ended without a response");
            })
        };
        // More than one idle interval elapses while indexing waits.
        for _ in 0..8 {
            tokio::time::timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap();
        }
        drop(pause);
        let response = search.await.unwrap();
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Prepared"),
            "{response}"
        );
        tokio::time::sleep(Duration::from_millis(800)).await;
        let expired = post()
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
            .send()
            .await
            .unwrap();
        assert!(
            !expired.status().is_success(),
            "Idle session did not expire"
        );
        ct.cancel();
        server.abort();
    }

    #[tokio::test]
    async fn cancelled_queued_requests_release_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Game.csproj"), "<Project></Project>").unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        // Measure cancellation of queued work independently of cold .NET discovery.
        app.search(root.path().to_str().unwrap(), "type:X limit:1")
            .await
            .unwrap();
        let permit = app.workers.clone().acquire_owned().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let request = {
            let app = app.clone();
            let path = root.path().to_str().unwrap().to_owned();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                app.request_cancellable(
                    &path,
                    Request::Search("type:X limit:1".into()),
                    &cancel,
                    None,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        cancel.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        drop(permit);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.search(root.path().to_str().unwrap(), "type:X limit:1"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, "No matches.");
    }
}
