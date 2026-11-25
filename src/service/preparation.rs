//! One preparation job per workspace. Callers choose inventory or full-index readiness.
use super::*;
use crate::workspace::Manifest;
use anyhow::Context;
use tokio::sync::watch;

mod dependencies;

type RepositoryContext = (crate::repository::Identity, String, String, usize);

fn materialized_policy(
    policy: &mut Policy,
    applied: &crate::repository::manager::State,
) -> Result<()> {
    policy.remote.as_mut().unwrap().tracked = Arc::new(
        applied
            .prepared
            .tracked
            .keys()
            .filter(|path| {
                !applied.prepared.unavailable.contains(*path)
                    && !applied.prepared.omitted.contains_key(*path)
            })
            .cloned()
            .collect(),
    );
    policy.materialized(
        policy.remote.as_ref().unwrap().workspace.clone(),
        applied.discovery_identity()?,
    );
    Ok(())
}
enum RepositoryNavigation {
    Found(CallToolResult),
    Missing { revision: String, path: String },
    Unknown,
}
pub(super) struct Inventory {
    pub manifest: Arc<Manifest>,
    pub context: Option<RepositoryContext>,
    watch: crate::watch::Snapshot,
    heap_bytes: std::sync::OnceLock<u64>,
    summaries: Mutex<Vec<CachedSummary>>,
}

struct CachedSummary {
    context: RepositoryContext,
    root: PathBuf,
    manifest: std::sync::Weak<Manifest>,
    sources: Vec<std::sync::Weak<crate::native::jobs::Snapshot>>,
    summary: crate::summary::Summary,
}

impl Inventory {
    pub(super) fn summary(
        &self,
        context: &RepositoryContext,
        root: &Path,
        base: &Arc<Manifest>,
        manifest: &Manifest,
        sources: &[std::sync::Weak<crate::native::jobs::Snapshot>],
    ) -> crate::summary::Summary {
        let mut cached = self.summaries.lock().unwrap();
        let base = Arc::downgrade(base);
        if let Some(saved) = cached.iter().find(|saved| {
            &saved.context == context
                && saved.root == root
                && saved.manifest.ptr_eq(&base)
                && saved.sources.len() == sources.len()
                && saved
                    .sources
                    .iter()
                    .zip(sources)
                    .all(|(left, right)| left.ptr_eq(right))
        }) {
            return saved.summary.clone();
        }
        let summary = crate::summary::Summary::build(
            &context.0, &context.1, &context.2, context.3, root, manifest,
        );
        if cached.len() == 8 {
            cached.remove(0);
        }
        cached.push(CachedSummary {
            context: context.clone(),
            root: root.to_owned(),
            manifest: base,
            sources: sources.to_vec(),
            summary: summary.clone(),
        });
        summary
    }

    fn summary_heap_bytes(&self) -> u64 {
        self.summaries
            .lock()
            .unwrap()
            .iter()
            .map(|cached| {
                cached.summary.key.capacity()
                    + cached.summary.table.capacity()
                    + cached.root.as_os_str().len()
                    + cached.context.1.capacity()
                    + cached.context.2.capacity()
                    + 256
                    + cached.sources.capacity()
                        * std::mem::size_of::<std::sync::Weak<crate::native::jobs::Snapshot>>()
            })
            .sum::<usize>() as u64
    }

    fn retained_heap_bytes(&self) -> u64 {
        *self.heap_bytes.get_or_init(|| {
            crate::memory::encoded_size(self.manifest.as_ref()).saturating_mul(3) as u64
        }) + self.manifest.derived.heap_bytes()
    }

