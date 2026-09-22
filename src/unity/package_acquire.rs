use crate::{acquisition, repository::manager::write_json};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
struct RegistryPackage {
    name: String,
    version: String,
    dist: Distribution,
}
#[derive(Deserialize)]
struct RegistryMetadata {
    versions: std::collections::BTreeMap<String, RegistryPackage>,
}
#[derive(Deserialize)]
struct Distribution {
    tarball: String,
    integrity: Option<String>,
    shasum: Option<String>,
}

fn credential(registry: &url::Url) -> Result<Option<String>> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let path = PathBuf::from(home).join(".upmconfig.toml");
    if !path.is_file() {
        return Ok(None);
    }
    let configuration: toml::Value = toml::from_str(&fs::read_to_string(path)?)?;
    let Some(auth) = configuration.get("npmAuth").and_then(toml::Value::as_table) else {
        return Ok(None);
    };
    for (source, values) in auth {
        let Ok(source) = acquisition::https(source) else {
            continue;
        };
        if source.origin() == registry.origin()
            && source.path().trim_end_matches('/') == registry.path().trim_end_matches('/')
        {
            return Ok(values
                .get("token")
                .and_then(toml::Value::as_str)
                .map(str::to_owned));
        }
    }
    Ok(None)
}

fn check(root: &Path, name: &str, version: Option<&str>) -> Result<()> {
    let manifest: super::packages::PackageManifest =
        serde_json::from_slice(&fs::read(root.join("package.json"))?)?;
    ensure!(
        manifest.name == name && version.is_none_or(|v| manifest.version == v),
        "Acquired package manifest does not match its locked source identity"
    );
    Ok(())
}

pub fn complete(root: &Path, identity: &str) -> bool {
    let read = || -> Result<bool> {
        let saved: String = serde_json::from_slice(&fs::read(root.join("identity.json"))?)?;
        let inventory: Vec<String> =
            serde_json::from_slice(&fs::read(root.join("inventory.json"))?)?;
        Ok(saved == identity
            && inventory
                .iter()
                .all(|p| root.join("contents").join(p).is_file()))
    };
    read().unwrap_or(false)
}

