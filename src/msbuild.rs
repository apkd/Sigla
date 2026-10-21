//! Extraction and invocation of the embedded, short-lived MSBuild integration.
use crate::process;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
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
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub excluded_entries: Vec<PathBuf>,
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

/// One discovery pass, with one remote context and metadata accumulator.
/// Discard after materialization requests; project evaluation can retry within the pass.
#[derive(Default)]
pub(crate) struct Session {
    scopes: BTreeMap<PathBuf, Scope>,
    bootstrap: Option<Result<Bootstrap, std::sync::Arc<anyhow::Error>>>,
    resolved: BTreeMap<Scope, PathBuf>,
}

struct Bootstrap {
    version: String,
    root: PathBuf,
    fxr: PathBuf,
    roots: Vec<PathBuf>,
    approved: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Scope {
    policy: Option<PathBuf>,
    contents: [u8; 32],
}

#[derive(Debug)]
pub(crate) struct SdkFailure(String);

impl std::fmt::Display for SdkFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SdkFailure {}

#[derive(Debug)]
struct SetupFailure(std::sync::Arc<anyhow::Error>);
impl std::fmt::Display for SetupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MSBuild setup failed: {}", self.0)
    }
}
impl std::error::Error for SetupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

pub(crate) fn project_retry_can_help(error: &anyhow::Error) -> bool {
    !error
        .chain()
        .any(|cause| cause.is::<SdkFailure>() || cause.is::<SetupFailure>())
}

/// Match hostfxr's ancestor search in the environment in which it will run.
/// Local builds deliberately retain host inputs; remote builds see /workspace.
impl Session {
    pub(crate) fn scope(
        &mut self,
        entry: &Path,
        remote: Option<&crate::discovery::RemoteContext>,
        metadata: &mut BTreeSet<PathBuf>,
    ) -> Result<Scope> {
        let mut visited = Vec::new();
        let mut selected = Scope {
            policy: None,
            contents: [0; 32],
        };
        for directory in entry
            .parent()
            .context("MSBuild entry has no parent")?
            .ancestors()
        {
            if let Some(scope) = self.scopes.get(directory) {
                selected = scope.clone();
                break;
            }
            visited.push(directory.to_owned());
            let policy = directory.join("global.json");
            #[cfg(test)]
            POLICY_PROBES.with(|calls| calls.set(calls.get() + 1));
            metadata.insert(policy.clone());
            if let Some(remote) = remote {
                remote.require([policy.clone()])?;
            }
            if policy.is_file() {
                #[cfg(test)]
                POLICY_READS.with(|calls| calls.set(calls.get() + 1));
                if let Some(remote) = remote {
                    ensure!(
                        policy
                            .canonicalize()?
                            .starts_with(remote.workspace.canonicalize()?),
                        "SDK policy escapes the repository: {}",
                        policy.display()
                    );
                }
                selected = Scope {
                    contents: *blake3::hash(&fs::read(&policy)?).as_bytes(),
                    policy: Some(policy),
                };
                break;
            }
            if remote.is_some_and(|remote| directory == remote.workspace) {
                break;
            }
        }
        for directory in visited {
            self.scopes.insert(directory, selected.clone());
        }
        Ok(selected)
    }
}

/// Cheap launcher identity, checked even when source watches have not changed.
/// Include absent SDK directories so an empty installation can recover.
pub(crate) fn environment_identity() -> Result<Vec<u8>> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let host = std::env::split_paths(&path)
        .map(|directory| directory.join("dotnet"))
        .find(|candidate| candidate.is_file());
    let resolved = host.as_ref().and_then(|host| host.canonicalize().ok());
    let stamp = host
        .as_ref()
        .and_then(|host| crate::workspace::Stamp::read(host).ok());
    let roots = ["DOTNET_ROOT", "DOTNET_ROOT_X64", "DOTNET_ROOT_ARM64"]
        .map(|name| std::env::var_os(name).map(PathBuf::from));
    let inventory = inventory_identity(
        host.iter()
            .chain(resolved.iter())
            .filter_map(|host| host.parent())
            .chain(roots.iter().flatten().map(PathBuf::as_path)),
    );
    Ok(serde_json::to_vec(&(
        path, host, resolved, stamp, roots, inventory,
    ))?)
}

