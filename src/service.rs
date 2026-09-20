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
pub struct App {
    policy: Policy,
    cache: PathBuf,
    workspaces: Mutex<HashMap<PathBuf, WorkspaceSlot>>,
    workers: Arc<tokio::sync::Semaphore>,
    assemblies: Arc<crate::store::Store>,
    monitor: Arc<crate::watch::Monitor>,
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
        ensure!(workers > 0, "At least one worker is required");
        let assemblies = Arc::new(crate::store::Store::open(&cache.join("assemblies"))?);
        Ok(Self {
            policy,
            cache,
            workspaces: Mutex::new(HashMap::new()),
            workers: Arc::new(tokio::sync::Semaphore::new(workers)),
            assemblies,
            monitor: Arc::new(crate::watch::Monitor::default()),
        })
    }
    pub async fn search(self: &Arc<Self>, path: &str, query: &str) -> Result<String> {
        ensure!(
            query.len() <= 16 * 1024 && path.len() <= 4096,
            "Query or path exceeds request size limit"
        );
        let query = Query::parse(query)?;
        let entry = self.policy.canonical(Path::new(path))?;
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
        let mut state = workspace.lock_owned().await;
        let permit = self.workers.clone().acquire_owned().await?;
        let app = self.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            app.trim_idle();
            let result = (|| {
                if state.is_none() {
                    *state = Some(Workspace::open(
                        entry,
                        &app.cache,
                        app.policy.clone(),
                        app.assemblies.clone(),
                        app.monitor.clone(),
                    )?);
                }
                let workspace = state.as_mut().unwrap();
                // initial indexing belongs to the workspace and survives a disconnected caller.
                workspace.refresh()?;
                let store = workspace.store.clone();
                let assemblies = workspace.assemblies.clone();
                let manifest = workspace.manifest.clone();
                // Pin both LMDB snapshots under the publication gate. Transactions
                // stay on this blocking thread through rendering and destruction.
                let mut search = Search::new(&store, &assemblies, &manifest, &cancel)?;
                drop(state);
                search.run(&query)
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
    pub project_path: String,
    pub query: String,
}

#[derive(Clone)]
pub struct Mcp {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
}
impl Mcp {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
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
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<Arguments>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        match self
            .app
            .search_cancellable(&args.project_path, &args.query, &context.ct)
            .await
        {
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
