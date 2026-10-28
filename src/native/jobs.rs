//! Independent native/shader workspaces over the ordinary content cache.
use super::Group;
use crate::{
    repository::manager::Branch,
    store::Store,
    workspace::{Manifest, Workspace},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

pub struct Snapshot {
    pub store: Arc<Store>,
    pub manifest: Manifest,
}
type Outcome = std::result::Result<Arc<Snapshot>, Arc<anyhow::Error>>;
#[derive(Clone)]
pub struct Ticket {
    outcome: Arc<watch::Receiver<Option<Outcome>>>,
    progress: Arc<Progress>,
    telemetry_id: Option<String>,
}
impl Ticket {
    pub fn telemetry_id(&self) -> Option<String> {
        self.telemetry_id.clone()
    }
    pub fn ready(&self) -> Option<Outcome> {
        self.outcome.borrow().clone()
    }
    pub async fn wait(self) -> Result<Arc<Snapshot>> {
        let mut outcome = self.outcome.as_ref().clone();
        loop {
            if let Some(result) = self.ready() {
                return result.map_err(crate::diagnostics::shared_error);
            }
            outcome
                .changed()
                .await
                .context("Source indexing worker stopped")?;
        }
    }
    pub fn remaining_seconds(&self) -> Option<u64> {
        let completed = self.progress.completed.load(Ordering::Relaxed);
        if completed == 0 {
            return None;
        }
        let remaining = self.progress.total.saturating_sub(completed);
        let seconds = (self.progress.started.elapsed().as_secs_f64() * remaining as f64
            / completed as f64)
            .ceil() as u64;
        Some(seconds.div_ceil(5).max(1) * 5)
    }
}
struct Progress {
    started: Instant,
    completed: AtomicU64,
    total: u64,
    retained: AtomicU64,
}
struct Job {
    generation: [u8; 32],
    ticket: Ticket,
    cancel: CancellationToken,
    writer: Arc<tokio::sync::Mutex<()>>,
    used: Instant,
}
pub struct Jobs {
    running: Mutex<HashMap<(PathBuf, Group), Job>>,
    workers: Arc<Semaphore>,
    shutdown: CancellationToken,
    #[cfg(test)]
    pauses: [Arc<Semaphore>; 2],
}
impl Default for Jobs {
    fn default() -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            workers: Arc::new(Semaphore::new(
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(4),
            )),
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            pauses: std::array::from_fn(|_| Arc::new(Semaphore::new(1))),
        }
    }
}

pub fn generation(manifest: &Manifest, group: Group) -> Result<[u8; 32]> {
    let files: BTreeMap<_, _> = manifest
        .deferred
        .iter()
        .filter(|(_, f)| Group::of(f.language) == Some(group))
        .collect();
    Ok(*blake3::hash(&postcard::to_allocvec(&(
        crate::store::ANALYSIS_VERSION,
        manifest.discovery_policy,
        &manifest.projects,
        files,
    ))?)
    .as_bytes())
}

