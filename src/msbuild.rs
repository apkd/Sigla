//! Extraction and invocation of the embedded, short-lived MSBuild integration.
use crate::process;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const PAYLOAD: &[(&str, &[u8])] = &[
    (
        "Sigla.Discovery.dll",
        include_bytes!(concat!(env!("OUT_DIR"), "/managed/Sigla.Discovery.dll")),
    ),
    (
        "Sigla.Discovery.deps.json",
        include_bytes!(concat!(
            env!("OUT_DIR"),
            "/managed/Sigla.Discovery.deps.json"
        )),
    ),
    (
        "Microsoft.Build.Locator.dll",
        include_bytes!(concat!(
            env!("OUT_DIR"),
            "/managed/Microsoft.Build.Locator.dll"
        )),
    ),
];

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub projects: Vec<Project>,
    pub needs_restore: bool,
    pub dependency_state: BTreeMap<String, String>,
    #[serde(default)]
    pub required_inputs: Vec<PathBuf>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct Project {
    pub identity: String,
    pub origin: PathBuf,
    pub properties: BTreeMap<String, String>,
    pub sources: Vec<PathBuf>,
    pub references: Vec<Reference>,
    pub assemblies: Vec<Assembly>,
    pub imports: Vec<PathBuf>,
    pub globs: Vec<PathBuf>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct Reference {
    pub identity: String,
    pub aliases: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct Assembly {
    pub path: PathBuf,
    pub aliases: String,
    pub source_project: String,
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new("dotnet");
    command
        .current_dir(directory)
        .env("MSBUILDDISABLENODEREUSE", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        .env("DOTNET_NOLOGO", "1");
    command
}

fn payload(cache: &Path, major: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::DirBuilderExt;
    static EXTRACTION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _extraction = EXTRACTION.lock().unwrap();
    let runtime = format!(
        r#"{{"runtimeOptions":{{"tfm":"net{major}.0","framework":{{"name":"Microsoft.NETCore.App","version":"{major}.0.0"}},"rollForward":"LatestMinor"}}}}"#
    );
    let mut digest = blake3::Hasher::new();
    for (name, bytes) in PAYLOAD {
        digest.update(name.as_bytes());
        digest.update(bytes);
    }
    digest.update(runtime.as_bytes());
    let root = cache
        .join("integration")
        .join(digest.finalize().to_hex().as_str());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    for (name, bytes) in PAYLOAD.iter().copied().chain(std::iter::once((
        "Sigla.Discovery.runtimeconfig.json",
        runtime.as_bytes(),
    ))) {
        let path = root.join(name);
        if path.exists() {
            ensure!(
                !fs::symlink_metadata(&path)?.file_type().is_symlink() && fs::read(&path)? == bytes,
                "Embedded integration cache is damaged: {}",
                path.display()
            );
        } else {
            let mut file = tempfile::NamedTempFile::new_in(&root)?;
            file.write_all(bytes)?;
            file.as_file().sync_all()?;
            file.persist(path)?;
        }
    }
    Ok(root)
}

pub fn discover_in(
    entries: &[PathBuf],
    cache: &Path,
    remote: Option<&crate::discovery::RemoteContext>,
) -> Result<Snapshot> {
    fs::create_dir_all(cache)?;
    let directory = entries
        .first()
        .context("MSBuild entry is missing")?
        .parent()
        .unwrap();
    let sdks = process::run(command(cache).arg("--list-sdks"), Duration::from_secs(30))?;
    let installed: Vec<_> = std::str::from_utf8(&sdks)?
        .lines()
        .filter_map(|line| {
            let (version, location) = line.split_once(" [")?;
            let major = version.split('.').next()?;
            matches!(major, "8" | "10").then(|| {
                (
                    version.to_owned(),
                    PathBuf::from(location.trim_end_matches(']')).join(version),
                )
            })
        })
        .collect();
    let (bootstrap_version, bootstrap_sdk) = installed
        .iter()
        .max_by_key(|(v, _)| semver::Version::parse(v).ok())
        .context("MSBuild discovery requires an installed .NET 8 or .NET 10 SDK")?;
    let host_root = bootstrap_sdk.parent().unwrap().parent().unwrap();
    let host = host_root.join("dotnet");
    let fxr = fs::read_dir(host_root.join("host/fxr"))?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .max_by_key(|e| semver::Version::parse(&e.file_name().to_string_lossy()).ok())
        .context("Installed hostfxr is missing")?
        .path()
        .join("libhostfxr.so");
    let roots = installed
        .iter()
        .map(|(_, p)| p.parent().unwrap().parent().unwrap().to_owned())
        .collect::<Vec<_>>();
    let sandbox = remote
        .map(|context| crate::sandbox::Sandbox::new(context, &roots))
        .transpose()?;
    let job = tempfile::Builder::new()
        .prefix("discovery-")
        .tempdir_in(cache)?;
    let request = job.path().join("request.json");
    let output = job.path().join("result.json");
    let bootstrap = payload(cache, bootstrap_version.split('.').next().unwrap())?;
    let mut resolve = match &sandbox {
        Some(sandbox) => {
            let mut command = sandbox.command(&host, &bootstrap, job.path(), directory, false)?;
            command
                .args(["/integration/Sigla.Discovery.dll", "--resolve-sdk"])
                .arg(host_root)
                .arg(sandbox.input(directory)?)
                .arg(&fxr)
                .arg("/job/result.json");
            command
        }
        None => {
            let mut command = Command::new(&host);
            command
                .current_dir(directory)
                .arg(bootstrap.join("Sigla.Discovery.dll"))
                .arg("--resolve-sdk")
                .arg(host_root)
                .arg(directory)
                .arg(&fxr)
                .arg(&output);
            command
        }
    };
    process::run(&mut resolve, Duration::from_secs(30))?;
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase", deny_unknown_fields)]
    struct Sdk {
        sdk: PathBuf,
    }
    let resolved: Sdk =
        serde_json::from_slice(&crate::sandbox::read_job_file(&output, 64 * 1024)?)?;
    let sdk = resolved.sdk.canonicalize()?;
    let (version, _) = installed
        .iter()
        .find(|(_, p)| p.canonicalize().ok().as_ref() == Some(&sdk))
        .context("global.json selected a toolchain outside approved installed SDKs")?;
    let root = payload(cache, version.split('.').next().unwrap())?;
    let mapped_entries = entries
        .iter()
        .map(|entry| match &sandbox {
            Some(s) => s.input(entry),
            None => Ok(entry.clone()),
        })
        .collect::<Result<Vec<_>>>()?;
    let state_dir = cache.join("restore-state");
    fs::create_dir_all(&state_dir)?;
    let state_file = state_dir.join(
        blake3::hash(&serde_json::to_vec(entries)?)
            .to_hex()
            .as_str(),
    );
    let previous: BTreeMap<String, String> = if state_file.is_file() {
        serde_json::from_slice(&fs::read(&state_file)?)?
    } else {
        BTreeMap::new()
    };
    for restored in [false, true] {
        crate::sandbox::write_job_file(
            &request,
            &serde_json::to_vec(
                &serde_json::json!({"Entries": mapped_entries, "DependencyState": previous, "Restored": restored,
                    "Tracked": remote.map(|r| r.tracked.iter().map(|p| Path::new("/workspace").join(p)).collect::<Vec<_>>())}),
            )?,
        )?;
        let mut discovery = match &sandbox {
            Some(sandbox) => {
                let mut command = sandbox.command(&host, &root, job.path(), directory, false)?;
                command
                    .arg("/integration/Sigla.Discovery.dll")
                    .arg(&sdk)
                    .args(["/job/request.json", "/job/result.json"]);
                command
            }
            None => {
                let mut command = command(directory);
                command
                    .arg(root.join("Sigla.Discovery.dll"))
                    .arg(&sdk)
                    .arg(&request)
                    .arg(&output);
                command
            }
        };
        process::run(&mut discovery, Duration::from_secs(120))?;
        if let Some(sandbox) = &sandbox {
            sandbox.validate_writes()?;
        }
        let mut snapshot: Snapshot =
            serde_json::from_slice(&crate::sandbox::read_job_file(&output, 64 * 1024 * 1024)?)?;
        ensure!(
            snapshot.version == 1,
            "Unsupported MSBuild snapshot version"
        );
        if !snapshot.required_inputs.is_empty() {
            let remote = remote.context("Unexpected tracked-input request from local MSBuild")?;
            let paths = snapshot
                .required_inputs
                .iter()
                .map(|p| -> Result<PathBuf> {
                    let relative = p
                        .strip_prefix("/workspace")
                        .context("MSBuild requested an input outside the workspace")?;
                    crate::repository::selection::validate_path(
                        relative.to_str().context("Invalid requested input")?,
                    )?;
                    Ok(remote.workspace.join(relative))
                })
                .collect::<Result<Vec<_>>>()?;
            remote.require(paths)?;
            anyhow::bail!("MSBuild returned an invalid tracked-input request");
        }
        if !snapshot.needs_restore {
            let mut state = tempfile::NamedTempFile::new_in(&state_dir)?;
            serde_json::to_writer(&mut state, &snapshot.dependency_state)?;
            state.persist(&state_file)?;
            if let Some(sandbox) = &sandbox {
                sandbox.validate_writes()?;
                map_snapshot(&mut snapshot, sandbox)?;
            }
            return Ok(snapshot);
        }
        ensure!(
            !restored,
            "Dependency assets remain unavailable after restore"
        );
        for entry in &mapped_entries {
            let mut restore = match &sandbox {
                Some(s) => s.command(&host, &root, job.path(), directory, true)?,
                None => command(directory),
            };
            restore.arg(sdk.join("MSBuild.dll")).arg(entry).args([
                "-target:Restore",
                "-nodeReuse:false",
                "-nologo",
                "-m:1",
            ]);
            let mut log = tempfile::tempfile()?;
            let result = process::capture(
                &mut restore,
                Duration::from_secs(300),
                None,
                Some(log.try_clone()?),
            )?;
            if !result.status.success() {
                let stdout = restore_log_tail(&mut log)
                    .unwrap_or_else(|error| format!("Cannot read restore log: {error}"));
                anyhow::bail!(
                    "Dependency restore failed for {} ({}).\nstdout:\n{}\nstderr:\n{}",
                    entry.display(),
                    result.status,
                    stdout,
                    diagnostic_tail(&result.stderr),
                );
            }
            if let Some(sandbox) = &sandbox {
                sandbox.validate_writes()?;
            }
        }
    }
    unreachable!()
}

const DIAGNOSTIC_LIMIT: usize = 16 * 1024;

fn diagnostic_tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(DIAGNOSTIC_LIMIT);
    let text = String::from_utf8_lossy(&bytes[start..]);
    if text.trim().is_empty() {
        "(no output)".into()
    } else if start > 0 {
        format!("[earlier output omitted]\n{text}")
    } else {
        text.into_owned()
    }
}

fn restore_log_tail(log: &mut fs::File) -> std::io::Result<String> {
    let length = log.seek(SeekFrom::End(0))?;
    log.seek(SeekFrom::Start(
        length.saturating_sub(DIAGNOSTIC_LIMIT as u64 + 1),
    ))?;
    let mut bytes = Vec::new();
    log.take(DIAGNOSTIC_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(diagnostic_tail(&bytes))
}

fn map_snapshot(snapshot: &mut Snapshot, sandbox: &crate::sandbox::Sandbox) -> Result<()> {
    let known: std::collections::HashSet<_> =
        snapshot.projects.iter().map(|p| p.origin.clone()).collect();
    let prefix = blake3::hash(sandbox.source.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    for project in &mut snapshot.projects {
        project.identity = format!("{prefix}:{}", project.identity);
        project.origin = sandbox.output(&project.origin)?;
        ensure!(
            project.origin.starts_with(&sandbox.source),
            "A project origin must belong to the canonical workspace"
        );
        for path in project.sources.iter_mut().chain(project.imports.iter_mut()) {
            *path = sandbox
                .output(path)
                .with_context(|| format!("Invalid returned discovery input {}", path.display()))?;
        }
        for path in &mut project.globs {
            *path = sandbox.watch_directory(path)?;
        }
        project
            .assemblies
            .retain(|a| !known.contains(Path::new(&a.source_project)));
        for assembly in &mut project.assemblies {
            assembly.path = sandbox.output(&assembly.path)?;
        }
        for reference in &mut project.references {
            reference.identity = format!("{prefix}:{}", reference.identity);
        }
        if let Some(assets) = project
            .properties
            .get_mut("ProjectAssetsFile")
            .filter(|s| !s.is_empty())
        {
            *assets = sandbox
                .output(Path::new(assets))?
                .to_string_lossy()
                .into_owned();
        }
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn log_tail_handles_empty_and_large_output() {
        let mut log = tempfile::tempfile().unwrap();
        assert!(!restore_log_tail(&mut log).unwrap().trim().is_empty());
        log.write_all(&vec![b'x'; DIAGNOSTIC_LIMIT * 2]).unwrap();
        let diagnostic = "final restore diagnostic";
        log.write_all(diagnostic.as_bytes()).unwrap();
        let tail = restore_log_tail(&mut log).unwrap();
        assert!(tail.ends_with(diagnostic));
        assert!(tail.len() < DIAGNOSTIC_LIMIT * 2);
    }
}
