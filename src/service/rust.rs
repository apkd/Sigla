//! Complete Rust readiness while independent managed discovery continues.
use super::*;
use crate::{
    discovery::rust::{Proof, Stage},
    model::Language,
};
use tokio::sync::watch;

type State = Arc<tokio::sync::Mutex<Option<Workspace>>>;
#[derive(Clone)]
enum Progress {
    Pending,
    Disabled,
    Ready(Arc<Ready>),
}

#[derive(Clone)]
pub(super) struct Ticket(watch::Receiver<Progress>);
#[derive(Clone)]
pub(super) struct Publisher(watch::Sender<Progress>);
pub(super) struct Ready {
    state: State,
    manifest: Arc<crate::workspace::Manifest>,
    generation: u64,
    revision: String,
    proof: Proof,
}

pub(super) struct IndexRequest {
    pub stage: Stage,
    pub entry: PathBuf,
    pub policy: Policy,
    pub branch: Arc<crate::repository::manager::Branch>,
    pub generation: u64,
    pub operation: crate::telemetry::Operation,
    pub work: Arc<crate::demand::Work>,
}

pub(super) fn channel() -> (Publisher, Ticket) {
    let (send, receive) = watch::channel(Progress::Pending);
    (Publisher(send), Ticket(receive))
}
impl Publisher {
    pub fn disable(&self) {
        self.0.send_replace(Progress::Disabled);
    }

    pub async fn index(&self, app: Arc<App>, request: IndexRequest) {
        let IndexRequest {
            stage,
            entry,
            mut policy,
            branch,
            generation,
            operation,
            work,
        } = request;
        let result = async {
            let state = Arc::new(tokio::sync::Mutex::new(None));
            let (permit, mut guard, branch_state) = work
                .until(app.admit_work(
                    state.clone(),
                    Some(&branch),
                    &app.indexing_workers,
                    "rust_indexing_admission",
                    &operation,
                ))
                .await?;
            let applied = branch_state
                .as_ref()
                .and_then(|s| s.as_ref())
                .ok_or_else(|| anyhow::anyhow!("Repository inputs are unavailable"))?;
            ensure!(
                !applied.repair
                    && branch.generation.load(std::sync::atomic::Ordering::Acquire) == generation,
                "Repository changed before Rust indexing"
            );
            let revision = applied.prepared.revision.clone();
            policy.rust_only = true;
            let cache = branch.analysis_cache();
            let owner = branch.root.clone();
            let (manifest, proof) = tokio::task::spawn_blocking(move || -> Result<_> {
                let _permit = permit;
                let _branch_state = branch_state;
                ensure!(!stage.proof.changed(), "Rust completeness proof changed");
                let mut code = Workspace::open(
                    entry,
                    &cache,
                    policy,
                    &app.cache.join("analysis"),
                    Some(&owner),
                    app.monitor.clone(),
                )?;
                let plan = code.plan_discovery(stage.discovery, work.as_ref())?;
                code.apply_plan_cancellable(plan, work.as_ref())?;
                ensure!(
                    !stage.proof.changed(),
                    "Rust completeness proof changed during indexing"
                );
                let manifest = code.manifest.clone();
                *guard = Some(code);
                Ok((manifest, stage.proof))
            })
            .await??;
            Ok::<_, anyhow::Error>(Arc::new(Ready {
                state,
                manifest,
                generation,
                revision,
                proof,
            }))
        }
        .await;
        match result {
            Ok(ready) => {
                self.0.send_replace(Progress::Ready(ready));
            }
            Err(error) => {
                tracing::debug!(error = %format!("{error:#}"), "Using full workspace readiness");
                self.disable();
            }
        }
    }
}
impl Ticket {
    pub async fn ready(&mut self) -> Option<Arc<Ready>> {
        loop {
            match self.0.borrow().clone() {
                Progress::Ready(ready) => return (!ready.proof.changed()).then_some(ready),
                Progress::Disabled => return None,
                Progress::Pending => (),
            }
            if self.0.changed().await.is_err() {
                return None;
            }
        }
    }
    pub fn heap_bytes(&self) -> (u64, u64) {
        let progress = self.0.borrow();
        let Progress::Ready(ready) = &*progress else {
            return (0, 0);
        };
        let bytes = ready
            .state
            .try_lock()
            .ok()
            .and_then(|s| {
                s.as_ref()
                    .map(|code| code.retained_bytes as u64 + code.manifest.derived.heap_bytes())
            })
            .unwrap_or_else(|| crate::memory::encoded_size(ready.manifest.as_ref()) as u64 * 3)
            + ready.proof.retained_bytes();
        (
            bytes,
            if Arc::strong_count(ready) > 1 {
                bytes
            } else {
                0
            },
        )
    }
}

pub(super) fn requested(query: &Query) -> bool {
    !matches!(query.selector.as_str(), "file" | "text")
        && query.target.location.is_none()
        && !query
            .filters
            .iter()
            .any(|f| f.key == "project" && !f.negate)
        && query.file_language_filters(Language::Rust)
        && [
            Language::CSharp,
            Language::Markdown,
            Language::Text,
            Language::C,
            Language::Cpp,
            Language::Header,
            Language::Hlsl,
            Language::Glsl,
            Language::ShaderLab,
        ]
        .into_iter()
        .all(|language| !query.file_language_filters(language))
}

