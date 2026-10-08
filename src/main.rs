use anyhow::{Result, ensure};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sigla::{
    discovery::Policy,
    service::{App, Mcp},
};
use std::{io::Write, net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    name = "sigla",
    version,
    about = "Source navigation for C#, Rust, C/C++, and shaders"
)]
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
    /// show licenses and copyright notices for bundled components.
    Licenses,
    /// Build and publish precomputed Unity metadata.
    Metadata {
        #[command(subcommand)]
        command: sigla::metadata_archive::Command,
    },
    /// inspect managed disk usage and preview eviction without changing the cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    #[command(name = "__git-job", hide = true)]
    GitJob { input: PathBuf, output: PathBuf },
    #[command(name = "__git-rebuild", hide = true)]
    GitRebuild { input: PathBuf, output: PathBuf },
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
#[derive(Subcommand)]
enum CacheCommand {
    /// show the running service's policy, or its last saved policy when stopped.
    Inspect {
        /// emit the full report as JSON.
        #[arg(long)]
        json: bool,
    },
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
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    if let Command::Licenses = cli.command {
        std::io::stdout()
            .lock()
            .write_all(include_bytes!(concat!(env!("OUT_DIR"), "/licenses.txt")))?;
        return Ok(());
    }
    if let Command::Cache {
        command: CacheCommand::Inspect { json },
    } = &cli.command
    {
        let explicit =
            |name| matches.value_source(name) == Some(clap::parser::ValueSource::CommandLine);
        let report = sigla::cache::inspect::inspect(
            &cli.options.cache_dir,
            sigla::cache::inspect::Overrides {
                max_bytes: explicit("max_cache_size_gb")
                    .then_some(cli.options.max_cache_size_gb * 1_000_000_000),
                headroom_percent: explicit("free_disk_space_headroom_percent")
                    .then_some(cli.options.free_disk_space_headroom_percent),
            },
        )
        .await?;
        println!(
            "{}",
            if *json {
                serde_json::to_string_pretty(&report)?
            } else {
                report.render()
            }
        );
        return Ok(());
    }
    if let Command::GitJob { input, output } = &cli.command {
        let (input, output) = (input.clone(), output.clone());
        return tokio::task::spawn_blocking(move || {
            sigla::repository::job::worker(&input, &output)
        })
        .await?;
    }
    if let Command::GitRebuild { input, output } = &cli.command {
        let (input, output) = (input.clone(), output.clone());
        return tokio::task::spawn_blocking(move || {
            sigla::repository::rebuild::worker(&input, &output)
        })
        .await?;
    }
    if let Command::Metadata { command } = cli.command {
        return tokio::task::spawn_blocking(move || command.run()).await?;
    }
    let remote = cli.options.validate()?;
    let cache_limits = cli.options.cache_limits();
    let mut policy = Policy::new(cli.options.local_roots())?;
    policy.unity_platform = cli.options.unity_platform;
    policy.unity_editors = cli.options.unity_editors;
    let app = Arc::new(
        match remote {
            Some(remote) => {
                App::remote(policy, cli.options.cache_dir, cli.workers as usize, remote)?
            }
            None if cli.options.mode == sigla::config::Mode::Hybrid => App::hybrid(
                policy,
                cli.options.cache_dir,
                cli.workers as usize,
                cli.options.upstream.as_deref().unwrap(),
                cli.options.upstream_token_file.as_deref(),
            )?,
            None => App::new(policy, cli.options.cache_dir, cli.workers as usize)?,
        }
        .with_cache_limits(cache_limits),
    );
    match cli.command {
        Command::Licenses
        | Command::Metadata { .. }
        | Command::GitJob { .. }
        | Command::GitRebuild { .. }
        | Command::Cache { .. } => {
            unreachable!()
        }
        Command::Query { project, query } => {
            let result = app.search(&project, &query).await;
            if let Ok(text) = &result {
                println!("{text}");
            }
            app.shutdown().await;
            sigla::shutdown();
            result?;
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
            let inspection = app.start_inspection()?;
            app.start_setup();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    ct.cancel();
                    app.shutdown().await;
                    sigla::shutdown();
                })
                .await?;
            let _ = inspection.await;
        }
    }
    Ok(())
}
use axum::response::IntoResponse;
