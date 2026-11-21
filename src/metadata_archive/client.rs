use super::*;
mod materialization;
use crate::{
    acquisition,
    unity::{ReleaseBranch, UnityVersion},
};
pub(super) use materialization::many as materialize_sections;
use std::{
    io::Read,
    sync::{Arc, Condvar, LazyLock, Mutex},
    time::{Duration, Instant},
};

type CatalogResult = std::result::Result<Arc<Catalog>, Arc<anyhow::Error>>;
#[derive(Default)]
struct CatalogState {
    completed: Option<(Instant, CatalogResult)>,
    forced_at: Option<Instant>,
    fetching: bool,
}
#[derive(Default)]
struct CatalogCache {
    state: Mutex<CatalogState>,
    ready: Condvar,
}
static CATALOG: LazyLock<CatalogCache> = LazyLock::new(CatalogCache::default);

impl CatalogCache {
    fn current(
        &self,
        refresh: bool,
        clock: impl Fn() -> Instant,
        fetch: impl FnOnce() -> Result<Catalog>,
    ) -> Result<Arc<Catalog>> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.fetching {
                state = self.ready.wait(state).unwrap();
                continue;
            }
            if let Some((at, outcome)) = &state.completed {
                let now = clock();
                let fresh = if outcome.is_err() {
                    now.saturating_duration_since(*at) < Duration::from_secs(30)
                } else if refresh {
                    state.forced_at.is_some_and(|at| {
                        now.saturating_duration_since(at) < Duration::from_secs(30)
                    })
                } else {
                    now.saturating_duration_since(*at) < Duration::from_secs(600)
                };
                if fresh {
                    return outcome.clone().map_err(crate::diagnostics::shared_error);
                }
            }
            state.fetching = true;
            break;
        }
        drop(state);
        // A panic must also release waiters. Fetching never owns the state lock.
        let pending = CatalogFetch(self);
        let outcome = fetch().map(Arc::new).map_err(Arc::new);
        {
            let mut state = self.state.lock().unwrap();
            if outcome
                .as_ref()
                .is_err_and(|error| crate::diagnostics::cancelled(error))
            {
                state.completed = None;
                state.forced_at = None;
            } else {
                let now = clock();
                state.completed = Some((now, outcome.clone()));
                if refresh {
                    state.forced_at = Some(now);
                }
            }
        }
        drop(pending);
        outcome.map_err(crate::diagnostics::shared_error)
    }
}

struct CatalogFetch<'a>(&'a CatalogCache);
impl Drop for CatalogFetch<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().fetching = false;
        self.0.ready.notify_all();
    }
}

pub(super) fn catalog(repo: &str) -> Result<Catalog> {
    crate::telemetry::record("metadata_catalog", Some(repo), None, || fetch_catalog(repo))
}

fn fetch_catalog(repo: &str) -> Result<Catalog> {
    let url = format!("https://github.com/{repo}/releases/latest/download/catalog.bin");
    let client = acquisition::client()?;
    let mut response = client.get(acquisition::https(&url)?).send()?;
    for delay in [100, 250, 500] {
        if response.status() != reqwest::StatusCode::NOT_FOUND {
            break;
        }
        std::thread::sleep(Duration::from_millis(delay));
        response = client.get(acquisition::https(&url)?).send()?;
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        response = client
            .get(format!(
                "https://github.com/{repo}/releases/latest/download/catalog.retiring.bin"
            ))
            .send()?;
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        response = client
            .get(format!(
                "https://github.com/{repo}/releases/download/{TAG}/catalog.bin"
            ))
            .send()?;
    }
    let mut bytes = Vec::new();
    response
        .error_for_status()?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 64 * 1024 * 1024,
        "Catalog exceeds size bound"
    );
    let catalog = Catalog::decode(payload(&bytes)?)?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for artifact in &catalog.artifacts {
        safe(&artifact.name)?;
        safe(&artifact.release)?;
        ensure!(
            !artifact.name.contains('/')
                && !artifact.release.contains('/')
                && names.insert(&artifact.name),
            "Invalid catalog asset identity"
        );
        ensure!(
            artifact.manifest.entries.is_empty(),
            "Catalog contains bundle file inventories"
        );
        sections::validate(&artifact.sections, artifact.header_size, artifact.size)?;
    }
    let mut packed = std::collections::BTreeSet::new();
    for pack in &catalog.packs {
        safe(&pack.name)?;
        safe(&pack.release)?;
        ensure!(
            !pack.name.contains('/')
                && !pack.release.contains('/')
                && names.insert(&pack.name)
                && pack.members.len() >= 2
                && pack.size <= packing::MAX_PACK,
            "Invalid pack identity"
        );
        for member in &pack.members {
            ensure!(
                packed.insert(member)
                    && catalog
                        .artifacts
                        .iter()
                        .any(|a| &a.name == member && a.manifest.origin.kind == "package"),
                "Invalid pack membership"
            );
        }
    }
    Ok(())
}
fn current(refresh: bool) -> Result<Arc<Catalog>> {
    CATALOG.current(refresh, Instant::now, || catalog(REPOSITORY))
}
pub(super) fn download(repo: &str, release: &str, name: &str, output: &Path) -> Result<()> {
    download_with(repo, release, name, |response| {
        acquisition::transfer(response, &mut fs::File::create(output)?, None)
    })
}

