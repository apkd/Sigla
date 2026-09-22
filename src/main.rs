use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sigla::{
    discovery::Policy,
    service::{App, Mcp},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(name = "sigla", version, about = "C# and Rust source navigation")]
struct Cli {
    #[command(flatten)]
    options: sigla::config::Options,
    /// maximum concurrent indexing/search jobs across all workspaces.
    #[arg(long,global=true,default_value_t=2,value_parser=clap::value_parser!(u16).range(1..=16))]
    workers: u16,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(name = "__git-job", hide = true)]
    GitJob { input: PathBuf, output: PathBuf },
    /// run the shared HTTP MCP service at /mcp.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7331")]
        listen: SocketAddr,
        /// read a bearer token from this private file; required off loopback.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// additional accepted Host header values (host or host:port).
        #[arg(long)]
        allowed_host: Vec<String>,
        /// accepted browser origins for a reverse-proxy deployment.
        #[arg(long)]
        allowed_origin: Vec<String>,
    },
    /// execute the same search locally, for development and diagnostics.
    Query { project: String, query: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("sigla=info")
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Command::GitJob { input, output } = &cli.command {
        return sigla::repository::job::worker(input, output);
    }
    let remote = cli.options.validate()?;
    let mut policy = Policy::new(cli.options.local_roots())?;
    policy.unity_platform = cli.options.unity_platform;
    let app = Arc::new(match remote {
        Some(remote) => App::remote(policy, cli.options.cache_dir, cli.workers as usize, remote)?,
        None => App::new(policy, cli.options.cache_dir, cli.workers as usize)?,
    });
    match cli.command {
        Command::GitJob { .. } => unreachable!(),
        Command::Query { project, query } => {
            let result = app.search(&project, &query).await;
            sigla::shutdown();
            println!("{}", result?);
        }
        Command::Serve {
            listen,
            token_file,
            allowed_host,
            allowed_origin,
        } => {
            ensure!(
                listen.ip().is_loopback() || token_file.is_some(),
                "Non-loopback binding requires --token-file and TLS termination"
            );
            let token = token_file
                .map(std::fs::read_to_string)
                .transpose()?
                .map(|s| s.trim().to_owned());
            ensure!(
                token.as_ref().is_none_or(|t| t.len() >= 24),
                "Bearer token must contain at least 24 characters"
            );
            let ct = tokio_util::sync::CancellationToken::new();
            let mut config = StreamableHttpServerConfig::default()
                .enforce_origin_validation()
                .with_json_response(true)
                .with_max_request_body_bytes(64 * 1024)
                .with_cancellation_token(ct.clone());
            if !allowed_host.is_empty() {
                config = config.with_allowed_hosts(allowed_host);
            }
            if !allowed_origin.is_empty() {
                config = config.with_allowed_origins(allowed_origin);
            }
            let mcp_app = app.clone();
            let service = StreamableHttpService::new(
                move || Ok(Mcp::new(mcp_app.clone())),
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let router =
                axum::Router::new()
                    .nest_service("/mcp", service)
                    .layer(axum::middleware::from_fn(
                        move |request: axum::extract::Request, next: axum::middleware::Next| {
                            let token = token.clone();
                            async move {
                                if let Some(token) = token {
                                    let expected = format!("Bearer {token}");
                                    let actual = request
                                        .headers()
                                        .get(axum::http::header::AUTHORIZATION)
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or("");
                                    let a = blake3::hash(actual.as_bytes());
                                    let b = blake3::hash(expected.as_bytes());
                                    if a != b {
                                        return axum::http::StatusCode::UNAUTHORIZED
                                            .into_response();
                                    }
                                }
                                next.run(request).await
                            }
                        },
                    ));
            let listener = tokio::net::TcpListener::bind(listen).await?;
            let listen = listener.local_addr()?;
            tracing::info!(%listen,"Sigla listening at /mcp");
            app.start_setup();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    ct.cancel();
                    sigla::shutdown();
                })
                .await?;
        }
    }
    Ok(())
}
use axum::response::IntoResponse;
