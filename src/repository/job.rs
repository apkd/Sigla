//! Network jobs run in the same executable, under a subprocess deadline.
use super::materialize::{Prepared, Request};
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command, time::Duration};

pub fn execute(request: &Request, cache: &Path) -> Result<Prepared> {
    let directory = tempfile::Builder::new()
        .prefix("git-job-")
        .tempdir_in(cache)?;
    let input = directory.path().join("request.json");
    let output = directory.path().join("result.json");
    fs::write(&input, serde_json::to_vec(request)?)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__git-job")
        .arg(&input)
        .arg(&output)
        .current_dir(directory.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("DISPLAY")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    crate::process::run(&mut command, Duration::from_secs(300))?;
    ensure!(
        fs::metadata(&output)?.len() <= 256 * 1024 * 1024,
        "Repository inventory exceeds size limit"
    );
    let result: std::result::Result<Prepared, String> = serde_json::from_slice(&fs::read(output)?)?;
    result.map_err(anyhow::Error::msg)
}

pub fn worker(input: &Path, output: &Path) -> Result<()> {
    crate::process::inherit_process_group();
    ensure!(
        fs::metadata(input)?.len() <= 256 * 1024 * 1024,
        "Repository request exceeds size limit"
    );
    let request: Request = serde_json::from_slice(&fs::read(input)?)?;
    let result = super::materialize::prepare(&request).map_err(|error| error.to_string());
    fs::write(output, serde_json::to_vec(&result)?)?;
    Ok(())
}
