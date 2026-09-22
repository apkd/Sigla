//! replay a JSON workload against one shared service instance.
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc, time::Instant};
#[derive(Deserialize)]
struct Workload {
    roots: Vec<PathBuf>,
    cache: PathBuf,
    cases: Vec<Case>,
    repeats: usize,
    #[serde(default = "serial")]
    concurrency: usize,
}
fn serial() -> usize {
    1
}
#[derive(Clone, Deserialize)]
struct Case {
    project: String,
    query: String,
}
#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("Expected workload JSON file")?;
    let workload: Workload = serde_json::from_slice(&std::fs::read(path)?)?;
    let app = Arc::new(sigla::service::App::new(
        sigla::discovery::Policy::new(workload.roots)?,
        workload.cache,
        2,
    )?);
    for iteration in 0..workload.repeats {
        for cases in workload.cases.chunks(workload.concurrency.max(1)) {
            let mut jobs = tokio::task::JoinSet::new();
            for case in cases {
                let case = case.clone();
                let app = app.clone();
                jobs.spawn(async move {
            let start = Instant::now();
            let output = app.search(&case.project, &case.query).await?;
            let status = std::fs::read_to_string("/proc/self/status")?;
            let memory = |prefix: &str| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix))
                    .unwrap_or("")
                    .trim()
            };
            println!(
                "{}",
                serde_json::json!({"iteration":iteration,"project":case.project,"query":case.query,"elapsed_us":start.elapsed().as_micros(),"output_bytes":output.len(),"output_hash":blake3::hash(output.as_bytes()).to_hex().to_string(),"rss":memory("VmRSS:"),"anonymous_rss":memory("RssAnon:"),"file_rss":memory("RssFile:"),"peak_rss":memory("VmHWM:"),"threads":memory("Threads:")})
            );
            Ok::<_, anyhow::Error>(())
            });
            }
            while let Some(result) = jobs.join_next().await {
                result??;
            }
        }
    }
    Ok(())
}