    pub async fn current(self: &Arc<Self>) -> Result<bool> {
        let changes = self.watch.changes();
        if !changes.any() {
            return Ok(true);
        }
        let inventory = self.clone();
        Ok(tokio::task::spawn_blocking(move || {
            inventory.manifest.changed_sources_current(Some(&changes))
        })
        .await?)
    }
}
#[derive(Clone)]
enum Progress {
    Pending(&'static str),
    Files(Arc<Inventory>),
    Refreshing(Arc<Inventory>, Option<u64>),
    RetryFailed(Arc<Inventory>, Option<u64>, std::time::Instant),
    Ready(Arc<Inventory>, Option<u64>),
    Failed(Arc<anyhow::Error>, Option<Arc<Inventory>>),
    Paused(Option<Arc<Inventory>>, std::time::Instant),
}
struct Continuation {
    plan: crate::workspace::IndexPlan,
    base: Arc<Manifest>,
    generation: Option<u64>,
    heap_bytes: std::sync::OnceLock<u64>,
}
#[derive(Clone)]
pub(super) struct Ticket {
    progress: watch::Receiver<Progress>,
    pub(super) rust: super::rust::Ticket,
    id: u64,
    telemetry_id: Option<String>,
    demand: Arc<crate::demand::Demand>,
    continuation: Arc<Mutex<Option<Continuation>>>,
}
impl Ticket {
    pub fn interest(&self) -> crate::demand::Interest {
        self.demand.interest()
    }
    pub fn expired(&self) -> bool {
        matches!(&*self.progress.borrow(), Progress::Paused(_, since) if since.elapsed() >= crate::demand::RETAIN_PAUSED)
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn telemetry_id(&self) -> Option<String> {
        self.telemetry_id.clone()
    }
    #[cfg(test)]
    pub fn discovering(&self) -> bool {
        matches!(*self.progress.borrow(), Progress::Pending(_))
    }
    pub fn running(&self) -> bool {
        matches!(
            *self.progress.borrow(),
            Progress::Pending(_) | Progress::Files(_) | Progress::Refreshing(..)
        )
    }
    pub fn current_ready(&self, generation: Option<u64>) -> bool {
        matches!(
            &*self.progress.borrow(),
            Progress::Ready(_, _) | Progress::RetryFailed(..)
        ) && self.refresh_reason(generation).is_none()
    }
    pub fn refresh_reason(&self, generation: Option<u64>) -> Option<&'static str> {
        let progress = self.progress.borrow();
        let (inventory, prepared) = match &*progress {
            Progress::Ready(inventory, prepared)
            | Progress::RetryFailed(inventory, prepared, _)
            | Progress::Refreshing(inventory, prepared) => (inventory, prepared),
            Progress::Failed(..) => return Some("previous preparation failed"),
            Progress::Paused(..) => return Some("resume preparation"),
            Progress::Pending(_) | Progress::Files(_) => return None,
        };
        if *prepared != generation {
            return Some("repository generation changed");
        }
        inventory
            .watch
            .change_reason()
            .or_else(|| match &*progress {
                Progress::Ready(..)
                    if inventory
                        .manifest
                        .dependency_retry
                        .is_some_and(|retry| retry.due(crate::cache::now())) =>
                {
                    Some("dependency retry due")
                }
                Progress::RetryFailed(_, _, after) if std::time::Instant::now() >= *after => {
                    Some("retry after preparation failure")
                }
                _ => None,
            })
    }
    pub fn retry_inventory(
        &self,
        generation: Option<u64>,
    ) -> Option<(Arc<Inventory>, Option<u64>)> {
        match &*self.progress.borrow() {
            Progress::Ready(inventory, prepared)
            | Progress::RetryFailed(inventory, prepared, _)
                if *prepared == generation && !inventory.watch.changed() =>
            {
                Some((inventory.clone(), *prepared))
            }
            _ => None,
        }
    }
    pub fn retained_inventory(&self) -> bool {
        matches!(*self.progress.borrow(), Progress::Failed(_, Some(_)))
    }
    pub fn retained_heap_bytes(&self, workspace_manifest: Option<&Arc<Manifest>>) -> (u64, u64) {
        let (rust_bytes, rust_pinned) = self.rust.heap_bytes();
        let progress = self.progress.borrow();
        let inventory = match &*progress {
            Progress::Failed(_, Some(inventory))
            | Progress::Paused(Some(inventory), _)
            | Progress::Refreshing(inventory, _)
            | Progress::RetryFailed(inventory, _, _)
            | Progress::Files(inventory)
            | Progress::Ready(inventory, _) => Some(inventory),
            _ => None,
        };
        let mut bytes = inventory.map_or(0, |inventory| {
            let manifest_bytes = if workspace_manifest
                .is_some_and(|manifest| Arc::ptr_eq(manifest, &inventory.manifest))
            {
                0
            } else {
                inventory.retained_heap_bytes()
            };
            manifest_bytes + inventory.summary_heap_bytes()
        });
        if let Some(saved) = self.continuation.lock().unwrap().as_ref()
            && !inventory
                .is_some_and(|inventory| Arc::ptr_eq(&saved.plan.manifest, &inventory.manifest))
            && !workspace_manifest
                .is_some_and(|manifest| Arc::ptr_eq(manifest, &saved.plan.manifest))
        {
            bytes += *saved.heap_bytes.get_or_init(|| {
                crate::memory::encoded_size(saved.plan.manifest.as_ref()).saturating_mul(3) as u64
            }) + saved.plan.manifest.derived.heap_bytes();
        }
        let pinned = if inventory.is_some_and(|inventory| Arc::strong_count(inventory) > 1) {
            bytes
        } else {
            0
        };
        (bytes + rust_bytes, pinned + rust_pinned)
    }
    pub async fn inventory(&mut self) -> Result<Arc<Inventory>> {
        loop {
            match self.progress.borrow().clone() {
                Progress::Files(files)
                | Progress::Refreshing(files, _)
                | Progress::RetryFailed(files, _, _)
                | Progress::Ready(files, _)
                | Progress::Failed(_, Some(files)) => return Ok(files),
                Progress::Failed(error, None) => {
                    return Err(crate::diagnostics::shared_error(error));
                }
                Progress::Paused(..) => {
                    return Err(
                        crate::diagnostics::RefreshRequired("Resume idle preparation").into(),
                    );
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
                Progress::Ready(..) | Progress::RetryFailed(..) => return Ok(()),
                Progress::Failed(error, _) => {
                    return Err(crate::diagnostics::shared_error(error));
                }
                Progress::Paused(..) => {
                    return Err(
                        crate::diagnostics::RefreshRequired("Resume idle preparation").into(),
                    );
                }
                Progress::Pending(stage) => {
                    tracing::Span::current().record("stage", stage);
                }
                Progress::Files(_) | Progress::Refreshing(..) => {
                    tracing::Span::current().record("stage", "source_indexing");
                }
            }
            self.progress.changed().await?;
        }
    }
    pub async fn readable(&mut self, wait: bool) -> Result<(Arc<Inventory>, Option<u64>)> {
        if !wait && let Progress::Refreshing(inventory, generation) = &*self.progress.borrow() {
            return Ok((inventory.clone(), *generation));
        }
        self.complete().await?;
        match &*self.progress.borrow() {
            Progress::Ready(inventory, generation)
            | Progress::RetryFailed(inventory, generation, _) => {
                Ok((inventory.clone(), *generation))
            }
            _ => unreachable!("completed preparation retains its inventory"),
        }
    }
}

fn publish_progress(send: &watch::Sender<Progress>, next: Progress) {
    send.send_if_modified(|current| {
        if matches!(current, Progress::Refreshing(..)) {
            false
        } else {
            *current = next;
            true
        }
    });
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
    pub(super) fn repository_workspace(
        &self,
        branch: &crate::repository::manager::Branch,
    ) -> Result<(PathBuf, Policy, PathBuf)> {
        let source = branch.source().canonicalize()?;
        let mut policy = Policy::new(vec![source.clone()])?;
        policy.unity_platform = self.policy.unity_platform;
        let options = &self.remote.as_ref().unwrap().options;
        policy.remote = Some(crate::discovery::RemoteContext {
            workspace: source.clone(),
            tracked: Default::default(),
            writable: branch.root.join("generated"),
            shared: self.cache.clone(),
            repositories: options.rules.clone(),
            selection_identity: options.selection.identity.clone(),
        });
        Ok((source, policy, branch.analysis_cache()))
    }

    /// Restore a complete cache for navigation without requesting acquisition or discovery.
    pub(super) async fn restore_repository_workspace(
        self: &Arc<Self>,
        branch: Arc<crate::repository::manager::Branch>,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        operation: &crate::telemetry::Operation,
    ) -> Result<()> {
        {
            let state = branch.state.read().await;
            if !state.as_ref().is_some_and(|state| {
                self.remote.as_ref().unwrap().usable(&branch, state)
                    && state.indexed_revision.as_ref() == Some(&state.prepared.revision)
            }) {
                return Ok(());
            }
        }
        let (entry, mut policy, cache) = self.repository_workspace(&branch)?;
        let workspace = {
            let mut registry = self.workspaces.lock().unwrap();
            let slot = registry
                .entry(entry.clone())
                .or_insert_with(|| WorkspaceSlot {
                    state: Arc::new(tokio::sync::Mutex::new(None)),
                    preparing: None,
                    used: Instant::now(),
                });
            if slot.preparing.is_some() {
                return Ok(());
            }
            slot.state.clone()
        };
        let (permit, mut state, branch_state) = self
            .admit_work(
                workspace.clone(),
                Some(&branch),
                &self.navigation_workers,
                "cached_workspace_admission",
                operation,
            )
            .await?;
        let applied = branch_state.as_ref().and_then(|state| state.as_ref());
        if !applied.is_some_and(|state| {
            self.remote.as_ref().unwrap().usable(&branch, state)
                && state.indexed_revision.as_ref() == Some(&state.prepared.revision)
        }) {
            return Ok(());
        }
        materialized_policy(&mut policy, applied.unwrap())?;
        let app = self.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _activity = activity;
            let _permit = permit;
            if state.is_none() {
                *state = Some(Workspace::open(
                    entry.clone(),
                    &cache,
                    policy.clone(),
                    &app.cache.join("analysis"),
                    Some(&branch.root),
                    app.monitor.clone(),
                )?);
            }
            let code = state.as_mut().unwrap();
            code.update_policy(policy);
            let valid = code.validate_cached()?;
            let mut registry = app.workspaces.lock().unwrap();
            let Some(slot) = registry
                .get_mut(&entry)
                .filter(|slot| Arc::ptr_eq(&slot.state, &workspace) && slot.preparing.is_none())
            else {
                return Ok(());
            };
            if valid {
                let inventory = Arc::new(Inventory {
                    manifest: code.manifest.clone(),
                    watch: code.watch_snapshot(),
                    context: context(
                        Some(&branch),
                        branch_state.as_ref().and_then(|state| state.as_ref()),
                    ),
                    heap_bytes: Default::default(),
                    summaries: Mutex::new(Vec::new()),
                });
                let (send, rust) = super::rust::channel();
                send.disable();
                slot.preparing = Some(Ticket {
                    progress: watch::channel(Progress::Ready(
                        inventory,
                        Some(branch.generation.load(std::sync::atomic::Ordering::Acquire)),
                    ))
                    .1,
                    rust,
                    id: 0,
                    telemetry_id: None,
                    demand: branch.demand.clone(),
                    continuation: Arc::default(),
                });
                slot.used = Instant::now();
            } else {
                registry.remove(&entry);
            }
            Ok(())
        })
        .await?
    }

