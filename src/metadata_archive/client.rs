use super::*;
use crate::{
    acquisition,
    unity::{ReleaseBranch, UnityVersion},
};
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
    crate::telemetry::record("metadata_download", Some(repo), Some(name), || {
        let started = Instant::now();
        let url = format!("https://github.com/{repo}/releases/download/{release}/{name}");
        let response = acquisition::client()?
            .get(acquisition::https(&url)?)
            .send()?
            .error_for_status()?;
        acquisition::transfer(response, &mut fs::File::create(output)?, None)?;
        tracing::debug!(artifact = %name, elapsed_ms = started.elapsed().as_millis(), "Metadata asset downloaded");
        Ok(())
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
            let archive = if let Some(path) = packed {
                path
            } else {
                let path = stage.path().join("bundle.tar.zst");
                download(REPOSITORY, &artifact.release, &artifact.name, &path)?;
                path
            };
            let output = stage.path().join("contents");
            let started = Instant::now();
            let manifest =
                crate::telemetry::record("metadata_unpack", None, Some(&artifact.name), || {
                    bundle::unpack(&archive, &output, Some(artifact))
                })?;
            tracing::debug!(artifact = %artifact.name, elapsed_ms = started.elapsed().as_millis(), "Metadata bundle unpacked");
            if manifest.origin.kind == "editor" {
                save(
                    &output.join("Editor/Data/manifest.bin"),
                    &binary::encode(&manifest)?,
                )?;
            }
            let started = Instant::now();
            let stats =
                crate::telemetry::record("metadata_import", None, Some(&artifact.name), || {
                    crate::cache::blobs::Store::open(cache)?.import_owned_tree(&output)
                })?;
            tracing::debug!(artifact = %artifact.name, elapsed_ms = started.elapsed().as_millis(), files = stats.files, bytes = stats.hashed_bytes, adopted = stats.adopted, reused = stats.reused, "Metadata bundle imported");
            fs::rename(output, &contents)?;
            Ok(contents)
        })
    })?;
    leases.record_metadata_use(&root);
    Ok(contents)
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
fn acquire_editor(
    cache: &Path,
    declared: UnityVersion,
    leases: &crate::cache::views::LeaseSet,
) -> Result<(PathBuf, Origin)> {
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        let artifact = match select_editor(&catalog, declared) {
            Ok(artifact) => artifact,
            Err(_) if !refresh => continue,
            Err(error) => return Err(error),
        };
        match materialize(cache, artifact, &catalog, leases) {
            Ok(contents) => {
                return Ok((
                    contents.join("Editor/Data"),
                    artifact.manifest.origin.clone(),
                ));
            }
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
pub fn prefetch(cache: &Path, branch: ReleaseBranch) -> Result<PathBuf> {
    let leases = crate::cache::views::LeaseSet::new(cache)?;
    Ok(acquire_editor(cache, format!("{branch}.0a0").parse()?, &leases)?.0)
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
    leases: &crate::cache::views::LeaseSet,
) -> Result<crate::unity::catalog::Editor> {
    let text = fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?;
    let declared = text
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .context("Unity project has no editor version")?
        .trim()
        .parse()?;
    let (data, selected) = acquire_editor(cache, declared, leases).context(
        crate::diagnostics::DependencyUnavailable("Unity editor metadata is unavailable"),
    )?;
    Ok(crate::unity::catalog::Editor {
        declared,
        selected: selected.version.parse()?,
        data,
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
        match materialize(cache, artifact, &catalog, leases) {
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
