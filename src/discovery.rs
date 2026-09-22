use crate::model::{Language, MetadataReference, Project, ProjectReference, SourceInput};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Policy {
    pub roots: Vec<PathBuf>,
    pub unity_platform: crate::unity::Platform,
    pub remote: Option<RemoteContext>,
}

#[derive(Clone)]
pub struct RemoteContext {
    pub workspace: PathBuf,
    pub writable: PathBuf,
    pub shared: PathBuf,
    pub repositories: Vec<crate::repository::Rule>,
    pub selection_identity: String,
    pub tracked: std::sync::Arc<BTreeSet<String>>,
}

#[derive(Debug)]
pub struct RequiredInputs(pub Vec<String>);
impl std::fmt::Display for RequiredInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Discovery requires omitted tracked inputs: {}",
            self.0.join(", ")
        )
    }
}
impl std::error::Error for RequiredInputs {}

impl RemoteContext {
    pub fn require(&self, paths: impl IntoIterator<Item = PathBuf>) -> Result<()> {
        let mut missing = BTreeSet::new();
        for path in paths {
            if path.is_file() {
                continue;
            }
            if let Ok(relative) = path.strip_prefix(&self.workspace) {
                let relative = relative.to_str().context("Invalid discovery input path")?;
                crate::repository::selection::validate_path(relative)?;
                if self.tracked.contains(relative) {
                    missing.insert(relative.to_owned());
                }
            }
        }
        if !missing.is_empty() {
            return Err(RequiredInputs(missing.into_iter().collect()).into());
        }
        Ok(())
    }

    pub fn require_analysis_tree(&self, directory: &Path) -> Result<()> {
        self.require(
            self.tracked
                .iter()
                .map(|p| self.workspace.join(p))
                .filter(|p| {
                    p.strip_prefix(directory).is_ok_and(|relative| {
                        !relative
                            .components()
                            .any(|c| crate::unity::ignored_name(c.as_os_str()))
                    }) && crate::acquisition::analysis_input(p)
                }),
        )
    }
}