    pub(super) async fn exact_asset_filename(
        self: &Arc<Self>,
        inventory: Arc<Inventory>,
        query: Query,
        usage: (PathBuf, Option<Arc<crate::repository::manager::Branch>>),
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<Option<CallToolResult>> {
        let permit = self.navigation_workers.clone().acquire_owned().await?;
        let branch_state = match &usage.1 {
            Some(branch) => Some(branch.state.clone().read_owned().await),
            None => None,
        };
        if let Some(state) = &branch_state {
            ensure!(
                state.as_ref().is_some_and(|state| {
                    self.remote
                        .as_ref()
                        .unwrap()
                        .usable(usage.1.as_ref().unwrap(), state)
                        && inventory
                            .context
                            .as_ref()
                            .is_some_and(|context| context.2 == state.prepared.revision)
                }),
                crate::diagnostics::RefreshRequired(
                    "Repository changed before file lookup; retry query"
                )
            );
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let app = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            Ok(crate::unity::assets::selection::exact_workspace_file(
                &inventory.manifest,
                branch_state
                    .as_ref()
                    .and_then(|state| state.as_ref())
                    .map(|state| &state.prepared),
                &query,
                &cancel,
            )?
            .then(|| {
                app.record_usage(&usage.0, usage.1.as_deref(), &inventory.manifest);
                CallToolResult::success(vec![ContentBlock::text(crate::navigation::quote(
                    &query.target.name,
                ))])
            }))
        })
        .await?;
        guard.disarm();
        result
    }

    pub(super) async fn view_asset_file(
        self: &Arc<Self>,
        inventory: Arc<Inventory>,
        path: String,
        mode: crate::navigation::Mode,
        usage: (PathBuf, Option<Arc<crate::repository::manager::Branch>>),
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        operation: crate::telemetry::Operation,
    ) -> Result<Option<CallToolResult>> {
        for acquired in [false, true] {
            let (revision, selected) = match self
                .view_asset_snapshot(
                    inventory.clone(),
                    path.clone(),
                    mode,
                    usage.clone(),
                    activity.clone(),
                    operation.clone(),
                )
                .await?
            {
                RepositoryNavigation::Found(result) => return Ok(Some(result)),
                RepositoryNavigation::Unknown => return Ok(None),
                RepositoryNavigation::Missing { revision, path } => (revision, path),
            };
            ensure!(!acquired, "Asset source is unavailable");
            let branch = usage.1.as_ref().unwrap().clone();
            let remote = self.remote.as_ref().unwrap().clone();
            let activity = activity.clone();
            let operation = operation.clone();
            let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
            let guard = cancel.as_ref().clone().drop_guard();
            tokio::spawn(async move {
                let _activity = activity;
                remote
                    .asset_file(&branch, &revision, &selected, &operation, cancel)
                    .await
            })
            .await??;
            guard.disarm();
        }
        unreachable!()
    }

