use super::catalog::Editor;
use crate::discovery::Policy;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest<T = String> {
    dependencies: BTreeMap<String, T>,
    #[serde(default)]
    scoped_registries: Vec<Registry>,
    #[serde(default)]
    testables: Vec<String>,
}
#[derive(Deserialize)]
struct Registry {
    url: String,
    scopes: Vec<String>,
}
#[derive(Deserialize)]
struct Lock<T> {
    dependencies: BTreeMap<String, T>,
}
#[derive(Deserialize)]
struct Locked {
    version: String,
    source: String,
    depth: u32,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    url: Option<String>,
    hash: Option<String>,
}
#[derive(Clone, Deserialize)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
}
pub struct Package {
    pub manifest: PackageManifest,
    pub root: PathBuf,
    pub identity: String,
    pub testable: bool,
}
pub struct Packages {
    pub selected: Vec<Package>,
    pub diagnostics: Vec<String>,
    pub watched: BTreeSet<PathBuf>,
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    super::settings::json(path)
        .with_context(|| format!("Malformed package input {}", path.display()))
}
fn name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn package(
    root: PathBuf,
    expected: &str,
    _version: Option<&str>,
    identity: String,
    testable: bool,
) -> Result<Package> {
    let manifest: PackageManifest = read(&root.join("package.json"))?;
    ensure!(
        manifest.name == expected && !manifest.version.is_empty(),
        "Package manifest identity does not match {expected}"
    );
    Ok(Package {
        manifest,
        root: root.canonicalize()?,
        identity,
        testable,
    })
}