impl App {
    pub(super) async fn search_rust(
        self: &Arc<Self>,
        ready: Arc<Ready>,
        selection: (Arc<crate::repository::manager::Branch>, Option<&str>),
        query: Query,
        activity: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
        operation: crate::telemetry::Operation,
        wait: &indexes::Wait,
    ) -> Result<CallToolResult> {
        let (branch, alias) = selection;
        let Some((permit, state, branch_state)) = wait
            .until(self.admit_query(ready.state.clone(), Some(&branch), &operation))
            .await?
        else {
            return Ok(indexes::pending("Rust query admission"));
        };
        let current = state.as_ref().ok_or(crate::diagnostics::RefreshRequired(
            "Rust workspace was retired",
        ))?;
        ensure!(
            Arc::ptr_eq(&current.manifest, &ready.manifest)
                && !ready.proof.changed()
                && branch.generation.load(std::sync::atomic::Ordering::Acquire) == ready.generation
                && branch_state
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .is_some_and(|s| !s.repair && s.prepared.revision == ready.revision),
            crate::diagnostics::RefreshRequired("Repository changed before Rust search")
        );
        let prepared = &branch_state.as_ref().unwrap().as_ref().unwrap().prepared;
        let summary = crate::summary::Summary::build(
            &branch.repository.identity,
            alias.unwrap_or(&branch.selector(prepared)),
            &ready.revision,
            prepared.tracked.len(),
            &branch.source(),
            &ready.manifest,
        );
        #[cfg(test)]
        {
            self.rust_query_started.notify_one();
            let _pause = self.rust_query_pause.acquire().await?;
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancellation = cancel.clone().drop_guard();
        let result = tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _permit = permit;
            let _branch_state = branch_state;
            let code = state.as_ref().unwrap();
            let store = code.store.clone();
            let manifest = code.manifest.clone();
            let mut search = Search::new(&store, &manifest, &cancel)?;
            drop(state);
            let text = if query.selector == "file" {
                search.files(&query, &branch.source())
            } else {
                search.run(&query)
            }?;
            // A partial workspace must not replace the full owner's dependency inventory.
            branch.record_use();
            operation.complete(true);
            drop(ready);
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            summary.attach(&mut result);
            Ok(result)
        })
        .await?;
        cancellation.disarm();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rust_language_sets_use_independent_readiness() {
        for query in [
            "type:* lang:rust",
            "type:* lang:rs",
            "type:* lang:rust -lang:cs",
        ] {
            assert!(requested(&Query::parse(query).unwrap()), "{query}");
        }
        for query in [
            "type:*",
            "type:* -lang:cs",
            "type:* lang:cs",
            "type:* lang:rust -lang:rust",
            "file:* lang:rust",
            "text:Nested lang:rust",
            "type:* lang:rust project:managed",
            "type:@src/lib.rs:1:1 lang:rust",
        ] {
            assert!(!requested(&Query::parse(query).unwrap()), "{query}");
        }
    }

