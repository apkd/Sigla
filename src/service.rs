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
type Startup = tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>;
pub struct App {
    _ownership: std::fs::File,
    policy: Policy,
    cache: PathBuf,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceSlot>>,
    workers: Arc<tokio::sync::Semaphore>,
    assemblies: Arc<crate::store::Store>,
    monitor: Arc<crate::watch::Monitor>,
    remote: Option<Arc<crate::repository::manager::Manager>>,
    startup: Mutex<Option<Startup>>,
}
impl App {
    async fn search_cancellable(
        self: &Arc<Self>,
        path: &str,
        query: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Query cancelled"),
            result = self.search(path, query) => result,
        }
    }
    fn trim_idle(&self) {
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
        let assemblies = Arc::new(crate::store::Store::open(&cache.join("assemblies"))?);
        Ok(Self {
            _ownership: ownership,
            policy,
            cache,
            workspaces: Mutex::new(HashMap::new()),
            workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            assemblies,
            monitor: Arc::new(crate::watch::Monitor::default()),
            remote: None,
            startup: Mutex::new(None),
        })
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
        Ok(app)
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
        if let Some(remote) = &self.remote {
            let weak = Arc::downgrade(self);
            let interval = remote
                .options
                .refresh_interval
                .min(std::time::Duration::from_secs(60));
            tokio::spawn(async move {
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
            remote.expire(&branch).await?;
        }
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
        self.ready().await?;
        ensure!(
            query.len() <= 16 * 1024 && path.len() <= 4096,
            "Query or path exceeds request size limit"
        );
        let query = Query::parse(query)?;
        self.expire_idle().await?;
        let repository = crate::repository::Repository::parse(path)?;
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
        let branch_state = match &branch {
            Some(branch) => Some(branch.state.clone().lock_owned().await),
            None => None,
        };
        let (entry, policy, cache) = if let Some(branch) = &branch {
            let state = branch_state
                .as_ref()
                .unwrap()
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Repository inputs are unavailable"))?;
            ensure!(!state.repair, "Repository materialization requires repair");
            let source = branch.source().canonicalize()?;
            let mut policy = Policy::new(vec![source.clone()])?;
            policy.unity_platform = self.policy.unity_platform;
            policy.remote = Some(crate::discovery::RemoteContext {
                workspace: source.clone(),
                tracked: Arc::new(state.prepared.tracked.keys().cloned().collect()),
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
        let managed = branch.is_some();
        let preparing_branch = branch.clone();
        let (mut state, mut branch_state) = tokio::spawn(async move {
            let mut state = workspace.lock_owned().await;
            let mut branch_state = branch_state;
            let mut policy = policy;
            let prepared = loop {
                let app = preparing_app.clone();
                let entry = entry.clone();
                let cache = cache.clone();
                let current_policy = policy.clone();
                let (next, result) = tokio::task::spawn_blocking(move || -> Result<_> {
                    if state.is_none() {
                        *state = Some(Workspace::open(
                            entry,
                            &cache,
                            current_policy.clone(),
                            app.assemblies.clone(),
                            app.monitor.clone(),
                        )?);
                    }
                    state.as_mut().unwrap().update_policy(current_policy);
                    if managed {
                        state.as_mut().unwrap().materialized();
                    }
                    let result = state.as_mut().unwrap().prepare();
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
                        policy.remote.as_mut().unwrap().tracked =
                            Arc::new(applied.prepared.tracked.keys().cloned().collect());
                        branch_state = Some(gate);
                    }
                }
            };
            if let Some(prepared) = prepared {
                let permit = preparing_app.workers.clone().acquire_owned().await?;
                state = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
                    state.as_mut().unwrap().apply(prepared)?;
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
            let metadata = branch_state.as_ref().unwrap().as_ref().unwrap();
            format!(
                "{} #{} @{}",
                branch.repository.identity,
                branch.name,
                metadata.indexed_revision.as_deref().unwrap()
            )
        });
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
                let assemblies = workspace.assemblies.clone();
                let manifest = workspace.manifest.clone();
                // Pin both LMDB snapshots under the publication gate. Transactions
                // stay on this blocking thread through rendering and destruction.
                let mut search = Search::new(&store, &assemblies, &manifest, &cancel)?;
                drop(state);
                let mut text = search.run(&query)?;
                for diagnostic in &manifest.diagnostics {
                    text.push_str("\n\n");
                    text.push_str(diagnostic);
                }
                if let Some(context) = context {
                    text = format!("{context}\n\n{text}");
                }
                Ok(text)
            })();
            app.trim_idle();
            result
        })
        .await?;
        guard.disarm();
        result
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub project: String,
    pub query: String,
}

#[derive(Clone)]
pub struct Mcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
    heartbeat_period: std::time::Duration,
}
impl Mcp {
    pub fn new(app: Arc<App>) -> Self {
        Self {
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

Bare names find declarations. Narrow by kind with type:, method:, function:, property:, field:, trait:, or module:.
Aliases: t: for type:, m: for method:, x: for text:.
Qualified names and parameter signatures narrow targets.
@path:line:column finds a declaration (1-based Unicode columns).

uses:, calls:, and writes: find explicit references, calls, and direct updates.
derived: and impl: follow inheritance and implementation.
in:TARGET restricts containment. calls:* in:TARGET shows outgoing calls.

Filters: project:, path:, namespace:, access:, attr:. Prefix a filter with - to exclude matches.
Use match:exact (default) or match:loose.
text:"literal" searches source.
limit:N caps the result count (default 20). limit:5 returns up to five complete matches."#,
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<Arguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let search = self
            .app
            .search_cancellable(&args.project, &args.query, &context.ct);
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
                        "Client stopped responding while waiting for the search; shared preparation continues"
                    );
                }
            }
        };
        let result = tokio::select! {
            result = search => result,
            error = heartbeat => Err(error),
        };
        match result {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(crate::render::error(&e))]),
        }
    }
}
#[tool_handler(router = self.tool_router, name = "sigla", version = "0.1.0")]
impl ServerHandler for Mcp {}

#[cfg(test)]
mod tests {
    use super::*;
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
        let permit = app.workers.clone().acquire_owned().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let request = {
            let app = app.clone();
            let path = root.path().to_str().unwrap().to_owned();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                app.search_cancellable(&path, "type:X limit:1", &cancel)
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
