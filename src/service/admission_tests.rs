use super::*;
use std::{future::Future, task::Poll};

fn fixture(workers: usize) -> (tempfile::TempDir, tempfile::TempDir, App) {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let app = App::new(
        Policy::new(vec![root.path().into()]).unwrap(),
        cache.path().into(),
        workers,
    )
    .unwrap();
    (root, cache, app)
}

#[tokio::test]
async fn waiting_for_a_worker_releases_workspace_and_branch_locks() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let request = app.admit_query(workspace.clone(), Some(&branch), &operation);
    tokio::pin!(request);

    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    let workspace_guard = workspace
        .try_lock()
        .expect("workspace must remain available");
    let publication = tokio::time::timeout(std::time::Duration::from_secs(2), branch.publish())
        .await
        .expect("branch must remain available");
    drop(publication);
    drop(held);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(workspace_guard);

    let (permit, workspace_guard, analysis) = request.await.unwrap();
    assert!(analysis.is_some());
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn waiting_for_workspace_or_branch_does_not_reserve_a_worker() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let operation = crate::telemetry::Operation::default();

    let held = workspace.lock().await;
    let mut request = Box::pin(app.admit_query(workspace.clone(), Some(&branch), &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(held);
    drop(request);

    let publication = branch.publish().await;
    let request = app.admit_query(workspace.clone(), Some(&branch), &operation);
    tokio::pin!(request);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(publication);
    let (permit, workspace_guard, analysis) = request.await.unwrap();
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn branch_publication_after_worker_wait_returns_the_worker() {
    let (root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let branch = Arc::new(crate::repository::manager::Branch::for_test(
        root.path().into(),
    ));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut request = Box::pin(app.admit_query(workspace.clone(), Some(&branch), &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());

    let publication = branch.publish().await;
    drop(held);
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    assert_eq!(app.workers.available_permits(), 1);
    drop(publication);
    let (permit, guard, analysis) = request.await.unwrap();
    drop((permit, guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn cancelling_admission_releases_queue_and_locks() {
    let (_root, _cache, app) = fixture(1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut request = Box::pin(app.admit_query(workspace.clone(), None, &operation));
    let status = std::future::poll_fn(|cx| Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(status.is_pending());
    drop(request);
    assert!(workspace.try_lock().is_ok());
    drop(held);
    assert_eq!(app.workers.available_permits(), 1);
    let (permit, workspace_guard, analysis) =
        app.admit_query(workspace, None, &operation).await.unwrap();
    drop((permit, workspace_guard, analysis));
    assert_eq!(app.workers.available_permits(), 1);
}

#[tokio::test]
async fn query_rechecks_workspace_generation_after_admission_wait() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(1);
    std::fs::write(root.path().join("lib.rs"), "pub struct Available;").unwrap();
    let app = Arc::new(app);
    let path = root.path().to_str().unwrap();
    assert!(
        app.search(path, "type:Available")
            .await
            .unwrap()
            .contains("Available")
    );
    let workspace = app
        .workspaces
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .state
        .clone();
    let held = app.workers.clone().acquire_owned().await.unwrap();
    let request = app.request(
        path,
        Request::Search("type:Available".into()),
        crate::telemetry::Operation::default(),
    );
    tokio::pin!(request);
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = app.admission_started.notified() => {},
            result = &mut request => panic!("query finished before admission wait: {result:?}"),
        }
    })
    .await
    .unwrap();
    {
        let mut state = workspace.lock().await;
        let manifest = &mut state.as_mut().unwrap().manifest;
        Arc::make_mut(manifest).environment[0] ^= 1;
    }
    drop(held);
    let error = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<crate::diagnostics::RefreshRequired>())
    );
    app.shutdown().await;
}

#[tokio::test]
async fn queued_history_leaves_a_worker_for_interactive_queries() {
    let (root, _cache, app) = fixture(2);
    let app = Arc::new(app);
    let held = app.history_workers.clone().acquire_owned().await.unwrap();
    let operation = crate::telemetry::Operation::default();
    let mut history =
        Box::pin(app.history_request(root.path().to_str().unwrap(), "log", operation.clone()));
    let status = std::future::poll_fn(|cx| Poll::Ready(history.as_mut().poll(cx))).await;
    assert!(status.is_pending());

    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let (permit, guard, branch) = app.admit_query(workspace, None, &operation).await.unwrap();
    assert_eq!(app.workers.available_permits(), 1);
    drop((permit, guard, branch));
    drop(history);
    drop(held);
    assert_eq!(app.workers.available_permits(), 2);
}

#[tokio::test]
async fn cancelled_running_history_keeps_its_permits_until_work_exits() {
    use std::time::Duration;

    let (root, _cache, app) = fixture(2);
    let app = Arc::new(app);
    let pause = app.history_pause.clone().acquire_owned().await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let request = {
        let app = app.clone();
        let cancel = cancel.clone();
        let path = root.path().to_string_lossy().into_owned();
        tokio::spawn(async move {
            app.request_cancellable(&path, Request::History("log".into()), &cancel, None)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), app.history_started.notified())
        .await
        .unwrap();
    assert_eq!(app.history_workers.available_permits(), 0);
    assert_eq!(app.workers.available_permits(), 1);

    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(app.history_workers.available_permits(), 0);
    assert_eq!(app.workers.available_permits(), 1);
    let workspace = Arc::new(tokio::sync::Mutex::new(None));
    let operation = crate::telemetry::Operation::default();
    let (permit, guard, analysis) = app.admit_query(workspace, None, &operation).await.unwrap();
    drop((permit, guard, analysis));

    drop(pause);
    let history = tokio::time::timeout(
        Duration::from_secs(5),
        app.history_workers.clone().acquire_owned(),
    )
    .await
    .unwrap()
    .unwrap();
    let workers = tokio::time::timeout(
        Duration::from_secs(5),
        app.workers.clone().acquire_many_owned(2),
    )
    .await
    .unwrap()
    .unwrap();
    drop((history, workers));
    app.shutdown().await;
}
