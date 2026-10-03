use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    transport::{
        StreamableHttpClientTransport,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use sigla::{
    discovery::Policy,
    service::{App, Mcp},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct Probe {
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
}
impl ServerHandler for Probe {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.as_ref().unwrap();
        if args.get("query").is_some_and(|q| q == "slow") {
            // Keep preparation alive beyond the upstream session's idle lifetime.
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                context
                    .peer
                    .send_request(rmcp::model::ServerRequest::PingRequest(Default::default()))
                    .await
                    .unwrap();
            }
        }
        if args.get("query").is_some_and(|q| q == "wait") {
            context
                .peer
                .send_request(rmcp::model::ServerRequest::PingRequest(Default::default()))
                .await
                .unwrap();
            self.started.notify_one();
            context.ct.cancelled().await;
            self.cancelled.notify_one();
        }
        // Multiple content blocks, structured data, metadata and error status must all survive.
        Ok(serde_json::from_value::<CallToolResult>(serde_json::json!({
            "content":[{"type":"text","text":"first"},{"type":"text","text":"second"}],
            "structuredContent":{"tool":request.name,"arguments":request.arguments},
            "isError":args.get("query").is_some_and(|q| q == "fail"),
            "_meta":{"source":"probe"}
        }))
        .unwrap()
        .into())
    }
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
    available: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SummaryProbe;

impl ServerHandler for SummaryProbe {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn call_tool(
        &self,
        _: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(serde_json::from_value::<CallToolResult>(serde_json::json!({
            "content":[
                {"type":"text","text":"summary", "_meta":{"sigla/repository-summary":true}},
                {"type":"text","text":"body"}
            ],
            "_meta":{"sigla/repository":{"key":"owner/repo#main","table":"summary"}}
        }))
        .unwrap()
        .into())
    }
}

#[tokio::test]
async fn repository_summaries_follow_downstream_sessions_and_survive_upstream_reconnects() {
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream = serve_factory(
        {
            let connections = connections.clone();
            move || {
                connections.fetch_add(1, Ordering::SeqCst);
                SummaryProbe
            }
        },
        None,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let app = Arc::new(
        App::hybrid(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
            &upstream.url,
            None,
        )
        .unwrap(),
    );
    let hybrid = serve_factory(
        {
            let app = app.clone();
            move || Mcp::new(app.clone())
        },
        None,
    )
    .await;
    let first =
        ().serve(StreamableHttpClientTransport::from_uri(hybrid.url.clone()))
            .await
            .unwrap();
    let second =
        ().serve(StreamableHttpClientTransport::from_uri(hybrid.url.clone()))
            .await
            .unwrap();
    let request = || {
        CallToolRequestParams::new("search").with_arguments(
            serde_json::json!({"codebase":"owner/repo", "query":"type:X"})
                .as_object()
                .unwrap()
                .clone(),
        )
    };
    for client in [&first, &second] {
        assert_eq!(client.call_tool(request()).await.unwrap().content.len(), 2);
        let result = client.call_tool(request()).await.unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].as_text().unwrap().text, "body");
    }
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    upstream.available.store(false, Ordering::SeqCst);
    assert_eq!(
        first.call_tool(request()).await.unwrap().is_error,
        Some(true)
    );
    upstream.available.store(true, Ordering::SeqCst);
    let result = first.call_tool(request()).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 1);
    assert_eq!(connections.load(Ordering::SeqCst), 3);
    // Modern HTTP requests create a fresh Mcp per call, but share idle timers.
    let http = reqwest::Client::new();
    for expected in [2, 1] {
        let response: serde_json::Value = http
            .post(&hybrid.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "search")
            .json(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"search", "arguments":{"codebase":"owner/repo", "query":"type:X"},
                    "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities":{}}}
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            response["result"]["content"].as_array().map(Vec::len),
            Some(expected),
            "{response}"
        );
    }
    first.cancel().await.unwrap();
    second.cancel().await.unwrap();
    app.shutdown().await;
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve<S: ServerHandler + Clone + Send + Sync + 'static>(
    handler: S,
    token: Option<String>,
) -> Server {
    serve_factory(move || handler.clone(), token).await
}

async fn serve_factory<S: ServerHandler + Send + Sync + 'static>(
    factory: impl Fn() -> S + Send + Sync + 'static,
    token: Option<String>,
) -> Server {
    use axum::response::IntoResponse;
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(Duration::from_millis(500));
    let service = StreamableHttpService::new(
        move || Ok(factory()),
        Arc::new(sessions),
        StreamableHttpServerConfig::default().with_json_response(true),
    );
    let available = Arc::new(AtomicBool::new(true));
    let gate = available.clone();
    let router =
        axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let token = token.clone();
                    let gate = gate.clone();
                    async move {
                        if !gate.load(Ordering::SeqCst) {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        if token.is_some_and(|t| {
                            request
                                .headers()
                                .get("authorization")
                                .and_then(|h| h.to_str().ok())
                                != Some(format!("Bearer {t}").as_str())
                        }) {
                            return axum::http::StatusCode::UNAUTHORIZED.into_response();
                        }
                        next.run(request).await
                    }
                },
            ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server {
        url,
        task,
        available,
    }
}

