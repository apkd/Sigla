//! External preparation waits have bounded capacity separate from runnable work.
use super::*;
use tokio::sync::OwnedSemaphorePermit;

pub(super) async fn wait<T>(
    app: &App,
    lifecycle: &mut Option<OwnedSemaphorePermit>,
    work: &crate::demand::Work,
    operation: &crate::telemetry::Operation,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    // Never queue for a waiting slot while holding runnable capacity: resuming
    // jobs retain their waiting slot until runnable admission succeeds.
    let waiting = app.preparation_waiters.clone().try_acquire_owned().ok();
    if waiting.is_some() {
        lifecycle.take();
    }
    // The future owns and joins every blocking child, even after cancellation.
    let result = future.await?;
    if waiting.is_some() {
        let _wait = operation.wait("preparation_resume");
        *lifecycle = Some(
            work.until(async { Ok(app.preparation_lifecycle.clone().acquire_owned().await?) })
                .await?,
        );
    }
    Ok(result)
}

pub(super) struct Prefetch {
    pub root: PathBuf,
    pub policy: Policy,
    pub leases: crate::cache::views::LeaseSet,
}

pub(super) async fn prefetch(
    app: &App,
    branch: &Arc<crate::repository::manager::Branch>,
    revision: &str,
    request: Prefetch,
    work: Arc<crate::demand::Work>,
    operation: &crate::telemetry::Operation,
) -> Result<()> {
    let remote = request.policy.remote.as_ref().unwrap();
    let manager = app.remote.as_ref().unwrap();
    let hydrate = |paths| {
        manager.require_inputs_cancellable(branch, revision, paths, operation, work.clone())
    };
    let assets = remote.missing_analysis_inputs([request.root.join("Assets").as_path()])?;
    let (selected, packages) = tokio::sync::oneshot::channel();
    let child_work = work.clone();
    let root = request.root.clone();
    let policy = request.policy.clone();
    let prefetch = tokio::task::spawn_blocking(move || {
        child_work.check()?;
        let operation = crate::telemetry::operation(
            "unity_dependency_acquisition",
            Some(&root.to_string_lossy()),
            None,
        );
        let result =
            crate::unity::prefetch_dependencies(&root, &policy, &request.leases, |roots| {
                let _ = selected.send(roots);
            });
        operation.finish(if result.is_ok() { "success" } else { "failure" });
        child_work.check()?;
        result
    });
    let hydration = async {
        if !assets.is_empty() {
            hydrate(assets).await?;
        }
        // Local scopes are known before official package archives are acquired.
        if let Ok(roots) = packages.await {
            let paths = remote.missing_analysis_inputs(roots.iter().map(PathBuf::as_path))?;
            if !paths.is_empty() {
                hydrate(paths).await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let (prepared, hydrated) = tokio::join!(prefetch, hydration);
    hydrated?;
    if let Err(error) = prepared? {
        if error.is::<crate::demand::Paused>() || crate::diagnostics::cancelled(&error) {
            return Err(error);
        }
        if let Some(required) = error.downcast_ref::<crate::discovery::RequiredInputs>() {
            hydrate(required.0.clone()).await?;
        } else {
            // Normal discovery owns diagnostics and dependency retry policy.
            tracing::debug!(%error, "Unity dependency preflight incomplete");
        }
    }
    work.check()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, task::Poll, time::Duration};

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Arc<App>) {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let app = App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap();
        (root, cache, Arc::new(app))
    }

    async fn pending<T>(future: &mut std::pin::Pin<Box<impl Future<Output = T>>>) {
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn dependency_wait_allows_unrelated_rust_preparation() {
        let (root, _cache, app) = fixture();
        std::fs::write(root.path().join("code.rs"), "pub struct Available;").unwrap();
        let demand = Arc::new(crate::demand::Demand::default());
        let _interest = demand.interest();
        let work = demand.work();
        let operation = crate::telemetry::Operation::default();
        let mut lifecycle = Some(
            app.preparation_lifecycle
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
        let (finish, receive) = tokio::sync::oneshot::channel::<()>();
        let mut waiting = Box::pin(wait(&app, &mut lifecycle, &work, &operation, async {
            receive.await?;
            Ok(())
        }));
        pending(&mut waiting).await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            app.search(
                root.path().to_str().unwrap(),
                "type:Available wait:complete",
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.contains("Available"));
        finish.send(()).unwrap();
        waiting.await.unwrap();
        drop(lifecycle);
        app.shutdown().await;
    }

    #[tokio::test]
    async fn full_wait_capacity_keeps_runnable_slot_without_blocking_resumption() {
        let (_root, _cache, app) = fixture();
        let demand = Arc::new(crate::demand::Demand::default());
        let _interest = demand.interest();
        let work = demand.work();
        let operation = crate::telemetry::Operation::default();
        let mut first_slot = Some(
            app.preparation_lifecycle
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
        let (first_send, first_receive) = tokio::sync::oneshot::channel::<()>();
        let mut first = Box::pin(wait(&app, &mut first_slot, &work, &operation, async {
            first_receive.await?;
            Ok(())
        }));
        pending(&mut first).await;
        let mut second_slot = Some(
            app.preparation_lifecycle
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
        let (second_send, second_receive) = tokio::sync::oneshot::channel::<()>();
        let mut second = Box::pin(wait(&app, &mut second_slot, &work, &operation, async {
            second_receive.await?;
            Ok(())
        }));
        pending(&mut second).await;
        first_send.send(()).unwrap();
        pending(&mut first).await;
        assert!(app.preparation_waiters.clone().try_acquire_owned().is_err());
        assert!(
            app.preparation_lifecycle
                .clone()
                .try_acquire_owned()
                .is_err()
        );
        second_send.send(()).unwrap();
        second.await.unwrap();
        assert!(second_slot.is_some());
        drop(second_slot);
        first.await.unwrap();
        assert!(first_slot.is_some());
        assert!(app.preparation_waiters.clone().try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn cancellation_retains_wait_capacity_until_blocking_child_exits() {
        let (_root, _cache, app) = fixture();
        let demand = Arc::new(crate::demand::Demand::new(Duration::ZERO));
        let interest = demand.interest();
        let work = demand.work();
        let operation = crate::telemetry::Operation::default();
        let mut lifecycle = Some(
            app.preparation_lifecycle
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        );
        let (finish, receive) = std::sync::mpsc::channel();
        let mut waiting = Box::pin(wait(&app, &mut lifecycle, &work, &operation, async {
            tokio::task::spawn_blocking(move || receive.recv()).await??;
            work.check()
        }));
        pending(&mut waiting).await;
        drop(interest);
        pending(&mut waiting).await;
        assert!(app.preparation_waiters.clone().try_acquire_owned().is_err());
        finish.send(()).unwrap();
        assert!(waiting.await.unwrap_err().is::<crate::demand::Paused>());
        assert!(app.preparation_waiters.clone().try_acquire_owned().is_ok());
        assert!(lifecycle.is_none());
    }
}
