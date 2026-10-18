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
type Outcome = std::result::Result<Arc<Index>, Arc<anyhow::Error>>;
pub type Ticket = watch::Receiver<Option<Outcome>>;
pub fn empty() -> Ticket {
    watch::channel(Some(Ok(Arc::new(Index::default())))).1
}
pub struct Request {
    pub activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub branch: Option<Arc<Branch>>,
    pub root: PathBuf,
    pub cache: PathBuf,
    pub records: std::sync::Arc<crate::cache::artifacts::Store>,
    pub expected: [u8; 32],
    pub revision: Option<String>,
    pub refresh: bool,
}
struct Job {
    generation: [u8; 32],
    ticket: Ticket,
    cancel: tokio_util::sync::CancellationToken,
    used: std::time::Instant,
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
    Ok(*blake3::hash(&postcard::to_allocvec(&(
        env!("SIGLA_ASSET_FINGERPRINT"),
        manifest,
    ))?)
    .as_bytes())
}
impl Jobs {
    pub fn trim_completed(&self) {
        self.trim_to(0);
    }
    pub fn trim_to(&self, limit: usize) {
        let mut jobs = self.running.lock().unwrap();
        let mut completed: Vec<_> = jobs
            .iter()
            .filter_map(|(key, job)| {
                job.ticket.borrow().as_ref().map(|result| {
                    (
                        key.clone(),
                        job.used,
                        result
                            .as_ref()
                            .map_or(256, |index| index.retained_bytes + 256),
                        result
                            .as_ref()
                            .map_or(true, |index| Arc::strong_count(index) == 1),
                    )
                })
            })
            .collect();
        completed.sort_by_key(|(_, used, _, _)| *used);
        let mut bytes: usize = completed.iter().map(|(_, _, size, _)| size).sum();
        for (key, _, size, idle) in completed {
            if bytes <= limit {
                break;
            }
            if !idle {
                continue;
            }
            tracing::debug!(workspace = %key.display(), retained_bytes = size, total_bytes = bytes, budget_bytes = limit, "Evicting Unity asset index");
            jobs.remove(&key);
            bytes -= size;
        }
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
            activity,
            workspace,
            branch,
            root,
            cache,
            records,
            expected,
            revision,
            refresh,
        } = request;
        let generation = *blake3::hash(
            &postcard::to_allocvec(&(expected, &revision)).expect("serializable asset identity"),
        )
        .as_bytes();
        let mut jobs = self.running.lock().unwrap();
        let (same_generation, previous) = if let Some(job) = jobs.get_mut(&root) {
            let outcome = job.ticket.borrow();
            if job.generation == generation
                && (outcome.is_none() || outcome.as_ref().is_some_and(|r| r.is_ok() && !refresh))
            {
                job.used = std::time::Instant::now();
                return job.ticket.clone();
            }
            let previous = outcome.as_ref().and_then(|r| r.as_ref().ok()).cloned();
            job.cancel.cancel();
            (job.generation == generation, previous)
        } else {
            (false, None)
        };
        let (sender, ticket) = watch::channel(None);
        let cancel = self.shutdown.child_token();
        jobs.insert(
            root.clone(),
            Job {
                generation,
                ticket: ticket.clone(),
                cancel: cancel.clone(),
                used: std::time::Instant::now(),
            },
        );
        let workers = self.workers.clone();
        tokio::spawn(async move {
            let log_root = root.clone();
            let outcome = async {
                let permit = tokio::select! {
                    _ = cancel.cancelled() => anyhow::bail!("Asset indexing cancelled or superseded"),
                    permit = workers.acquire_owned() => permit?,
                };
                let state = workspace.lock_owned().await;
                let branch_state = match &branch {
                    Some(branch) => Some(branch.state.clone().read_owned().await),
                    None => None,
                };
                tokio::task::spawn_blocking(move || -> Result<Arc<Index>> {
                    let _activity = activity;
                    let _permit = permit;
                    let code = state.as_ref().context("Code workspace unavailable")?;
                    ensure!(
                        self::generation(&code.manifest)? == expected,
                        crate::diagnostics::RefreshRequired(
                            "Workspace changed before asset indexing; retry query"
                        )
                    );
                    ensure!(
                        branch_state
                            .as_ref()
                            .and_then(|s| s.as_ref())
                            .map(|s| &s.prepared.revision)
                            == revision.as_ref(),
                        crate::diagnostics::RefreshRequired(
                            "Repository changed before asset indexing; retry query"
                        )
                    );
                    let store = code.store.snapshot()?;
                    let manifest = code.manifest.clone();
                    let omitted = branch_state
                        .as_ref()
                        .and_then(|s| s.as_ref())
                        .map(|s| s.prepared.omitted.clone())
                        .unwrap_or_default();
                    drop(branch_state);
                    drop(state);
                    let started = std::time::Instant::now();
                    let (index, built) = if let Some(previous) = previous.as_ref().filter(|index| same_generation && branch.is_none() && index.current()) {
                        (previous.clone(), false)
                    } else if let Some(index) = super::snapshot::load(&cache, generation) {
                        (Arc::new(index), false)
                    } else {
                        (Arc::new(super::build(
                            &root,
                            &cache,
                            &manifest,
                            &store,
                            super::BuildOptions { previous: previous.as_deref(), records: Some(&records), remote: branch.is_some(), omitted: &omitted, cancel: &cancel },
                        )?), true)
                    };
                    if let Some(branch) = &branch {
                        let state = branch
                            .state
                            .try_read()
                            .map_err(|_| crate::diagnostics::RefreshRequired(
                                "Repository is refreshing; retry asset query"
                            ))?;
                        ensure!(
                            state.as_ref().map(|s| &s.prepared.revision) == revision.as_ref(),
                            crate::diagnostics::RefreshRequired(
                                "Repository changed during asset indexing; retry query"
                            )
                        );
                    }
                    ensure!(!cancel.is_cancelled(), "Asset indexing superseded");
                    if built {
                        super::snapshot::save(&cache, generation, &index)?;
                        tracing::info!(workspace = %root.display(), files = index.assets.len(), elapsed_ms = started.elapsed().as_millis(), "Unity asset index ready");
                    }
                    tracing::debug!(workspace = %root.display(), retained_bytes = index.retained_bytes, built, elapsed_ms = started.elapsed().as_millis(), "Unity asset index acquired");
                    Ok(index)
                })
                .await?
            }
            .await
            .map_err(Arc::new);
            if let Err(error) = &outcome {
                tracing::warn!(workspace = %log_root.display(), error = %format_args!("{error:#}"), "Unity asset index failed");
            }
            sender.send_replace(Some(outcome));
        });
        ticket
    }
}
pub async fn wait(mut ticket: Ticket) -> Result<Arc<Index>> {
    loop {
        if let Some(result) = ticket.borrow().clone() {
            return result.map_err(crate::diagnostics::shared_error);
        }
        ticket
            .changed()
            .await
            .context("Asset indexing worker stopped")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn asset_wait_preserves_refresh_classification_and_can_read_a_ready_result() {
        let failure = anyhow::Error::new(crate::diagnostics::RefreshRequired(
            "Repository is refreshing; retry asset query",
        ))
        .context("asset publication");
        let (sender, ticket) = watch::channel(Some(Err(Arc::new(failure))));
        let error = wait(ticket.clone()).await.err().expect("refresh must fail");
        let details = crate::diagnostics::details(&error, "asset-refresh");
        assert_eq!(details["error_code"], "UNAVAILABLE");
        assert_eq!(details["retryable"], true);

        sender.send_replace(Some(Ok(Arc::new(Index::default()))));
        assert!(wait(ticket).await.is_ok());
    }
}