fn download_with<T>(
    repo: &str,
    release: &str,
    name: &str,
    consume: impl FnOnce(reqwest::blocking::Response) -> Result<T>,
) -> Result<T> {
    crate::telemetry::record("metadata_download", Some(repo), Some(name), || {
        let started = Instant::now();
        let url = format!("https://github.com/{repo}/releases/download/{release}/{name}");
        let response = acquisition::client()?
            .get(acquisition::https(&url)?)
            .send()?
            .error_for_status()?;
        let result = consume(response)?;
        tracing::debug!(artifact = %name, elapsed_ms = started.elapsed().as_millis(), "Metadata asset downloaded");
        Ok(result)
    })
}

/// Cache the compressed members once, so requesting another member uses no network.
pub(super) fn packed_source(
    cache: &Path,
    repo: &str,
    catalog: &Catalog,
    artifact: &Artifact,
    leases: &crate::cache::views::LeaseSet,
) -> Result<Option<PathBuf>> {
    let Some(pack) = catalog
        .packs
        .iter()
        .find(|p| p.members.contains(&artifact.name))
    else {
        return Ok(None);
    };
    let root = cache.join("unity-metadata-packs").join(hex(&pack.hash));
    leases.retain(&root)?;
    let source = acquisition::shared_versioned(&root.to_string_lossy(), &pack.release, || {
        let contents = root.join("contents");
        if !contents.is_dir() {
            fs::create_dir_all(&root)?;
            let stage = tempfile::tempdir_in(&root)?;
            let path = stage.path().join("pack.tar");
            download(repo, &pack.release, &pack.name, &path)?;
            let output = stage.path().join("contents");
            let started = Instant::now();
            crate::telemetry::record("metadata_pack_unpack", None, Some(&pack.name), || {
                packing::unpack(&path, &output, pack, catalog)
            })?;
            tracing::debug!(artifact = %pack.name, elapsed_ms = started.elapsed().as_millis(), "Metadata pack unpacked");
            fs::rename(output, &contents)?;
        }
        Ok(Some(contents.join(&artifact.name)))
    })?;
    leases.record_metadata_use(&root);
    Ok(source)
}

