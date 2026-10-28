//! History leaves dispatch before workspace/snapshot preparation. Blocking work
//! retains both permits until it really finishes, including after cancellation.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl App {
    pub(super) async fn history_request(
        self: &Arc<Self>,
        path: &str,
        query: &str,
        operation: crate::telemetry::Operation,
    ) -> Result<CallToolResult> {
        operation.stage("query_parse");
        crate::history::validate(query)?;
        let repository = crate::repository::Repository::project(path, self.remote.is_some())?;
        if let Some(repository) = &repository {
            operation.repository(&repository.identity.to_string());
            if let Some(selector) = &repository.selector {
                operation.target(selector);
            }
        }
        let rules = if repository.is_some() {
            Some(
                self.remote
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Repository identifiers require --mode remote"))?
                    .options
                    .rules
                    .clone(),
            )
        } else {
            None
        };
        let local = if repository.is_none() {
            // Existing names containing '#' are literal paths, as in other tools.
            let (entry, selector) = if Path::new(path).exists() {
                (path, None)
            } else {
                path.rsplit_once('#')
                    .map_or((path, None), |(p, r)| (p, Some(r.to_owned())))
            };
            if let Some(selector) = &selector
                && selector != "HEAD"
            {
                crate::repository::materialize::Target::selector(selector)?;
            }
            let entry = self.policy.canonical(Path::new(entry))?;
            let directory = if entry.is_file() {
                entry.parent().unwrap().to_owned()
            } else {
                entry
            };
            Some((directory, selector))
        } else {
            None
        };
        if let Some((directory, selector)) = &local {
            operation.repository(&directory.display().to_string());
            if let Some(selector) = selector {
                operation.target(selector);
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel = Cancel(cancel.clone());
        operation.stage("history_admission");
        let history_permit = {
            let _wait = operation.wait("history_admission");
            self.history_workers.clone().acquire_owned().await?
        };
        tracing::Span::current().record("stage", "query_admission");
        operation.stage("query_admission");
        let permit = {
            let _wait = operation.wait("query_admission");
            self.workers.clone().acquire_owned().await?
        };
        tracing::Span::current().record("stage", "maintenance");
        operation.stage("maintenance_admission");
        let activity = {
            let _wait = operation.wait("maintenance_admission");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.activity.clone().read_owned(),
            )
            .await
            .map_err(|_| crate::diagnostics::MaintenanceBusy)?
        };
        let app = self.clone();
        let query = query.to_owned();
        tracing::Span::current().record("stage", "evaluation");
        operation.stage("evaluation");
        let blocking_operation =
            crate::telemetry::operation("history_work", Some(path), Some(&query));
        if let Some(id) = blocking_operation.id() {
            operation.link(&id);
        }
        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            let (_permit, _history_permit, _activity) = (permit, history_permit, activity);
            blocking_operation.stage("evaluation");
            #[cfg(test)]
            {
                app.history_started.notify_one();
                drop(futures_executor::block_on(app.history_pause.acquire()).unwrap());
            }
            let result = (|| -> Result<String> {
                crate::repository::history::check(&cancel)?;
                let (mut store, selector) = if let Some(repository) = repository {
                    let selector = repository.selector.clone();
                    (
                        crate::repository::history::Store::remote(
                            repository,
                            rules.as_ref().unwrap(),
                            &app.cache,
                            cancel,
                        )?,
                        selector,
                    )
                } else {
                    let (path, selector) = local.unwrap();
                    (
                        crate::repository::history::Store::local(&path, cancel)?,
                        selector,
                    )
                };
                store.set_operation(blocking_operation.clone());
                let owner = store.owner();
                if let Some(owner) = &owner {
                    app.owners
                        .lock()
                        .unwrap()
                        .record_history(owner, crate::cache::now());
                }
                let result = crate::history::run_cached(
                    store,
                    selector.as_deref(),
                    &query,
                    app.history_cache.clone(),
                );
                app.trim_idle();
                if let Some(owner) = &owner {
                    let mut owners = app.owners.lock().unwrap();
                    owners.record_history(owner, crate::cache::now());
                    owners.flush()?;
                }
                result
            })();
            let diagnostic = result
                .as_ref()
                .err()
                .and_then(crate::diagnostics::monitoring);
            if let Some(diagnostic) = diagnostic {
                blocking_operation.diagnose(diagnostic);
            }
            blocking_operation.finish(if result.is_ok() {
                "success"
            } else if diagnostic == Some(crate::telemetry::Diagnostic::RequestCancelled) {
                "cancelled"
            } else {
                "error"
            });
            result
        })
        .await??;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn history_waits_for_worker_and_cancel_does_not_start_preparation() {
        let directory = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![directory.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        let permit = app.workers.clone().acquire_owned().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let path = directory.path().to_str().unwrap();
        let request = app.request_cancellable(path, Request::History("log".into()), &cancel, None);
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut request)
                .await
                .is_err()
        );
        cancel.cancel();
        assert!(request.await.unwrap_err().to_string().contains("cancelled"));
        assert!(app.startup.lock().unwrap().is_none());
        assert!(app.workspaces.lock().unwrap().is_empty());
        drop(permit);
        // Cancellation did not leak the permit or start a hidden Git/workspace job.
        let _permit = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.workers.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!cache.path().join("analysis").exists());
    }
}
