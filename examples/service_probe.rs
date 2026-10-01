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
    #[serde(default)]
    idle_ms: u64,
    #[serde(default)]
    trim_idle: bool,
}
fn serial() -> usize {
    1
}
#[derive(Clone, Deserialize)]
struct Case {
    project: String,
    query: String,
}

fn memory_sample() -> Result<serde_json::Value> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let memory = |prefix: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or("")
            .trim()
    };
    let mut sample = serde_json::json!({
        "rss":memory("VmRSS:"), "anonymous_rss":memory("RssAnon:"),
        "file_rss":memory("RssFile:"), "shared_rss":memory("RssShmem:"),
        "peak_rss":memory("VmHWM:"), "threads":memory("Threads:")
    });
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let heap = unsafe { libc::mallinfo2() };
        sample["heap"] =
            serde_json::json!({"used":heap.uordblks,"free":heap.fordblks,"mapped":heap.hblkhd});
    }
    Ok(sample)
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
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
            let elapsed = start.elapsed().as_micros();
            let mut sample = memory_sample()?;
            sample.as_object_mut().unwrap().extend(serde_json::json!({"iteration":iteration,"project":case.project,"query":case.query,"elapsed_us":elapsed,"output_bytes":output.len(),"output_hash":blake3::hash(output.as_bytes()).to_hex().to_string()}).as_object().unwrap().clone());
            println!("{sample}");
            Ok::<_, anyhow::Error>(())
            });
            }
            while let Some(result) = jobs.join_next().await {
                result??;
            }
        }
    }
    if workload.idle_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(workload.idle_ms)).await;
        let mut sample = memory_sample()?;
        sample["phase"] = "idle".into();
        println!("{sample}");
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if workload.trim_idle {
        let start = Instant::now();
        unsafe { libc::malloc_trim(0) };
        let elapsed = start.elapsed().as_micros();
        let mut sample = memory_sample()?;
        sample["phase"] = "trimmed".into();
        sample["elapsed_us"] = serde_json::json!(elapsed);
        println!("{sample}");
    }
    Ok(())
}
