//! Asset work has its own queue and never holds the code workspace during parsing.
use super::Index;
use crate::{
    repository::manager::Branch,
    workspace::{Manifest, Workspace},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{Semaphore, watch};
type Outcome = std::result::Result<Arc<Index>, String>;
pub type Ticket = watch::Receiver<Option<Outcome>>;
pub fn empty() -> Ticket {
    watch::channel(Some(Ok(Arc::new(Index::default())))).1
}
pub struct Request {
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub branch: Option<Arc<Branch>>,
    pub root: PathBuf,
    pub cache: PathBuf,
    pub expected: [u8; 32],
    pub revision: Option<String>,
    pub refresh: bool,
}
struct Job {
    generation: [u8; 32],
    ticket: Ticket,
    cancel: tokio_util::sync::CancellationToken,
}
pub struct Jobs {
    running: Mutex<HashMap<PathBuf, Job>>,
    workers: Arc<Semaphore>,
    shutdown: tokio_util::sync::CancellationToken,
}
impl Default for Jobs {
    fn default() -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            workers: Arc::new(Semaphore::new(1)),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }
}
pub fn generation(manifest: &Manifest) -> Result<[u8; 32]> {
    Ok(*blake3::hash(&postcard::to_allocvec(manifest)?).as_bytes())
}
impl Jobs {
    pub fn trim_completed(&self) {
        self.running
            .lock()
            .unwrap()
            .retain(|_, job| job.ticket.borrow().is_none());
    }
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
    #[cfg(test)]
    pub async fn pause(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.workers.clone().acquire_owned().await.unwrap()
    }
    pub fn start(&self, request: Request) -> Ticket {
        let Request {
            workspace,
            branch,
            root,
            cache,
            expected,
            revision,
            refresh,
        } = request;
        let generation = *blake3::hash(
            &postcard::to_allocvec(&(expected, &revision)).expect("serializable asset identity"),
        )
        .as_bytes();
        let mut jobs = self.running.lock().unwrap();
        let mut previous = None;
        if let Some(job) = jobs.get(&root) {
            let outcome = job.ticket.borrow();
            if job.generation == generation
                && (outcome.is_none() || !refresh && outcome.as_ref().is_some_and(|r| r.is_ok()))
            {
                return job.ticket.clone();
            }
            if job.generation == generation {
                previous = outcome.as_ref().and_then(|r| r.as_ref().ok()).cloned();
            }
            job.cancel.cancel();
        }
        if jobs.len() >= 8 {
            jobs.retain(|_, job| job.ticket.borrow().is_none());
        }
        let (sender, ticket) = watch::channel(None);
        let cancel = self.shutdown.child_token();
        jobs.insert(
            root.clone(),
            Job {
                generation,
                ticket: ticket.clone(),
                cancel: cancel.clone(),
            },
        );
        let workers = self.workers.clone();
        tokio::spawn(async move {
            let outcome = async {
                let permit = tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("Asset indexing cancelled or superseded"),
                    permit = workers.acquire_owned() => permit?,
                };
                let state = workspace.lock_owned().await;
                let branch_state = match &branch {
                    Some(branch) => Some(branch.state.clone().lock_owned().await),
                    None => None,
                };
                tokio::task::spawn_blocking(move || -> Result<Arc<Index>> {
                    let _permit = permit;
                    let code = state.as_ref().context("Code workspace unavailable")?;
                    ensure!(
                        self::generation(&code.manifest)? == expected,
                        "Workspace changed before asset indexing; retry query"
                    );
                    ensure!(
                        branch_state
                            .as_ref()
                            .and_then(|s| s.as_ref())
                            .map(|s| &s.prepared.revision)
                            == revision.as_ref(),
                        "Repository changed before asset indexing; retry query"
                    );
                    let store = code.store.clone();
                    let manifest = code.manifest.clone();
                    let tx = store.query_read()?;
                    let omitted = branch_state
                        .as_ref()
                        .and_then(|s| s.as_ref())
                        .map(|s| s.prepared.omitted.clone())
                        .unwrap_or_default();
                    drop(branch_state);
                    drop(state);
                    if let Some(previous) = previous.filter(|index| branch.is_none() && index.current()) {
                        return Ok(previous);
                    }
                    let index = super::build(
                        &root,
                        &cache,
                        &manifest,
                        &store,
                        &tx,
                        super::BuildOptions { remote: branch.is_some(), omitted: &omitted, cancel: &cancel },
                    )?;
                    if let Some(branch) = &branch {
                        let state = branch
                            .state
                            .try_lock()
                            .context("Repository is refreshing; retry asset query")?;
                        ensure!(
                            state.as_ref().map(|s| &s.prepared.revision) == revision.as_ref(),
                            "Repository changed during asset indexing; retry query"
                        );
                    }
                    Ok(Arc::new(index))
                })
                .await?
            }
            .await
            .map_err(|error| format!("{error:#}"));
            sender.send_replace(Some(outcome));
        });
        ticket
    }
}
pub async fn wait(mut ticket: Ticket) -> Result<Arc<Index>> {
    loop {
        if let Some(result) = ticket.borrow().clone() {
            return result.map_err(anyhow::Error::msg);
        }
        ticket
            .changed()
            .await
            .context("Asset indexing worker stopped")?;
    }
}
