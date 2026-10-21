//! Replay native extraction in isolated processes with the normal Tokio worker stack.
//! cargo run --example native_extraction_probe -- CORPUS_DIRECTORY > probe.jsonl
use anyhow::{Context, Result, ensure};
use clap::Parser;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    path: PathBuf,
    /// Extract one file in this process, suitable for running under a debugger.
    #[arg(long)]
    file: bool,
    #[arg(long, default_value_t = 60)]
    timeout_seconds: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.file {
        let language = sigla::native::language(&args.path).context("unsupported file")?;
        let bytes = std::fs::read(&args.path)?;
        eprintln!(
            "extracting {} sha256={:x}",
            args.path.display(),
            Sha256::digest(&bytes)
        );
        let source = sigla::model::decode(&bytes, language)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()?;
        let facts = runtime.block_on(async move {
            tokio::task::spawn_blocking(move || sigla::extract::extract(&source, language, &[], ""))
                .await
        })??;
        println!(
            "{}",
            serde_json::json!({
                "declarations": facts.declarations.len(),
                "occurrences": facts.occurrences.len(),
                "parse_errors": facts.errors,
            })
        );
        return Ok(());
    }
    ensure!(args.path.is_dir(), "pass an existing corpus directory");
    let mut pending = vec![args.path];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && entry.file_name() != ".git" {
                pending.push(entry.path());
            } else if kind.is_file() && sigla::native::language(&entry.path()).is_some() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    let executable = std::env::current_exe()?;
    let mut failed = 0;
    for path in files {
        let started = Instant::now();
        let mut child = Command::new(&executable)
            .arg("--file")
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut timed_out = false;
        while child.try_wait()?.is_none() {
            if started.elapsed() >= Duration::from_secs(args.timeout_seconds) {
                child.kill()?;
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            failed += 1;
            eprintln!("{}: {}", path.display(), output.status);
        }
        println!(
            "{}",
            serde_json::json!({
                "path": path,
                "elapsed_ms": started.elapsed().as_millis(),
                "status": output.status.to_string(),
                "timed_out": timed_out,
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            })
        );
    }
    ensure!(failed == 0, "{failed} files failed extraction");
    Ok(())
}