impl Policy {
    pub fn identity(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&serde_json::to_vec(&(
            3u32,
            &self.roots,
            self.unity_platform,
            self.remote
                .as_ref()
                .map(|r| (&r.repositories, &r.selection_identity)),
        ))?)
        .as_bytes())
    }
    pub fn new(roots: Vec<PathBuf>) -> Result<Self> {
        Ok(Self {
            unity_platform: Default::default(),
            remote: None,
            roots: roots
                .into_iter()
                .map(|p| {
                    p.canonicalize().with_context(|| {
                        format!(
                            "Cannot open allowed root {}",
                            crate::render::inline(&p.to_string_lossy())
                        )
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
    pub fn canonical(&self, path: &Path) -> Result<PathBuf> {
        let path = path.canonicalize().with_context(|| {
            format!(
                "Cannot open {}",
                crate::render::inline(&path.to_string_lossy())
            )
        })?;
        ensure!(
            self.roots.iter().any(|r| path.starts_with(r)),
            "Path is outside configured read roots: {}",
            crate::render::inline(&path.to_string_lossy())
        );
        Ok(path)
    }
}

pub struct Discovery {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub sources: Vec<SourceInput>,
    pub metadata: BTreeSet<PathBuf>,
    pub dependencies: BTreeSet<PathBuf>,
    pub diagnostics: Vec<String>,
}

pub fn discover(entry: &Path, policy: &Policy) -> Result<Discovery> {
    discover_cached(entry, policy, Path::new("/tmp/sigla"))
}

pub fn discover_cached(entry: &Path, policy: &Policy, cache: &Path) -> Result<Discovery> {
    let entry = policy.canonical(entry)?;
    let root = if entry.is_dir() {
        entry.clone()
    } else {
        entry.parent().unwrap().to_owned()
    };
    let mut result = Discovery {
        root,
        projects: Vec::new(),
        sources: Vec::new(),
        metadata: BTreeSet::new(),
        dependencies: BTreeSet::new(),
        diagnostics: Vec::new(),
    };
    let mut inputs = BTreeSet::new();
    if entry.is_dir() {
        collect_entries(&entry, policy, &mut inputs, &mut result.metadata)?;
    } else {
        inputs.insert(entry);
    }
    ensure!(
        !inputs.is_empty(),
        "No supported projects found in {}",
        result.root.display()
    );
    let mut seen = HashSet::new();
    let mut managed = Vec::new();
    for input in inputs {
        if input.is_dir() {
            crate::unity::discover(&input, policy, cache, &mut result)?;
        } else if input.file_name().is_some_and(|n| n == "Cargo.toml") {
            load_cargo(&input, policy, &mut result, &mut seen)?;
        } else if input
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("csproj" | "sln" | "slnx")))
        {
            managed.push(input);
        } else {
            bail!("Unsupported project entry {}", input.display());
        }
    }
    if !managed.is_empty() {
        load_csharp(&managed, policy, cache, &mut result)?;
    }
    let mut identities = std::collections::HashMap::new();
    let mut contexts: Vec<Project> = Vec::new();
    let mut remap = Vec::new();
    for mut project in std::mem::take(&mut result.projects) {
        project.defines.sort();
        project.defines.dedup();
        project
            .references
            .sort_by(|a, b| (&a.target, &a.aliases).cmp(&(&b.target, &b.aliases)));
        project
            .assemblies
            .sort_by(|a, b| (&a.path, &a.aliases).cmp(&(&b.path, &b.aliases)));
        let index = if let Some(&index) = identities.get(&project.identity) {
            ensure!(
                serde_json::to_value(&contexts[index])? == serde_json::to_value(&project)?,
                "Discovery returned conflicting instances of the same project context"
            );
            index
        } else {
            let index = contexts.len();
            identities.insert(project.identity.clone(), index);
            contexts.push(project);
            index
        };
        remap.push(index);
    }
    for source in &mut result.sources {
        source.project = remap[source.project];
    }
    result.projects = contexts;
    let projects_by_name: std::collections::HashMap<_, _> = result
        .projects
        .iter()
        .map(|p| (p.name.clone(), p.identity.clone()))
        .collect();
    for project in &mut result.projects {
        let Some(path) = project
            .origin
            .as_ref()
            .filter(|p| p.file_name().is_some_and(|n| n == "Cargo.toml"))
        else {
            continue;
        };
        let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
        for section in ["dependencies", "dev-dependencies"] {
            if let Some(deps) = manifest.get(section).and_then(toml::Value::as_table) {
                for (alias, dep) in deps {
                    let name = dep
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(alias)
                        .replace('-', "_");
                    if dep.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                        && let Some(identity) = projects_by_name.get(&name)
                    {
                        project.references.push(ProjectReference {
                            target: identity.clone(),
                            aliases: Vec::new(),
                        });
                    }
                }
            }
        }
    }
    result
        .sources
        .sort_by(|a, b| (&a.path, a.project, &a.module).cmp(&(&b.path, b.project, &b.module)));
    result
        .sources
        .dedup_by(|a, b| a.path == b.path && a.project == b.project && a.module == b.module);
    Ok(result)
}

fn collect_entries(
    directory: &Path,
    policy: &Policy,
    inputs: &mut BTreeSet<PathBuf>,
    metadata: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if directory.join("Assets").is_dir()
        && directory
            .join("ProjectSettings/ProjectVersion.txt")
            .is_file()
    {
        inputs.insert(directory.into());
        return Ok(());
    }
    metadata.insert(directory.into());
    let mut children = Vec::new();
    let mut projects = Vec::new();
    let mut solutions = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | "bin" | "obj" | "node_modules" | ".vs")
            )
        {
            children.push(policy.canonical(&path)?);
        } else if kind.is_file() {
            match path.extension().and_then(|e| e.to_str()) {
                Some("sln" | "slnx") => solutions.push(path),
                Some("csproj") => projects.push(path),
                Some("toml") if entry.file_name() == "Cargo.toml" => {
                    inputs.insert(path);
                }
                _ => {}
            }
        }
    }
    inputs.extend(if solutions.is_empty() {
        projects
    } else {
        solutions
    });
    for child in children {
        collect_entries(&child, policy, inputs, metadata)?;
    }
    Ok(())
}

fn load_csharp(
    entries: &[PathBuf],
    policy: &Policy,
    cache: &Path,
    result: &mut Discovery,
) -> Result<()> {
    let snapshot = crate::msbuild::discover_in(entries, cache, policy.remote.as_ref())?;
    let mut approved = policy.clone();
    if let Some(remote) = &policy.remote {
        approved.roots.push(remote.writable.join("upper"));
        approved.roots.push(remote.writable.join("packages"));
    }
    let policy = &approved;
    let source_projects: HashSet<_> = snapshot.projects.iter().map(|p| p.origin.clone()).collect();
    for project in snapshot.projects {
        let origin = policy.canonical(&project.origin)?;
        result.metadata.insert(origin.clone());
        for path in project.imports {
            let path = path.canonicalize()?;
            result.dependencies.insert(path.clone());
            result.metadata.insert(path);
        }
        for directory in project.globs {
            watch_tree(&directory, policy, &mut result.metadata)?;
        }
        if let Some(assets) = project
            .properties
            .get("ProjectAssetsFile")
            .filter(|p| !p.is_empty())
        {
            result.metadata.insert(policy.canonical(Path::new(assets))?);
        }
        let index = result.projects.len();
        for source in project.sources {
            result.sources.push(SourceInput {
                path: policy.canonical(&source)?,
                project: index,
                module: String::new(),
                language: Language::CSharp,
                metadata: false,
            });
        }
        let mut assemblies = Vec::new();
        for assembly in project.assemblies {
            if !assembly.source_project.is_empty()
                && source_projects.contains(Path::new(&assembly.source_project))
            {
                continue;
            }
            let path = assembly.path.canonicalize()?;
            result.dependencies.insert(path.clone());
            assemblies.push(MetadataReference {
                path,
                aliases: split(&assembly.aliases),
                provenance: "MSBuild".into(),
            });
        }
        result.projects.push(Project {
            identity: project.identity,
            origin: Some(origin.clone()),
            name: project
                .properties
                .get("AssemblyName")
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or_else(|| origin.file_stem().unwrap().to_string_lossy().into_owned()),
            defines: split(
                project
                    .properties
                    .get("DefineConstants")
                    .map(String::as_str)
                    .unwrap_or(""),
            ),
            references: project
                .references
                .into_iter()
                .map(|r| ProjectReference {
                    target: r.identity,
                    aliases: split(&r.aliases),
                })
                .collect(),
            assemblies,
            edition: String::new(),
            compiler_options: project.properties,
            source_roots: policy
                .remote
                .as_ref()
                .map(|remote| {
                    vec![
                        crate::model::SourceRoot {
                            physical: remote.workspace.clone(),
                            logical: String::new(),
                        },
                        crate::model::SourceRoot {
                            physical: remote.writable.join("upper"),
                            logical: String::new(),
                        },
                        crate::model::SourceRoot {
                            physical: remote.writable.join("packages"),
                            logical: "Packages/.nuget".into(),
                        },
                    ]
                })
                .unwrap_or_default(),
        });
    }
    Ok(())
}

fn watch_tree(directory: &Path, policy: &Policy, metadata: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !directory.is_dir() {
        if let Some(parent) = directory.ancestors().find(|p| p.is_dir()) {
            metadata.insert(policy.canonical(parent)?);
        }
        return Ok(());
    }
    let directory = policy.canonical(directory)?;
    metadata.insert(directory.clone());
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some(".git" | "bin" | "obj" | "target" | "node_modules")
            )
        {
            watch_tree(&entry.path(), policy, metadata)?;
        }
    }
    Ok(())
}