    async fn view_asset_snapshot(
        self: &Arc<Self>,
        inventory: Arc<Inventory>,
        path: String,
        mode: crate::navigation::Mode,
        usage: (PathBuf, Option<Arc<crate::repository::manager::Branch>>),
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        operation: crate::telemetry::Operation,
    ) -> Result<RepositoryNavigation> {
        let permit = {
            let _wait = operation.wait("query_admission");
            self.navigation_workers.clone().acquire_owned().await?
        };
        let branch_state = match &usage.1 {
            Some(branch) => Some(branch.state.clone().read_owned().await),
            None => None,
        };
        if let Some(state) = &branch_state {
            let state = state
                .as_ref()
                .context("Repository inputs are unavailable")?;
            ensure!(
                !state.repair
                    && inventory
                        .context
                        .as_ref()
                        .is_some_and(|context| context.2 == state.prepared.revision),
                crate::diagnostics::RefreshRequired(
                    "Repository changed before asset view; retry query"
                )
            );
        }
        let cache = self.asset_cache(&inventory.manifest, usage.1.as_deref());
        let leases = crate::cache::views::LeaseSet::new(&self.cache)?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let app = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            let prepared = branch_state
                .as_ref()
                .and_then(|state| state.as_ref())
                .map(|state| &state.prepared);
            let omitted = std::collections::BTreeMap::new();
            let unavailable = std::collections::BTreeSet::new();
            let manifest = &inventory.manifest;
            let reader = crate::unity::assets::view::Reader {
                root: &manifest.root,
                cache: &cache,
                manifest,
                records: &app.asset_records,
                leases: &leases,
                remote: app.remote.is_some(),
                omitted: prepared.map_or(&omitted, |prepared| &prepared.omitted),
                unavailable: prepared.map_or(&unavailable, |prepared| &prepared.unavailable),
                deferred: prepared.map_or(&unavailable, |prepared| &prepared.deferred_assets),
                cancel: &cancel,
            };
            let Some(resolved) = reader.resolve(&path, mode)? else {
                return Ok(RepositoryNavigation::Unknown);
            };
            if let Some(prepared) = prepared
                && let Ok(relative) = resolved.file.strip_prefix(&manifest.root)
                && let Some(relative) = relative.to_str()
                && prepared.deferred_assets.contains(relative)
            {
                return Ok(RepositoryNavigation::Missing {
                    revision: prepared.revision.clone(),
                    path: relative.into(),
                });
            }
            let Some(text) = reader.render(&resolved, mode)? else {
                return Ok(RepositoryNavigation::Unknown);
            };
            app.record_usage(&usage.0, usage.1.as_deref(), manifest);
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            if mode != crate::navigation::Mode::Raw
                && let Some(context) = &inventory.context
            {
                let root = usage
                    .1
                    .as_ref()
                    .map(|branch| branch.source())
                    .unwrap_or_else(|| manifest.root.clone());
                inventory
                    .summary(context, &root, manifest, manifest, &[])
                    .attach(&mut result);
            }
            Ok(RepositoryNavigation::Found(result))
        })
        .await?;
        guard.disarm();
        result
    }

    /// Repository paths and bytes are usable before build discovery acquires dependencies.
    pub(super) async fn navigate_repository(
        self: &Arc<Self>,
        branch: Arc<crate::repository::manager::Branch>,
        request: super::Request,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<Option<CallToolResult>> {
        let result = self
            .navigate_repository_snapshot(branch.clone(), request.clone(), activity.clone(), None)
            .await?;
        let (revision, path) = match result {
            RepositoryNavigation::Found(result) => return Ok(Some(result)),
            RepositoryNavigation::Unknown => return Ok(None),
            RepositoryNavigation::Missing { revision, path } => (revision, path),
        };
        tracing::Span::current().record("stage", "acquisition");
        let file = self
            .remote
            .as_ref()
            .unwrap()
            .source_file(&branch, &revision, &path, activity.clone())
            .await?;
        match self
            .navigate_repository_snapshot(branch, request, activity, Some(file))
            .await?
        {
            RepositoryNavigation::Found(result) => Ok(Some(result)),
            _ => anyhow::bail!("Source contents are unavailable"),
        }
    }

    async fn navigate_repository_snapshot(
        self: &Arc<Self>,
        branch: Arc<crate::repository::manager::Branch>,
        request: super::Request,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        file: Option<crate::repository::manager::FileView>,
    ) -> Result<RepositoryNavigation> {
        tracing::Span::current().record("stage", "query_admission");
        let permit = self.navigation_workers.clone().acquire_owned().await?;
        tracing::Span::current().record("stage", "evaluation");
        let state = branch.state.clone().read_owned().await;
        let inventory = branch.inventory.borrow().clone();
        let state = (file.is_none()
            && state
                .as_ref()
                .is_some_and(|state| self.remote.as_ref().unwrap().usable(&branch, state)))
        .then_some(state);
        if let Some(prepared) = state
            .as_ref()
            .and_then(|state| state.as_ref())
            .map(|state| &state.prepared)
            .or_else(|| inventory.as_ref().map(|inventory| &inventory.prepared))
        {
            tracing::Span::current().record("selector", branch.selector(prepared));
            tracing::Span::current().record("revision", prepared.revision.as_str());
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let guard = cancel.clone().drop_guard();
        let app = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            let root = branch.source();
            let (prepared, contents) = if let Some(file) = &file {
                (&file.prepared, Some(file.source.path()))
            } else if let Some(state) = state.as_ref().and_then(|s| s.as_ref()) {
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
                super::Request::Search(query) => {
                    let query = Query::parse(&query)?;
                    let manifest = Manifest { root, ..Default::default() };
                    let found = crate::unity::assets::selection::exact_file(&manifest, Some(prepared), &query, &cancel)?;
                    if found { branch.record_use(); }
                    return Ok(if found { RepositoryNavigation::Found(CallToolResult::success(vec![ContentBlock::text(crate::navigation::quote(&query.target.name))])) } else { RepositoryNavigation::Unknown });
                }
            };
            if let Some(mode) = mode
                && !path.starts_with("unity@")
            {
                let metadata = crate::navigation::location(&path).is_ok_and(|(path, _)| Path::new(path).extension().is_some_and(|extension| extension == "meta"));
                let candidates: std::collections::BTreeMap<_, _> = prepared.selected.keys()
                    .chain(prepared.deferred_assets.iter()).chain(prepared.omitted.keys())
                    .chain(prepared.unavailable.iter())
                    .filter(|path| metadata || !Path::new(path).extension().is_some_and(|extension| extension == "meta"))
                    .filter(|path| crate::unity::assets::view::supported(Path::new(path))
                        || crate::navigation::sources::repository_language(Path::new(path), false).is_some())
                    .map(|path| (path.clone(), ())).collect();
                use crate::navigation::sources::FileSelection;
                let selected = crate::navigation::sources::resolve_file(&candidates, &root, &path, mode, false)?;
                if let FileSelection::Choices { count, text } = &selected && *count > 1 {
                    ensure!(mode != crate::navigation::Mode::Raw, "{text}");
                    return Ok(RepositoryNavigation::Found(CallToolResult::success(vec![ContentBlock::text(text.clone())])));
                }
                if let FileSelection::File { path: logical, lines, .. } = selected
                    && crate::unity::assets::view::supported(Path::new(logical)) {
                    let target = contents.map(|contents| contents.join(logical));
                    if !target.as_deref().map(crate::repository::materialize::source_ready).transpose()?.unwrap_or(false) && !prepared.unavailable.contains(logical) && !prepared.omitted.contains_key(logical) {
                        return Ok(RepositoryNavigation::Missing { revision: prepared.revision.clone(), path: logical.into() });
                    }
                    let target = target.unwrap_or_else(|| root.join(logical));
                    let manifest = Manifest { root: root.clone(), ..Default::default() };
                    let leases = crate::cache::views::LeaseSet::new(&app.cache)?;
                    let cache = app.asset_cache(&manifest, Some(&branch));
                    let reader = crate::unity::assets::view::Reader {
                        root: &root, cache: &cache, manifest: &manifest, records: &app.asset_records,
                        leases: &leases, remote: true, omitted: &prepared.omitted,
                        unavailable: &prepared.unavailable, cancel: &cancel,
                        deferred: &prepared.deferred_assets,
                    };
                    let normalized = lines.map_or_else(|| logical.to_owned(), |(first,last)| format!("{logical}:{first}-{last}"));
                    if let Some(text) = reader.view_resolved(logical, &target, &normalized, mode)? {
                        branch.record_use();
                        return Ok(RepositoryNavigation::Found(CallToolResult::success(vec![ContentBlock::text(text)])));
                    }
                }
            }
            let mut missing = None;
            let Some(text) = crate::navigation::sources::repository(
                &root, contents, prepared, &path, mode, &cancel,
                |path| {
                    missing = Some(path.to_owned());
                    Ok(None)
                },
            )? else {
                return Ok(missing.map_or(RepositoryNavigation::Unknown, |path|
                    RepositoryNavigation::Missing { revision: prepared.revision.clone(), path }));
            };
            branch.record_use();
            let mut result = CallToolResult::success(vec![ContentBlock::text(if mode.is_none() {
                format!("> Repository file inventory; symbols and assets are indexed when requested.\n\n{text}")
            } else {
                text
            })]);
            if mode.is_none() {
                result.structured_content = Some(serde_json::json!({"complete": false}));
            }
            Ok(RepositoryNavigation::Found(result))
        }).await?;
        guard.disarm();
        result
    }

    pub(super) async fn navigate_early(
        self: &Arc<Self>,
        inventory: Arc<Inventory>,
        request: super::Request,
        query: Option<Query>,
        usage: (PathBuf, Option<Arc<crate::repository::manager::Branch>>),
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<CallToolResult> {
        tracing::Span::current().record("stage", "evaluation");
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
            let raw = matches!(request, super::Request::View(_, crate::navigation::Mode::Raw));
            let assets_pending =
                !matches!(request, super::Request::View(..))
                && !indexes::source_only(&request, query.as_ref()) && indexes::has_unity(manifest);
            let mut text = match request {
                super::Request::History(_) => unreachable!("history bypasses snapshot preparation"),
                super::Request::Search(_) => sources.files(query.as_ref().unwrap(), &root),
                super::Request::Browse(path) => sources.browse(&root, &path, app.remote.is_none()),
                super::Request::View(path, mode) => {
                    sources.view(&root, &path, mode, app.remote.is_none())
                }
            }?;
            if assets_pending {
                text = format!("> Index incomplete: Unity assets. This view covers the available sources.\n\n{text}");
            }
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            if assets_pending {
                result.structured_content = Some(serde_json::json!({"complete": false}));
            }
            if !raw && let Some(context) = &inventory.context {
                inventory.summary(context, &root, manifest, manifest, &[])
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
    pub reason: &'static str,
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub entry: PathBuf,
    pub policy: Policy,
    pub cache: PathBuf,
    pub branch: Option<Arc<crate::repository::manager::Branch>>,
    pub activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    pub retry: Option<(Arc<Inventory>, Option<u64>)>,
    pub resume: Option<Ticket>,
}
pub(super) fn start(preparing_app: Arc<App>, request: Request) -> Ticket {
    let Request {
        reason,
        workspace,
        entry,
        policy,
        cache,
        branch: preparing_branch,
        activity: preparing_activity,
        retry,
        resume,
    } = request;
    let resume = resume.filter(|ticket| !ticket.expired());
    let retrying = retry.is_some();
    let retry_base = retry.clone();
    let demand = preparing_branch
        .as_ref()
        .map(|branch| branch.demand.clone())
        .or_else(|| resume.as_ref().map(|ticket| ticket.demand.clone()))
        .unwrap_or_default();
    // Starting preparation represents a caller's request. Bridge the handoff
    // until the returned ticket's foreground interest is registered.
    let _starting = demand.interest();
    let continuation = resume
        .as_ref()
        .map(|ticket| ticket.continuation.clone())
        .unwrap_or_default();
    let retained = continuation.clone();
    let work = Arc::new(demand.work());
    let (rust_send, rust_receive) = super::rust::channel();
    if retrying {
        rust_send.disable();
    }
    let (send, receive) = watch::channel(retry.map_or(
        Progress::Pending("preparation"),
        |(inventory, generation)| Progress::Refreshing(inventory, generation),
    ));
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
        tracing::info!(shared_job = job, workspace = %entry.display(), reason, "Workspace preparation started");
        let mut lifecycle = None;
        let mut rust_job: Option<tokio::task::JoinHandle<()>> = None;
        let outcome = async {
            if let Some(branch) = &preparing_branch {
                publish_progress(&send, Progress::Pending("acquisition"));
                operation.stage("repository_acquisition");
                work.until(preparing_app
                    .remote
                    .as_ref()
                    .unwrap()
                    .ready(branch, &operation))
                    .await?;
                publish_progress(&send, Progress::Pending("preparation"));
            }
            operation.stage("preparation_admission");
            publish_progress(&send, Progress::Pending("preparation_admission"));
            #[cfg(test)]
            if preparing_app.preparation_lifecycle.available_permits() == 0 {
                preparing_app.preparation_admission_started.notify_one();
            }
            lifecycle = Some({
                let _wait = operation.wait("preparation_admission");
                work.until(async { Ok(preparing_app
                    .preparation_lifecycle
                    .clone()
                    .acquire_owned()
                    .await?) }).await?
            });
            #[cfg(test)]
            preparing_app.preparation_started.notify_one();
            let mut policy = policy;
            if preparing_branch.is_some() {
                policy.unity_prefetched = Some(Default::default());
            }
            let dependency_leases = policy.leases(&cache)?;
            let mut discovery_budget = None;
            let (state, branch_state) = loop {
                if let Some(job) = rust_job.take() {
                    let _ = job.await;
                    rust_send.disable();
                }
                operation.stage("discovery_admission");
                let (mut permit, mut state, mut branch_state) = work.until(preparing_app
                    .admit_work(
                        workspace.clone(),
                        preparing_branch.as_ref(),
                        &preparing_app.preparation_workers,
                        "discovery_admission",
                        &operation,
                    ))
                    .await?;
                let budget = *discovery_budget.get_or_insert_with(crate::msbuild::Budget::default);
                if let Some(gate) = &branch_state {
                    let applied = gate
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("Repository inputs are unavailable"))?;
                    ensure!(
                        !applied.repair,
                        "Repository materialization requires repair"
                    );
                    materialized_policy(&mut policy, applied)?;
                }
                let saved = retained.lock().unwrap().take();
                let saved = if let Some(saved) = saved {
                    let (saved, current) = tokio::task::spawn_blocking(move || {
                        let current = saved.plan.manifest.changed_sources_current(None);
                        (saved, current)
                    }).await?;
                    current.then_some(saved)
                } else { None };
                let resumed = saved.is_some();
                let prepared = if let Some(saved) = saved.filter(|saved|
                    state.as_ref().is_some_and(|code| Arc::ptr_eq(&saved.base, &code.manifest))
                    && saved.generation == preparing_branch.as_ref().map(|branch| branch.generation.load(std::sync::atomic::Ordering::Acquire))
                    && !saved.plan.watch.changed()) {
                    Some(saved.plan)
                } else {
                work.check()?;
                let rediscovered = if retrying {
                    let code = state.as_mut().unwrap();
                    let (inventory, generation) = retry_base.as_ref().unwrap();
                    ensure!(Arc::ptr_eq(&inventory.manifest, &code.manifest)
                        && !inventory.watch.changed()
                        && *generation == preparing_branch.as_ref().map(|branch|
                            branch.generation.load(std::sync::atomic::Ordering::Acquire)),
                        crate::diagnostics::RefreshRequired("Workspace changed before dependency retry"));
                    code.update_policy(policy.clone());
                    let base = code.manifest.clone();
                    let watch = code.watch_snapshot();
                    let rediscovery = code.rediscovery();
                    let generation = preparing_branch.as_ref().map(|branch|
                        branch.generation.load(std::sync::atomic::Ordering::Acquire));
                    drop(branch_state);
                    drop(state);
                    #[cfg(test)]
                    let pause = {
                        preparing_app.discovery_started.notify_one();
                        preparing_app.discovery_pause.clone().acquire_owned().await?
                    };
                    operation.stage("dependency_retry");
                    let result = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        rediscovery.run(budget)
                    }).await?;
                    #[cfg(test)]
                    drop(pause);
                    (permit, state, branch_state) = work.until(preparing_app.admit_work(
                        workspace.clone(), preparing_branch.as_ref(),
                        &preparing_app.preparation_workers, "discovery_admission", &operation,
                    )).await?;
                    if !Arc::ptr_eq(&base, &state.as_ref().unwrap().manifest)
                        || watch.changed()
                        || generation != preparing_branch.as_ref().map(|branch|
                            branch.generation.load(std::sync::atomic::Ordering::Acquire))
                    {
                        return Err(crate::diagnostics::RefreshRequired(
                            "Workspace changed during dependency discovery; retry query"
                        ).into());
                    }
                    Some(result)
                } else { None };
                let app = preparing_app.clone();
                let discovery_entry = entry.clone();
                let cache = cache.clone();
                let current_policy = policy.clone();
                let owner = preparing_branch.as_ref().map(|branch| branch.root.clone());
                let rust_eligible = !retrying && branch_state.as_ref().and_then(|s| s.as_ref()).is_some_and(|s|
                    s.prepared.unavailable.is_empty() && s.prepared.omitted.is_empty()
                        && s.prepared.deferred_assets.is_empty());
                if !rust_eligible { rust_send.disable(); }
                let (stage_send, mut stage_receive) = tokio::sync::oneshot::channel();
                let generation = preparing_branch.as_ref().map(|branch|
                    branch.generation.load(std::sync::atomic::Ordering::Acquire));
                let rust_policy = policy.clone();
                #[cfg(test)]
                let _discovery_pause = {
                    preparing_app.discovery_started.notify_one();
                    preparing_app.discovery_pause.clone().acquire_owned().await?
                };
                operation.stage("discovery");
                let planning_work = work.clone();
                let mut discovering = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
                    if state.is_none() {
                        *state = Some(Workspace::open(
                            discovery_entry,
                            &cache,
                            current_policy.clone(),
                            &app.cache.join("analysis"),
                            owner.as_deref(),
                            app.monitor.clone(),
                        )?);
                    }
                    state.as_mut().unwrap().update_policy(current_policy.clone());
                    let code = state.as_mut().unwrap();
                    let mut stage_send = Some(stage_send);
                    let mut stage = |discovery: &crate::discovery::Discovery, managed: &[PathBuf]| {
                        if rust_eligible && let Some(send) = stage_send.take() {
                            let staged = crate::discovery::rust::stage(discovery, managed, &current_policy, app.monitor.clone());
                            match staged {
                                Ok(Some(staged)) => {
                                    let _ = send.send(staged);
                                    #[cfg(test)]
                                    {
                                        app.rust_staged.notify_one();
                                        let _pause = tokio::runtime::Handle::current().block_on(app.managed_pause.acquire());
                                    }
                                }
                                Ok(None) => (),
                                Err(error) => tracing::debug!(%error, "Rust completeness proof unavailable"),
                            }
                        }
                    };
                    let result = rediscovered.map_or_else(
                        || code.prepare_staged(budget, Some(&mut stage)),
                        |prepared| prepared.map(Some),
                    )
                        .and_then(|p| p.map(|p| code.plan_cancellable(p, planning_work.as_ref())).transpose());
                    crate::memory::reclaim();
                    Ok((state, result))
                });
                let discovered = tokio::select! {
                    result = &mut discovering => result,
                    staged = &mut stage_receive => {
                        if let Ok(staged) = staged {
                            let send = rust_send.clone();
                            let app = preparing_app.clone();
                            let entry = entry.clone();
                            let branch = preparing_branch.as_ref().unwrap().clone();
                            let operation = operation.clone();
                            let work = work.clone();
                            rust_job = Some(tokio::spawn(async move {
                                send.index(app, super::rust::IndexRequest {
                                    stage: staged, entry, policy: rust_policy, branch,
                                    generation: generation.unwrap(), operation, work,
                                }).await;
                            }));
                        } else { rust_send.disable(); }
                        discovering.await
                    }
                };
                if rust_job.is_none() { rust_send.disable(); }
                let (next, result) = discovered??;
                #[cfg(test)]
                drop(_discovery_pause);
                state = next;
                match result {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        let required = error.downcast_ref::<crate::discovery::RequiredInputs>()
                            .map(|required| required.0.clone());
                        let unity = error.downcast_ref::<crate::discovery::UnityDependencies>()
                            .map(|request| request.0.clone());
                        if required.is_none() && unity.is_none() {
                            return Err(error);
                        }
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
                        drop(branch_state);
                        drop(state);
                        // A queued publisher may precede the child's next analysis
                        // read. Release our pin before joining that child.
                        if let Some(job) = rust_job.take() { let _ = job.await; }
                        rust_send.disable();
                        publish_progress(&send, Progress::Pending("acquisition"));
                        operation.stage(if unity.is_some() { "unity_dependencies" } else { "required_input_acquisition" });
                        work.check()?;
                        dependencies::wait(&preparing_app, &mut lifecycle, &work, &operation, async {
                            if let Some(root) = &unity {
                                dependencies::prefetch(&preparing_app, branch, &revision,
                                    dependencies::Prefetch {
                                        root: root.clone(), policy: policy.clone(),
                                        leases: dependency_leases.clone(),
                                    }, work.clone(), &operation,
                                ).await
                            } else {
                                preparing_app.remote.as_ref().unwrap()
                                    .require_inputs_cancellable(branch, &revision, required.unwrap(), &operation, work.clone())
                                    .await
                            }
                        }).await?;
                        if let Some(root) = unity {
                            policy.unity_prefetched.as_mut().unwrap().insert(root);
                        }
                        publish_progress(&send, Progress::Pending("preparation"));
                        continue;
                    }
                }
                };
                let Some(prepared) = prepared else {
                    break (state, branch_state);
                };
                publish_progress(&send, Progress::Files(Arc::new(Inventory {
                    manifest: prepared.manifest.clone(),
                    context: context(
                        preparing_branch.as_deref(),
                        branch_state.as_ref().and_then(|s| s.as_ref()),
                    ),
                    watch: prepared.watch.clone(),
                    heap_bytes: std::sync::OnceLock::new(),
                    summaries: Mutex::new(Vec::new()),
                })));
                let queued = std::time::Instant::now();
                let manifest = state.as_ref().unwrap().manifest.clone();
                let generation = preparing_branch.as_ref().map(|branch|
                    branch.generation.load(std::sync::atomic::Ordering::Acquire));
                *retained.lock().unwrap() = Some(Continuation { plan: prepared.clone(), base: manifest.clone(), generation, heap_bytes: Default::default() });
                drop(branch_state);
                drop(state);
                operation.stage("indexing_admission");
                let (permit, mut state, branch_state) = work.until(preparing_app
                    .admit_work(
                        workspace.clone(),
                        preparing_branch.as_ref(),
                        &preparing_app.indexing_workers,
                        "indexing_admission",
                        &operation,
                    ))
                    .await?;
                if !Arc::ptr_eq(&manifest, &state.as_ref().unwrap().manifest)
                    || generation != preparing_branch.as_ref().map(|branch|
                    branch.generation.load(std::sync::atomic::Ordering::Acquire))
                {
                    retained.lock().unwrap().take();
                    ensure!(!retrying, crate::diagnostics::RefreshRequired("Workspace changed before dependency publication"));
                    // Publication replaced the inputs while indexing was queued.
                    // Rebuild the plan from the newly admitted repository view.
                    send.send_replace(Progress::Pending("preparation"));
                    continue;
                }
                tracing::debug!(workspace = %entry.display(), wait_ms = queued.elapsed().as_millis(), "indexing worker admitted");
                #[cfg(test)]
                let _pause = {
                    preparing_app.indexing_started.notify_one();
                    preparing_app.indexing_pause.clone().acquire_owned().await?
                };
                operation.stage("workspace_indexing");
                let indexing_work = work.clone();
                let (state, indexed) = tokio::task::spawn_blocking(move || -> Result<_> {
                    let _permit = permit;
                    let changes = prepared.watch.changes();
                    if (resumed || changes.any()) && !prepared.manifest.changed_sources_current(if resumed { None } else { Some(&changes) }) {
                        return Ok((state, false));
                    }
                    state.as_mut().unwrap().apply_plan_cancellable(prepared, indexing_work.as_ref())?;
                    crate::memory::reclaim();
                    Ok((state, true))
                })
                .await??;
                if !indexed {
                    retained.lock().unwrap().take();
                    ensure!(!retrying, crate::diagnostics::RefreshRequired("Sources changed before dependency publication"));
                    send.send_replace(Progress::Pending("preparation"));
                    continue;
                }
                break (state, branch_state);
            };

            let code = state.as_ref().unwrap();
            let inventory = Arc::new(Inventory {
                manifest: code.manifest.clone(),
                watch: code.watch_snapshot(),
                context: context(
                    preparing_branch.as_deref(),
                    branch_state.as_ref().and_then(|s| s.as_ref()),
                ),
                heap_bytes: std::sync::OnceLock::new(),
                summaries: Mutex::new(Vec::new()),
            });
            let generation = preparing_branch
                .as_ref()
                .map(|branch| branch.generation.load(std::sync::atomic::Ordering::Acquire));
            drop(branch_state);
            if let (Some(branch), Some(generation)) = (&preparing_branch, generation) {
                operation.stage("index_marker_publication");
                branch.mark_indexed(generation, inventory.manifest.dependency_retry.is_none(), &work, &operation).await?;
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
        // The lifecycle permit and cache ownership cover every blocking child.
        if let Some(job) = rust_job {
            let _ = job.await;
        }
        rust_send.disable();
        let progress = match outcome {
            Ok((inventory, generation)) => {
                operation.finish("success");
                Progress::Ready(inventory, generation)
            }
            Err(error) => {
                if error.is::<crate::demand::Paused>() {
                    operation.finish("paused");
                    let inventory = match &*send.borrow() {
                        Progress::Files(inventory) | Progress::Refreshing(inventory, _) => {
                            Some(inventory.clone())
                        }
                        _ => None,
                    };
                    Progress::Paused(inventory, std::time::Instant::now())
                } else {
                    tracing::warn!(shared_job = job, workspace = %entry.display(), error = %format!("{error:#}"), "Workspace indexing failed");
                    if let Some(diagnostic) = crate::diagnostics::monitoring(&error) {
                        operation.diagnose(diagnostic);
                    }
                    operation.finish("failure");
                    let previous = send.borrow().clone();
                    match previous {
                        Progress::Refreshing(inventory, generation)
                            if workspace.lock().await.as_ref().is_some_and(|code| {
                                code.published_current(&inventory.manifest).unwrap_or(false)
                            }) =>
                        {
                            Progress::RetryFailed(
                                inventory,
                                generation,
                                std::time::Instant::now() + std::time::Duration::from_secs(30),
                            )
                        }
                        Progress::Files(inventory) => {
                            Progress::Failed(Arc::new(error), Some(inventory))
                        }
                        _ => Progress::Failed(Arc::new(error), None),
                    }
                }
            }
        };
        if !matches!(progress, Progress::Paused(..)) {
            retained.lock().unwrap().take();
        }
        send.send_replace(progress);
        drop(workspace);
        drop(preparing_branch);
        drop(lifecycle);
        preparing_app.trim_idle_after_completion().await;
        // Shutdown waits on activity, including final trimming and its cache owner.
        drop(preparing_app);
        drop(preparing_activity);
    });
    Ticket {
        progress: receive,
        rust: rust_receive,
        id: job,
        telemetry_id,
        demand,
        continuation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_indexing_releases_capacity_and_resumes_its_plan() {
        use std::time::Duration;
        for changed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("code.rs"), "pub struct Ready;").unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::hard_link(root.path().join("code.rs"), outside.path().join("alias.rs"))
                .unwrap();
            let policy = Policy::new(vec![root.path().into()]).unwrap();
            let app = Arc::new(App::new(policy.clone(), cache.path().into(), 1).unwrap());
            let demand = Arc::new(crate::demand::Demand::new(Duration::ZERO));
            let interest = demand.interest();
            let seed = Ticket {
                progress: watch::channel(Progress::Paused(None, Instant::now())).1,
                rust: super::super::rust::channel().1,
                id: 0,
                telemetry_id: None,
                demand,
                continuation: Arc::default(),
            };
            let held_indexer = app.indexing_workers.clone().acquire_owned().await.unwrap();
            let workspace = Arc::new(tokio::sync::Mutex::new(None));
            let mut ticket = start(
                app.clone(),
                Request {
                    reason: "new workspace",
                    workspace: workspace.clone(),
                    entry: root.path().into(),
                    policy,
                    cache: cache.path().into(),
                    branch: None,
                    activity: Arc::new(app.activity.clone().read_owned().await),
                    retry: None,
                    resume: Some(seed),
                },
            );
            app.workspaces.lock().unwrap().insert(
                root.path().into(),
                super::super::WorkspaceSlot {
                    state: workspace.clone(),
                    preparing: Some(ticket.clone()),
                    used: Instant::now(),
                },
            );
            tokio::time::timeout(Duration::from_secs(10), ticket.inventory())
                .await
                .unwrap()
                .unwrap();
            assert!(ticket.running());
            drop(interest);
            assert!(
                tokio::time::timeout(Duration::from_secs(10), ticket.complete())
                    .await
                    .unwrap()
                    .is_err()
            );
            assert!(matches!(*ticket.progress.borrow(), Progress::Paused(..)));
            assert!(ticket.continuation.lock().unwrap().is_some());
            // Cache trimming may briefly inspect this mutex after completion.
            drop(
                tokio::time::timeout(Duration::from_secs(10), workspace.lock())
                    .await
                    .unwrap(),
            );
            // Completion also releases the parent lifecycle and activity guard.
            tokio::time::timeout(Duration::from_secs(10), async {
                while app.preparation_lifecycle.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // A fresh discovery would block here. Resuming must use the retained plan.
            let _discovery = if changed {
                std::fs::write(outside.path().join("alias.rs"), "pub struct Updated;").unwrap();
                assert!(
                    !ticket
                        .continuation
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .plan
                        .watch
                        .changed()
                );
                None
            } else {
                Some(app.discovery_pause.clone().acquire_owned().await.unwrap())
            };
            drop(held_indexer);
            let symbol = if changed { "Updated" } else { "Ready" };
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                app.search(
                    root.path().to_str().unwrap(),
                    &format!("type:{symbol} wait:complete"),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(result.contains(symbol));
            app.shutdown().await;
        }
    }

    #[tokio::test]
    async fn dependency_retry_keeps_searches_available_and_rechecks_changed_sources() {
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source = root.path().join("code.rs");
        std::fs::write(&source, "pub struct Ready;").unwrap();
        let policy = Policy::new(vec![root.path().into()]).unwrap();
        let app = Arc::new(App::new(policy.clone(), cache.path().into(), 2).unwrap());
        let path = root.path().to_str().unwrap();
        assert!(
            app.search(path, "type:Ready")
                .await
                .unwrap()
                .contains("Ready")
        );
        app.discovery_started.notified().await;
        let pause = app.discovery_pause.clone().acquire_owned().await.unwrap();
        let activity = Arc::new(app.activity.clone().read_owned().await);
        let mut ticket = {
            let mut registry = app.workspaces.lock().unwrap();
            let slot = registry.get_mut(root.path()).unwrap();
            let retry = slot
                .preparing
                .as_ref()
                .unwrap()
                .retry_inventory(None)
                .unwrap();
            let ticket = start(
                app.clone(),
                Request {
                    reason: "dependency retry due",
                    workspace: slot.state.clone(),
                    entry: root.path().into(),
                    policy,
                    cache: cache.path().into(),
                    branch: None,
                    activity,
                    retry: Some(retry),
                    resume: None,
                },
            );
            slot.preparing = Some(ticket.clone());
            ticket
        };
        tokio::time::timeout(Duration::from_secs(5), app.discovery_started.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ticket.rust.ready())
                .await
                .unwrap()
                .is_none()
        );
        let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                app.search(path, "type:Ready"),
                app.search(path, "type:Ready")
            )
        })
        .await
        .unwrap();
        assert!(first.unwrap().contains("Ready"));
        assert!(second.unwrap().contains("Ready"));
        assert_eq!(
            app.workspaces.lock().unwrap()[root.path()]
                .preparing
                .as_ref()
                .unwrap()
                .id(),
            ticket.id()
        );
        assert!(ticket.running());
        std::fs::write(&source, "pub struct Updated;").unwrap();
        drop(pause);
        tokio::time::timeout(Duration::from_secs(5), ticket.complete())
            .await
            .unwrap()
            .unwrap();
        assert!(
            app.search(path, "type:Updated")
                .await
                .unwrap()
                .contains("Updated")
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_preparation_trimming_before_reopening_cache() {
        use std::{future::Future, task::Poll, time::Duration};

        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("code.rs"), "pub struct Ready;").unwrap();
        let policy = Policy::new(vec![root.path().into()]).unwrap();
        let app = Arc::new(App::new(policy.clone(), cache.path().into(), 1).unwrap());
        let pause = app.indexing_pause.clone().acquire_owned().await.unwrap();
        let workspace = Arc::new(tokio::sync::Mutex::new(None));
        let mut ticket = start(
            app.clone(),
            Request {
                reason: "new workspace",
                workspace: workspace.clone(),
                entry: root.path().into(),
                policy: policy.clone(),
                cache: cache.path().into(),
                branch: None,
                activity: Arc::new(app.activity.clone().read_owned().await),
                retry: None,
                resume: None,
            },
        );
        app.workspaces.lock().unwrap().insert(
            root.path().into(),
            super::super::WorkspaceSlot {
                state: workspace,
                preparing: Some(ticket.clone()),
                used: std::time::Instant::now(),
            },
        );
        let mut stopping = Box::pin(app.shutdown());
        std::future::poll_fn(|cx| {
            assert!(stopping.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        // Hold the registry after shutdown captures its preparation tickets.
        // The final trim must wait for this owner to release the registry.
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let owner = app.clone();
        let holder = std::thread::spawn(move || {
            let _registry = owner.workspaces.lock().unwrap();
            started.send(()).unwrap();
            let _ = wait.recv();
        });
        ready.await.unwrap();
        drop(pause);
        tokio::time::timeout(Duration::from_secs(5), ticket.complete())
            .await
            .unwrap()
            .unwrap();
        let stopped =
            std::future::poll_fn(|cx| Poll::Ready(stopping.as_mut().poll(cx).is_ready())).await;
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(
            !stopped,
            "Shutdown returned while preparation still owned the cache"
        );
        tokio::time::timeout(Duration::from_secs(5), stopping)
            .await
            .unwrap();
        drop(app);
        App::new(policy, cache.path().into(), 1).unwrap();
    }

    #[test]
    fn retained_inventory_does_not_double_count_the_workspace_manifest() {
        let manifest = Arc::new(Manifest::default());
        let monitor = Arc::new(crate::watch::Monitor::default());
        let inventory = Arc::new(Inventory {
            manifest: manifest.clone(),
            context: None,
            watch: monitor.snapshot(&Default::default(), 0),
            heap_bytes: std::sync::OnceLock::new(),
            summaries: Mutex::new(Vec::new()),
        });
        let (_send, progress) = watch::channel(Progress::Failed(
            Arc::new(anyhow::anyhow!("indexing failed")),
            Some(inventory),
        ));
        let ticket = Ticket {
            progress,
            rust: super::super::rust::channel().1,
            id: 1,
            telemetry_id: None,
            demand: Arc::default(),
            continuation: Arc::default(),
        };

        assert_eq!(ticket.retained_heap_bytes(Some(&manifest)), (0, 0));
        assert_eq!(
            ticket.retained_heap_bytes(None).0,
            crate::memory::encoded_size(manifest.as_ref()).saturating_mul(3) as u64
        );
    }
}
