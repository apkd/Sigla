use crate::{discovery::Policy, query::Query, search::Search, workspace::Workspace};
use anyhow::{Result, ensure};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

struct WorkspaceSlot {
    state: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    used: Instant,
}
#[derive(Debug)]
enum Request {
    Search(String),
    Browse(String),
    View(String, crate::navigation::Mode),
}
type Startup = tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>;
pub struct App {
    _ownership: std::fs::File,
    policy: Policy,
    cache: PathBuf,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceSlot>>,
    workers: Arc<tokio::sync::Semaphore>,
    monitor: Arc<crate::watch::Monitor>,
    remote: Option<Arc<crate::repository::manager::Manager>>,
    upstream: Option<crate::upstream::Upstream>,
    startup: Mutex<Option<Startup>>,
    stateless_summaries: crate::summary::Stateless,
    #[cfg(test)]
    preparation_started: tokio::sync::Notify,
}
impl App {
    async fn request_cancellable(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        cancel: &tokio_util::sync::CancellationToken,
        upstream: Option<&crate::upstream::Upstream>,
    ) -> Result<CallToolResult> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Query cancelled"),
            result = self.dispatch_with_upstream(path, request, upstream) => result,
        }
    }
    fn trim_idle(&self) {
        crate::memory::reclaim();
        loop {
            match crate::memory::resident_bytes() {
                Ok(bytes) if bytes > crate::memory::IDLE_CACHE_HIGH_WATER => {}
                Ok(_) => return,
                Err(error) => {
                    tracing::warn!(%error, "Cannot measure resident cache memory");
                    return;
                }
            }
            let retired = {
                let mut registry = self.workspaces.lock().unwrap();
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
            // release the LMDB mapping outside the registry lock before measuring again.
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
        crate::cache_migration::migrate(&cache)?;
        crate::memory::start_reclaimer()?;
        let app = Self {
            _ownership: ownership,
            policy,
            cache,
            workspaces: Mutex::new(HashMap::new()),
            workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            monitor: Arc::new(crate::watch::Monitor::default()),
            remote: None,
            upstream: None,
            startup: Mutex::new(None),
            stateless_summaries: Default::default(),
            #[cfg(test)]
            preparation_started: tokio::sync::Notify::new(),
        };
        app.maintain_analysis(true)?;
        Ok(app)
    }

    pub fn remote(
        policy: Policy,
        cache: PathBuf,
        workers: usize,
        options: crate::config::RemoteOptions,
    ) -> Result<Self> {
        let mut app = Self::new(policy, cache.clone(), workers)?;
        let manager = crate::repository::manager::Manager::new(cache, options)?;
        app.remote = Some(manager);
        app.maintain_analysis(true)?;
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
        if let Some(upstream) = &self.upstream {
            upstream.shutdown().await;
        }
    }

    pub fn start_setup(self: &Arc<Self>) {
        let mut startup = self.startup.lock().unwrap();
        if startup.is_some() {
            return;
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
            let interval = self
                .remote
                .as_ref()
                .map_or(std::time::Duration::from_secs(60), |remote| {
                    remote.options.maintenance_interval()
                });
            tokio::spawn(async move {
                let mut last_collection = Instant::now();
                loop {
                    tokio::time::sleep(interval).await;
                    let Some(app) = weak.upgrade() else { return };
                    if let Some(remote) = &app.remote {
                        // Fetch failures stay on the branch preparation state, never on valid query results.
                        if let Err(error) = remote.maintain().await {
                            tracing::error!(%error, "Cannot persist repository ownership state");
                        }
                        if let Err(error) = app.expire_idle().await {
                            tracing::error!(%error, "Cannot expire repository storage");
                        }
                    }
                    if last_collection.elapsed() < std::time::Duration::from_secs(60) {
                        continue;
                    }
                    last_collection = Instant::now();
                    let maintenance = app.clone();
                    if let Err(error) =
                        tokio::task::spawn_blocking(move || maintenance.maintain_analysis(false))
                            .await
                            .unwrap_or_else(|error| Err(error.into()))
                    {
                        tracing::error!(%error, "Cannot maintain analysis storage");
                    }
                    {
                        let cache = app.cache.clone();
                        let ttl = app
                            .remote
                            .as_ref()
                            .map_or(crate::config::DEFAULT_REPO_TTL, |remote| {
                                remote.options.repo_ttl
                            });
                        if let Err(error) = tokio::task::spawn_blocking(move || {
                            crate::repository::cache::maintain_all(&cache, ttl)
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.into()))
                        {
                            tracing::error!(%error, "Cannot maintain Git pools");
                        }
                    }
                }
            });
        }
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                for version in versions {
                    crate::unity::acquire::prefetch(&cache, version)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = send.send(Some(result));
        });
    }

    async fn expire_idle(&self) -> Result<()> {
        let Some(remote) = &self.remote else {
            return Ok(());
        };
        for branch in remote.expired() {
            let Ok(_gate) = branch.state.try_lock() else {
                continue;
            };
            let retired = {
                let mut registry = self.workspaces.lock().unwrap();
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
            remote
                .expire(&branch, || self.retire_owner(&branch.root))
                .await?;
        }
        Ok(())
    }

    fn retire_owner(&self, owner: &Path) -> Result<()> {
        let analysis = self.cache.join("analysis");
        if !analysis.try_exists()? {
            return Ok(());
        }
        let database = crate::store::Database::open(&analysis)?;
        for (key, info) in database.workspaces()? {
            if info.owner.as_deref() == Some(owner) {
                database.existing(key)?.unwrap().mark_deleting()?;
            }
        }
        Ok(())
    }

    fn maintain_analysis(&self, startup: bool) -> Result<()> {
        use crate::store::shared::Phase;
        if let Some(remote) = &self.remote {
            remote.expire_unloaded(|owner| self.retire_owner(owner))?;
        }
        let analysis = self.cache.join("analysis");
        if !analysis.try_exists()? {
            return Ok(());
        }
        let database = crate::store::Database::open(&analysis)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        for (key, info) in database.workspaces()? {
            let mut registry = self.workspaces.lock().unwrap();
            if registry
                .get(&info.entry)
                .is_some_and(|slot| Arc::strong_count(&slot.state) != 1)
            {
                continue;
            }
            let missing =
                match std::fs::symlink_metadata(info.owner.as_deref().unwrap_or(&info.entry)) {
                    Ok(_) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(_) => false,
                };
            if info.phase != Phase::Deleting && !missing {
                continue;
            }
            registry.remove(&info.entry);
            let Some(scope) = database.existing(key)? else {
                continue;
            };
            scope.mark_deleting()?;
            // The registry lock prevents a request opening this scope during retirement.
            // Persisted deleting state rejects new opens until the next batch completes.
            loop {
                if scope.delete_batch(now, 128)? || !startup {
                    break;
                }
            }
            drop(registry);
        }
        database.collect(now, 128)?;
        Ok(())
    }

    async fn ready(self: &Arc<Self>) -> Result<()> {
        self.start_setup();
        let mut ready = self.startup.lock().unwrap().as_ref().unwrap().clone();
        loop {
            if let Some(result) = ready.borrow().clone() {
                return result.map_err(|e| anyhow::anyhow!("Startup setup failed: {e}"));
            }
            ready.changed().await?;
        }
    }
    pub async fn search(self: &Arc<Self>, path: &str, query: &str) -> Result<String> {
        ensure!(query.len() <= 16 * 1024, "Query exceeds request size limit");
        result_text(self.dispatch(path, Request::Search(query.into())).await?)
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
            .dispatch_with_upstream(path, request, self.upstream.as_ref())
            .await?;
        Ok(crate::summary::Session::default().present(result))
    }
    async fn dispatch_with_upstream(
        self: &Arc<Self>,
        path: &str,
        request: Request,
        upstream: Option<&crate::upstream::Upstream>,
    ) -> Result<CallToolResult> {
        ensure!(path.len() <= 4096, "Project exceeds request size limit");
        match &request {
            Request::Browse(path) | Request::View(path, _) => {
                ensure!(path.len() <= 4096, "Path exceeds request size limit")
            }
            Request::Search(query) => {
                ensure!(query.len() <= 16 * 1024, "Query exceeds request size limit")
            }
        }
        if let Some(upstream) = upstream
            && let Some(repository) = crate::repository::Repository::project(path, true)?
        {
            let mut project = repository.transport;
            if let Some((_, selector)) = path.trim().split_once('#') {
                project.push('#');
                // Preserve encoding so the upstream decodes the selector exactly once.
                project.push_str(selector);
            }
            let (name, args) = match request {
                Request::Search(query) => (
                    "search",
                    serde_json::json!({"project":project,"query":query}),
                ),
                Request::Browse(path) => {
                    ("browse", serde_json::json!({"project":project,"path":path}))
                }
                Request::View(path, mode) => (
                    "view",
                    serde_json::json!({"project":project,"path":path,"mode":match mode {
                        crate::navigation::Mode::Exact => "exact",
                        crate::navigation::Mode::Minified => "minified",
                    }}),
                ),
            };
            return upstream.call(name, args.as_object().unwrap().clone()).await;
        }
        self.request(path, request).await
    }

    async fn request(self: &Arc<Self>, path: &str, request: Request) -> Result<CallToolResult> {
        let query = match &request {
            Request::Search(query) => Some(
                Query::parse(query)
                    .map_err(|error| anyhow::anyhow!("Invalid search query: {error}"))?,
            ),
            _ => None,
        };
        self.ready().await?;
        self.expire_idle().await?;
        let repository = crate::repository::Repository::project(path, self.remote.is_some())?;
        let branch = match repository {
            Some(repository) => Some(
                self.remote
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Repository identifiers require --mode remote"))?
                    .resolve(repository)
                    .await?,
            ),
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
        let (workspace, retired) = {
            let mut registry = self.workspaces.lock().unwrap();
            let mut retired = Vec::new();
            let slot = registry
                .entry(entry.clone())
                .or_insert_with(|| WorkspaceSlot {
                    state: Arc::new(tokio::sync::Mutex::new(None)),
                    used: Instant::now(),
                });
            slot.used = Instant::now();
            let state = slot.state.clone();
            while registry.len() > 8 {
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
            (state, retired)
        };
        drop(retired);
        // The service owns preparation. Dropping a caller only drops its wait.
        let preparing_app = self.clone();
        let preparing_branch = branch.clone();
        let (mut state, mut branch_state) = tokio::spawn(async move {
            #[cfg(test)]
            preparing_app.preparation_started.notify_one();
            // Request lock order: workspace -> branch state. RequiredInputs
            // releases and reacquires branch state while retaining the workspace.
            let mut state = workspace.lock_owned().await;
            let mut branch_state = match &preparing_branch {
                Some(branch) => Some(branch.state.clone().lock_owned().await),
                None => None,
            };
            let mut policy = policy;
            if let Some(gate) = &branch_state {
                let applied = gate
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Repository inputs are unavailable"))?;
                ensure!(
                    !applied.repair,
                    "Repository materialization requires repair"
                );
                policy.remote.as_mut().unwrap().tracked = Arc::new(
                    applied
                        .prepared
                        .tracked
                        .keys()
                        .filter(|p| !applied.prepared.unavailable.contains(*p))
                        .cloned()
                        .collect(),
                );
            }
            let prepared = loop {
                let app = preparing_app.clone();
                let entry = entry.clone();
                let cache = cache.clone();
                let current_policy = policy.clone();
                let owner = preparing_branch.as_ref().map(|branch| branch.root.clone());
                let generation = preparing_branch
                    .as_ref()
                    .map(|branch| branch.generation.load(std::sync::atomic::Ordering::Acquire));
                let (next, result) = tokio::task::spawn_blocking(move || -> Result<_> {
                    if state.is_none() {
                        *state = Some(Workspace::open(
                            entry,
                            &cache,
                            current_policy.clone(),
                            &app.cache.join("analysis"),
                            owner.as_deref(),
                            app.monitor.clone(),
                        )?);
                    }
                    state.as_mut().unwrap().update_policy(current_policy);
                    if let Some(generation) = generation {
                        state.as_mut().unwrap().materialized(generation);
                    }
                    let result = state.as_mut().unwrap().prepare();
                    crate::memory::reclaim();
                    Ok((state, result))
                })
                .await??;
                state = next;
                match result {
                    Ok(prepared) => break prepared,
                    Err(error) => {
                        let Some(required) =
                            error.downcast_ref::<crate::discovery::RequiredInputs>()
                        else {
                            return Err(error);
                        };
                        let paths = required.0.clone();
                        let branch = preparing_branch
                            .as_ref()
                            .ok_or(error.context("No repository materializer is available"))?;
                        let revision = branch_state
                            .as_ref()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .prepared
                            .revision
                            .clone();
                        drop(branch_state.take());
                        preparing_app
                            .remote
                            .as_ref()
                            .unwrap()
                            .require_inputs(branch, &revision, paths)
                            .await?;
                        let gate = branch.state.clone().lock_owned().await;
                        let applied = gate
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("Repository inputs are unavailable"))?;
                        ensure!(
                            !applied.repair,
                            "Repository materialization requires repair"
                        );
                        policy.remote.as_mut().unwrap().tracked = Arc::new(
                            applied
                                .prepared
                                .tracked
                                .keys()
                                .filter(|p| !applied.prepared.unavailable.contains(*p))
                                .cloned()
                                .collect(),
                        );
                        branch_state = Some(gate);
                    }
                }
            };
            if let Some(prepared) = prepared {
                let permit = preparing_app.workers.clone().acquire_owned().await?;
                state = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
                    state.as_mut().unwrap().apply(prepared)?;
                    crate::memory::reclaim();
                    Ok(state)
                })
                .await??;
            }
            Ok::<_, anyhow::Error>((state, branch_state))
        })
        .await??;
        if let (Some(branch), Some(gate)) = (&branch, branch_state.as_mut()) {
            let metadata = gate.as_mut().unwrap();
            if metadata.indexed_revision.as_ref() != Some(&metadata.prepared.revision) {
                metadata.indexed_revision = Some(metadata.prepared.revision.clone());
                metadata.last_use = branch.last_use.load(std::sync::atomic::Ordering::Relaxed);
                branch.persist(metadata)?;
            }
        }
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
        let permit = self.workers.clone().acquire_owned().await?;
        let app = self.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            app.trim_idle();
            let result = (|| {
                let _branch_state = branch_state;
                let workspace = state.as_mut().unwrap();
                let store = workspace.store.clone();
                let manifest = workspace.manifest.clone();
                // Pin the shared LMDB snapshot under the publication gate. It
                // stays on this blocking thread through rendering and destruction.
                let mut search = Search::new(&store, &manifest, &cancel)?;
                drop(state);
                let root = repository_root.as_deref().unwrap_or(&manifest.root);
                let text = match request {
                    Request::Search(_) => {
                        let query = query.as_ref().unwrap();
                        if query.selector == "file" {
                            search.files(query, root)
                        } else {
                            search.run(query)
                        }
                    }
                    Request::Browse(path) => search.browse(root, &path, allow_absolute),
                    Request::View(path, mode) => search.view(root, &path, mode, allow_absolute),
                }?;
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                if let Some((identity, branch, revision, tracked)) = context {
                    crate::summary::Summary::build(
                        &identity, &branch, &revision, tracked, root, &manifest,
                    )
                    .attach(&mut result);
                }
                Ok(result)
            })();
            app.trim_idle();
            result
        })
        .await?;
        guard.disarm();
        result
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
    /// Local project path or repository URL with optional #branch, #tag, full commit ID, or unique cached commit prefix of at least 7 hex characters. Explicit #refs/heads/name and #refs/tags/name are supported.
    pub project: String,
    pub query: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowseArguments {
    /// Local project path or repository URL with optional #branch, #tag, full commit ID, or unique cached commit prefix of at least 7 hex characters. Explicit #refs/heads/name and #refs/tags/name are supported.
    pub project: String,
    #[serde(default)]
    pub path: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewArguments {
    /// Local project path or repository URL with optional #branch, #tag, full commit ID, or unique cached commit prefix of at least 7 hex characters. Explicit #refs/heads/name and #refs/tags/name are supported.
    pub project: String,
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
        description = r#"Find symbols, follow references, and explore C# and Rust codebases.

Declarations
Bare names find declarations. Qualified names and signatures narrow targets.
writes: includes out arguments and possible writes through ref.

Kinds
t: type: c: class: i: interface: struct: enum: delegate: m: method: function: property: field: trait: module:

Filters
project: path: namespace: access: attr: in:
Prefix filters with - to exclude matches.
File queries support only project: and path: filters.
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
        self.execute(&args.project, Ok(Request::Search(args.query)), context)
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
        self.execute(&args.project, Ok(Request::Browse(args.path)), context)
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
            &args.project,
            Ok(Request::View(args.path, args.mode)),
            context,
        )
        .await
    }
}
impl Mcp {
    #[tracing::instrument(skip(self, context), fields(request_id = ?context.id))]
    async fn execute(
        &self,
        project: &str,
        request: Result<Request>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let search = async {
            self.app
                .request_cancellable(project, request?, &context.ct, self.upstream.as_deref())
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
                tracing::error!(project, error = %format!("{e:#}"), "Tool request failed");
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
impl ServerHandler for Mcp {}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn queued_preparation_does_not_block_branch_reacquisition() {
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
                "selected": {}, "tracked": {"Cargo.toml": "b", "src/lib.rs": "c"}, "directories": ["src"]}
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
                    rules: vec![Rule::parse_private("https://github.com/fixture/repo").unwrap()],
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
                used: Instant::now(),
            },
        );
        // Stand in for A's workspace guard while it releases branch state to
        // handle RequiredInputs. B must not retain branch state while queued.
        let held_workspace = workspace.lock().await;
        let request = {
            let app = app.clone();
            tokio::spawn(async move { app.search(project, "Prepared").await })
        };
        tokio::time::timeout(Duration::from_secs(5), app.preparation_started.notified())
            .await
            .unwrap();
        let reacquired = branch.state.try_lock().is_ok();
        drop(held_workspace);
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
        app.expire_idle().await.unwrap();
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
        let database = crate::store::Database::open(&cache.path().join("analysis")).unwrap();
        let before = database.workspaces().unwrap()[0].1.id;
        app.maintain_analysis(false).unwrap();
        assert_eq!(database.workspaces().unwrap()[0].1.id, before);
        std::fs::remove_file(project.join("README.md")).unwrap();
        std::fs::remove_dir(&project).unwrap();
        app.maintain_analysis(false).unwrap();
        assert!(database.workspaces().unwrap().is_empty());
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("README.md"), "After").unwrap();
        assert!(
            app.view(project.to_str().unwrap(), "README.md", "exact")
                .await
                .unwrap()
                .contains("After")
        );
        assert!(database.workspaces().unwrap()[0].1.id > before);
    }

    #[test]
    fn startup_finishes_interrupted_namespace_retirement() {
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
        assert!(
            crate::store::Database::open(&app.cache.join("analysis"))
                .unwrap()
                .workspaces()
                .unwrap()
                .is_empty()
        );
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
        let (ready, waiting) = tokio::sync::watch::channel(None);
        *app.startup.lock().unwrap() = Some(waiting);
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
        let request = post().json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"project":root.path(),"query":"type:Prepared"}}}));
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
        // More than one idle interval elapses while the service-owned setup waits.
        for _ in 0..8 {
            tokio::time::timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap();
        }
        ready.send(Some(Ok(()))).unwrap();
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