fn inventory_identity<'a>(
    roots: impl Iterator<Item = &'a Path>,
) -> Vec<(PathBuf, Option<crate::workspace::Stamp>)> {
    roots
        .map(|root| root.join("sdk"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            let stamp = crate::workspace::Stamp::read_metadata(&path).ok();
            (path, stamp)
        })
        .collect()
}

pub(crate) fn discover_in(
    entries: &[PathBuf],
    cache: &Path,
    remote: Option<&crate::discovery::RemoteContext>,
    scope: &Scope,
    session: &mut Session,
    metadata: &mut BTreeSet<PathBuf>,
    leases: &crate::cache::views::LeaseSet,
) -> Result<Snapshot> {
    leases.retain(cache)?;
    if let Some(Err(error)) = &session.bootstrap {
        return Err(SetupFailure(error.clone()).into());
    }
    fs::create_dir_all(cache)?;
    let cache = cache.canonicalize()?;
    let cache = cache.as_path();
    let artifacts = cache.join("dotnet");
    if remote.is_none() {
        fs::create_dir_all(&artifacts)?;
        let path = artifacts.join("Artifacts.props");
        let props = include_bytes!("../managed/Artifacts.props");
        if fs::read(&path).ok().as_deref() != Some(props.as_slice()) {
            fs::write(path, props)?;
        }
    }
    let directory = entries
        .first()
        .context("MSBuild entry is missing")?
        .parent()
        .unwrap();
    let local_packages = remote
        .is_none()
        .then(|| crate::cache::packages::View::local(cache, directory, leases))
        .transpose()?;
    if session.bootstrap.is_none() {
        let bootstrap = (|| -> Result<Bootstrap> {
            for root in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
                let host = root.join("dotnet");
                if host.is_file() {
                    metadata.insert(host.clone());
                    let resolved = host.canonicalize()?;
                    let root = resolved.parent().context("dotnet host has no directory")?;
                    metadata.insert(root.to_owned());
                    if root.join("sdk").is_dir() {
                        metadata.insert(root.join("sdk"));
                    }
                    break;
                }
            }
            #[cfg(test)]
            INVENTORY_CALLS.with(|calls| calls.set(calls.get() + 1));
            let sdks = process::run(command(cache).arg("--list-sdks"), Duration::from_secs(30))?;
            let installed: Vec<(String, PathBuf)> = std::str::from_utf8(&sdks)?
                .lines()
                .filter_map(|line| {
                    let (version, location) = line.split_once(" [")?;
                    let major = version.split('.').next()?;
                    let location = PathBuf::from(location.trim_end_matches(']'));
                    metadata.insert(location.clone());
                    matches!(major, "8" | "10")
                        .then(|| (version.to_owned(), location.join(version)))
                })
                .collect();
            #[cfg(test)]
            BOOTSTRAP_CALLS.with(|calls| calls.set(calls.get() + 1));
            let (bootstrap_version, bootstrap_sdk) = installed
                .iter()
                .max_by_key(|(v, _)| semver::Version::parse(v).ok())
                .ok_or_else(|| {
                    SdkFailure(
                        "MSBuild discovery requires an installed .NET 8 or .NET 10 SDK".into(),
                    )
                })?;
            let host_root = bootstrap_sdk.parent().unwrap().parent().unwrap();
            let host = host_root.join("dotnet");
            metadata.insert(host.clone());
            metadata.insert(host_root.join("host/fxr"));
            let fxr = fs::read_dir(host_root.join("host/fxr"))?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .max_by_key(|e| semver::Version::parse(&e.file_name().to_string_lossy()).ok())
                .context("Installed hostfxr is missing")?
                .path()
                .join("libhostfxr.so");
            metadata.insert(fxr.clone());
            let roots = installed
                .iter()
                .map(|(_, p)| p.parent().unwrap().parent().unwrap().to_owned())
                .collect::<Vec<_>>();
            let mut approved = BTreeMap::new();
            for (version, path) in &installed {
                if let Ok(path) = path.canonicalize() {
                    approved.entry(path).or_insert_with(|| version.clone());
                }
            }
            Ok(Bootstrap {
                version: bootstrap_version.clone(),
                root: host_root.to_owned(),
                fxr,
                roots,
                approved,
            })
        })();
        session.bootstrap = Some(bootstrap.map_err(std::sync::Arc::new));
    }
    let bootstrap = session
        .bootstrap
        .as_ref()
        .unwrap()
        .as_ref()
        .map_err(|error| SetupFailure(error.clone()))?;
    let bootstrap_version = &bootstrap.version;
    let host_root = &bootstrap.root;
    let host = host_root.join("dotnet");
    let fxr = &bootstrap.fxr;
    let sandbox = remote
        .map(|context| crate::sandbox::Sandbox::new(context, &bootstrap.roots, leases))
        .transpose()
        .map_err(|error| SetupFailure(std::sync::Arc::new(error)))?;
    let job = tempfile::Builder::new()
        .prefix("discovery-")
        .tempdir_in(cache)?;
    let request = job.path().join("request.json");
    let output = job.path().join("result.json");
    let sdk = if let Some(sdk) = session.resolved.get(scope) {
        sdk.clone()
    } else {
        let sdk = (|| -> Result<PathBuf> {
            let integration = payload(cache, bootstrap_version.split('.').next().unwrap())?;
            let mut resolve = match &sandbox {
                Some(sandbox) => {
                    let mut command =
                        sandbox.command(&host, &integration, job.path(), directory, false)?;
                    command
                        .args(["/integration/Sigla.Discovery.dll", "--resolve-sdk"])
                        .arg(host_root)
                        .arg(sandbox.input(directory)?)
                        .arg(fxr)
                        .arg("/job/result.json");
                    command
                }
                None => {
                    let mut command = crate::sandbox::local_command(
                        &host,
                        directory,
                        cache,
                        job.path(),
                        local_packages.as_ref().unwrap(),
                    )?;
                    command
                        .current_dir(directory)
                        .arg(integration.join("Sigla.Discovery.dll"))
                        .arg("--resolve-sdk")
                        .arg(host_root)
                        .arg(directory)
                        .arg(fxr)
                        .arg(&output);
                    command
                }
            };
            #[cfg(test)]
            RESOLVER_CALLS.with(|calls| calls.set(calls.get() + 1));
            let resolution = process::capture(&mut resolve, Duration::from_secs(30), None, None)?;
            ensure!(
                resolution.status.success(),
                "SDK resolver helper failed ({}): {}",
                resolution.status,
                String::from_utf8_lossy(&resolution.stderr)
            );
            #[derive(Deserialize)]
            #[serde(rename_all = "PascalCase", deny_unknown_fields)]
            struct Sdk {
                sdk: Option<PathBuf>,
                status: i32,
            }
            let resolved: Sdk =
                serde_json::from_slice(&crate::sandbox::read_job_file(&output, 64 * 1024)?)?;
            ensure!(
                resolved.status != 0 || resolved.sdk.is_some(),
                "SDK resolver returned success without a selected SDK"
            );
            let sdk = resolved
                .sdk
                .filter(|_| resolved.status == 0)
                .ok_or_else(|| {
                    SdkFailure(format!(
                        "SDK policy resolution failed: {} [hostfxr status {:#x}, policy {}]",
                        String::from_utf8_lossy(&resolution.stderr).trim(),
                        resolved.status,
                        scope
                            .policy
                            .as_deref()
                            .map_or_else(|| "default".into(), |path| path.display().to_string()),
                    ))
                })?
                .canonicalize()?;
            if !bootstrap.approved.contains_key(&sdk) {
                return Err(SdkFailure(
                    "global.json selected a toolchain outside approved installed SDKs".into(),
                )
                .into());
            }
            Ok(sdk)
        })()
        .map_err(|error| SetupFailure(std::sync::Arc::new(error)))?;
        session.resolved.insert(scope.clone(), sdk.clone());
        sdk
    };
    let version = bootstrap
        .approved
        .get(&sdk)
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
        fs::read(&state_file)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
            .unwrap_or_else(|error| {
                tracing::warn!("Ignoring unreadable restore state: {error:#}");
                BTreeMap::new()
            })
    } else {
        BTreeMap::new()
    };
    let mut diagnostics = Vec::new();
    for restored in [false, true] {
        crate::sandbox::write_job_file(
            &request,
            &serde_json::to_vec(
                &serde_json::json!({"Entries": mapped_entries, "DependencyState": previous, "Restored": restored,
                    "ArtifactsPath": remote.is_none().then_some(&artifacts),
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
                let mut command = crate::sandbox::local_command(
                    &host,
                    directory,
                    cache,
                    job.path(),
                    local_packages.as_ref().unwrap(),
                )?;
                local_artifacts(&mut command, &artifacts);
                command
                    .arg(root.join("Sigla.Discovery.dll"))
                    .arg(&sdk)
                    .arg(&request)
                    .arg(&output);
                command
            }
        };
        process::run(&mut discovery, Duration::from_secs(120))?;
        match &sandbox {
            Some(s) => s.packages.seal(&host, &root)?,
            None => local_packages.as_ref().unwrap().seal(&host, &root)?,
        }
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
            snapshot.diagnostics.extend(diagnostics);
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
                None => {
                    let mut command = crate::sandbox::local_command(
                        &host,
                        directory,
                        cache,
                        job.path(),
                        local_packages.as_ref().unwrap(),
                    )?;
                    local_artifacts(&mut command, &artifacts);
                    command
                }
            };
            restore.arg(sdk.join("MSBuild.dll")).arg(entry).args([
                "-target:Restore",
                "-nodeReuse:false",
                "-nologo",
                "-m:1",
            ]);
            if remote.is_none() {
                // MSBuild treats commas and semicolons as property separators even within one argv entry.
                let path = artifacts
                    .to_str()
                    .context("Invalid artifacts path")?
                    .replace('%', "%25")
                    .replace(';', "%3B")
                    .replace(',', "%2C");
                restore
                    .args([
                        "-p:UseArtifactsOutput=true",
                        "-p:IncludeProjectNameInArtifactsPaths=true",
                    ])
                    .arg(format!("-p:ArtifactsPath={path}"));
            }
            let mut log = tempfile::tempfile()?;
            let captured = process::capture(
                &mut restore,
                Duration::from_secs(300),
                None,
                Some(log.try_clone()?),
            );
            if let Some(sandbox) = &sandbox {
                sandbox.validate_writes()?;
            }
            let result = match captured {
                Ok(result) => result,
                Err(error) => {
                    diagnostics.push(format!("Dependency restore could not finish for {}: {error:#}. Using available project details.", entry.display()));
                    continue;
                }
            };
            if !result.status.success() {
                let stdout = restore_log_tail(&mut log)
                    .unwrap_or_else(|error| format!("Cannot read restore log: {error}"));
                let stderr = diagnostic_tail(&result.stderr);
                tracing::warn!(project = %entry.display(), status = %result.status, %stdout, %stderr, "Dependency restore failed");
                let detail = format!("{stdout}\n{stderr}");
                diagnostics.push(format!(
                    "Dependency restore failed for {}: {}. Using available project details.",
                    entry.display(),
                    restore_reason(&detail),
                ));
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
thread_local! {
    static RESOLVER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static INVENTORY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static POLICY_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static POLICY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BOOTSTRAP_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn local_artifacts(command: &mut Command, artifacts: &Path) {
    if let Some(original) = std::env::var_os("CustomBeforeDirectoryBuildProps") {
        command.env("SiglaBeforeDirectoryBuildProps", original);
    }
    command.env(
        "CustomBeforeDirectoryBuildProps",
        artifacts.join("Artifacts.props"),
    );
    // Reuse already-restored host packages as read-only inputs; new downloads stay in the cache.
    let packages = std::env::var_os("NUGET_PACKAGES")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("DOTNET_CLI_HOME")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".nuget/packages"))
        });
    if let Some(packages) = packages.filter(|path| path.is_dir()) {
        command.env("SiglaFallbackPackages", packages);
    }
}