fn split(value: &str) -> Vec<String> {
    value
        .split([';', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn load_cargo(
    path: &Path,
    policy: &Policy,
    result: &mut Discovery,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let path = policy.canonical(path)?;
    if !seen.insert(path.clone()) {
        return Ok(());
    }
    result.metadata.insert(path.clone());
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let base = path.parent().unwrap();
    if let Some(workspace) = value.get("workspace") {
        let excludes = workspace
            .get("exclude")
            .and_then(toml::Value::as_array)
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(members) = workspace.get("members").and_then(toml::Value::as_array) {
            for member in members.iter().filter_map(toml::Value::as_str) {
                for dir in member_dirs(base, member)? {
                    let relative = dir.strip_prefix(base).unwrap_or(&dir);
                    if excludes.iter().any(|e| {
                        globset::Glob::new(e).is_ok_and(|g| g.compile_matcher().is_match(relative))
                    }) {
                        continue;
                    }
                    load_cargo(&dir.join("Cargo.toml"), policy, result, seen)?;
                }
            }
        }
    }
    let Some(package) = value.get("package") else {
        return Ok(());
    };
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .context("Cargo package missing name")?
        .replace('-', "_");
    let edition = cargo_edition(package, base, policy, &mut result.metadata)?;
    let mut references = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(key).and_then(toml::Value::as_table) {
            for dep in deps.values() {
                if let Some(p) = dep.get("path").and_then(toml::Value::as_str) {
                    references.push(policy.canonical(&base.join(p).join("Cargo.toml"))?);
                }
            }
        }
    }
    let project = result.projects.len();
    result.projects.push(Project {
        identity: path.to_string_lossy().into_owned(),
        origin: Some(path.clone()),
        name: name.clone(),
        defines: Vec::new(),
        references: references
            .iter()
            .map(|p| ProjectReference {
                target: p.to_string_lossy().into_owned(),
                aliases: Vec::new(),
            })
            .collect(),
        assemblies: Vec::new(),
        edition,
        compiler_options: Default::default(),
        source_roots: Vec::new(),
    });
    let mut roots = Vec::new();
    for (kind, default) in [("lib", "src/lib.rs"), ("bin", "src/main.rs")] {
        if let Some(t) = value.get(kind).and_then(toml::Value::as_table) {
            roots.push((
                base.join(
                    t.get("path")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(default),
                ),
                t.get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(&name)
                    .replace('-', "_"),
            ));
        } else if let Some(ts) = value.get(kind).and_then(toml::Value::as_array) {
            for t in ts {
                if let Some(p) = t.get("path").and_then(toml::Value::as_str) {
                    roots.push((base.join(p), name.clone()));
                }
            }
        } else if base.join(default).is_file() {
            roots.push((base.join(default), name.clone()));
        }
    }
    for dir in ["src/bin", "tests", "examples", "benches"] {
        if let Ok(entries) = std::fs::read_dir(base.join(dir)) {
            for e in entries {
                let p = e?.path();
                if p.extension().is_some_and(|e| e == "rs") {
                    roots.push((p, name.clone()));
                } else if p.join("main.rs").exists() {
                    roots.push((p.join("main.rs"), name.clone()));
                }
            }
        }
    }
    for (root, module) in roots {
        result.sources.push(SourceInput {
            path: policy.canonical(&root)?,
            project,
            module,
            language: Language::Rust,
            metadata: false,
        });
    }
    for reference in references {
        load_cargo(&reference, policy, result, seen)?;
    }
    Ok(())
}

fn cargo_edition(
    package: &toml::Value,
    base: &Path,
    policy: &Policy,
    metadata: &mut BTreeSet<PathBuf>,
) -> Result<String> {
    let Some(edition) = package.get("edition") else {
        return Ok("2015".into());
    };
    if let Some(edition) = edition.as_str() {
        return Ok(edition.into());
    }
    ensure!(
        edition.get("workspace").and_then(toml::Value::as_bool) == Some(true),
        "Invalid Cargo edition"
    );
    let candidates = package
        .get("workspace")
        .and_then(toml::Value::as_str)
        .map(|p| vec![base.join(p)])
        .unwrap_or_else(|| base.ancestors().map(Path::to_path_buf).collect());
    for candidate in candidates {
        let manifest = candidate.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let manifest = policy.canonical(&manifest)?;
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest)?)?;
        if let Some(workspace) = value.get("workspace") {
            metadata.insert(manifest);
            return workspace
                .get("package")
                .and_then(|p| p.get("edition"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .context("Workspace does not define an inherited package edition");
        }
    }
    bail!("Cannot find workspace for inherited Cargo edition")
}

fn member_dirs(base: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    if !pattern.contains('*') {
        return Ok(vec![base.join(pattern)]);
    }
    let mut dirs = vec![base.to_path_buf()];
    for component in Path::new(pattern).components() {
        let matcher =
            globset::Glob::new(&component.as_os_str().to_string_lossy())?.compile_matcher();
        let mut next = Vec::new();
        for dir in dirs {
            for e in std::fs::read_dir(dir)? {
                let e = e?;
                if e.file_type()?.is_dir() && matcher.is_match(e.file_name()) {
                    next.push(e.path());
                }
            }
        }
        dirs = next;
    }
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cargo_edition_defaults_and_workspace_inheritance() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("member");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace.package]\nedition='2021'\n",
        )
        .unwrap();
        let policy = Policy::new(vec![root.path().into()]).unwrap();
        let mut metadata = BTreeSet::new();
        assert_eq!(
            cargo_edition(
                &toml::from_str("name='sample'").unwrap(),
                &child,
                &policy,
                &mut metadata
            )
            .unwrap(),
            "2015"
        );
        assert_eq!(
            cargo_edition(
                &toml::from_str("edition.workspace=true").unwrap(),
                &child,
                &policy,
                &mut metadata
            )
            .unwrap(),
            "2021"
        );
        assert!(metadata.contains(&root.path().join("Cargo.toml")));
    }
}
