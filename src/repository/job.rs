//! Network jobs run in the same executable, under a subprocess deadline.
use super::materialize::{Prepared, Request};
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
struct Output {
    result: std::result::Result<Prepared, String>,
    git_ms: u128,
    lfs_ms: u128,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Input {
    request: Request,
    prepared: Option<Prepared>,
}

pub fn execute(request: &Request, cache: &Path) -> Result<Prepared> {
    run(request, cache, None)
}

pub fn hydrate(request: &Request, cache: &Path, prepared: Prepared) -> Result<Prepared> {
    if matches!(request.contents, super::materialize::Contents::Inventory)
        || prepared.selected.is_empty()
    {
        return Ok(prepared);
    }
    run(request, cache, Some(prepared))
}

fn run(request: &Request, cache: &Path, prepared: Option<Prepared>) -> Result<Prepared> {
    let lfs = prepared.is_some();
    let started = std::time::Instant::now();
    let repository = super::Repository::parse(&request.repository)?
        .ok_or_else(|| anyhow::anyhow!("Expected a repository identifier"))?;
    let preference = cache
        .join("transports")
        .join(repository.identity.storage_key());
    let mut request = request.clone();
    request.preferred_transport = if !request.allow_private {
        None
    } else {
        match fs::read_to_string(&preference) {
            Ok(endpoint) => Some(endpoint),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    };
    let directory = tempfile::Builder::new()
        .prefix("git-job-")
        .tempdir_in(cache)?;
    let input = directory.path().join("request.json");
    let output = directory.path().join("result.json");
    fs::write(
        &input,
        serde_json::to_vec(&Input {
            request: request.clone(),
            prepared,
        })?,
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__git-job")
        .arg(&input)
        .arg(&output)
        .current_dir(directory.path());
    super::credentials::noninteractive(&mut command)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_SHALLOW_FILE");
    let output_log = crate::process::capture(&mut command, Duration::from_secs(300), None, None)?;
    if !output_log.stderr.is_empty() {
        tracing::warn!("{}", String::from_utf8_lossy(&output_log.stderr).trim());
    }
    ensure!(
        output_log.status.success(),
        "Repository preparation subprocess failed"
    );
    ensure!(
        fs::metadata(&output)?.len() <= 256 * 1024 * 1024,
        "Repository inventory exceeds size limit"
    );
    let output: Output = serde_json::from_slice(&fs::read(output)?)?;
    let prepared = output.result.map_err(anyhow::Error::msg)?;
    tracing::info!(
        repository = %repository.identity,
        phase = if lfs { "lfs" } else { "git" },
        target = ?request.target,
        elapsed_ms = started.elapsed().as_millis(),
        git_ms = output.git_ms,
        lfs_ms = output.lfs_ms,
        transfer_bytes = prepared.transfer_bytes,
        files = prepared.selected.len(),
        unavailable = prepared.unavailable.len(),
        "Repository acquisition phase completed"
    );
    if !lfs && let Some(endpoint) = &prepared.transport {
        fs::create_dir_all(preference.parent().unwrap())?;
        let mut temporary = tempfile::NamedTempFile::new_in(preference.parent().unwrap())?;
        std::io::Write::write_all(&mut temporary, endpoint.as_bytes())?;
        temporary.persist(&preference)?;
    }
    Ok(prepared)
}

pub fn worker(input: &Path, output: &Path) -> Result<()> {
    crate::process::inherit_process_group();
    ensure!(
        fs::metadata(input)?.len() <= 256 * 1024 * 1024,
        "Repository request exceeds size limit"
    );
    let Input { request, prepared } = serde_json::from_slice(&fs::read(input)?)?;
    let started = std::time::Instant::now();
    let lfs = prepared.is_some();
    let result = if let Some(mut prepared) = prepared {
        super::lfs::hydrate(
            &request,
            &mut prepared,
            input.parent().unwrap().parent().unwrap(),
        )
        .map(|()| prepared)
    } else {
        super::materialize::prepare(&request)
    }
    .map_err(|error| format!("{error:#}"));
    let elapsed = started.elapsed().as_millis();
    fs::write(
        output,
        serde_json::to_vec(&Output {
            result,
            git_ms: if lfs { 0 } else { elapsed },
            lfs_ms: if lfs { elapsed } else { 0 },
        })?,
    )?;
    Ok(())
}