impl Packages {
    pub fn local(root: &Path, policy: &Policy, editor: &Editor, cache: &Path) -> Result<Self> {
        let cache = policy
            .remote
            .as_ref()
            .map_or(cache, |remote| remote.shared.as_path());
        let manifest_path = root.join("Packages/manifest.json");
        let lock_path = root.join("Packages/packages-lock.json");
        let mut diagnostics = Vec::new();
        let raw: Manifest<serde_json::Value> = read(&manifest_path).unwrap_or_else(|error| {
            diagnostics.push(format!(
                "Unavailable package manifest: {error:#}. Discovering embedded packages."
            ));
            Manifest::default()
        });
        let manifest = Manifest {
            dependencies: raw
                .dependencies
                .into_iter()
                .filter_map(|(package, value)| {
                    match value
                        .as_str()
                        .filter(|request| !request.is_empty() && name(&package))
                    {
                        Some(request) => Some((package, request.to_owned())),
                        None => {
                            diagnostics.push(format!("Excluded invalid package request {package}"));
                            None
                        }
                    }
                })
                .collect::<BTreeMap<_, _>>(),
            scoped_registries: raw.scoped_registries,
            testables: raw.testables,
        };
        let raw: Lock<serde_json::Value> = read(&lock_path).unwrap_or_else(|error| {
            diagnostics.push(format!(
                "Unavailable package lock: {error:#}. Locked dependencies remain unresolved."
            ));
            Lock {
                dependencies: BTreeMap::new(),
            }
        });
        let mut dependencies = BTreeMap::new();
        for (package, value) in raw.dependencies {
            let parsed = serde_json::from_value::<Locked>(value)
                .map_err(anyhow::Error::from)
                .and_then(|node| {
                    ensure!(
                        name(&package)
                            && !node.version.is_empty()
                            && matches!(
                                node.source.as_str(),
                                "registry" | "git" | "local" | "embedded" | "builtin"
                            ),
                        "Invalid locked package"
                    );
                    ensure!(
                        node.dependencies.keys().all(|d| name(d)),
                        "Invalid package dependency name"
                    );
                    Ok(node)
                });
            match parsed {
                Ok(node) => {
                    dependencies.insert(package, node);
                }
                Err(error) => diagnostics.push(format!("Excluded package {package}: {error:#}")),
            }
        }
        let lock = Lock { dependencies };
        let mut watched = BTreeSet::from([manifest_path, lock_path, root.join("Packages")]);
        let mut embedded = BTreeMap::new();
        for (path, kind) in super::entries(&root.join("Packages"), &mut diagnostics) {
            if kind.is_dir() && path.join("package.json").is_file() {
                watched.insert(path.join("package.json"));
                let loaded = (|| -> Result<Package> {
                    let path = policy.canonical(&path)?;
                    let info: PackageManifest = read(&path.join("package.json"))?;
                    ensure!(name(&info.name), "Invalid embedded package name");
                    package(
                        path,
                        &info.name,
                        None,
                        format!("embedded:{}", info.name),
                        true,
                    )
                })();
                match loaded {
                    Ok(value) => {
                        if embedded.contains_key(&value.manifest.name) {
                            diagnostics.push(format!(
                                "Excluded duplicate embedded package {} at {}",
                                value.manifest.name,
                                path.display()
                            ));
                        } else {
                            embedded.insert(value.manifest.name.clone(), value);
                        }
                    }
                    Err(error) => diagnostics.push(format!(
                        "Excluded embedded package {}: {error:#}",
                        path.display()
                    )),
                }
            }
        }
        for (package, request) in &manifest.dependencies {
            if !name(package) || request.is_empty() {
                diagnostics.push(format!("Invalid direct package request {package}"));
                continue;
            }
            if embedded.contains_key(package) {
                continue;
            }
            let Some(node) = lock.dependencies.get(package) else {
                continue;
            };
            if node.depth != 0 || &node.version != request {
                diagnostics.push(format!("Manifest and package lock disagree for {package}; using available locked contents."));
            }
        }
        let cache_root = root.join("Library/PackageCache");
        let mut candidates = Vec::new();
        if cache_root.is_dir() {
            watched.insert(cache_root.clone());
            for (path, kind) in super::entries(&cache_root, &mut diagnostics) {
                if kind.is_dir()
                    && let Ok(info) = read::<PackageManifest>(&path.join("package.json"))
                {
                    candidates.push((info, path));
                }
            }
        } else if root.join("Library").is_dir() {
            watched.insert(root.join("Library"));
        }
        let mut pending: VecDeque<_> = manifest
            .dependencies
            .keys()
            .chain(embedded.keys())
            .cloned()
            .collect();
        let mut visited = BTreeSet::new();
        let mut selected = Vec::new();
        while let Some(package_name) = pending.pop_front() {
            if !visited.insert(package_name.clone()) {
                continue;
            }
            if let Some(package) = embedded.remove(&package_name) {
                pending.extend(package.manifest.dependencies.keys().cloned());
                watched.insert(package.root.join("package.json"));
                selected.push(package);
                continue;
            }
            let Some(node) = lock.dependencies.get(&package_name) else {
                diagnostics.push(format!("Excluded package {package_name}: no usable lock entry. References to this package remain unresolved."));
                continue;
            };
            pending.extend(node.dependencies.keys().cloned());
            let registry = manifest
                .scoped_registries
                .iter()
                .flat_map(|r| r.scopes.iter().map(move |scope| (scope, &r.url)))
                .filter(|(scope, _)| package_name.starts_with(scope.as_str()))
                .max_by_key(|(scope, _)| scope.len())
                .map(|(_, url)| url.as_str())
                .unwrap_or("https://packages.unity.com");
            if node.source == "registry"
                && let Some(url) = &node.url
                && url.trim_end_matches('/') != registry.trim_end_matches('/')
            {
                diagnostics.push(format!("Excluded package {package_name}: locked registry disagrees with scoped registry."));
                continue;
            }
            let identity = serde_json::to_string(&(
                node.source.as_str(),
                registry,
                &package_name,
                &node.version,
                &node.hash,
            ))?;
            let testable = manifest.testables.contains(&package_name);
            let available = (|| -> Result<Package> {
                match node.source.as_str() {
                    "builtin" => package(
                        editor
                            .data
                            .join("Resources/PackageManager/BuiltInPackages")
                            .join(&package_name),
                        &package_name,
                        None,
                        identity.clone(),
                        testable,
                    ),
                    "local" => {
                        let relative = node
                            .version
                            .strip_prefix("file:")
                            .context("Local package is missing its file: source")?;
                        let path = policy.canonical(&root.join("Packages").join(relative))?;
                        let path = if path.is_file() {
                            watched.insert(path.clone());
                            super::package_acquire::local_archive(cache, &path, &package_name)?
                        } else {
                            path
                        };
                        package(path, &package_name, None, identity.clone(), testable)
                    }
                    "embedded" => bail!("Embedded package contents are absent"),
                    "git" => {
                        let source = super::package_acquire::GitSource::parse(
                            &node.version,
                            node.hash
                                .as_deref()
                                .context("Git package lock has no commit")?,
                        )?;
                        let path = source.acquire(cache, &package_name, policy.remote.as_ref())?;
                        package(path, &package_name, None, source.identity, testable)
                    }
                    "registry" => {
                        let shared = cache
                            .join("unity-packages")
                            .join(blake3::hash(identity.as_bytes()).to_hex().as_str());
                        if super::package_acquire::complete(&shared, &identity) {
                            return package(
                                shared.join("contents"),
                                &package_name,
                                (node.source == "registry").then_some(node.version.as_str()),
                                identity.clone(),
                                testable,
                            );
                        }
                        if node.source == "registry"
                            && let Some((_, path)) = candidates.iter().find(|(info, _)| {
                                info.name == package_name && info.version == node.version
                            })
                        {
                            return package(
                                policy.canonical(path)?,
                                &package_name,
                                Some(&node.version),
                                identity.clone(),
                                testable,
                            );
                        }
                        if node.source == "registry" && policy.remote.is_some() {
                            let path = super::package_acquire::registry(
                                &shared,
                                &identity,
                                registry,
                                &package_name,
                                &node.version,
                            )?;
                            return package(
                                path,
                                &package_name,
                                Some(&node.version),
                                identity.clone(),
                                testable,
                            );
                        }
                        bail!("No matching offline package contents are available")
                    }
                    _ => unreachable!(),
                }
            })();
            let available = available.or_else(|error| {
                let mut matching: Vec<_> = candidates.iter().filter(|(info, _)| info.name == package_name).collect();
                matching.sort_by(|a, b| a.1.cmp(&b.1));
                if let Some((info, path)) = matching.first() {
                    diagnostics.push(format!("Package {package_name} selection failed: {error:#}. Using available version {}.", info.version));
                    package(policy.canonical(path)?, &package_name, None, identity.clone(), testable)
                } else { Err(error) }
            });
            match available {
                Ok(package) => {
                    if matches!(node.source.as_str(), "registry" | "builtin") && package.manifest.version != node.version {
                        tracing::info!("Package {package_name} requests {}, using available version {}.", node.version, package.manifest.version);
                    }
                    if node.source == "local" && package.manifest.dependencies != node.dependencies { tracing::info!("Local package dependencies disagree with the lock for {package_name}; using available contents."); }
                    if node.source == "builtin" {
                        for (dependency, requested) in &package.manifest.dependencies {
                            if lock
                                .dependencies
                                .get(dependency)
                                .is_none_or(|selected| &selected.version != requested)
                            {
                                tracing::info!("Selected editor requests {dependency} {requested}, which differs from the lock; using available contents.");
                            }
                        }
                    }
                    watched.insert(package.root.join("package.json"));
                    selected.push(package);
                }
                Err(error) => diagnostics.push(format!("Excluded package {package_name}: {error}. References to this package remain unresolved.")),
            }
        }
        Ok(Self {
            selected,
            diagnostics,
            watched,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_dependency_mismatch_keeps_available_packages() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, contents: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            "Packages/manifest.json",
            r#"{"dependencies":{"com.example.core":"1.0"}}"#,
        );
        write(
            "Packages/packages-lock.json",
            r#"{"dependencies":{"com.example.core":{"version":"1.0","source":"builtin","depth":0,"dependencies":{"com.example.dep":"1.0"}},"com.example.dep":{"version":"1.0","source":"registry","depth":1}}}"#,
        );
        write(
            "Editor/Resources/PackageManager/BuiltInPackages/com.example.core/package.json",
            r#"{"name":"com.example.core","version":"1.0","dependencies":{"com.example.dep":"2.0"}}"#,
        );
        write(
            "Library/PackageCache/dep/package.json",
            r#"{"name":"com.example.dep","version":"2.0"}"#,
        );
        let editor = Editor {
            declared: "6000.3.0f1".parse().unwrap(),
            selected: "6000.3.0f1".parse().unwrap(),
            data: root.path().join("Editor"),
            declared_revision: None,
            selected_revision: None,
        };
        let packages = Packages::local(
            root.path(),
            &Policy::new(vec![root.path().into()]).unwrap(),
            &editor,
            root.path(),
        )
        .unwrap();
        assert_eq!(packages.selected.len(), 2);
        assert!(!packages.diagnostics.is_empty());
        assert!(
            packages
                .selected
                .iter()
                .all(|package| package.root.is_dir())
        );
        write("Packages/broken/package.json", "{");
        write(
            "Packages/packages-lock.json",
            r#"{"dependencies":{"com.example.core":{"version":"1.0","source":"builtin","depth":0,"dependencies":{"com.example.dep":"1.0","com.example.absent":"1.0"}},"com.example.dep":{"version":"1.0","source":"registry","depth":1},"com.example.malformed":{"version":false}}}"#,
        );
        let partial = Packages::local(
            root.path(),
            &Policy::new(vec![root.path().into()]).unwrap(),
            &editor,
            root.path(),
        )
        .unwrap();
        assert_eq!(
            partial
                .selected
                .iter()
                .map(|p| &p.manifest.name)
                .collect::<BTreeSet<_>>(),
            packages
                .selected
                .iter()
                .map(|p| &p.manifest.name)
                .collect::<BTreeSet<_>>()
        );
        assert!(partial.diagnostics.len() > packages.diagnostics.len());
    }
}