    async fn mixed_fixture() -> Result<(
        tempfile::TempDir,
        Arc<App>,
        Arc<crate::repository::manager::Branch>,
        PathBuf,
    )> {
        let (cache, app, branch, source) =
            super::super::admission_tests::remote_preparation_fixture().await;
        std::fs::create_dir_all(source.join("managed"))?;
        let files = [
            (
                "managed/managed.csproj",
                "<Project><ItemGroup><Compile Include='Code.cs'/></ItemGroup></Project>",
            ),
            ("managed/global.json", "{}"),
            ("managed/Code.cs", "public class Managed {}"),
            ("src/lib.rs", "mod nested; pub struct Before;"),
            ("src/nested.rs", "pub struct Nested;"),
        ];
        {
            let mut publication = branch.publish().await;
            let prepared = &mut publication.as_mut().unwrap().prepared;
            for (path, text) in files {
                std::fs::write(source.join(path), text)?;
                prepared.tracked.insert(path.into(), path.into());
                prepared.selected.insert(path.into(), path.into());
            }
            prepared.directories.insert("managed".into());
            branch
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        Ok((cache, app, branch, source))
    }

    #[tokio::test]
    async fn rust_finishes_before_managed_discovery_and_rejects_old_generations() -> Result<()> {
        use std::time::Duration;
        let (_cache, app, branch, source) = mixed_fixture().await?;
        let pause = app.managed_pause.clone().acquire_owned().await?;
        let project = "https://github.com/fixture/repo#refs/heads/main";
        let rust_request = {
            let app = app.clone();
            tokio::spawn(async move {
                app.dispatch(
                    project,
                    Request::Search("type:Nested lang:rust wait:complete".into()),
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(10), app.rust_staged.notified()).await?;
        let early = tokio::time::timeout(Duration::from_secs(10), rust_request).await???;
        assert!(result_text(early.clone())?.contains("Nested"));
        let early_summary: crate::summary::Summary =
            serde_json::from_value(early.meta.as_ref().unwrap().0["sigla/repository"].clone())?;
        assert!(early_summary.table.contains("fixture/repo"));
        assert!(
            early_summary
                .table
                .contains("Rust; other languages are still preparing")
        );
        assert!(
            early_summary.table.contains(
                &branch
                    .state
                    .read()
                    .await
                    .as_ref()
                    .unwrap()
                    .prepared
                    .revision[..8]
            )
        );
        let mut ticket = app.workspaces.lock().unwrap()[&source]
            .preparing
            .as_ref()
            .unwrap()
            .clone();
        let ready = ticket.rust.ready().await.unwrap();
        // Setup deadlines stop applying once the query is admitted.
        let query_pause = app.rust_query_pause.clone().acquire_owned().await?;
        // Consume the notification from the first request.
        app.rust_query_started.notified().await;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let admitted = {
            let app = app.clone();
            let ready = ready.clone();
            let branch = branch.clone();
            tokio::spawn(async move {
                app.search_rust(
                    ready,
                    (branch, None),
                    Query::parse("type:Nested lang:rust")?,
                    Arc::new(app.activity.clone().read_owned().await),
                    crate::telemetry::Operation::default(),
                    &indexes::Wait(Some(deadline)),
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), app.rust_query_started.notified()).await?;
        tokio::time::sleep_until(deadline + Duration::from_millis(10)).await;
        drop(query_pause);
        let admitted = admitted.await??;
        assert!(
            admitted
                .structured_content
                .as_ref()
                .and_then(|v| v.get("complete"))
                != Some(&serde_json::Value::Bool(false))
        );
        assert!(result_text(admitted)?.contains("Nested"));
        let managed = app.search(project, "type:Managed lang:cs wait:complete");
        tokio::pin!(managed);
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut managed)
                .await
                .is_err()
        );
        drop(pause);
        let managed = tokio::time::timeout(Duration::from_secs(10), managed).await??;
        let full_state = app.workspaces.lock().unwrap()[&source].state.clone();
        let full_manifest = full_state.lock().await.as_ref().unwrap().manifest.clone();
        assert!(
            managed.contains("Managed"),
            "{managed}; diagnostics={:?}; files={:?}",
            full_manifest.diagnostics,
            full_manifest
                .files
                .values()
                .map(|f| &f.path)
                .collect::<Vec<_>>()
        );
        ticket.complete().await?;
        let full = app
            .dispatch(
                project,
                Request::Search("type:Nested lang:rust wait:complete".into()),
            )
            .await?;
        let full_summary: crate::summary::Summary =
            serde_json::from_value(full.meta.as_ref().unwrap().0["sigla/repository"].clone())?;
        assert_ne!(early_summary.key, full_summary.key);
        let query_text = |result: &CallToolResult| {
            result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .filter(|t| {
                    t.meta.as_ref().is_none_or(|m| {
                        m.0.get("sigla/repository-summary") != Some(&serde_json::Value::Bool(true))
                    })
                })
                .map(|t| t.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(query_text(&early), query_text(&full));
        {
            let mut publication = branch.publish().await;
            publication.as_mut().unwrap().prepared.revision = "b".repeat(40);
            branch
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        let error = app
            .search_rust(
                ready,
                (branch, None),
                Query::parse("type:Nested lang:rust")?,
                Arc::new(app.activity.clone().read_owned().await),
                crate::telemetry::Operation::default(),
                &indexes::Wait(None),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::diagnostics::RefreshRequired>()
                .is_some()
        );
        app.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn queued_rust_work_retries_after_publication_and_joins_before_shutdown() -> Result<()> {
        use std::time::Duration;
        let (_cache, app, branch, source) = mixed_fixture().await?;
        let capacity = app.indexing_workers.available_permits();
        let worker = app
            .indexing_workers
            .clone()
            .acquire_many_owned(capacity as u32)
            .await?;
        let managed = app.managed_pause.clone().acquire_owned().await?;
        let request = {
            let app = app.clone();
            tokio::spawn(async move {
                app.search(
                    "https://github.com/fixture/repo#refs/heads/main",
                    "type:After lang:rust wait:complete",
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(10), app.rust_staged.notified()).await?;
        tokio::time::timeout(Duration::from_secs(10), app.admission_started.notified()).await?;
        let publication = branch.publish();
        tokio::pin!(publication);
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut publication)
                .await
                .is_err()
        );
        drop(managed);
        let mut publication = tokio::time::timeout(Duration::from_secs(10), publication).await?;
        std::fs::write(source.join("src/nested.rs"), "pub struct After;")?;
        let prepared = &mut publication.as_mut().unwrap().prepared;
        prepared
            .selected
            .insert("src/nested.rs".into(), "changed".into());
        prepared
            .tracked
            .insert("src/nested.rs".into(), "changed".into());
        branch
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        drop(publication);
        drop(worker);
        let result = tokio::time::timeout(Duration::from_secs(10), request).await???;
        assert!(result.contains("After"), "{result}");
        tokio::time::timeout(Duration::from_secs(10), app.shutdown()).await?;
        assert_eq!(app.indexing_workers.available_permits(), capacity);
        Ok(())
    }
}
