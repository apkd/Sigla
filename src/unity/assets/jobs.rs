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
#[derive(Clone)]
pub struct Ticket {
    outcome: Arc<watch::Receiver<Option<Outcome>>>,
    telemetry_id: Option<String>,
}
impl Ticket {
    pub fn telemetry_id(&self) -> Option<String> {
        self.telemetry_id.clone()
    }
    pub fn ready_index(&self) -> Option<Arc<Index>> {
        self.ready().and_then(Result::ok)
    }

    pub fn ready(&self) -> Option<Outcome> {
        self.outcome.borrow().clone()
    }

    fn borrow(&self) -> watch::Ref<'_, Option<Outcome>> {
        self.outcome.borrow()
    }
}
pub fn empty() -> Ticket {
    Ticket {
        outcome: Arc::new(watch::channel(Some(Ok(Arc::new(Index::default())))).1),
        telemetry_id: None,
    }
}
pub struct Request {
    pub activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    pub workspace: Arc<tokio::sync::Mutex<Option<Workspace>>>,
    pub branch: Option<Arc<Branch>>,
    pub remote: Option<Arc<crate::repository::manager::Manager>>,
    pub root: PathBuf,
    pub cache: PathBuf,
    pub records: std::sync::Arc<crate::cache::artifacts::Store>,
    pub expected: [u8; 32],
    pub revision: Option<String>,
    pub refresh: bool,
    pub leases: crate::cache::views::LeaseSet,
}
struct Completed {
    request: Request,
    index: Arc<Index>,
    snapshot: Option<super::snapshot::Pending>,
    started: std::time::Instant,
    origin: &'static str,
    _permit: tokio::sync::OwnedSemaphorePermit,
    _dependencies: crate::cache::views::LeaseSet,
}
struct Job {
    generation: [u8; 32],
    ticket: Ticket,
    cancel: tokio_util::sync::CancellationToken,
    used: std::time::Instant,
    retry_after: std::time::Instant,
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
                        Arc::strong_count(&job.ticket.outcome) == 1
                            && result
                                .as_ref()
                                .map_or(true, |index| Arc::strong_count(index) == 1),
                    )
                })
            })
            .collect();
        completed.sort_by_key(|(_, used, _, _)| *used);
        let mut bytes: usize = completed.iter().map(|(_, _, size, _)| size).sum();
        let mut retired = Vec::new();
        for (key, _, size, idle) in completed {
            if bytes <= limit {
                break;
            }
            if !idle {
                continue;
            }
            tracing::debug!(workspace = %key.display(), retained_bytes = size, total_bytes = bytes, budget_bytes = limit, "Evicting Unity asset index");
            retired.push(jobs.remove(&key).unwrap());
            bytes -= size;
        }
        // Large graphs can take time to free. Let unrelated roots start first.
        drop(jobs);
        let count = retired.len();
        let started = std::time::Instant::now();
        drop(retired);
        if count > 0 {
            tracing::debug!(
                count,
                elapsed_ms = started.elapsed().as_millis(),
                "Unity asset indexes retired"
            );
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
        let generation = *blake3::hash(
            &postcard::to_allocvec(&(request.expected, &request.revision))
                .expect("serializable asset identity"),
        )
        .as_bytes();
        let mut jobs = self.running.lock().unwrap();
        let (same_generation, previous) = if let Some(job) = jobs.get_mut(&request.root) {
            let outcome = job.ticket.borrow();
            let reuse = match outcome.as_ref() {
                None => true,
                Some(Ok(_)) => !request.refresh,
                Some(Err(error)) => {
                    request.branch.is_some()
                        && std::time::Instant::now() < job.retry_after
                        && !error
                            .chain()
                            .any(|cause| cause.is::<crate::diagnostics::RefreshRequired>())
                }
            };
            if job.generation == generation && reuse {
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
        let repository = request
            .branch
            .as_ref()
            .map(|branch| branch.repository.identity.to_string())
            .or_else(|| Some(request.root.display().to_string()));
        let operation = crate::telemetry::operation(
            "unity_asset_index",
            repository.as_deref(),
            request.revision.as_deref(),
        );
        if let Some(branch) = &request.branch {
            operation.target(&branch.name);
        }
        let ticket = Ticket {
            outcome: Arc::new(ticket),
            telemetry_id: operation.id(),
        };
        let cancel = self.shutdown.child_token();
        jobs.insert(
            request.root.clone(),
            Job {
                generation,
                ticket: ticket.clone(),
                cancel: cancel.clone(),
                used: std::time::Instant::now(),
                // Failed remote acquisition must not restart on every code query.
                retry_after: std::time::Instant::now() + std::time::Duration::from_secs(30),
            },
        );
        let workers = self.workers.clone();
        tokio::spawn(async move {
            let log_root = request.root.clone();
            let outcome = async {
                operation.stage("worker_admission");
                let _wait = operation.wait("worker_admission");
                let permit = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => anyhow::bail!("Asset indexing cancelled or superseded"),
                    permit = workers.acquire_owned() => permit?,
                };
                drop(_wait);
                if let (Some(remote), Some(branch)) = (&request.remote, &request.branch) {
                    operation.stage("asset_acquisition");
                    let _wait = operation.wait("asset_acquisition");
                    // Retain the request, leases and worker until blocking acquisition exits.
                    remote.complete_assets(branch, request.revision.as_deref().unwrap(), &operation).await?;
                }
                operation.stage("asset_capture");
                let _wait = operation.wait("asset_capture");
                let (state, branch_state) = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => anyhow::bail!("Asset indexing cancelled or superseded"),
                    guards = async {
                        let state = request.workspace.clone().lock_owned().await;
                        let branch_state = match &request.branch {
                            Some(branch) => Some(branch.state.clone().read_owned().await),
                            None => None,
                        };
                        (state, branch_state)
                    } => guards,
                };
                drop(_wait);
                let building_cancel = cancel.clone();
                operation.stage("asset_indexing");
                let completed = tokio::task::spawn_blocking(move || -> Result<Completed> {
                    let code = state.as_ref().context("Code workspace unavailable")?;
                    ensure!(
                        self::generation(&code.manifest)? == request.expected,
                        crate::diagnostics::RefreshRequired(
                            "Workspace changed before asset indexing; retry query"
                        )
                    );
                    ensure!(
                        branch_state.as_ref().and_then(|s| s.as_ref())
                            .is_none_or(|state| !state.repair),
                        crate::diagnostics::RefreshRequired(
                            "Repository materialization requires repair; retry asset query"
                        )
                    );
                    ensure!(
                        branch_state
                            .as_ref()
                            .and_then(|s| s.as_ref())
                            .map(|s| &s.prepared.revision)
                            == request.revision.as_ref(),
                        crate::diagnostics::RefreshRequired(
                            "Repository changed before asset indexing; retry query"
                        )
                    );
                    let store = code.store.snapshot()?;
                    let manifest = code.manifest.clone();
                    let dependencies = code.leases();
                    let omitted = branch_state
                        .as_ref()
                        .and_then(|s| s.as_ref())
                        .map(|s| s.prepared.omitted.clone())
                        .unwrap_or_default();
                    drop(branch_state);
                    drop(state);
                    let started = std::time::Instant::now();
                    let retained = if same_generation && request.branch.is_none() {
                        if let Some(index) = &previous {
                            let checked = std::time::Instant::now();
                            let current = index.current(&building_cancel)?;
                            tracing::debug!(
                                workspace = %request.root.display(),
                                observed = index.observed.len(),
                                current,
                                elapsed_ms = checked.elapsed().as_millis(),
                                "Unity retained asset validation"
                            );
                            current.then(|| index.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let (index, built, origin) = if let Some(previous) = retained {
                        (previous, false, "retained")
                    } else if let Some(index) = super::snapshot::load(&request.cache, generation, &building_cancel)? {
                        (Arc::new(index), false, "snapshot")
                    } else {
                        (Arc::new(super::build(
                            &request.root,
                            &request.cache,
                            &manifest,
                            &store,
                            super::BuildOptions { previous: previous.as_deref(), records: Some(&request.records), remote: request.branch.is_some(), omitted: &omitted, cancel: &building_cancel },
                        )?), true, "built")
                    };
                    ensure!(!building_cancel.is_cancelled(), "Asset indexing superseded");
                    let snapshot = built
                        .then(|| super::snapshot::stage(&request.cache, generation, &index))
                        .transpose()?;
                    Ok(Completed { request, index, snapshot, started, origin, _permit: permit, _dependencies: dependencies })
                })
                .await??;
                publish(completed, &cancel, sender.clone(), operation.clone()).await
            }
            .await
            .map_err(Arc::new);
            operation.finish(if outcome.is_ok() {
                "success"
            } else if cancel.is_cancelled() {
                "cancelled"
            } else {
                "failure"
            });
            if let Err(error) = outcome {
                tracing::warn!(workspace = %log_root.display(), error = %format_args!("{error:#}"), "Unity asset index failed");
                sender.send_replace(Some(Err(error)));
            }
        });
        ticket
    }
}

async fn publish(
    completed: Completed,
    cancel: &tokio_util::sync::CancellationToken,
    sender: watch::Sender<Option<Outcome>>,
    operation: crate::telemetry::Operation,
) -> Result<()> {
    // Use the same workspace -> state order as the initial capture. The request
    // retains activity, dependency leases and the branch handle through publication.
    operation.stage("publication");
    let _wait = operation.wait("asset_publication");
    let (state, branch_state) = tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("Asset indexing cancelled or superseded"),
        guards = async {
            let state = completed.request.workspace.clone().lock_owned().await;
            let branch_state = match &completed.request.branch {
                Some(branch) => Some(branch.state.clone().read_owned().await),
                None => None,
            };
            (state, branch_state)
        } => guards,
    };
    drop(_wait);
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        // Move every lifetime guard into blocking work, even if its waiter drops.
        let completed = completed;
        let code = state.as_ref().context("Code workspace unavailable")?;
        ensure!(
            generation(&code.manifest)? == completed.request.expected,
            crate::diagnostics::RefreshRequired(
                "Workspace changed during asset indexing; retry query"
            )
        );
        ensure!(
            branch_state.as_ref().and_then(|s| s.as_ref())
                .is_none_or(|state| !state.repair),
            crate::diagnostics::RefreshRequired(
                "Repository materialization requires repair; retry asset query"
            )
        );
        ensure!(
            branch_state.as_ref().and_then(|s| s.as_ref())
                .map(|s| &s.prepared.revision) == completed.request.revision.as_ref(),
            crate::diagnostics::RefreshRequired(
                "Repository changed during asset indexing; retry query"
            )
        );
        ensure!(!cancel.is_cancelled(), "Asset indexing superseded");
        let built = completed.snapshot.is_some();
        if let Some(snapshot) = completed.snapshot {
            snapshot.publish()?;
            tracing::info!(workspace = %completed.request.root.display(), files = completed.index.assets.len(), elapsed_ms = completed.started.elapsed().as_millis(), "Unity asset index ready");
        }
        tracing::debug!(
            workspace = %completed.request.root.display(),
            retained_bytes = completed.index.retained_bytes,
            built,
            origin = completed.origin,
            elapsed_ms = completed.started.elapsed().as_millis(),
            "Unity asset index acquired"
        );
        sender.send_replace(Some(Ok(completed.index)));
        // Both guards cover the snapshot activation and the watcher notification.
        drop(branch_state);
        drop(state);
        Ok(())
    }).await?
}
pub async fn wait(ticket: Ticket) -> Result<Arc<Index>> {
    let mut outcome = ticket.outcome.as_ref().clone();
    loop {
        if let Some(result) = outcome.borrow().clone() {
            return result.map_err(crate::diagnostics::shared_error);
        }
        outcome
            .changed()
            .await
            .context("Asset indexing worker stopped")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_keeps_indexes_pinned_by_tickets_or_queries() {
        let jobs = Jobs::default();
        let ticket = empty();
        jobs.running.lock().unwrap().insert(
            "workspace".into(),
            Job {
                generation: [0; 32],
                ticket: ticket.clone(),
                cancel: tokio_util::sync::CancellationToken::new(),
                used: std::time::Instant::now(),
                retry_after: std::time::Instant::now(),
            },
        );
        jobs.trim_completed();
        assert_eq!(jobs.running.lock().unwrap().len(), 1);
        let index = ticket.ready_index().unwrap();
        drop(ticket);
        jobs.trim_completed();
        assert_eq!(jobs.running.lock().unwrap().len(), 1);
        drop(index);
        jobs.trim_completed();
        assert!(jobs.running.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_waiters_observe_the_same_publication() {
        use std::{future::Future, task::Poll};
        let (sender, outcome) = watch::channel(None);
        let ticket = Ticket {
            outcome: Arc::new(outcome),
            telemetry_id: None,
        };
        let first = wait(ticket.clone());
        let second = wait(ticket);
        tokio::pin!(first, second);
        std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let index = Arc::new(Index::default());
        sender.send_replace(Some(Ok(index.clone())));
        assert!(Arc::ptr_eq(&first.await.unwrap(), &index));
        assert!(Arc::ptr_eq(&second.await.unwrap(), &index));
    }

    #[tokio::test]
    async fn failed_remote_jobs_share_the_failure_until_retry_is_due() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let jobs = Jobs::default();
        jobs.workers.close();
        let fixture = completed(root.path(), cache.path()).await;
        let first = jobs.start(fixture.request);
        assert!(wait(first.clone()).await.is_err());
        let fixture = completed(root.path(), cache.path()).await;
        let shared = jobs.start(fixture.request);
        assert!(Arc::ptr_eq(&first.outcome, &shared.outcome));
        jobs.running
            .lock()
            .unwrap()
            .get_mut(root.path())
            .unwrap()
            .retry_after = std::time::Instant::now();
        let fixture = completed(root.path(), cache.path()).await;
        let retried = jobs.start(fixture.request);
        assert!(!Arc::ptr_eq(&first.outcome, &retried.outcome));
        assert!(wait(retried).await.is_err());
    }

    async fn completed(root: &std::path::Path, cache: &std::path::Path) -> Completed {
        let workspace = Workspace::open(
            root.into(),
            cache,
            crate::discovery::Policy::new(vec![root.into()]).unwrap(),
            &cache.join("analysis"),
            None,
            Arc::new(crate::watch::Monitor::default()),
        )
        .unwrap();
        let expected = generation(&workspace.manifest).unwrap();
        let dependencies = workspace.leases();
        let branch = Arc::new(Branch::for_test(root.into()));
        *branch.state.write().await = Some(crate::repository::manager::State {
            schema: 1,
            repository: branch.repository.identity.clone(),
            transport: branch.repository.transport.clone(),
            branch: branch.name.clone(),
            target: Some(branch.target.clone()),
            last_use: 0,
            refreshed: 0,
            policy: String::new(),
            prepared: crate::repository::materialize::Prepared {
                deferred_assets: Default::default(),
                revision: "initial".into(),
                branch: None,
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                selected: Default::default(),
                tracked: Default::default(),
                directories: Default::default(),
            },
            indexed_revision: None,
            repair: false,
            additional: Default::default(),
        });
        let assets = cache.join("unity-assets/test");
        let leases = crate::cache::views::LeaseSet::new(cache).unwrap();
        leases.retain(&assets).unwrap();
        let index = Arc::new(Index::default());
        let snapshot = Some(super::super::snapshot::stage(&assets, expected, &index).unwrap());
        Completed {
            request: Request {
                activity: Arc::new(Arc::new(tokio::sync::RwLock::new(())).read_owned().await),
                workspace: Arc::new(tokio::sync::Mutex::new(Some(workspace))),
                branch: Some(branch),
                remote: None,
                root: root.into(),
                cache: assets,
                records: Arc::new(crate::cache::artifacts::Store::open(cache).unwrap()),
                expected,
                revision: Some("initial".into()),
                refresh: false,
                leases,
            },
            index,
            snapshot,
            started: std::time::Instant::now(),
            origin: "built",
            _permit: Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap(),
            _dependencies: dependencies,
        }
    }

    #[tokio::test]
    async fn publication_waits_for_refresh_and_validates_before_activating_snapshot() {
        use std::{future::Future, task::Poll};
        for (changed, repair) in [(false, false), (true, false), (false, true)] {
            let root = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let completed = completed(root.path(), cache.path()).await;
            let assets = completed.request.cache.clone();
            let generation = completed.request.expected;
            let branch = completed.request.branch.clone().unwrap();
            let mut refreshing = branch.state.write().await;
            let cancel = tokio_util::sync::CancellationToken::new();
            let (sender, ticket) = watch::channel(None);
            let ticket = Ticket {
                outcome: Arc::new(ticket),
                telemetry_id: None,
            };
            let publication = publish(
                completed,
                &cancel,
                sender,
                crate::telemetry::Operation::default(),
            );
            tokio::pin!(publication);
            std::future::poll_fn(|cx| {
                assert!(publication.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            assert!(ticket.borrow().is_none());
            assert!(
                super::super::snapshot::load(&assets, generation, &())
                    .unwrap()
                    .is_none()
            );
            if changed {
                refreshing.as_mut().unwrap().prepared.revision = "changed".into();
            }
            refreshing.as_mut().unwrap().repair = repair;
            drop(refreshing);
            let result = publication.await;
            if changed || repair {
                let error = result.unwrap_err();
                assert!(
                    error
                        .chain()
                        .any(|cause| cause.is::<crate::diagnostics::RefreshRequired>())
                );
                assert!(ticket.borrow().is_none());
                assert!(
                    super::super::snapshot::load(&assets, generation, &())
                        .unwrap()
                        .is_none()
                );
            } else {
                result.unwrap();
                assert!(wait(ticket).await.is_ok());
                assert!(
                    super::super::snapshot::load(&assets, generation, &())
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[tokio::test]
    async fn asset_capture_rejects_repository_repair() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let completed = completed(root.path(), cache.path()).await;
        completed
            .request
            .branch
            .as_ref()
            .unwrap()
            .state
            .write()
            .await
            .as_mut()
            .unwrap()
            .repair = true;
        let jobs = Jobs::default();
        let error = wait(jobs.start(completed.request)).await.err().unwrap();
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<crate::diagnostics::RefreshRequired>())
        );
    }

    #[tokio::test]
    async fn asset_build_pins_dependencies_after_workspace_retirement() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let completed = completed(root.path(), cache.path()).await;
        let dependency = cache.path().join("unity-packages/dependency");
        std::fs::create_dir_all(&dependency).unwrap();
        let workspace = completed.request.workspace.clone();
        workspace
            .lock()
            .await
            .as_ref()
            .unwrap()
            .leases()
            .retain(&dependency)
            .unwrap();
        let registry = crate::cache::views::Registry::open(cache.path()).unwrap();
        drop(workspace.lock().await.take());
        assert!(
            registry
                .try_retire(&dependency, || false)
                .unwrap()
                .is_none()
        );
        drop(completed);
        assert!(
            registry
                .try_retire(&dependency, || false)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn superseded_publication_releases_locks_without_waiting_for_refresh() {
        use std::{future::Future, task::Poll};
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let completed = completed(root.path(), cache.path()).await;
        let workspace = completed.request.workspace.clone();
        let branch = completed.request.branch.clone().unwrap();
        let assets = completed.request.cache.clone();
        let generation = completed.request.expected;
        let _refreshing = branch.state.write().await;
        let cancel = tokio_util::sync::CancellationToken::new();
        let (sender, ticket) = watch::channel(None);
        let ticket = Ticket {
            outcome: Arc::new(ticket),
            telemetry_id: None,
        };
        let publication = publish(
            completed,
            &cancel,
            sender,
            crate::telemetry::Operation::default(),
        );
        tokio::pin!(publication);
        std::future::poll_fn(|cx| {
            assert!(publication.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(workspace.try_lock().is_err());
        cancel.cancel();
        assert!(publication.await.is_err());
        assert!(workspace.try_lock().is_ok());
        assert!(ticket.borrow().is_none());
        assert!(
            super::super::snapshot::load(&assets, generation, &())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn asset_wait_preserves_refresh_classification_and_can_read_a_ready_result() {
        let failure = anyhow::Error::new(crate::diagnostics::RefreshRequired(
            "Repository is refreshing; retry asset query",
        ))
        .context("asset publication");
        let (sender, ticket) = watch::channel(Some(Err(Arc::new(failure))));
        let ticket = Ticket {
            outcome: Arc::new(ticket),
            telemetry_id: None,
        };
        let error = wait(ticket.clone()).await.err().expect("refresh must fail");
        let details = crate::diagnostics::details(&error, "asset-refresh");
        assert_eq!(details["error_code"], "UNAVAILABLE");
        assert_eq!(details["retryable"], true);

        sender.send_replace(Some(Ok(Arc::new(Index::default()))));
        assert!(wait(ticket).await.is_ok());
    }
}
