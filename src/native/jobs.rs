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
type Outcome = std::result::Result<Arc<Snapshot>, String>;
#[derive(Clone)]
pub struct Ticket {
    outcome: watch::Receiver<Option<Outcome>>,
    progress: Arc<Progress>,
}
impl Ticket {
    pub fn ready(&self) -> Option<Outcome> {
        self.outcome.borrow().clone()
    }
    pub async fn wait(mut self) -> Result<Arc<Snapshot>> {
        loop {
            if let Some(result) = self.ready() {
                return result.map_err(anyhow::Error::msg);
            }
            self.outcome
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
}
struct Job {
    generation: [u8; 32],
    ticket: Ticket,
    cancel: CancellationToken,
    writer: Arc<tokio::sync::Mutex<()>>,
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
        self.running
            .lock()
            .unwrap()
            .retain(|_, job| job.ticket.ready().is_none());
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
        if let Some(job) = jobs.get(&key)
            && job.generation == generation
            && job.ticket.ready().is_none_or(|r| r.is_ok())
        {
            return Ok(job.ticket.clone());
        }
        let writer = if let Some(previous) = jobs.get(&key) {
            previous.cancel.cancel();
            previous.writer.clone()
        } else {
            Arc::new(tokio::sync::Mutex::new(()))
        };
        if jobs.len() >= 16 {
            jobs.retain(|_, job| job.ticket.ready().is_none());
        }
        let (send, outcome) = watch::channel(None);
        let progress = Arc::new(Progress {
            started: Instant::now(),
            completed: AtomicU64::new(0),
            total: manifest
                .deferred
                .values()
                .filter(|f| Group::of(f.language) == Some(group))
                .map(|f| f.stamp.size.max(1))
                .sum(),
        });
        let ticket = Ticket {
            outcome,
            progress: progress.clone(),
        };
        let cancel = self.shutdown.child_token();
        jobs.insert(
            key,
            Job {
                generation,
                ticket: ticket.clone(),
                cancel: cancel.clone(),
                writer: writer.clone(),
            },
        );
        let workers = self.workers.clone();
        #[cfg(test)]
        let pause = self.pauses[group as usize].clone();
        tokio::spawn(async move {
            let _activity = activity;
            let outcome = async {
                let _writer = writer.lock_owned().await;
                #[cfg(test)]
                let _pause = tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("Source indexing superseded"),
                    permit = pause.acquire_owned() => permit?,
                };
                ensure!(!cancel.is_cancelled(), "Source indexing superseded");
                let fingerprint = *blake3::hash(&postcard::to_allocvec(&(
                    "source-group", crate::store::ANALYSIS_VERSION, &entry, group,
                    manifest.discovery_policy,
                ))?).as_bytes();
                let owner = branch.as_ref().map(|b| b.root.as_path());
                let store = Store::open_workspace(&analysis, fingerprint, &entry, owner)?;
                store.begin_refresh()?;
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
                let mut pending = snapshot.files.clone().into_iter();
                let mut tasks = tokio::task::JoinSet::new();
                let mut failure = None;
                loop {
                    while tasks.len() < 4 && !cancel.is_cancelled() {
                        let Some((key, file)) = pending.next() else { break; };
                        let store = store.clone();
                        let manifest = manifest.clone();
                        let workers = workers.clone();
                        let cancel = cancel.clone();
                        tasks.spawn(async move {
                            let permit = tokio::select! {
                                _ = cancel.cancelled() => anyhow::bail!("Source indexing superseded"),
                                permit = workers.acquire_owned() => permit?,
                            };
                            tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                let result = crate::workspace::index_native(
                                    &store, &key, &file, &manifest.projects,
                                );
                                (key, file.stamp.size.max(1), result)
                            }).await.context("Source extraction worker stopped")
                        });
                    }
                    let Some(result) = tasks.join_next().await else { break; };
                    let (key, bytes, result) = match result {
                        Ok(Ok(result)) => result,
                        result => {
                            failure = Some(format!("Source worker failed: {result:?}"));
                            cancel.cancel();
                            continue;
                        }
                    };
                    progress.completed.fetch_add(bytes, Ordering::Relaxed);
                    if let Err(error) = result {
                        let file = snapshot.files.remove(&key).unwrap();
                        snapshot.diagnostics.push(format!(
                            "Skipped {}: {error:#}", file.path.display(),
                        ));
                    }
                }
                if let Some(failure) = failure {
                    anyhow::bail!(failure);
                }
                ensure!(!cancel.is_cancelled(), "Source indexing superseded");
                // A completed superseded worker must never publish over its successor.
                let state = workspace.lock_owned().await;
                let current = state.as_ref().context("Code workspace unavailable")?;
                ensure!(
                    self::generation(&current.manifest, group)? == generation && !cancel.is_cancelled(),
                    "Workspace changed during source indexing; retry query",
                );
                for file in snapshot.files.values() {
                    ensure!(
                        crate::workspace::Stamp::read(&file.path)? == file.stamp,
                        "Source changed during indexing; retry query",
                    );
                }
                store.save_manifest(&snapshot)?;
                for diagnostic in &snapshot.diagnostics {
                    tracing::warn!("{diagnostic}");
                }
                tracing::info!(
                    group = group.name(), files = snapshot.files.len(),
                    elapsed_ms = progress.started.elapsed().as_millis(), "source group ready",
                );
                let snapshot = Arc::new(Snapshot { store, manifest: snapshot });
                send.send_replace(Some(Ok(snapshot.clone())));
                drop(state);
                drop(branch);
                Ok(snapshot)
            }.await.map_err(|e: anyhow::Error| format!("{e:#}"));
            if outcome.is_err() {
                send.send_replace(Some(outcome));
            }
        });
        Ok(ticket)
    }
}
