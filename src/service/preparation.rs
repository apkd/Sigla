//! One preparation job per workspace. Callers choose inventory or full-index readiness.
use super::*;
use crate::workspace::Manifest;
use anyhow::Context;
use tokio::sync::watch;

type RepositoryContext = (crate::repository::Identity, String, String, usize);
pub(super) struct Inventory {
    pub manifest: Arc<Manifest>,
    pub context: Option<RepositoryContext>,
    watch: crate::watch::Snapshot,
}
impl Inventory {
    pub async fn current(self: &Arc<Self>) -> Result<bool> {
        if !self.watch.changed() {
            return Ok(true);
        }
        let inventory = self.clone();
        Ok(tokio::task::spawn_blocking(move || inventory.manifest.sources_current()).await?)
    }
}
#[derive(Clone)]
enum Progress {
    Pending(&'static str),
    Files(Arc<Inventory>),
    Ready(Arc<Inventory>, Option<u64>),
    Failed(Arc<anyhow::Error>, Option<Arc<Inventory>>),
}
#[derive(Clone)]
pub(super) struct Ticket {
    progress: watch::Receiver<Progress>,
    id: u64,
    telemetry_id: Option<String>,
}
impl Ticket {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn telemetry_id(&self) -> Option<String> {
        self.telemetry_id.clone()
    }
    pub fn discovering(&self) -> bool {
        matches!(*self.progress.borrow(), Progress::Pending(_))
    }
    pub fn running(&self) -> bool {
        matches!(
            *self.progress.borrow(),
            Progress::Pending(_) | Progress::Files(_)
        )
    }
    pub fn needs_refresh(&self, generation: Option<u64>) -> bool {
        match &*self.progress.borrow() {
            Progress::Ready(inventory, prepared) => {
                *prepared != generation || inventory.watch.changed()
            }
            Progress::Failed(..) => true,
            Progress::Pending(_) | Progress::Files(_) => false,
        }
    }
    pub fn retained_inventory(&self) -> bool {
        matches!(*self.progress.borrow(), Progress::Failed(_, Some(_)))
    }
    pub async fn inventory(&mut self) -> Result<Arc<Inventory>> {
        loop {
            match self.progress.borrow().clone() {
                Progress::Files(files)
                | Progress::Ready(files, _)
                | Progress::Failed(_, Some(files)) => return Ok(files),
                Progress::Failed(error, None) => {
                    return Err(crate::diagnostics::shared_error(error));
                }
                Progress::Pending(stage) => {
                    tracing::Span::current().record("stage", stage);
                }
            }
            self.progress.changed().await?;
        }
    }
    pub async fn complete(&mut self) -> Result<()> {
        loop {
            match self.progress.borrow().clone() {
                Progress::Ready(..) => return Ok(()),
                Progress::Failed(error, _) => {
                    return Err(crate::diagnostics::shared_error(error));
                }
                Progress::Pending(stage) => {
                    tracing::Span::current().record("stage", stage);
                }
                Progress::Files(_) => {
                    tracing::Span::current().record("stage", "source_indexing");
                }
            }
            self.progress.changed().await?;
        }
    }
}
fn context(
    branch: Option<&crate::repository::manager::Branch>,
    state: Option<&crate::repository::manager::State>,
) -> Option<RepositoryContext> {
    branch.zip(state).map(|(branch, state)| {
        (
            branch.repository.identity.clone(),
            branch.selector(&state.prepared),
            state.prepared.revision.clone(),
            state.prepared.tracked.len(),
        )
    })
}

impl App {
    /// Repository paths and bytes are usable before build discovery acquires dependencies.
    pub(super) async fn navigate_repository(
        self: &Arc<Self>,
        branch: Arc<crate::repository::manager::Branch>,
        request: super::Request,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<Option<CallToolResult>> {
        let result = self
            .navigate_repository_snapshot(branch.clone(), request.clone(), activity.clone())
            .await?;
        if result.is_some() || !matches!(request, super::Request::View(..)) {
            return Ok(result);
        }
        tracing::Span::current().record("stage", "acquisition");
        self.remote.as_ref().unwrap().sources_ready(&branch).await?;
        self.navigate_repository_snapshot(branch, request, activity)
            .await
    }

    async fn navigate_repository_snapshot(
        self: &Arc<Self>,
        branch: Arc<crate::repository::manager::Branch>,
        request: super::Request,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<Option<CallToolResult>> {
        tracing::Span::current().record("stage", "query_admission");
        let permit = self.navigation_workers.clone().acquire_owned().await?;
        tracing::Span::current().record("stage", "evaluation");
        let state = branch.state.clone().read_owned().await;
        let state = state.as_ref().is_some().then_some(state);
        let inventory = branch.inventory.borrow().clone();
        if let Some(prepared) = state
            .as_ref()
            .and_then(|state| state.as_ref())
            .map(|state| &state.prepared)
            .or_else(|| inventory.as_ref().map(|inventory| &inventory.prepared))
        {
            tracing::Span::current().record("selector", branch.selector(prepared));
            tracing::Span::current().record("revision", prepared.revision.as_str());
        }
        let remote = self.remote.as_ref().unwrap().clone();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            let root = branch.source();
            let (prepared, contents) = if let Some(state) = state.as_ref().and_then(|s| s.as_ref()) {
                ensure!(!state.repair, "Repository materialization requires repair");
                (&state.prepared, Some(root.as_path()))
            } else {
                let inventory = inventory.as_ref().context("Repository inventory is unavailable")?;
                (&inventory.prepared, inventory.source.as_ref().map(|s| s.path()))
            };
            let (path, mode) = match request {
                super::Request::History(_) => unreachable!("history bypasses snapshot preparation"),
                super::Request::Browse(path) => (path, None),
                super::Request::View(path, mode) => (path, Some(mode)),
                super::Request::Search(_) => unreachable!(),
            };
            let Some(text) = crate::navigation::sources::repository(
                &root, contents, prepared, &path, mode,
                |path| remote.source_blob(&branch, prepared, path),
            )? else {
                return Ok(None);
            };
            branch.record_use();
            Ok(Some(CallToolResult::success(vec![ContentBlock::text(format!(
                "{text}\n\n> Repository inventory available; preparation and indexing are still running."
            ))])))
        }).await?
    }

