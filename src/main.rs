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
    /// Allowed source and reference roots. Repeat to allow several directories.
    #[arg(long, global = true, default_value = "/")]
    root: Vec<PathBuf>,
    #[arg(long, global = true)]
    cache: Option<PathBuf>,
    /// maximum concurrent indexing/search jobs across all workspaces.
    #[arg(long,global=true,default_value_t=2,value_parser=clap::value_parser!(u16).range(1..=16))]
    workers: u16,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
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
    Query { project_path: String, query: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sigla=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let cache = cli.cache.unwrap_or_else(|| {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
            })
            .join("sigla")
    });
    let app = Arc::new(App::new(
        Policy::new(cli.root)?,
        cache,
        cli.workers as usize,
    )?);
    match cli.command {
        Command::Query {
            project_path,
            query,
        } => println!("{}", app.search(&project_path, &query).await?),
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
            let service = StreamableHttpService::new(
                move || Ok(Mcp::new(app.clone())),
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
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    ct.cancel();
                })
                .await?;
        }
    }
    Ok(())
}
use axum::response::IntoResponse;
