use super::*;
use crate::{
    acquisition,
    unity::{ReleaseBranch, UnityVersion},
};
use std::{
    io::Read,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

type CachedCatalog = Option<(Instant, Arc<Catalog>)>;
static CATALOG: LazyLock<Mutex<CachedCatalog>> = LazyLock::new(|| Mutex::new(None));

pub(super) fn catalog(repo: &str) -> Result<Catalog> {
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
    let mut cached = CATALOG
        .lock()
        .map_err(|_| anyhow::anyhow!("Catalog lock poisoned"))?;
    if !refresh
        && let Some((at, catalog)) = &*cached
        && at.elapsed() < Duration::from_secs(600)
    {
        return Ok(catalog.clone());
    }
    let catalog = Arc::new(catalog(REPOSITORY)?);
    *cached = Some((Instant::now(), catalog.clone()));
    Ok(catalog)
}
pub(super) fn download(repo: &str, release: &str, name: &str, output: &Path) -> Result<()> {
    let url = format!("https://github.com/{repo}/releases/download/{release}/{name}");
    let response = acquisition::client()?
        .get(acquisition::https(&url)?)
        .send()?
        .error_for_status()?;
    acquisition::transfer(response, &mut fs::File::create(output)?, None)?;
    Ok(())
}

/// Cache the compressed members once, so requesting another member uses no network.
pub(super) fn packed_source(
    cache: &Path,
    repo: &str,
    catalog: &Catalog,
    artifact: &Artifact,
) -> Result<Option<PathBuf>> {
    let Some(pack) = catalog
        .packs
        .iter()
        .find(|p| p.members.contains(&artifact.name))
    else {
        return Ok(None);
    };
    let root = cache.join("unity-metadata-packs").join(hex(&pack.hash));
    acquisition::shared_versioned(&root.to_string_lossy(), &pack.release, || {
        let contents = root.join("contents");
        if !contents.is_dir() {
            fs::create_dir_all(&root)?;
            let stage = tempfile::tempdir_in(&root)?;
            let path = stage.path().join("pack.tar");
            download(repo, &pack.release, &pack.name, &path)?;
            let output = stage.path().join("contents");
            packing::unpack(&path, &output, pack, catalog)?;
            fs::rename(output, &contents)?;
        }
        Ok(Some(contents.join(&artifact.name)))
    })
}

pub(super) fn materialize(cache: &Path, artifact: &Artifact, catalog: &Catalog) -> Result<PathBuf> {
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    if root.join("contents/manifest.bin").is_file() {
        return Ok(root.join("contents"));
    }
    // Complete transport acquisition before taking a materialization permit.
    // Nested acquisition permits can deadlock concurrent package requests.
    let packed = packed_source(cache, REPOSITORY, catalog, artifact)?;
    acquisition::shared_versioned(&root.to_string_lossy(), &artifact.release, || {
        let contents = root.join("contents");
        if contents.join("manifest.bin").is_file() {
            return Ok(contents);
        }
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
        let manifest = bundle::unpack(&archive, &output, Some(artifact))?;
        if manifest.origin.kind == "editor" {
            save(
                &output.join("Editor/Data/manifest.bin"),
                &binary::encode(&manifest)?,
            )?;
        }
        crate::cache::blobs::Store::open(cache)?.import_tree(&output)?;
        fs::rename(output, &contents)?;
        Ok(contents)
    })
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
fn acquire_editor(cache: &Path, declared: UnityVersion) -> Result<(PathBuf, Origin)> {
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        let artifact = select_editor(&catalog, declared)?;
        match materialize(cache, artifact, &catalog) {
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
    Ok(acquire_editor(cache, format!("{branch}.0a0").parse()?)?.0)
}
pub(crate) fn configured(path: &Path, branches: &[ReleaseBranch]) -> bool {
    let Ok(cached) = CATALOG.lock() else {
        return false;
    };
    let Some((_, catalog)) = &*cached else {
        return false;
    };
    branches.iter().any(|branch| {
        format!("{branch}.0a0")
            .parse()
            .ok()
            .and_then(|version| select_editor(catalog, version).ok())
            .is_some_and(|artifact| {
                path.file_name()
                    .is_some_and(|name| name == hex(&artifact.hash).as_str())
            })
    })
}
pub(crate) fn editor(project: &Path, cache: &Path) -> Result<crate::unity::catalog::Editor> {
    let text = fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?;
    let declared = text
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .context("Unity project has no editor version")?
        .trim()
        .parse()?;
    let (data, selected) = acquire_editor(cache, declared)?;
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
) -> Result<(PathBuf, String)> {
    for refresh in [false, true] {
        let catalog = current(refresh)?;
        let (artifact, info) = select_package(&catalog, name, requested, editor)?;
        match materialize(cache, artifact, &catalog) {
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
