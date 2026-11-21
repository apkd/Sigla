//! Network jobs run in the same executable, under a subprocess deadline.
use super::materialize::{Acquired, Metrics, Prepared, Request, Reuse};
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
struct Output {
    result: std::result::Result<Completed, Failure>,
    git_ms: u128,
    lfs_ms: u128,
    metrics: Metrics,
    lfs_metrics: super::lfs::Metrics,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Completed {
    Prepared(Prepared),
    Hydrated(super::lfs::Changes),
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
enum Input {
    Prepare(Request),
    Materialize(Request, Box<Reuse>),
    Hydrate(super::lfs::Hydration),
}

pub fn execute(request: &Request, cache: &Path) -> Result<Prepared> {
    acquire(request, cache, None).map(|output| output.prepared)
}

/// Resolve a live ref in the bounded, noninteractive worker without pool or staging admission.
pub(super) fn reference(
    cache: &Path,
    repository: &super::Repository,
    target: &super::materialize::Target,
    allow_private: bool,
) -> Result<(String, Option<String>, Option<String>)> {
    let prepared = execute(
        &Request {
            contents: super::materialize::Contents::Reference,
            transfer_used: 0,
            unlimited_transfer: false,
            repository: repository.transport.clone(),
            allow_private,
            preferred_transport: None,
            target: target.clone(),
            store: Default::default(),
            staging: Default::default(),
            include: vec![],
            exclude: vec![],
            required: vec![],
            additional: vec![],
            previous: Default::default(),
            subdirectory: None,
        },
        cache,
    )?;
    Ok((prepared.revision, prepared.branch, prepared.transport))
}

pub(super) fn acquire(request: &Request, cache: &Path, reuse: Option<Reuse>) -> Result<Acquired> {
    let input = match reuse {
        Some(reuse) => Input::Materialize(request.clone(), Box::new(reuse)),
        None => Input::Prepare(request.clone()),
    };
    let (completed, metrics) = run(request, cache, input, None)?;
    let Completed::Prepared(prepared) = completed else {
        anyhow::bail!("Unexpected hydration result for Git acquisition");
    };
    Ok(Acquired {
        prepared,
        staged: metrics.staged,
    })
}

pub fn hydrate(
    request: &Request,
    cache: &Path,
    mut prepared: Prepared,
    paths: Vec<String>,
) -> Result<Prepared> {
    if matches!(request.contents, super::materialize::Contents::Inventory) || paths.is_empty() {
        return Ok(prepared);
    }
    let input = Input::Hydrate(super::lfs::Hydration::new(request, &prepared, paths));
    let (completed, _) = run(request, cache, input, Some(&prepared))?;
    let Completed::Hydrated(changes) = completed else {
        anyhow::bail!("Unexpected Git result for LFS hydration");
    };
    changes.apply(&mut prepared);
    Ok(prepared)
}

fn run(
    request: &Request,
    cache: &Path,
    mut input: Input,
    prior: Option<&Prepared>,
) -> Result<(Completed, Metrics)> {
    let lfs = matches!(&input, Input::Hydrate(..));
    let started = std::time::Instant::now();
    let repository = super::Repository::parse(&request.repository)?
        .ok_or_else(|| anyhow::anyhow!("Expected a repository identifier"))?;
    let preference = cache
        .join("transports")
        .join(repository.identity.storage_key());
    if let Input::Prepare(request) | Input::Materialize(request, _) = &mut input {
        request.preferred_transport = if !request.allow_private {
            None
        } else {
            match fs::read_to_string(&preference) {
                Ok(endpoint) => Some(endpoint),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        };
    }
    let directory = tempfile::Builder::new()
        .prefix("git-job-")
        .tempdir_in(cache)?;
    let input_path = directory.path().join("request.json");
    let output = directory.path().join("result.json");
    fs::write(&input_path, serde_json::to_vec(&input)?)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__git-job")
        .arg(&input_path)
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
    let completed = output.result.map_err(Failure::into_error)?;
    let (transfer_bytes, files, unavailable) = match &completed {
        Completed::Prepared(prepared) => (
            prepared.transfer_bytes,
            prepared.selected.len(),
            prepared.unavailable.len(),
        ),
        Completed::Hydrated(changes) => {
            let prior = prior.ok_or_else(|| anyhow::anyhow!("Unexpected hydration result"))?;
            (
                prior.transfer_bytes + changes.transfer_bytes,
                prior.selected.len()
                    - changes
                        .omitted
                        .keys()
                        .filter(|p| prior.selected.contains_key(*p))
                        .count(),
                prior.unavailable.len()
                    + changes
                        .unavailable
                        .iter()
                        .filter(|p| !prior.unavailable.contains(*p))
                        .count(),
            )
        }
    };
    tracing::info!(
        repository = %repository.identity,
        phase = if lfs { "lfs" } else { "git" },
        target = ?request.target,
        elapsed_ms = started.elapsed().as_millis(),
        git_ms = output.git_ms,
        lfs_ms = output.lfs_ms,
        transfer_bytes,
        files,
        unavailable,
        inventory_reused = output.metrics.inventory_reused,
        extracted_files = output.metrics.extracted_files,
        extracted_bytes = output.metrics.extracted_bytes,
        reused_files = output.metrics.reused_files,
        reused_bytes = output.metrics.reused_bytes,
        lfs_reused_bytes = output.lfs_metrics.reused_bytes,
        lfs_checksum_bytes = output.lfs_metrics.checksum_bytes,
        lfs_linked_files = output.lfs_metrics.linked_files,
        "Repository acquisition phase completed"
    );
    if let Completed::Prepared(prepared) = &completed
        && let Some(endpoint) = &prepared.transport
    {
        fs::create_dir_all(preference.parent().unwrap())?;
        let mut temporary = tempfile::NamedTempFile::new_in(preference.parent().unwrap())?;
        std::io::Write::write_all(&mut temporary, endpoint.as_bytes())?;
        temporary.persist(&preference)?;
    }
    Ok((completed, output.metrics))
}

pub fn worker(input: &Path, output: &Path) -> Result<()> {
    crate::process::inherit_process_group();
    ensure!(
        fs::metadata(input)?.len() <= 256 * 1024 * 1024,
        "Repository request exceeds size limit"
    );
    let action: Input = serde_json::from_slice(&fs::read(input)?)?;
    let cache = input.parent().unwrap().parent().unwrap();
    fs::write(output, serde_json::to_vec(&perform(action, cache))?)?;
    Ok(())
}

fn perform(action: Input, cache: &Path) -> Output {
    let started = std::time::Instant::now();
    let lfs = matches!(&action, Input::Hydrate(..));
    let mut metrics = Metrics::default();
    let mut lfs_metrics = super::lfs::Metrics::default();
    let result = match action {
        Input::Hydrate(request) => {
            super::lfs::hydrate_selected(&request, cache).map(|(changes, measured)| {
                lfs_metrics = measured;
                Completed::Hydrated(changes)
            })
        }
        Input::Prepare(request) => super::materialize::prepare(&request, None, cache, &mut metrics)
            .map(Completed::Prepared),
        Input::Materialize(request, reuse) => {
            super::materialize::prepare(&request, Some(*reuse), cache, &mut metrics)
                .map(Completed::Prepared)
        }
    }
    .map_err(Failure::from_error);
    let elapsed = started.elapsed().as_millis();
    Output {
        result,
        git_ms: if lfs { 0 } else { elapsed },
        lfs_ms: if lfs { elapsed } else { 0 },
        metrics,
        lfs_metrics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_hydration_transports_only_candidates_and_merges_changed_paths() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("source");
        fs::create_dir(&staging).unwrap();
        fs::write(
            staging.join("bad.cs"),
            b"version https://git-lfs.github.com/spec/bad\n",
        )
        .unwrap();
        fs::write(
            staging.join("omit.bin"),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                "a".repeat(64)
            ),
        )
        .unwrap();
        let mut request = Request {
            contents: super::super::materialize::Contents::All,
            transfer_used: 0,
            unlimited_transfer: false,
            repository: "https://example.invalid/repo".into(),
            allow_private: false,
            preferred_transport: None,
            target: super::super::materialize::Target::Commit("a".repeat(40)),
            store: root.path().join("git"),
            staging,
            include: vec![],
            exclude: vec![],
            required: vec![],
            additional: vec![],
            previous: Default::default(),
            subdirectory: None,
        };
        let mut prepared = Prepared {
            deferred_assets: ["later.asset".into()].into(),
            omitted: [("old.bin".into(), "old omission".into())].into(),
            transfer_bytes: 123,
            unavailable: ["old.cs".into()].into(),
            transport: None,
            resolved_target: None,
            branch: Some("main".into()),
            revision: "a".repeat(40),
            selected: [
                ("bad.cs".into(), "bad-oid".into()),
                ("omit.bin".into(), "omitted-oid".into()),
            ]
            .into(),
            tracked: Default::default(),
            directories: ["retained".into()].into(),
        };
        let input = |request: &Request, prepared: &Prepared| {
            serde_json::to_vec(&Input::Hydrate(super::super::lfs::Hydration::new(
                request,
                prepared,
                vec!["bad.cs".into(), "omit.bin".into()],
            )))
            .unwrap()
        };
        let bytes = input(&request, &prepared);
        for index in 0..10_000 {
            let path = format!("unrelated/{index}.cs");
            request.previous.insert(path.clone(), "old-oid".into());
            prepared.selected.insert(path.clone(), "new-oid".into());
            prepared.tracked.insert(path, "new-oid".into());
        }
        assert_eq!(input(&request, &prepared), bytes);
        let before = prepared.clone();
        let output = perform(serde_json::from_slice(&bytes).unwrap(), root.path());
        let output: Output = serde_json::from_slice(&serde_json::to_vec(&output).unwrap()).unwrap();
        let Completed::Hydrated(changes) =
            output.result.unwrap_or_else(|e| panic!("{}", e.message))
        else {
            panic!("Expected hydration delta")
        };
        assert_eq!(changes.unavailable.len(), 1);
        assert_eq!(changes.omitted.len(), 1);
        changes.apply(&mut prepared);
        assert!(prepared.unavailable.contains("bad.cs") && prepared.unavailable.contains("old.cs"));
        assert!(
            prepared.omitted.contains_key("omit.bin") && prepared.omitted.contains_key("old.bin")
        );
        assert!(!prepared.selected.contains_key("omit.bin"));
        assert_eq!(prepared.selected.len(), before.selected.len() - 1);
        assert_eq!(prepared.tracked, before.tracked);
        assert_eq!(prepared.deferred_assets, before.deferred_assets);
        assert_eq!(prepared.revision, before.revision);
        assert_eq!(prepared.transfer_bytes, before.transfer_bytes);
    }

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