    pub(super) async fn navigate_early(
        self: &Arc<Self>,
        inventory: Arc<Inventory>,
        request: super::Request,
        query: Option<Query>,
        usage: (PathBuf, Option<Arc<crate::repository::manager::Branch>>),
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        operation: crate::telemetry::Operation,
    ) -> Result<CallToolResult> {
        tracing::Span::current().record("stage", "query_admission");
        operation.stage("query_admission");
        let permit = {
            let _wait = operation.wait("query_admission");
            self.navigation_workers.clone().acquire_owned().await?
        };
        tracing::Span::current().record("stage", "evaluation");
        operation.stage("evaluation");
        let app = self.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            let manifest = &inventory.manifest;
            let root = usage
                .1
                .as_ref()
                .map(|b| b.source())
                .unwrap_or_else(|| manifest.root.clone());
            let sources = crate::navigation::sources::Sources {
                manifest,
                assets: None,
                cancel: &cancel,
            };
            let assets_pending =
                !indexes::source_only(&request, query.as_ref()) && indexes::has_unity(manifest);
            let mut text = match request {
                super::Request::History(_) => unreachable!("history bypasses snapshot preparation"),
                super::Request::Search(_) => sources.files(query.as_ref().unwrap(), &root),
                super::Request::Browse(path) => sources.browse(&root, &path, app.remote.is_none()),
                super::Request::View(path, mode) => {
                    sources.view(&root, &path, mode, app.remote.is_none())
                }
            }?;
            if assets_pending {
                text.push_str("\n\n> Index incomplete: Unity assets");
            }
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            if let Some((identity, branch, revision, tracked)) = &inventory.context {
                crate::summary::Summary::build(
                    identity, branch, revision, *tracked, &root, manifest,
                )
                .attach(&mut result);
            }
            app.record_usage(&usage.0, usage.1.as_deref(), manifest);
            Ok(result)
        })
        .await?;
        guard.disarm();
        result
    }
}
pub(super) struct Request {
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub entry: PathBuf,
    pub policy: Policy,
    pub cache: PathBuf,
    pub branch: Option<Arc<crate::repository::manager::Branch>>,
    pub activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
}
pub(super) fn start(preparing_app: Arc<App>, request: Request) -> Ticket {
    let Request {
        workspace,
        entry,
        policy,
        cache,
        branch: preparing_branch,
        activity: preparing_activity,
    } = request;
    let (send, receive) = watch::channel(Progress::Pending("preparation"));
    static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let job = NEXT_JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let repository = preparing_branch
        .as_ref()
        .map(|branch| branch.repository.identity.to_string())
        .or_else(|| Some(entry.display().to_string()));
    let operation =
        crate::telemetry::operation("workspace_preparation", repository.as_deref(), None);
    if let Some(branch) = &preparing_branch {
        operation.target(&branch.name);
    }
    let telemetry_id = operation.id();
    tokio::spawn(async move {
        tracing::debug!(shared_job = job, workspace = %entry.display(), "Workspace preparation started");
        let outcome = async {
            #[cfg(test)]
            preparing_app.preparation_started.notify_one();
            // Lock order: workspace -> branch analysis -> branch state.
            // RequiredInputs releases both branch guards before acquisition.
            let mut state = {
                let _wait = operation.wait("workspace_lock");
                workspace.clone().lock_owned().await
            };
            if let Some(branch) = &preparing_branch {
                send.send_replace(Progress::Pending("acquisition"));
                operation.stage("repository_acquisition");
                preparing_app
                    .remote
                    .as_ref()
                    .unwrap()
                    .ready(branch, &operation)
                    .await?;
                send.send_replace(Progress::Pending("preparation"));
            }
            let mut branch_state = match &preparing_branch {
                Some(branch) => {
                    let _wait = operation.wait("branch_analysis");
                    Some(branch.analyze().await)
                }
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
                operation.stage("discovery_admission");
                let permit = {
                    let _wait = operation.wait("discovery_admission");
                    preparing_app.preparation_workers.clone().acquire_owned().await?
                };
                #[cfg(test)]
                let _discovery_pause = {
                    preparing_app.discovery_started.notify_one();
                    preparing_app.discovery_pause.clone().acquire_owned().await?
                };
                operation.stage("discovery");
                let (next, result) = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
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
                    let code = state.as_mut().unwrap();
                    let result = code
                        .prepare()
                        .and_then(|p| p.map(|p| code.plan(p)).transpose());
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
                        send.send_replace(Progress::Pending("acquisition"));
                        operation.stage("required_input_acquisition");
                        preparing_app
                            .remote
                            .as_ref()
                            .unwrap()
                            .require_inputs(branch, &revision, paths, &operation)
                            .await?;
                        send.send_replace(Progress::Pending("preparation"));
                        let gate = {
                            let _wait = operation.wait("branch_analysis");
                            branch.analyze().await
                        };
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
                send.send_replace(Progress::Files(Arc::new(Inventory {
                    manifest: prepared.manifest.clone(),
                    context: context(
                        preparing_branch.as_deref(),
                        branch_state.as_ref().and_then(|s| s.as_ref()),
                    ),
                    watch: prepared.watch.clone(),
                })));
                let queued = std::time::Instant::now();
                operation.stage("indexing_admission");
                let permit = {
                    let _wait = operation.wait("indexing_admission");
                    preparing_app.indexing_workers.clone().acquire_owned().await?
                };
                tracing::debug!(workspace = %entry.display(), wait_ms = queued.elapsed().as_millis(), "indexing worker admitted");
                #[cfg(test)]
                let _pause = preparing_app.indexing_pause.clone().acquire_owned().await?;
                operation.stage("workspace_indexing");
                state = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
                    state.as_mut().unwrap().apply_plan(prepared)?;
                    crate::memory::reclaim();
                    Ok(state)
                })
                .await??;
            }

            let code = state.as_ref().unwrap();
            let inventory = Arc::new(Inventory {
                manifest: code.manifest.clone(),
                watch: code.watch_snapshot(),
                context: context(
                    preparing_branch.as_deref(),
                    branch_state.as_ref().and_then(|s| s.as_ref()),
                ),
            });
            let generation = preparing_branch
                .as_ref()
                .map(|branch| branch.generation.load(std::sync::atomic::Ordering::Acquire));
            drop(branch_state);
            if let Some(branch) = &preparing_branch {
                let mut gate = {
                    let _wait = operation.wait("repository_publication");
                    branch.publish().await
                };
                let metadata = gate.as_mut().unwrap();
                // Publication can win the gap between releasing the read guard and
                // acquiring the writer. Only mark the generation we actually indexed.
                if !metadata.repair
                    && Some(branch.generation.load(std::sync::atomic::Ordering::Acquire))
                        == generation
                    && metadata.indexed_revision.as_ref() != Some(&metadata.prepared.revision)
                {
                    metadata.indexed_revision = Some(metadata.prepared.revision.clone());
                    metadata.last_use = branch.last_use.load(std::sync::atomic::Ordering::Relaxed);
                    let branch = branch.clone();
                    tokio::task::spawn_blocking(move || {
                        branch.persist(gate.as_ref().unwrap())
                    }).await??;
                }
            }
            let assets = preparing_app.start_assets(
                workspace.clone(),
                preparing_branch.clone(),
                &code.manifest,
                inventory
                    .context
                    .as_ref()
                    .map(|(_, _, revision, _)| revision.clone()),
                false,
                preparing_activity.clone(),
            )?;
            if let Some(job_id) = assets.telemetry_id() {
                operation.link(&job_id);
            }
            for group in [crate::native::Group::Native, crate::native::Group::Shaders] {
                let source = preparing_app.sources.start(crate::native::jobs::Request {
                    activity: preparing_activity.clone(),
                    workspace: workspace.clone(),
                    branch: preparing_branch.clone(),
                    entry: code.entry.clone(),
                    analysis: preparing_app.cache.join("analysis"),
                    manifest: code.manifest.clone(),
                    group,
                })?;
                if let Some(job_id) = source.telemetry_id() {
                    operation.link(&job_id);
                }
            }
            preparing_app.owners.lock().unwrap().observe(
                &entry,
                preparing_branch.as_ref().map(|b| b.root.as_path()),
                &code.manifest,
                None,
            );
            Ok::<_, anyhow::Error>((inventory, generation))
        }
        .await;
        let progress = match outcome {
            Ok((inventory, generation)) => {
                operation.finish("success");
                Progress::Ready(inventory, generation)
            }
            Err(error) => {
                tracing::warn!(shared_job = job, workspace = %entry.display(), error = %format!("{error:#}"), "Workspace indexing failed");
                if let Some(diagnostic) = crate::diagnostics::monitoring(&error) {
                    operation.diagnose(diagnostic);
                }
                operation.finish("failure");
                let inventory = match &*send.borrow() {
                    Progress::Files(inventory) => Some(inventory.clone()),
                    _ => None,
                };
                Progress::Failed(Arc::new(error), inventory)
            }
        };
        send.send_replace(progress);
    });
    Ticket {
        progress: receive,
        id: job,
        telemetry_id,
    }
}
