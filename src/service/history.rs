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
    ) -> Result<CallToolResult> {
        crate::history::validate(query)?;
        let repository = crate::repository::Repository::project(path, self.remote.is_some())?;
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
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel = Cancel(cancel.clone());
        let permit = self.workers.clone().acquire_owned().await?;
        let activity = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.activity.clone().read_owned(),
        )
        .await
        .map_err(|_| crate::diagnostics::MaintenanceBusy)?;
        let app = self.clone();
        let query = query.to_owned();
        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            let (_permit, _activity) = (permit, activity);
            crate::repository::history::check(&cancel)?;
            let (store, selector) = if let Some(repository) = repository {
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
            let owner = store.owner();
            if let Some(owner) = &owner {
                app.owners
                    .lock()
                    .unwrap()
                    .record_history(owner, crate::cache::now());
            }
            let result = crate::history::run(store, selector.as_deref(), &query);
            if let Some(owner) = &owner {
                let mut owners = app.owners.lock().unwrap();
                owners.record_history(owner, crate::cache::now());
                owners.flush()?;
            }
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