#[cfg(test)]
pub(super) fn materialize(
    cache: &Path,
    artifact: &Artifact,
    catalog: &Catalog,
    leases: &crate::cache::views::LeaseSet,
) -> Result<PathBuf> {
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    leases.retain(&root)?;
    if root.join("contents/manifest.bin").is_file() {
        leases.record_metadata_use(&root);
        return Ok(root.join("contents"));
    }
    // Complete transport acquisition before taking a materialization permit.
    // Nested acquisition permits can deadlock concurrent package requests.
    let packed = packed_source(cache, REPOSITORY, catalog, artifact, leases)?;
    let source_version = catalog
        .packs
        .iter()
        .find(|p| p.members.contains(&artifact.name))
        .map_or(artifact.release.as_str(), |p| p.name.as_str());
    let contents = acquisition::shared_versioned(&root.to_string_lossy(), source_version, || {
        let contents = root.join("contents");
        if contents.join("manifest.bin").is_file() {
            return Ok(contents);
        }
        crate::telemetry::record("metadata_materialize", None, Some(&artifact.name), || {
            fs::create_dir_all(&root)?;
            let stage = tempfile::Builder::new()
                .prefix("stage-")
                .tempdir_in(&root)?;
            let output = stage.path().join("contents");
            let blobs = crate::cache::blobs::Store::open(cache)?;
            let mut import = blobs.owned_tree(&output)?;
            let started = Instant::now();
            let manifest =
                crate::telemetry::record("metadata_unpack", None, Some(&artifact.name), || {
                    if let Some(path) = packed {
                        bundle::unpack_stream(fs::File::open(path)?, &output, artifact, &mut import)
                    } else {
                        download_with(REPOSITORY, &artifact.release, &artifact.name, |response| {
                            acquisition::stream(response, |reader| {
                                bundle::unpack_stream(reader, &output, artifact, &mut import)
                            })
                        })
                    }
                })?;
            tracing::debug!(artifact = %artifact.name, elapsed_ms = started.elapsed().as_millis(), "Metadata bundle unpacked");
            if manifest.origin.kind == "editor" {
                import.alias(
                    &output.join("manifest.bin"),
                    &output.join("Editor/Data/manifest.bin"),
                )?;
            }
            let started = Instant::now();
            let stats =
                crate::telemetry::record("metadata_import", None, Some(&artifact.name), || {
                    import.publish()
                })?;
            tracing::debug!(artifact = %artifact.name, elapsed_ms = started.elapsed().as_millis(), files = stats.files, bytes = stats.hashed_bytes, adopted = stats.adopted, reused = stats.reused, "Metadata bundle imported");
            fs::rename(output, &contents)?;
            fs::File::open(&root)?.sync_all()?;
            Ok(contents)
        })
    })?;
    leases.record_metadata_use(&root);
    Ok(contents)
}

fn materialize_section(
    cache: &Path,
    artifact: &Artifact,
    catalog: &Catalog,
    section: &Section,
    leases: &crate::cache::views::LeaseSet,
) -> Result<PathBuf> {
    materialize_section_with(cache, artifact, catalog, section, leases, |start, end| {
        fetch_range(&acquisition::client()?, artifact, start, end)
    })
}

fn lazy_ranges() -> impl Fn(&Artifact, u64, u64) -> Result<reqwest::blocking::Response> {
    let client = std::cell::LazyCell::new(|| acquisition::client().map_err(Arc::new));
    move |artifact, start, end| {
        let client = client
            .as_ref()
            .map_err(|error| crate::diagnostics::shared_error(error.clone()))?;
        fetch_range(client, artifact, start, end)
    }
}

fn fetch_range(
    client: &reqwest::blocking::Client,
    artifact: &Artifact,
    start: u64,
    end: u64,
) -> Result<reqwest::blocking::Response> {
    let url = format!(
        "https://github.com/{REPOSITORY}/releases/download/{}/{}",
        artifact.release, artifact.name
    );
    Ok(client
        .get(acquisition::https(&url)?)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()?
        .error_for_status()?)
}

