use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sigla::{
    discovery::Policy,
    service::{App, Mcp},
};
use std::sync::Arc;

#[tokio::test]
async fn http_contract_and_origin_validation() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='http_fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "pub struct FoundOverHttp;").unwrap();
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    let service = StreamableHttpService::new(
        move || Ok(Mcp::new(app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .enforce_origin_validation()
            .with_json_response(true),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let initialize = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"sigla-test","version":"1"}}});
    let response = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let session = response.headers().get("mcp-session-id").cloned();
    let _ = body(response).await;
    let mut initialized = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream");
    if let Some(session) = &session {
        initialized = initialized.header("mcp-session-id", session);
    }
    initialized
        .json(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    let mut request = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream");
    if let Some(session) = &session {
        request = request.header("mcp-session-id", session);
    }
    let response = request
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    let listed = body(response).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(tool["name"], "search");
    assert_eq!(
        tool["inputSchema"]["properties"].as_object().unwrap().len(),
        2
    );
    assert!(tool.get("outputSchema").is_none());
    let mut request = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream");
    if let Some(session) = &session {
        request = request.header("mcp-session-id", session);
    }
    let response=request.json(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"project_path":root.path(),"query":"FoundOverHttp limit:1"}}})).send().await.unwrap();
    let called = body(response).await;
    let result = &called["result"];
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("FoundOverHttp")
    );
    assert!(result.get("structuredContent").is_none());
    let rendered = result["content"][0]["text"].as_str().unwrap();
    assert!(rendered.starts_with("# `"), "{rendered}");
    assert!(rendered.contains("\n```rust\n"), "{rendered}");
    let denied = client
        .post(&url)
        .header("Origin", "https://untrusted.invalid")
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    server.abort();
}
async fn body(response: reqwest::Response) -> serde_json::Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|_| {
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .find_map(|l| serde_json::from_str(l).ok())
            .unwrap_or_else(|| panic!("Unexpected HTTP response: {text}"))
    })
}

#[tokio::test]
async fn executable_reads_outside_cwd_unless_roots_are_restricted() {
    let cwd = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("Game.csproj"),
        r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("Code.cs"),
        "class OutsideWorkingDirectory {}",
    )
    .unwrap();
    for restricted in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sigla"));
        command
            .current_dir(cwd.path())
            .arg("--cache")
            .arg(cache.path());
        if restricted {
            command.arg("--root").arg(cwd.path());
        }
        let output = command
            .arg("query")
            .arg(project.path())
            .arg("type:OutsideWorkingDirectory")
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.success(),
            !restricted,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !restricted {
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("class OutsideWorkingDirectory")
            );
        }
    }
}

#[tokio::test]
async fn executable_enforces_bearer_authentication() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let token = "sigla-test-token-do-not-use-in-production";
    let token_file = root.path().join("token");
    std::fs::write(&token_file, token).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_sigla"))
        .arg("--root")
        .arg(root.path())
        .arg("--cache")
        .arg(cache.path())
        .args(["serve", "--listen", &address.to_string(), "--token-file"])
        .arg(token_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let denied = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(response) = client.post(&url).send().await {
                break response;
            }
            assert!(
                process.try_wait().unwrap().is_none(),
                "Server exited before accepting requests"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
    let initialized=client.post(&url).bearer_auth(token).header("Accept","application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"auth-test","version":"1"}}}))
        .send().await.unwrap();
    assert!(initialized.status().is_success());
    process.kill().await.unwrap();
    process.wait().await.unwrap();
}