fn unpack(
    archive: File,
    cache: &Path,
    identity: &str,
    name: &str,
    version: Option<&str>,
) -> Result<PathBuf> {
    let stage = tempfile::Builder::new()
        .prefix("package-")
        .tempdir_in(cache)?;
    let inventory = acquisition::extract(
        flate2::read::GzDecoder::new(archive),
        stage.path(),
        acquisition::analysis_input,
    )?;
    let package = stage.path().join("package");
    let package = if package.is_dir() {
        package
    } else {
        stage.path().to_owned()
    };
    check(&package, name, version)?;
    let contents = cache.join("contents");
    if contents.exists() {
        fs::remove_dir_all(&contents)?;
    }
    let prefix = package.strip_prefix(stage.path())?;
    let inventory = inventory
        .iter()
        .map(|p| {
            Path::new(p)
                .strip_prefix(prefix)
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    fs::rename(package, &contents)?;
    write_json(&cache.join("inventory.json"), &inventory)?;
    write_json(&cache.join("identity.json"), &identity)?;
    Ok(contents)
}

pub fn registry(
    cache: &Path,
    identity: &str,
    registry: &str,
    name: &str,
    version: &str,
) -> Result<PathBuf> {
    acquisition::shared(&cache.to_string_lossy(), || {
        if complete(cache, identity) {
            return Ok(cache.join("contents"));
        }
        fs::create_dir_all(cache)?;
        let registry = acquisition::https(registry)?;
        let token = credential(&registry)?;
        let mut metadata = registry.clone();
        metadata
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid registry URL"))?
            .pop_if_empty()
            .push(name);
        let client = acquisition::client()?;
        let mut request = client.get(metadata);
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|_| anyhow::anyhow!("Package registry connection failed"))?;
        ensure!(
            response.status().is_success(),
            "Package registry returned HTTP {}",
            response.status().as_u16()
        );
        // Unity's registry exposes the version map at the package endpoint;
        // unlike npmjs, its /package/version route returns 404.
        let mut metadata: RegistryMetadata = response.json()?;
        let package = metadata
            .versions
            .remove(version)
            .context("Registry has no metadata for the locked version")?;
        ensure!(
            package.name == name && package.version == version,
            "Registry metadata does not match locked package"
        );
        let mut archive = tempfile::tempfile_in(cache)?;
        let integrity = package.dist.integrity.or_else(|| {
            package.dist.shasum.map(|s| {
                format!(
                    "sha1-{}",
                    base64::engine::general_purpose::STANDARD.encode(s)
                )
            })
        });
        let download = acquisition::https(&package.dist.tarball)?;
        let mut request = client.get(download.clone());
        if download.origin() == registry.origin()
            && let Some(token) = &token
        {
            request = request.bearer_auth(token);
        }
        acquisition::transfer(
            request
                .send()
                .map_err(|_| anyhow::anyhow!("Package archive connection failed"))?,
            &mut archive,
            integrity.as_deref(),
        )?;
        archive.seek(SeekFrom::Start(0))?;
        unpack(archive, cache, identity, name, Some(version))
    })
}

pub fn local_archive(cache: &Path, path: &Path, name: &str) -> Result<PathBuf> {
    let mut archive = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes = [0; 128 * 1024];
    loop {
        let n = archive.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hasher.update(&bytes[..n]);
    }
    let identity = format!("archive:{}:{}", path.display(), hasher.finalize());
    let root = cache
        .join("unity-packages")
        .join(blake3::hash(identity.as_bytes()).to_hex().as_str());
    acquisition::shared(&root.to_string_lossy(), || {
        if complete(&root, &identity) {
            return Ok(root.join("contents"));
        }
        fs::create_dir_all(&root)?;
        archive.seek(SeekFrom::Start(0))?;
        unpack(archive, &root, &identity, name, None).context("Cannot read local package archive")
    })
}

pub struct GitSource {
    repository: crate::repository::Repository,
    commit: String,
    subdirectory: Option<String>,
    pub identity: String,
}

impl GitSource {
    pub fn parse(request: &str, commit: &str) -> Result<Self> {
        gix_hash::ObjectId::from_hex(commit.as_bytes())
            .context("Git package lock does not contain a commit identity")?;
        let request = request.strip_prefix("git+").unwrap_or(request);
        let request = request.split('#').next().unwrap();
        let (address, query) = request.split_once('?').unwrap_or((request, ""));
        let mut subdirectory = None;
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            ensure!(
                key == "path" && subdirectory.is_none(),
                "Unsupported Git package URL parameter"
            );
            let path = value.trim_start_matches('/');
            crate::repository::selection::validate_path(path)?;
            subdirectory = Some(path.to_owned());
        }
        let repository = crate::repository::Repository::parse(address)?
            .context("Git package requires a supported repository URL")?;
        let identity =
            serde_json::to_string(&("git", &repository.identity, commit, &subdirectory))?;
        Ok(Self {
            repository,
            commit: commit.to_owned(),
            subdirectory,
            identity,
        })
    }

    pub fn acquire(
        &self,
        cache: &Path,
        name: &str,
        remote: Option<&crate::discovery::RemoteContext>,
    ) -> Result<PathBuf> {
        if let Some(remote) = remote {
            crate::repository::authorize(&remote.repositories, &self.repository)?;
        }
        let root = cache
            .join("unity-packages")
            .join(blake3::hash(self.identity.as_bytes()).to_hex().as_str());
        acquisition::shared(&root.to_string_lossy(), || {
            if complete(&root, &self.identity) {
                return Ok(root.join("contents"));
            }
            ensure!(
                remote.is_some(),
                "No matching offline Git package contents are available"
            );
            fs::create_dir_all(&root)?;
            let stage = tempfile::Builder::new()
                .prefix("git-package-")
                .tempdir_in(&root)?;
            let sources = stage.path().join("sources");
            let request = crate::repository::materialize::Request {
                repository: self.repository.transport.clone(),
                target: crate::repository::materialize::Target::Commit(self.commit.clone()),
                store: stage.path().join("git"),
                staging: sources.clone(),
                include: vec!["**/*.dll".into()],
                exclude: vec![],
                required: vec![],
                additional: vec![],
                previous: Default::default(),
                subdirectory: self.subdirectory.clone(),
            };
            let prepared = crate::repository::job::execute(&request, cache)?;
            let package = self
                .subdirectory
                .as_ref()
                .map_or_else(|| sources.clone(), |p| sources.join(p));
            check(&package, name, None)?;
            let inventory = prepared
                .selected
                .keys()
                .map(|p| {
                    self.subdirectory
                        .as_ref()
                        .map_or_else(
                            || Ok(Path::new(p)),
                            |prefix| Path::new(p).strip_prefix(prefix),
                        )
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let contents = root.join("contents");
            if contents.exists() {
                fs::remove_dir_all(&contents)?;
            }
            fs::rename(package, &contents)?;
            write_json(&root.join("inventory.json"), &inventory)?;
            write_json(&root.join("identity.json"), &self.identity)?;
            Ok(contents)
        })
    }
}