fn section_complete(destination: &Path, section: &Section) -> bool {
    (|| -> Result<bool> {
        let bytes = read(&destination.join("section.bin"))?;
        ensure!(
            blake3::hash(&bytes).as_bytes() == &section.manifest_hash,
            "Cached section inventory mismatch"
        );
        let frame: sections::Frame = binary::decode(payload(&bytes)?)?;
        ensure!(frame.role == section.role, "Cached section role mismatch");
        for entry in frame.entries.iter().filter(|entry| entry.group == "index") {
            if !crate::store::segment::Segment::compatible(
                &destination.join(&entry.path),
                entry.size,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    })()
    .unwrap_or(false)
}

pub(super) fn materialize_section_with(
    cache: &Path,
    artifact: &Artifact,
    catalog: &Catalog,
    section: &Section,
    leases: &crate::cache::views::LeaseSet,
    fetch: impl Fn(u64, u64) -> Result<reqwest::blocking::Response>,
) -> Result<PathBuf> {
    materialize_sections(cache, artifact, catalog, &[section], leases, fetch)
        .remove(&section.role)
        .unwrap()
}
fn select_editor(catalog: &Catalog, declared: UnityVersion) -> Result<&Artifact> {
    catalog
        .artifacts
        .iter()
        .filter(|a| a.manifest.origin.kind == "editor")
        .filter_map(|a| {
            a.manifest
                .origin
                .version
                .parse::<UnityVersion>()
                .ok()
                .filter(|v| *v >= declared)
                .map(|v| (v, a))
        })
        .min_by_key(|(v, _)| *v)
        .map(|(_, a)| a)
        .context("No equal or newer archived Unity editor is available")
}
struct AcquiredEditor {
    data: PathBuf,
    selected: Origin,
    archive: BTreeMap<String, Option<PathBuf>>,
    references: EditorReferences,
}

fn acquire_editor(
    cache: &Path,
    declared: UnityVersion,
    roles: &std::collections::BTreeSet<String>,
    leases: &crate::cache::views::LeaseSet,
) -> Result<AcquiredEditor> {
    let fetch = lazy_ranges();
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        let artifact = match select_editor(&catalog, declared) {
            Ok(artifact) => artifact,
            Err(_) if !refresh => continue,
            Err(error) => return Err(error),
        };
        let acquire = || -> Result<_> {
            let mut roots = BTreeMap::new();
            let selected: Vec<_> = artifact
                .sections
                .iter()
                .filter(|section| roles.contains(&section.role))
                .collect();
            let mut acquired = materialize_sections(
                cache,
                artifact,
                &catalog,
                &selected,
                leases,
                |start, end| fetch(artifact, start, end),
            );
            for role in roles {
                roots.insert(role.clone(), acquired.remove(role).transpose()?);
            }
            let data = roots
                .get("configuration")
                .and_then(Option::as_ref)
                .context("Archived editor has no configuration section")?
                .join("Editor/Data");
            Ok(AcquiredEditor {
                data,
                selected: artifact.manifest.origin.clone(),
                archive: roots,
                references: EditorReferences(catalog.clone()),
            })
        };
        match acquire() {
            Ok(editor) => return Ok(editor),
            Err(error) if !refresh && missing_asset(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}
fn missing_asset(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<reqwest::Error>())
        .any(|e| e.status().is_some_and(|s| matches!(s.as_u16(), 404 | 410)))
}

pub(crate) struct EditorReferences(pub(super) Arc<Catalog>);

pub(crate) fn editor_reference(
    editor: &crate::unity::catalog::Editor,
    name: &str,
    api: u32,
    cache: &Path,
    leases: &crate::cache::views::LeaseSet,
) -> Result<Option<PathBuf>> {
    let catalog = &editor
        .archive_references
        .as_ref()
        .context("Archived editor has no reference catalog")?
        .0;
    let paths = crate::unity::references::profile_paths(name, api);
    if paths.is_empty() {
        return Ok(None);
    }
    let artifact = catalog
        .artifacts
        .iter()
        .find(|artifact| {
            artifact.manifest.origin.kind == "editor"
                && editor
                    .data
                    .starts_with(cache.join("unity-metadata").join(hex(&artifact.hash)))
        })
        .context("Selected Unity editor is no longer in the metadata catalog")?;
    let group = if api == 6 { "standard" } else { "framework" };
    for path in paths {
        let path = Path::new("Editor/Data").join(path).with_extension("sigla");
        if let Some(root) = editor.archive.get(group).and_then(Option::as_ref) {
            let default = root.join(&path);
            if default.is_file() {
                return Ok(Some(default));
            }
        }
        let role = sections::assembly_role(group, path.to_str().context("Invalid reference path")?);
        if let Some(section) = artifact
            .sections
            .iter()
            .find(|section| section.role == role)
        {
            let root = materialize_section(cache, artifact, catalog, section, leases)?;
            let path = root.join(path);
            ensure!(
                path.is_file(),
                "Archived reference is missing from its section"
            );
            return Ok(Some(path));
        }
    }
    Ok(None)
}
pub fn prefetch(cache: &Path, branch: ReleaseBranch) -> Result<PathBuf> {
    let leases = crate::cache::views::LeaseSet::new(cache)?;
    let roles = ["configuration", "editor", "standard", "framework"]
        .map(String::from)
        .into();
    Ok(acquire_editor(cache, format!("{branch}.0a0").parse()?, &roles, &leases)?.data)
}
pub(crate) fn configured(path: &Path, branches: &[ReleaseBranch]) -> bool {
    if branches.is_empty() {
        return false;
    }
    let cached = CATALOG.state.lock().unwrap();
    let Some((at, Ok(catalog))) = &cached.completed else {
        // Until selection succeeds, any retained editor may be configured.
        return true;
    };
    if cached.fetching || at.elapsed() >= Duration::from_secs(600) {
        return true;
    }
    branches.iter().any(|branch| {
        format!("{branch}.0a0")
            .parse()
            .ok()
            .and_then(|version| select_editor(catalog, version).ok())
            .is_none_or(|artifact| {
                path.file_name()
                    .is_some_and(|name| name == hex(&artifact.hash).as_str())
            })
    })
}
pub(crate) fn editor(
    project: &Path,
    cache: &Path,
    platform: crate::unity::Platform,
    leases: &crate::cache::views::LeaseSet,
) -> Result<crate::unity::catalog::Editor> {
    let text = fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?;
    let declared = text
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .context("Unity project has no editor version")?
        .trim()
        .parse()?;
    let settings = crate::unity::settings::Settings::read(
        &project.join("ProjectSettings/ProjectSettings.asset"),
    )?;
    let mut roles: std::collections::BTreeSet<String> = [
        "configuration".into(),
        if platform == crate::unity::Platform::EditorLinux {
            "editor"
        } else if settings.backend == 0 {
            "player-mono"
        } else {
            "player-il2cpp"
        }
        .into(),
    ]
    .into();
    roles.insert(
        if settings.compilation_api(false) == 6 {
            "standard"
        } else {
            "framework"
        }
        .into(),
    );
    if platform == crate::unity::Platform::EditorLinux {
        roles.insert(
            if settings.compilation_api(true) == 6 {
                "standard"
            } else {
                "framework"
            }
            .into(),
        );
    }
    let AcquiredEditor {
        data,
        selected,
        archive,
        references,
    } = acquire_editor(cache, declared, &roles, leases).context(
        crate::diagnostics::DependencyUnavailable("Unity editor metadata is unavailable"),
    )?;
    Ok(crate::unity::catalog::Editor {
        declared,
        selected: selected.version.parse()?,
        data,
        archive,
        archive_references: Some(references),
        declared_revision: text
            .lines()
            .find_map(|l| l.strip_prefix("m_EditorVersionWithRevision:"))
            .map(|s| s.trim().to_owned()),
        selected_revision: Some(selected.revision),
    })
}
fn select_package<'a>(
    catalog: &'a Catalog,
    name: &str,
    requested: &str,
    editor: UnityVersion,
) -> Result<(&'a Artifact, &'a Package)> {
    let requested = (requested != "default")
        .then(|| registry::version(requested))
        .transpose()?;
    catalog
        .artifacts
        .iter()
        .filter(|a| {
            a.manifest.origin.kind == "package"
                || a.manifest.origin.kind == "editor"
                    && a.manifest.origin.version == editor.to_string()
        })
        .flat_map(|a| a.manifest.packages.iter().map(move |p| (a, p)))
        .filter(|(_, p)| p.name == name && registry::compatible(p, editor))
        .filter_map(|(a, p)| {
            registry::version(&p.version)
                .ok()
                .filter(|v| requested.as_ref().is_none_or(|r| v >= r))
                .map(|v| (v, a, p))
        })
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, a, p)| (a, p))
        .with_context(|| {
            format!(
                "No equal or newer compatible archived package {name}@{} for Unity {editor}",
                requested.map_or_else(|| "default".into(), |v| v.to_string())
            )
        })
}

