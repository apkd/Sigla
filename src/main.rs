use anyhow::{Result, ensure};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sigla::{
    discovery::Policy,
    service::{App, Mcp},
};
use std::{
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

#[derive(Parser)]
#[command(
    name = "sigla",
    version,
    about = "Source navigation for C#, Rust, C/C++, and shaders"
)]
struct Cli {
    #[command(flatten)]
    options: sigla::config::Options,
    /// Override each query, preparation, indexing and navigation pool. Defaults:
    /// queries use up to four available CPUs; other pools use two workers each.
    #[arg(long,global=true,value_parser=clap::value_parser!(u16).range(1..=16))]
    workers: Option<u16>,
    /// Parser admission memory in MiB; defaults to 25% of the effective memory
    /// limit, capped at 4 GiB. Source reads use a separate quarter of this budget.
    #[arg(long, global=true,value_parser=clap::value_parser!(u64).range(1..))]
    parser_memory_mib: Option<u64>,
    /// Completed-index and workspace cache memory in MiB; defaults to 25% of
    /// the effective memory limit, capped at 2 GiB. Asset indexes receive half.
    #[arg(long, global=true,value_parser=clap::value_parser!(u64).range(1..))]
    cache_memory_mib: Option<u64>,
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
    /// export monitoring history without opening the analysis cache.
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    #[command(name = "__git-job", hide = true)]
    GitJob { input: PathBuf, output: PathBuf },
    #[command(name = "__git-rebuild", hide = true)]
    GitRebuild { input: PathBuf, output: PathBuf },
    /// run the shared HTTP MCP service at /mcp.
    Serve {
        /// store monitoring history outside the analysis cache.
        #[arg(long)]
        monitor_dir: Option<PathBuf>,
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

#[derive(Subcommand)]
enum MonitorCommand {
    /// emit one incremental page of monitoring data as JSON.
    Export {
        #[arg(long)]
        monitor_dir: PathBuf,
        #[arg(long)]
        after: Option<String>,
        /// read {"after": cursor} from stdin for a fixed SSH export command.
        #[arg(long, conflicts_with = "after")]
        request_stdin: bool,
    },
}

fn export_cursor(input: impl Read) -> Result<Option<String>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ExportRequest {
        after: Option<String>,
    }
    let mut bytes = Vec::new();
    input.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1_048_576,
        "Monitoring request exceeds size limit"
    );
    Ok(serde_json::from_slice::<ExportRequest>(&bytes)?.after)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sigla=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    if let Command::Monitor {
        command:
            MonitorCommand::Export {
                monitor_dir,
                after,
                request_stdin,
            },
    } = &cli.command
    {
        let after = if *request_stdin {
            export_cursor(std::io::stdin().lock())?
        } else {
            after.clone()
        };
        serde_json::to_writer(
            std::io::stdout().lock(),
            &sigla::telemetry::export(monitor_dir, after.as_deref())?,
        )?;
        return Ok(());
    }
    if let Command::Licenses = cli.command {
        std::io::stdout()
            .lock()
            .write_all(include_bytes!(concat!(env!("OUT_DIR"), "/licenses.txt")))?;
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(run(cli, matches))
}

async fn run(cli: Cli, matches: clap::ArgMatches) -> Result<()> {
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
    App::configure_parser_memory(cli.parser_memory_mib)?;
    App::configure_cache_memory(cli.cache_memory_mib)?;
    if let Command::Metadata { command } = cli.command {
        return tokio::task::spawn_blocking(move || command.run()).await?;
    }
    let _monitor = match &cli.command {
        Command::Serve {
            monitor_dir: Some(directory),
            ..
        } => {
            let result = (|| -> Result<_> {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::create_dir_all(&cli.options.cache_dir)?;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o750)
                    .create(directory)?;
                ensure!(
                    !std::fs::canonicalize(directory)?
                        .starts_with(std::fs::canonicalize(&cli.options.cache_dir)?),
                    "--monitor-dir must be outside --cache-dir"
                );
                sigla::telemetry::init(directory)
            })();
            match result {
                Ok(guard) => Some(guard),
                Err(error) => {
                    tracing::warn!(%error, "Monitoring unavailable; continuing without recording");
                    None
                }
            }
        }
        _ => None,
    };
    let remote = cli.options.validate()?;
    let cache_limits = cli.options.cache_limits();
    let mut policy = Policy::new(cli.options.local_roots())?;
    policy.unity_platform = cli.options.unity_platform;
    policy.unity_editors = cli.options.unity_editors;
    let workers = usize::from(cli.workers.unwrap_or(2));
    let query_workers = cli.workers.map_or_else(
        || {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(4)
        },
        usize::from,
    );
    tracing::info!(
        query_workers,
        preparation_workers = workers,
        indexing_workers = workers,
        navigation_workers = workers,
        "Worker pools configured"
    );
    let app = Arc::new(
        match remote {
            Some(remote) => App::remote(policy, cli.options.cache_dir, workers, remote)?,
            None if cli.options.mode == sigla::config::Mode::Hybrid => App::hybrid(
                policy,
                cli.options.cache_dir,
                workers,
                cli.options.upstream.as_deref().unwrap(),
                cli.options.upstream_token_file.as_deref(),
            )?,
            None => App::new(policy, cli.options.cache_dir, workers)?,
        }
        .with_query_workers(query_workers)?
        .with_cache_limits(cache_limits),
    );
    match cli.command {
        Command::Licenses
        | Command::Metadata { .. }
        | Command::GitJob { .. }
        | Command::GitRebuild { .. }
        | Command::Cache { .. }
        | Command::Monitor { .. } => {
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
            monitor_dir: _,
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
#[cfg(test)]
mod monitoring_tests {
    use super::*;

    #[test]
    fn forced_export_accepts_only_a_bounded_cursor_request() {
        let cursor = "opaque \"cursor\" with spaces and Unicode λ";
        let input = serde_json::to_vec(&serde_json::json!({"after": cursor})).unwrap();
        assert_eq!(
            export_cursor(input.as_slice()).unwrap().as_deref(),
            Some(cursor)
        );
        assert!(
            export_cursor(br#"{"after":null}"#.as_slice())
                .unwrap()
                .is_none()
        );
        assert!(export_cursor(br#"{"monitor_dir":"/elsewhere"}"#.as_slice()).is_err());
        assert!(export_cursor(std::io::repeat(b' ')).is_err());
    }
}