pub struct Request {
    pub activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub branch: Option<Arc<Branch>>,
    pub entry: PathBuf,
    pub analysis: PathBuf,
    pub manifest: Arc<Manifest>,
    pub group: Group,
}
impl Jobs {
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
    pub fn forget(&self, entry: &std::path::Path) {
        self.running.lock().unwrap().retain(|(path, _), job| {
            if path != entry {
                return true;
            }
            job.cancel.cancel();
            false
        });
    }
    pub fn trim_completed(&self) {
        self.trim_to(0);
    }
    pub fn trim_to(&self, limit: usize) {
        let mut jobs = self.running.lock().unwrap();
        let mut completed: Vec<_> = jobs
            .iter()
            .filter_map(|(key, job)| {
                let outcome = job.ticket.outcome.borrow();
                let result = outcome.as_ref()?;
                Some((
                    key.clone(),
                    job.used,
                    (job.ticket.progress.retained.load(Ordering::Relaxed) as usize).max(256),
                    Arc::strong_count(&job.ticket.outcome) == 1
                        && result
                            .as_ref()
                            .map_or(true, |snapshot| Arc::strong_count(snapshot) == 1),
                ))
            })
            .collect();
        completed.sort_by_key(|(_, used, _, _)| *used);
        let mut bytes: usize = completed.iter().map(|(_, _, bytes, _)| bytes).sum();
        for (key, _, size, idle) in completed {
            if bytes <= limit {
                break;
            }
            if !idle {
                continue;
            }
            jobs.remove(&key);
            bytes -= size;
        }
    }
    #[cfg(test)]
    pub async fn pause(&self, group: Group) -> tokio::sync::OwnedSemaphorePermit {
        self.pauses[group as usize]
            .clone()
            .acquire_owned()
            .await
            .unwrap()
    }
    /// Call under the workspace publication gate; readers pin transactions under that gate too.
    pub fn start(&self, request: Request) -> Result<Ticket> {
        let Request {
            activity,
            workspace,
            branch,
            entry,
            analysis,
            manifest,
            group,
        } = request;
        let generation = generation(&manifest, group)?;
        let mut jobs = self.running.lock().unwrap();
        let key = (entry.clone(), group);
        if let Some(job) = jobs.get_mut(&key)
            && job.generation == generation
            && job.ticket.ready().is_none_or(|r| r.is_ok())
        {
            job.used = Instant::now();
            return Ok(job.ticket.clone());
        }
        let writer = if let Some(previous) = jobs.get(&key) {
            previous.cancel.cancel();
            previous.writer.clone()
        } else {
            Arc::new(tokio::sync::Mutex::new(()))
        };
        let (send, outcome) = watch::channel(None);
        let progress = Arc::new(Progress {
            started: Instant::now(),
            completed: AtomicU64::new(0),
            retained: AtomicU64::new(0),
            total: manifest
                .deferred
                .values()
                .filter(|f| Group::of(f.language) == Some(group))
                .map(|f| f.stamp.size.max(1))
                .sum(),
        });
        let ticket = Ticket {
            outcome: Arc::new(outcome),
            progress: progress.clone(),
            telemetry_id: None,
        };
        let cancel = self.shutdown.child_token();
        let repository = branch
            .as_ref()
            .map(|branch| branch.repository.identity.to_string())
            .or_else(|| Some(entry.display().to_string()));
        let operation = crate::telemetry::operation(
            match group {
                Group::Native => "native_index",
                Group::Shaders => "shader_index",
            },
            repository.as_deref(),
            None,
        );
        if let Some(branch) = &branch {
            operation.target(&branch.name);
        }
        let ticket = Ticket {
            telemetry_id: operation.id(),
            ..ticket
        };
        jobs.insert(
            key,
            Job {
                generation,
                ticket: ticket.clone(),
                cancel: cancel.clone(),
                writer: writer.clone(),
                used: Instant::now(),
            },
        );
        let workers = self.workers.clone();
        #[cfg(test)]
        let pause = self.pauses[group as usize].clone();
        tokio::spawn(async move {
            let _activity = activity;
            let outcome = async {
                operation.stage("writer_admission");
                let _writer = {
                    let _wait = operation.wait("writer_admission");
                    writer.lock_owned().await
                };
                #[cfg(test)]
                let _pause = tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("Source indexing superseded"),
                    permit = pause.acquire_owned() => permit?,
                };
                ensure!(!cancel.is_cancelled(), "Source indexing superseded");
                operation.stage("cache_lookup");
                let fingerprint = *blake3::hash(&postcard::to_allocvec(&(
                    "source-group", crate::store::ANALYSIS_VERSION, &entry, group,
                    manifest.discovery_policy,
                ))?).as_bytes();
                let mut snapshot = Manifest {
                    source_group: Some(group),
                    root: manifest.root.clone(),
                    projects: manifest.projects.clone(),
                    discovery_policy: manifest.discovery_policy,
                    files: manifest.deferred.iter()
                        .filter(|(_, f)| Group::of(f.language) == Some(group))
                        .map(|(k, f)| (k.clone(), f.clone())).collect(),
                    ..Default::default()
                };
                if snapshot.files.is_empty() {
                    let state = {
                        let _wait = operation.wait("workspace_lock");
                        workspace.lock().await
                    };
                    let code = state.as_ref().context("Code workspace unavailable")?;
                    ensure!(self::generation(&code.manifest, group)? == generation, "Source indexing superseded");
                    let snapshot = Arc::new(Snapshot { store: code.store.clone(), manifest: snapshot });
                    progress.retained.store(crate::memory::encoded_size(&snapshot.manifest) as u64 * 3 + 256, Ordering::Relaxed);
                    send.send_replace(Some(Ok(snapshot.clone())));
                    return Ok(snapshot);
                }
                let owner = branch.as_ref().map(|b| b.root.clone());
                let expected = postcard::to_allocvec(&(&snapshot.files, &snapshot.projects, snapshot.discovery_policy))?;
                let (store, cached) = tokio::task::spawn_blocking(move || -> Result<_> {
                    let store = Store::open_workspace(&analysis, fingerprint, &entry, owner.as_deref())?;
                    let cached = store.get_manifest::<Manifest>()?.filter(|saved| {
                        postcard::to_allocvec(&(&saved.files, &saved.projects, saved.discovery_policy)).ok().as_ref() == Some(&expected)
                            && saved.sources_current()
                    });
                    Ok((store, cached))
                }).await??;
                let reused = cached.is_some();
                if let Some(cached) = cached { snapshot = cached; }
                let batches = if reused { Vec::new() } else { crate::workspace::native_batches(&snapshot.files) };
                operation.stage("source_indexing");
                let mut pending = batches.into_iter();
                let mut tasks = tokio::task::JoinSet::new();
                let mut failure = None;
                loop {
                    while tasks.len() < 4 && !cancel.is_cancelled() {
                        let Some(files) = pending.next() else { break; };
                        let store = store.clone();
                        let manifest = manifest.clone();
                        let workers = workers.clone();
                        let cancel = cancel.clone();
                        let operation = operation.clone();
                        tasks.spawn(async move {
                            let _wait = operation.wait("source_worker_admission");
                            let permit = tokio::select! {
                                _ = cancel.cancelled() => anyhow::bail!("Source indexing superseded"),
                                permit = workers.acquire_owned() => permit?,
                            };
                            drop(_wait);
                            tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                let bytes = files.values().map(|file| file.stamp.size.max(1)).sum::<u64>();
                                let results = crate::workspace::index_native_batch(&store, &files, &manifest.projects, &cancel, &operation);
                                (bytes, results)
                            }).await.context("Source extraction worker stopped")
                        });
                    }
                    let Some(result) = tasks.join_next().await else { break; };
                    let (bytes, results) = match result {
                        Ok(Ok(result)) => result,
                        result => {
                            failure = Some(format!("Source worker failed: {result:?}"));
                            cancel.cancel();
                            continue;
                        }
                    };
                    progress.completed.fetch_add(bytes, Ordering::Relaxed);
                    for (key, result) in results {
                        if let Err(error) = result {
                            let file = snapshot.files.remove(&key).unwrap();
                            snapshot.diagnostics.push(format!(
                                "Skipped {}: {error:#}", file.path.display(),
                            ));
                        }
                    }
                }
                if let Some(failure) = failure {
                    anyhow::bail!(failure);
                }
                ensure!(!cancel.is_cancelled(), "Source indexing superseded");
                // A completed superseded worker must never publish over its successor.
                operation.stage("publication");
                let state = {
                    let _wait = operation.wait("workspace_lock");
                    workspace.lock_owned().await
                };
                let current = state.as_ref().context("Code workspace unavailable")?;
                ensure!(!cancel.is_cancelled(), "Source indexing superseded");
                ensure!(
                    self::generation(&current.manifest, group)? == generation,
                    crate::diagnostics::RefreshRequired(
                        "Workspace changed during source indexing; retry query"
                    ),
                );
                let snapshot = tokio::task::spawn_blocking(move || -> Result<_> {
                    ensure!(snapshot.sources_current(), crate::diagnostics::RefreshRequired("Source changed during indexing; retry query"));
                    if !reused { store.save_manifest(&snapshot)?; }
                    Ok(Arc::new(Snapshot { store, manifest: snapshot }))
                }).await??;
                for diagnostic in &snapshot.manifest.diagnostics {
                    tracing::warn!("{diagnostic}");
                }
                tracing::info!(
                    group = group.name(), files = snapshot.manifest.files.len(), reused,
                    workspace = %snapshot.manifest.root.display(),
                    repository = ?branch.as_ref().map(|b| b.repository.identity.to_string()),
                    selector = ?branch.as_ref().map(|b| &b.name),
                    elapsed_ms = progress.started.elapsed().as_millis(), "source group ready",
                );
                progress.retained.store(crate::memory::encoded_size(&snapshot.manifest) as u64 * 3 + 256, Ordering::Relaxed);
                send.send_replace(Some(Ok(snapshot.clone())));
                drop(state);
                drop(branch);
                Ok(snapshot)
            }.await.map_err(Arc::<anyhow::Error>::new);
            match &outcome {
                Ok(_) => operation.finish("success"),
                Err(_) if cancel.is_cancelled() => operation.finish("cancelled"),
                Err(_) => operation.finish("failure"),
            }
            if outcome.is_err() {
                send.send_replace(Some(outcome));
            }
        });
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_keeps_snapshots_pinned_by_tickets_or_queries() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = Arc::new(Snapshot {
            store: Store::open_workspace(&root.path().join("analysis"), [1; 32], root.path(), None)
                .unwrap(),
            manifest: Manifest::default(),
        });
        let ticket = Ticket {
            outcome: Arc::new(watch::channel(Some(Ok(snapshot))).1),
            progress: Arc::new(Progress {
                started: Instant::now(),
                completed: AtomicU64::new(0),
                total: 0,
                retained: AtomicU64::new(1024),
            }),
            telemetry_id: None,
        };
        let jobs = Jobs::default();
        jobs.running.lock().unwrap().insert(
            (root.path().into(), Group::Native),
            Job {
                generation: [0; 32],
                ticket: ticket.clone(),
                cancel: CancellationToken::new(),
                writer: Arc::new(tokio::sync::Mutex::new(())),
                used: Instant::now(),
            },
        );
        jobs.trim_completed();
        assert_eq!(jobs.running.lock().unwrap().len(), 1);
        let snapshot = ticket.ready().unwrap().unwrap();
        drop(ticket);
        jobs.trim_completed();
        assert_eq!(jobs.running.lock().unwrap().len(), 1);
        drop(snapshot);
        jobs.trim_completed();
        assert!(jobs.running.lock().unwrap().is_empty());
    }
}
