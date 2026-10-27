//! Network jobs run in the same executable, under a subprocess deadline.
use super::materialize::{Metrics, Pinned, Prepared, Request};
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
struct Output {
    result: std::result::Result<Prepared, Failure>,
    git_ms: u128,
    lfs_ms: u128,
    metrics: Metrics,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum FailureKind {
    RepositoryTransfer,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Failure {
    message: String,
    kind: Option<FailureKind>,
}

impl Failure {
    fn from_error(error: anyhow::Error) -> Self {
        let kind = error
            .chain()
            .any(|cause| cause.is::<crate::diagnostics::RepositoryTransfer>())
            .then_some(FailureKind::RepositoryTransfer);
        Self {
            message: format!("{error:#}"),
            kind,
        }
    }

    fn into_error(self) -> anyhow::Error {
        match self.kind {
            Some(FailureKind::RepositoryTransfer) => {
                crate::diagnostics::RepositoryTransfer::new(std::io::Error::other(self.message))
                    .into()
            }
            None => anyhow::Error::msg(self.message),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Input {
    request: Request,
    action: Action,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Action {
    Prepare,
    Materialize(Pinned),
    Hydrate(Prepared),
}

pub fn execute(request: &Request, cache: &Path) -> Result<Prepared> {
    run(request, cache, Action::Prepare)
}

pub(super) fn materialize(request: &Request, cache: &Path, pinned: Pinned) -> Result<Prepared> {
    run(request, cache, Action::Materialize(pinned))
}

pub fn hydrate(request: &Request, cache: &Path, prepared: Prepared) -> Result<Prepared> {
    if matches!(request.contents, super::materialize::Contents::Inventory)
        || prepared.selected.is_empty()
    {
        return Ok(prepared);
    }
    run(request, cache, Action::Hydrate(prepared))
}

fn run(request: &Request, cache: &Path, action: Action) -> Result<Prepared> {
    let lfs = matches!(&action, Action::Hydrate(_));
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
            action,
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
    let prepared = output.result.map_err(Failure::into_error)?;
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
        inventory_reused = output.metrics.inventory_reused,
        extracted_files = output.metrics.extracted_files,
        extracted_bytes = output.metrics.extracted_bytes,
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
    let Input { request, action } = serde_json::from_slice(&fs::read(input)?)?;
    let started = std::time::Instant::now();
    let lfs = matches!(&action, Action::Hydrate(_));
    let mut metrics = Metrics::default();
    let result = match action {
        Action::Hydrate(mut prepared) => super::lfs::hydrate(
            &request,
            &mut prepared,
            input.parent().unwrap().parent().unwrap(),
        )
        .map(|()| prepared),
        Action::Prepare => super::materialize::prepare(&request, None, &mut metrics),
        Action::Materialize(pinned) => {
            super::materialize::prepare(&request, Some(pinned), &mut metrics)
        }
    }
    .map_err(Failure::from_error);
    let elapsed = started.elapsed().as_millis();
    fs::write(
        output,
        serde_json::to_vec(&Output {
            result,
            git_ms: if lfs { 0 } else { elapsed },
            lfs_ms: if lfs { elapsed } else { 0 },
            metrics,
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_failure_remains_typed_across_worker_serialization() {
        let error = anyhow::Error::new(crate::diagnostics::RepositoryTransfer::new(
            std::io::Error::other("private transfer cause"),
        ))
        .context("repository acquisition");
        let bytes = serde_json::to_vec(&Failure::from_error(error)).unwrap();
        let failure: Failure = serde_json::from_slice(&bytes).unwrap();
        let error = failure.into_error();
        assert_eq!(
            crate::diagnostics::monitoring(&error),
            Some(crate::telemetry::Diagnostic::RepositoryTransfer)
        );
        assert!(format!("{error:#}").contains("private transfer cause"));

        let unknown = Failure::from_error(anyhow::anyhow!("ordinary failure"));
        let bytes = serde_json::to_vec(&unknown).unwrap();
        let error: Failure = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(crate::diagnostics::monitoring(&error.into_error()), None);
    }
}
