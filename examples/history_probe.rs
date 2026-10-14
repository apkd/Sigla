//! Measure history directly through the service, including repeated requests.
use anyhow::{Result, ensure};
use clap::Parser;
use sigla::{config, discovery::Policy, service::App};
use std::{path::Path, sync::Arc, time::Instant};

#[derive(Parser)]
struct Options {
    #[command(flatten)]
    service: config::Options,
    codebase: String,
    #[arg(long, required = true)]
    query: Vec<String>,
    #[arg(long, default_value_t = 2)]
    repeat: usize,
}

fn allocated(path: &Path) -> Result<u64> {
    let output = std::process::Command::new("du")
        .arg("-sB1")
        .arg(path)
        .output()?;
    ensure!(output.status.success(), "Cannot measure cache allocation");
    Ok(std::str::from_utf8(&output.stdout)?
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .parse()?)
}

async fn run(options: Options) -> Result<()> {
    ensure!(options.repeat > 0, "Use at least one repetition");
    let remote = options.service.validate()?;
    let policy = Policy::new(options.service.local_roots())?;
    let cache = options.service.cache_dir;
    let app = Arc::new(match remote {
        Some(remote) => App::remote(policy, cache.clone(), 2, remote)?,
        None => App::new(policy, cache.clone(), 2)?,
    });
    let mut failed = false;
    for query in options.query {
        for sample in 1..=options.repeat {
            let before = allocated(&cache)?;
            let start = Instant::now();
            let result = app.history(&options.codebase, &query).await;
            let seconds = start.elapsed().as_secs_f64();
            failed |= result.is_err();
            println!(
                "{}",
                serde_json::json!({
                    "query": query, "sample": sample, "seconds": seconds,
                    "before_bytes": before, "allocated_bytes": allocated(&cache)?,
                    "result": result.as_ref().ok(),
                    "error": result.as_ref().err().map(|e| format!("{e:#}")),
                })
            );
        }
    }
    app.shutdown().await;
    ensure!(!failed, "At least one history request failed");
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run(Options::parse()))
}