pub(crate) struct PackagePlan {
    catalog: Arc<Catalog>,
    artifact: usize,
    pub info: Package,
}

impl PackagePlan {
    pub(crate) fn select(
        catalog: Arc<Catalog>,
        name: &str,
        requested: &str,
        editor: UnityVersion,
    ) -> Result<Self> {
        let (artifact, info) = select_package(&catalog, name, requested, editor)?;
        let artifact = catalog
            .artifacts
            .iter()
            .position(|candidate| std::ptr::eq(candidate, artifact))
            .unwrap();
        let info = info.clone();
        Ok(Self {
            catalog,
            artifact,
            info,
        })
    }
}

pub(crate) fn plan_package(
    name: &str,
    requested: &str,
    editor: UnityVersion,
) -> Result<PackagePlan> {
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        match PackagePlan::select(catalog, name, requested, editor) {
            Ok(plan) => return Ok(plan),
            Err(_) if !refresh => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

pub(crate) fn materialize_packages(
    cache: &Path,
    plans: &[&PackagePlan],
    leases: &crate::cache::views::LeaseSet,
) -> BTreeMap<String, Result<(PathBuf, String)>> {
    let fetch = lazy_ranges();
    let mut groups = BTreeMap::<[u8; 32], Vec<&PackagePlan>>::new();
    for plan in plans {
        groups
            .entry(plan.catalog.artifacts[plan.artifact].hash)
            .or_default()
            .push(plan);
    }
    let mut result = BTreeMap::new();
    for plans in groups.values() {
        let plan = plans[0];
        let artifact = &plan.catalog.artifacts[plan.artifact];
        let selected: Vec<_> = artifact
            .sections
            .iter()
            .filter(|section| {
                plans
                    .iter()
                    .any(|plan| section.role == format!("package/{}", plan.info.name))
            })
            .collect();
        let acquired = (|| -> Result<_> {
            let mut acquired = materialize_sections(
                cache,
                artifact,
                &plan.catalog,
                &selected,
                leases,
                |start, end| fetch(artifact, start, end),
            );
            if acquired
                .values()
                .any(|result| result.as_ref().is_err_and(missing_asset))
                && let Ok(updated) = current(true)
                && let Some(relocated) = updated
                    .artifacts
                    .iter()
                    .find(|candidate| candidate.hash == artifact.hash)
            {
                // A changed locator can repair acquisition without changing the
                // already resolved package graph. Different contents need rediscovery.
                ensure!(
                    relocated.manifest == artifact.manifest
                        && relocated.sections == artifact.sections,
                    "Relocated metadata changed its discovery inventory"
                );
                let retry: Vec<_> = relocated
                    .sections
                    .iter()
                    .filter(|section| acquired.get(&section.role).is_some_and(Result::is_err))
                    .collect();
                acquired.extend(materialize_sections(
                    cache,
                    relocated,
                    &updated,
                    &retry,
                    leases,
                    |start, end| fetch(relocated, start, end),
                ));
            }
            Ok(acquired)
        })();
        let mut acquired = match acquired {
            Ok(acquired) => acquired,
            Err(error) => {
                let error = Arc::new(error);
                for plan in plans {
                    result.insert(
                        plan.info.name.clone(),
                        Err(crate::diagnostics::shared_error(error.clone())),
                    );
                }
                continue;
            }
        };
        for plan in plans {
            let package = acquired
                .remove(&format!("package/{}", plan.info.name))
                .context("Archived package has no metadata section")
                .and_then(|value| value)
                .map(|root| {
                    let prefix = if artifact.manifest.origin.kind == "editor" {
                        "Editor/Data/Resources/PackageManager/BuiltInPackages"
                    } else {
                        "packages"
                    };
                    (
                        root.join(prefix).join(&plan.info.name),
                        format!(
                            "archive:{}:{}@{}",
                            hex(&artifact.hash),
                            plan.info.name,
                            plan.info.version
                        ),
                    )
                });
            result.insert(plan.info.name.clone(), package);
        }
    }
    result
}
pub fn package(
    cache: &Path,
    name: &str,
    requested: &str,
    editor: UnityVersion,
    leases: &crate::cache::views::LeaseSet,
) -> Result<(PathBuf, String)> {
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        let (artifact, info) = match select_package(&catalog, name, requested, editor) {
            Ok(selection) => selection,
            Err(_) if !refresh => continue,
            Err(error) => return Err(error),
        };
        let section = artifact
            .sections
            .iter()
            .find(|section| section.role == format!("package/{name}"))
            .context("Archived package has no metadata section")?;
        match materialize_section(cache, artifact, &catalog, section, leases) {
            Ok(contents) => {
                let prefix = if artifact.manifest.origin.kind == "editor" {
                    "Editor/Data/Resources/PackageManager/BuiltInPackages"
                } else {
                    "packages"
                };
                return Ok((
                    contents.join(prefix).join(name),
                    format!("archive:{}:{}@{}", hex(&artifact.hash), name, info.version),
                ));
            }
            Err(error) if !refresh && missing_asset(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_failures_and_forced_refreshes_share_a_cooldown() {
        let cache = CatalogCache::default();
        let start = Instant::now();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let unavailable = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(crate::diagnostics::DependencyUnavailable("unavailable").into())
        };
        assert!(cache.current(false, || start, unavailable).is_err());
        let error = cache.current(true, || start, unavailable).unwrap_err();
        assert!(
            error
                .chain()
                .any(|error| error.is::<crate::diagnostics::DependencyUnavailable>())
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let later = start + Duration::from_secs(60);
        let first = cache
            .current(true, || later, || Ok(Catalog::default()))
            .unwrap();
        let reused = cache
            .current(true, || later, || panic!("refresh was not coalesced"))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        let normal = cache
            .current(
                false,
                || later + Duration::from_secs(60),
                || panic!("catalog is still fresh"),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first, &normal));
        let refreshed = cache
            .current(
                true,
                || later + Duration::from_secs(60),
                || Ok(Catalog::default()),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &refreshed));
    }

    #[test]
    fn catalog_fetch_releases_the_lock_and_wakes_concurrent_readers() {
        let cache = CatalogCache::default();
        let started = std::sync::Barrier::new(2);
        let release = std::sync::Barrier::new(2);
        let now = Instant::now();
        std::thread::scope(|scope| {
            let leader = scope.spawn(|| {
                cache
                    .current(
                        false,
                        || now,
                        || {
                            started.wait();
                            release.wait();
                            Ok(Catalog::default())
                        },
                    )
                    .unwrap()
            });
            started.wait();
            assert!(cache.state.try_lock().unwrap().fetching);
            let followers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        cache
                            .current(false, || now, || panic!("duplicate catalog fetch"))
                            .unwrap()
                    })
                })
                .collect();
            release.wait();
            let catalog = leader.join().unwrap();
            for follower in followers {
                assert!(Arc::ptr_eq(&catalog, &follower.join().unwrap()));
            }
        });
    }

    #[test]
    fn catalog_fetch_panic_releases_admission() {
        let cache = CatalogCache::default();
        let panic = std::panic::catch_unwind(|| {
            cache.current(false, Instant::now, || panic!("fetch panic"))
        });
        assert!(panic.is_err());
        assert!(
            cache
                .current(false, Instant::now, || Ok(Catalog::default()))
                .is_ok()
        );
    }

    #[test]
    fn catalog_miss_can_refresh_a_recent_normal_fetch() {
        let cache = CatalogCache::default();
        let now = Instant::now();
        let old = cache
            .current(false, || now, || Ok(Catalog::default()))
            .unwrap();
        let new = cache
            .current(true, || now, || Ok(Catalog::default()))
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        let shared = cache
            .current(true, || now, || panic!("duplicate forced refresh"))
            .unwrap();
        assert!(Arc::ptr_eq(&new, &shared));
    }

    #[test]
    fn cancelled_catalog_fetch_does_not_delay_other_requests() {
        let cache = CatalogCache::default();
        let now = Instant::now();
        let error = cache
            .current(false, || now, || Err(crate::diagnostics::Cancelled.into()))
            .unwrap_err();
        assert!(crate::diagnostics::cancelled(&error));
        assert!(
            cache
                .current(false, || now, || Ok(Catalog::default()))
                .is_ok()
        );
    }

    #[test]
    fn package_selection_never_downgrades_or_ignores_editor_requirements() {
        let mut catalog = Catalog::default();
        for (version, minimum) in [
            ("2.0.0", None),
            ("2.2.0-preview.1", None),
            ("2.1.0", Some("7000.0.0a1")),
        ] {
            let package = Package {
                name: "com.example.test".into(),
                version: version.into(),
                minimum_editor: minimum.map(str::to_owned),
                dependencies: BTreeMap::new(),
            };
            catalog.artifacts.push(Artifact {
                name: version.into(),
                sections: Vec::new(),
                header_size: 0,
                hash: [0; 32],
                size: 0,
                release: TAG.into(),
                manifest: Manifest {
                    origin: Origin {
                        kind: "package".into(),
                        name: package.name.clone(),
                        version: version.into(),
                        revision: String::new(),
                        url: String::new(),
                        integrity: None,
                    },
                    packages: vec![package],
                    entries: vec![],
                    package_names: vec![],
                    recommended: BTreeMap::new(),
                },
            });
        }
        let editor = "6000.3.0f1".parse().unwrap();
        let (_, exact) = select_package(&catalog, "com.example.test", "2.0.0", editor).unwrap();
        assert_eq!(exact.version, "2.0.0");
        let (_, newer) = select_package(&catalog, "com.example.test", "2.0.1", editor).unwrap();
        assert_eq!(newer.version, "2.2.0-preview.1");
        assert!(select_package(&catalog, "com.example.test", "3.0.0", editor).is_err());
    }
}