const DIAGNOSTIC_LIMIT: usize = 16 * 1024;

fn restore_reason(detail: &str) -> &'static str {
    if detail.contains("UntrustedRoot") || detail.contains("certificate verify failed") {
        "TLS certificate trust failed"
    } else if detail.contains("NU1301") {
        "package feed is unavailable"
    } else if detail.contains("NU1101") || detail.contains("NU1102") {
        "a required package or version was not found"
    } else {
        "see server logs for details"
    }
}

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
    for entry in &mut snapshot.excluded_entries {
        *entry = sandbox.output(entry)?;
        ensure!(
            entry.starts_with(&sandbox.source),
            "Excluded project must belong to the canonical workspace"
        );
    }
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
        for (paths, directory) in [
            (&mut project.sources, false),
            (&mut project.imports, false),
            (&mut project.globs, true),
        ] {
            paths.retain_mut(|path| {
                let mapped = if directory {
                    sandbox.watch_directory(path)
                } else {
                    sandbox.output(path)
                };
                match mapped {
                    Ok(mapped) => {
                        *path = mapped;
                        true
                    }
                    Err(error) => {
                        snapshot.diagnostics.push(format!(
                            "Excluded discovery input {}: {error:#}",
                            path.display()
                        ));
                        false
                    }
                }
            });
        }
        project
            .assemblies
            .retain(|a| !known.contains(Path::new(&a.source_project)));
        project
            .assemblies
            .retain_mut(|assembly| match sandbox.output(&assembly.path) {
                Ok(path) => {
                    assembly.path = path;
                    true
                }
                Err(error) => {
                    snapshot.diagnostics.push(format!(
                        "Excluded reference {}: {error:#}",
                        assembly.path.display()
                    ));
                    false
                }
            });
        for reference in &mut project.references {
            reference.identity = format!("{prefix}:{}", reference.identity);
        }
        if let Some(assets) = project
            .properties
            .get_mut("ProjectAssetsFile")
            .filter(|s| !s.is_empty())
        {
            match sandbox.output(Path::new(assets)) {
                Ok(path) => *assets = path.to_string_lossy().into_owned(),
                Err(error) => {
                    snapshot
                        .diagnostics
                        .push(format!("Unavailable dependency assets {assets}: {error:#}"));
                    assets.clear();
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_tests {
    fn scope(
        entry: &Path,
        remote: Option<&crate::discovery::RemoteContext>,
        metadata: &mut BTreeSet<PathBuf>,
    ) -> Result<Scope> {
        Session::default().scope(entry, remote, metadata)
    }

    #[test]
    fn scope_lookup_work_depends_on_directories_not_project_count() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("global.json"), "{}").unwrap();
        let mut session = Session::default();
        let mut metadata = BTreeSet::new();
        POLICY_PROBES.set(0);
        POLICY_READS.set(0);
        let first = session
            .scope(
                &root.path().join("nested/First.csproj"),
                None,
                &mut metadata,
            )
            .unwrap();
        let work = (POLICY_PROBES.get(), POLICY_READS.get());
        for index in 0..100 {
            assert_eq!(
                session
                    .scope(
                        &root.path().join(format!("nested/Project{index}.csproj")),
                        None,
                        &mut metadata
                    )
                    .unwrap(),
                first
            );
        }
        assert_eq!((POLICY_PROBES.get(), POLICY_READS.get()), work);
        fs::write(root.path().join("nested/global.json"), "{}").unwrap();
        assert_ne!(
            Session::default()
                .scope(
                    &root.path().join("nested/First.csproj"),
                    None,
                    &mut metadata
                )
                .unwrap(),
            first
        );
    }

    #[test]
    #[ignore = "release benchmark; requires installed SDK"]
    fn benchmark_shared_sdk_policies() {
        for count in [100, 1000] {
            for nested in [false, true] {
                let fixture = tempfile::tempdir().unwrap();
                let cache = tempfile::tempdir().unwrap();
                let root = fixture.path();
                fs::write(
                    root.join("global.json"),
                    r#"{"sdk":{"version":"9999.0.100","rollForward":"disable"}}"#,
                )
                .unwrap();
                for index in 0..count {
                    let directory = root.join(format!("group{}", index % 10));
                    fs::create_dir_all(&directory).unwrap();
                    fs::write(
                        directory.join(format!("Project{index}.csproj")),
                        "<Project />",
                    )
                    .unwrap();
                    if index < 10 {
                        fs::write(directory.join("Source.cs"), "class Readable {}").unwrap();
                        if nested && index % 2 == 0 {
                            fs::copy(root.join("global.json"), directory.join("global.json"))
                                .unwrap();
                        }
                    }
                }
                POLICY_PROBES.with(|calls| calls.set(0));
                POLICY_READS.with(|calls| calls.set(0));
                INVENTORY_CALLS.with(|calls| calls.set(0));
                RESOLVER_CALLS.with(|calls| calls.set(0));
                BOOTSTRAP_CALLS.with(|calls| calls.set(0));
                let policy = crate::discovery::Policy::new(vec![root.into()]).unwrap();
                let started = std::time::Instant::now();
                let result =
                    crate::discovery::discover_cached(root, &policy, cache.path()).unwrap();
                eprintln!(
                    "sdk entries={count} nested={nested} elapsed_us={} probes={} policy_reads={} inventories={} resolvers={} bootstraps={} sources={}",
                    started.elapsed().as_micros(),
                    POLICY_PROBES.get(),
                    POLICY_READS.get(),
                    INVENTORY_CALLS.get(),
                    RESOLVER_CALLS.get(),
                    BOOTSTRAP_CALLS.get(),
                    result.sources.len()
                );
                assert_eq!(
                    result
                        .sources
                        .iter()
                        .filter(|source| source.language == crate::model::Language::CSharp)
                        .count(),
                    10
                );
            }
        }
    }

    #[test]
    fn empty_host_inventory_changes_when_an_sdk_is_installed() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("dotnet");
        fs::write(&host, "host").unwrap();
        let host_stamp = crate::workspace::Stamp::read(&host).unwrap();
        let identity = || inventory_identity(std::iter::once(root.path()));
        let absent = identity();
        fs::create_dir(root.path().join("sdk")).unwrap();
        let empty = identity();
        assert_ne!(absent, empty);
        fs::create_dir(root.path().join("sdk/toolchain")).unwrap();
        assert_ne!(empty, identity());
        assert_eq!(host_stamp, crate::workspace::Stamp::read(&host).unwrap());
    }

    #[test]
    fn ancestor_policy_watches_ignore_unrelated_sibling_changes() {
        let fixture = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = fixture.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(
            fixture.path().join("global.json"),
            r#"{"sdk":{"version":"9999.0.100","rollForward":"disable"}}"#,
        )
        .unwrap();
        fs::write(root.join("App.csproj"), "<Project />").unwrap();
        fs::write(root.join("Source.cs"), "class Source {}").unwrap();
        let mut workspace = crate::workspace::Workspace::open(
            root.clone(),
            cache.path(),
            crate::discovery::Policy::new(vec![root]).unwrap(),
            &cache.path().join("analysis"),
            None,
            Default::default(),
        )
        .unwrap();
        workspace.refresh().unwrap();
        fs::write(fixture.path().join("unrelated.txt"), "new sibling").unwrap();
        assert!(workspace.prepare().unwrap().is_none());
        fs::write(fixture.path().join("global.json"), "{}").unwrap();
        assert!(workspace.prepare().unwrap().is_some());
    }

    #[test]
    fn sdk_scopes_track_nearer_policy_absence_and_remote_inputs() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let policy = root.join("global.json");
        fs::write(&policy, "{}").unwrap();
        let entry = nested.join("App.csproj");
        let mut metadata = BTreeSet::new();
        let parent = scope(&entry, None, &mut metadata).unwrap();
        assert_eq!(parent.policy.as_ref(), Some(&policy));
        assert!(metadata.contains(&nested.join("global.json")));
        let absent = crate::workspace::Stamp::read_metadata(&nested.join("global.json")).unwrap();
        fs::write(nested.join("unrelated.txt"), "unrelated").unwrap();
        assert_eq!(
            crate::workspace::Stamp::read_metadata(&nested.join("global.json")).unwrap(),
            absent
        );
        fs::write(nested.join("global.json"), "{}").unwrap();
        assert_ne!(scope(&entry, None, &mut metadata).unwrap(), parent);
        assert_ne!(
            crate::workspace::Stamp::read_metadata(&nested.join("global.json")).unwrap(),
            absent
        );
        fs::remove_file(nested.join("global.json")).unwrap();
        let remote = crate::discovery::RemoteContext {
            workspace: root.clone(),
            writable: fixture.path().join("writable"),
            shared: fixture.path().join("cache"),
            repositories: Vec::new(),
            selection_identity: String::new(),
            tracked: std::sync::Arc::new(["nested/global.json".into()].into_iter().collect()),
        };
        let mut remote_session = Session::default();
        let error = remote_session
            .scope(&entry, Some(&remote), &mut metadata)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::discovery::RequiredInputs>()
                .is_some()
        );
        fs::write(nested.join("global.json"), "{}").unwrap();
        assert!(remote_session.scopes.is_empty());
        assert!(
            Session::default()
                .scope(&entry, Some(&remote), &mut metadata)
                .is_ok()
        );
        fs::remove_file(nested.join("global.json")).unwrap();
        fs::remove_file(policy).unwrap();
        fs::write(fixture.path().join("global.json"), "{}").unwrap();
        let mut remote = remote;
        remote.tracked = Default::default();
        assert!(
            scope(&entry, Some(&remote), &mut metadata)
                .unwrap()
                .policy
                .is_none()
        );
        assert!(scope(&entry, None, &mut metadata).unwrap().policy.is_some());
    }

    #[test]
    fn unavailable_sdk_resolution_scales_with_policy_scopes() {
        let fixture = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::write(
            root.join("global.json"),
            r#"{"sdk":{"version":"9999.0.100","rollForward":"disable"}}"#,
        )
        .unwrap();
        for directory in ["one", "two", "two/nested"] {
            fs::create_dir_all(root.join(directory)).unwrap();
            for project in ["A", "B"] {
                fs::write(
                    root.join(directory).join(format!("{project}.csproj")),
                    "<Project />",
                )
                .unwrap();
            }
            fs::write(root.join(directory).join("Source.cs"), "class Readable {}").unwrap();
        }
        RESOLVER_CALLS.with(|calls| calls.set(0));
        INVENTORY_CALLS.with(|calls| calls.set(0));
        let policy = crate::discovery::Policy::new(vec![root.into()]).unwrap();
        let found = crate::discovery::discover_cached(root, &policy, cache.path()).unwrap();
        RESOLVER_CALLS.with(|calls| assert_eq!(calls.get(), 1));
        INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 1));
        assert!(!found.diagnostics.is_empty());
        assert!(
            found
                .sources
                .iter()
                .any(|source| source.path.ends_with("Source.cs"))
        );
        assert!(
            found
                .projects
                .iter()
                .filter(|project| project.identity.starts_with("fallback:"))
                .all(|project| project.assemblies.is_empty())
        );
        // A second policy is a separate resolver scope even with identical contents.
        fs::copy(root.join("global.json"), root.join("two/global.json")).unwrap();
        RESOLVER_CALLS.with(|calls| calls.set(0));
        INVENTORY_CALLS.with(|calls| calls.set(0));
        BOOTSTRAP_CALLS.set(0);
        crate::discovery::discover_cached(root, &policy, cache.path()).unwrap();
        RESOLVER_CALLS.with(|calls| assert_eq!(calls.get(), 2));
        INVENTORY_CALLS.with(|calls| assert_eq!(calls.get(), 1));
        assert_eq!(BOOTSTRAP_CALLS.get(), 1);
    }

    #[test]
    fn restore_summaries_keep_the_cause_without_repeating_logs() {
        let verbose = "error NU1301: Unable to load service index\nThe SSL connection could not be established: UntrustedRoot\n".repeat(100);
        let summary = super::restore_reason(&verbose);
        assert!(summary.contains("certificate"));
        assert!(!summary.contains('\n'));
        assert!(summary.len() < verbose.len());
        let missing_package = "X.509 certificate chain validation will use the fallback certificate bundle\nerror NU1101: Unable to find package";
        assert!(super::restore_reason(missing_package).contains("package"));
    }
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