#[tokio::test]
async fn forwards_all_tools_and_preserves_results_and_cancellation() {
    let probe = Probe::default();
    let token = "hybrid-fixture-token";
    let upstream = serve(probe.clone(), Some(token.into())).await;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let token_path = root.path().join("token");
    std::fs::write(&token_path, token).unwrap();
    let app = Arc::new(
        App::hybrid(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
            &upstream.url,
            Some(&token_path),
        )
        .unwrap(),
    );
    let hybrid = serve(Mcp::new(app.clone()), None).await;
    let client =
        ().serve(StreamableHttpClientTransport::from_uri(hybrid.url.clone()))
            .await
            .unwrap();
    for (name, arguments) in [
        (
            "search",
            serde_json::json!({"codebase":"owner/repo#feature/test","query":"future:syntax"}),
        ),
        (
            "search",
            serde_json::json!({"codebase":"https://github.com/owner/repo","query":"fail"}),
        ),
        (
            "search",
            serde_json::json!({"codebase":"https://github.com/owner/repo#refs%2Ftags%2Frelease%2520literal","query":"type:Example"}),
        ),
        (
            "browse",
            serde_json::json!({"codebase":"git@github.com:owner/repo.git","path":"src"}),
        ),
        (
            "view",
            serde_json::json!({"codebase":"ssh://git@github.com/owner/repo.git","path":"src/File.cs:20-50","mode":"exact"}),
        ),
    ] {
        let result = client
            .call_tool(
                CallToolRequestParams::new(name)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        let mut expected = arguments.clone();
        if expected["codebase"] == "owner/repo#feature/test" {
            expected["codebase"] = "https://github.com/owner/repo#feature/test".into();
        }
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(
            value["structuredContent"],
            serde_json::json!({"tool":name,"arguments":expected})
        );
        assert_eq!(
            value["content"],
            serde_json::json!([{"type":"text","text":"first"},{"type":"text","text":"second"}])
        );
        assert_eq!(value["_meta"], serde_json::json!({"source":"probe"}));
        assert_eq!(
            value["isError"],
            arguments.get("query").is_some_and(|q| q == "fail")
        );
    }
    let waiting = {
        let app = app.clone();
        tokio::spawn(async move { app.search("owner/repo", "wait").await })
    };
    tokio::time::timeout(Duration::from_secs(5), probe.started.notified())
        .await
        .unwrap();
    // A pending upstream call must not serialize other calls behind it.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), app.browse("owner/repo", ""))
            .await
            .unwrap()
            .is_ok()
    );
    waiting.abort();
    let _ = waiting.await;
    tokio::time::timeout(Duration::from_secs(5), probe.cancelled.notified())
        .await
        .unwrap();
    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("search").with_arguments(
                    serde_json::json!({"codebase":"owner/repo","query":"wait"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), probe.started.notified())
        .await
        .unwrap();
    handle.cancel(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), probe.cancelled.notified())
        .await
        .unwrap();
    assert!(!cache.path().join("repositories").exists());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), app.search("owner/repo", "slow"))
            .await
            .unwrap()
            .is_ok()
    );
    client.cancel().await.unwrap();
    app.shutdown().await;
}

#[tokio::test]
async fn local_access_is_independent_and_upstream_recovers() {
    let upstream = serve(Probe::default(), None).await;
    upstream.available.store(false, Ordering::SeqCst);
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='hybrid_fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "pub struct LocalSymbol;").unwrap();
    let app = Arc::new(
        App::hybrid(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
            &upstream.url,
            None,
        )
        .unwrap(),
    );
    assert!(app.search("owner/repo", "LocalSymbol").await.is_err());
    assert!(
        app.search(root.path().to_str().unwrap(), "LocalSymbol")
            .await
            .unwrap()
            .contains("LocalSymbol")
    );
    assert!(app.browse("/", "").await.is_err());
    assert!(app.browse("./missing", "").await.is_err());
    upstream.available.store(true, Ordering::SeqCst);
    assert!(app.search("owner/repo", "anything").await.is_ok());
    upstream.available.store(false, Ordering::SeqCst);
    assert!(app.search("owner/repo", "anything").await.is_err());
    upstream.available.store(true, Ordering::SeqCst);
    assert!(app.search("owner/repo", "anything").await.is_ok());
    app.shutdown().await;
}

#[tokio::test]
async fn authentication_failure_does_not_expose_token() {
    let upstream = serve(Probe::default(), Some("expected-token".into())).await;
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    std::fs::write(&path, "private-wrong-token").unwrap();
    let app = Arc::new(
        App::hybrid(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
            &upstream.url,
            Some(&path),
        )
        .unwrap(),
    );
    let error = app
        .search("owner/repo", "anything")
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-wrong-token"));
    app.shutdown().await;
}

#[tokio::test]
async fn cli_prefers_existing_local_shorthand_path() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let project = root.path().join("owner/repo");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='local_precedence'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(project.join("src/lib.rs"), "pub struct LocalPrecedence;").unwrap();
    let upstream = serve(Probe::default(), None).await;
    upstream.available.store(false, Ordering::SeqCst);
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sigla"))
        .current_dir(root.path())
        .args([
            "--mode",
            "hybrid",
            "--upstream",
            &upstream.url,
            "--cache-dir",
        ])
        .arg(cache.path())
        .args(["query", "owner/repo", "LocalPrecedence"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("LocalPrecedence"));
}
