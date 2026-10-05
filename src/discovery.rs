use crate::model::{Language, MetadataReference, Project, ProjectReference, SourceInput};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Policy {
    pub roots: Vec<PathBuf>,
    pub unity_platform: crate::unity::Platform,
    pub unity_editors: Option<PathBuf>,
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
        let mut hash = blake3::Hasher::new();
        hash.update(&serde_json::to_vec(&(
            10u32,
            &self.roots,
            self.unity_platform,
            self.remote
                .as_ref()
                .map(|r| (&r.repositories, &r.selection_identity)),
        ))?);
        if let Some(editors) = &self.unity_editors {
            hash.update(&serde_json::to_vec(editors)?);
        }
        Ok(*hash.finalize().as_bytes())
    }
    pub fn new(roots: Vec<PathBuf>) -> Result<Self> {
        Ok(Self {
            unity_platform: Default::default(),
            unity_editors: None,
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

/// Linked worktrees have a gitfile pointing to metadata with a common Git directory.
/// Submodules also use gitfiles, but do not have this marker.
pub(crate) fn linked_worktree(directory: &Path) -> bool {
    let Ok(gitfile) = std::fs::read_to_string(directory.join(".git")) else {
        return false;
    };
    let Some(gitdir) = gitfile.strip_prefix("gitdir: ") else {
        return false;
    };
    let gitdir = gitdir.trim_end_matches(['\r', '\n']);
    !gitdir.is_empty() && directory.join(gitdir).join("commondir").is_file()
}

pub(crate) fn crosses_worktree(base: &Path, path: &Path) -> bool {
    path.ancestors()
        .take_while(|p| *p != base)
        .any(linked_worktree)
}

fn project_failure(error: &anyhow::Error) -> String {
    tracing::warn!(error = %format!("{error:#}"), "Project discovery is incomplete");
    let text = error.to_string();
    let first = text.lines().next().unwrap_or("Project discovery failed");
    let mut summary: String = first.chars().take(240).collect();
    if first.chars().count() > 240 {
        summary.push('…');
    }
    summary.trim_end_matches('.').into()
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
        if let Err(error) = collect_entries(
            &entry,
            policy,
            &mut inputs,
            &mut result.metadata,
            &mut result.diagnostics,
        ) {
            result
                .diagnostics
                .push(format!("Incomplete project discovery: {error:#}"));
        }
    } else {
        inputs.insert(entry.clone());
    }
    if inputs.is_empty() {
        fallback_sources(&entry, policy, &mut result)?;
        if !result.sources.is_empty() {
            result.diagnostics.push("No usable project files found. Searching readable sources; build settings and references are unavailable.".into());
        }
    }
    let mut seen = HashSet::new();
    let mut managed = Vec::new();
    for input in inputs {
        let projects = result.projects.len();
        let sources = result.sources.len();
        let loaded = if input.is_dir() {
            crate::unity::discover(&input, policy, cache, &mut result)
        } else if input.file_name().is_some_and(|n| n == "Cargo.toml") {
            load_cargo(&input, policy, &mut result, &mut seen)
        } else if input
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("csproj" | "sln" | "slnx")))
        {
            managed.push(input);
            continue;
        } else {
            fallback_sources(&input, policy, &mut result)
        };
        if let Err(error) = loaded {
            if error.downcast_ref::<RequiredInputs>().is_some() {
                return Err(error);
            }
            result.projects.truncate(projects);
            result.sources.truncate(sources);
            result.diagnostics.push(format!("Incomplete project details for {}: {}. Searching readable sources; references may be incomplete.", input.display(), project_failure(&error)));
            fallback_sources(&input, policy, &mut result)?;
        }
    }
    if !managed.is_empty() {
        let projects = result.projects.len();
        let sources = result.sources.len();
        if let Err(error) = load_csharp(&managed, policy, cache, &mut result) {
            if error.downcast_ref::<RequiredInputs>().is_some() {
                return Err(error);
            }
            result.projects.truncate(projects);
            result.sources.truncate(sources);
            result.diagnostics.push(format!("Incomplete .NET project details: {}. Searching readable sources; references may be incomplete.", project_failure(&error)));
            for input in &managed {
                if managed.len() > 1 {
                    let projects = result.projects.len();
                    let sources = result.sources.len();
                    match load_csharp(std::slice::from_ref(input), policy, cache, &mut result) {
                        Ok(()) => continue,
                        Err(error) => {
                            if error.downcast_ref::<RequiredInputs>().is_some() {
                                return Err(error);
                            }
                            result.projects.truncate(projects);
                            result.sources.truncate(sources);
                        }
                    }
                }
                fallback_sources(input, policy, &mut result)?;
            }
        }
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
            if serde_json::to_value(&contexts[index])? != serde_json::to_value(&project)? {
                result.diagnostics.push(format!(
                    "Conflicting project details for {}; using the first context.",
                    project.name
                ));
            }
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
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(manifest) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
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
    discover_documents(policy, &mut result);
    result
        .sources
        .sort_by(|a, b| (&a.path, a.project, &a.module).cmp(&(&b.path, b.project, &b.module)));
    result
        .sources
        .dedup_by(|a, b| a.path == b.path && a.project == b.project && a.module == b.module);
    Ok(result)
}

/// Documents have a repository context plus memberships in enclosing build
/// projects. The repository context is not a build project or a declaration.
fn discover_documents(policy: &Policy, result: &mut Discovery) {
    // Unity has already selected and approved these package roots (including its cache).
    let packages: BTreeSet<_> = result
        .projects
        .iter()
        .filter(|p| p.compiler_options.contains_key("UnityVersion"))
        .flat_map(|p| p.source_roots.iter().map(|r| r.physical.clone()))
        .collect();
    let mut pending = vec![result.root.clone()];
    pending.extend(packages.iter().cloned());
    let mut visited = BTreeSet::new();
    let mut documents = Vec::new();
    while let Some(directory) = pending.pop() {
        if !visited.insert(directory.clone()) {
            continue;
        }
        result.metadata.insert(directory.clone());
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                result.diagnostics.push(format!(
                    "Cannot read documents in {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result
                        .diagnostics
                        .push(format!("Cannot read directory entry: {error}"));
                    continue;
                }
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    result
                        .diagnostics
                        .push(format!("Cannot inspect {}: {error}", path.display()));
                    continue;
                }
            };
            if kind.is_dir() {
                if linked_worktree(&path) {
                    continue;
                }
                if matches!(
                    entry.file_name().to_str(),
                    Some("Library" | "Build" | "Builds")
                ) && directory.join("Assets").is_dir()
                {
                    continue;
                }
                if (!crate::unity::ignored_name(&entry.file_name())
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| !n.starts_with('.') && n.ends_with('~'))
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|n| matches!(n, ".github" | ".gitlab" | ".cargo" | ".config")))
                    && !matches!(
                        entry.file_name().to_str(),
                        Some(
                            "target"
                                | "bin"
                                | "obj"
                                | "node_modules"
                                | "Temp"
                                | "Logs"
                                | "UserSettings"
                        )
                    )
                {
                    pending.push(path);
                }
            } else if kind.is_file() {
                let Some(language) = crate::native::language(&path).or_else(|| {
                    (path.starts_with(&result.root)
                        && !path
                            .components()
                            .any(|c| c.as_os_str().to_string_lossy().ends_with('~')))
                    .then(|| crate::documents::language(&path))
                    .flatten()
                }) else {
                    continue;
                };
                if let Some(remote) = &policy.remote
                    && path.starts_with(&remote.workspace)
                    && !path
                        .strip_prefix(&remote.workspace)
                        .ok()
                        .and_then(Path::to_str)
                        .is_some_and(|p| remote.tracked.contains(p))
                {
                    continue;
                }
                let canonical = if language.native() && packages.iter().any(|p| path.starts_with(p))
                {
                    path.canonicalize().map_err(anyhow::Error::from)
                } else {
                    policy.canonical(&path)
                };
                match canonical {
                    Ok(path) => documents.push((path, language)),
                    Err(error) => result
                        .diagnostics
                        .push(format!("Skipped document {}: {error}", path.display())),
                }
            }
        }
    }
    if documents.is_empty() {
        return;
    }
    let context = result.projects.len();
    result.projects.push(Project {
        identity: format!("documents:{}", result.root.display()),
        origin: None,
        name: String::new(),
        defines: Vec::new(),
        references: Vec::new(),
        assemblies: Vec::new(),
        edition: String::new(),
        compiler_options: Default::default(),
        source_roots: result
            .projects
            .iter()
            .filter(|p| p.compiler_options.contains_key("UnityVersion"))
            .flat_map(|p| &p.source_roots)
            .fold(BTreeMap::new(), |mut roots, root| {
                roots
                    .entry((root.physical.clone(), root.logical.clone()))
                    .or_insert_with(|| root.clone());
                roots
            })
            .into_values()
            .collect(),
    });
    for (path, language) in documents {
        let projects = std::iter::once(context).chain(
            result.projects[..context]
                .iter()
                .enumerate()
                .filter(|(_, project)| {
                    (language.native() || path.parent() != Some(result.root.as_path()))
                        && (project.source_roots.iter().any(|r| {
                            path.starts_with(&r.physical)
                                && project
                                    .origin
                                    .as_ref()
                                    .is_some_and(|origin| origin.starts_with(&r.physical))
                        }) || project
                            .origin
                            .as_ref()
                            .and_then(|p| p.parent())
                            .is_some_and(|dir| path.starts_with(dir)))
                })
                .map(|(i, _)| i),
        );
        for project in projects {
            result.sources.push(SourceInput {
                path: path.clone(),
                project,
                module: String::new(),
                language,
                metadata: false,
            });
        }
    }
}

/// Recover source facts without claiming that build settings or references are known.
fn fallback_sources(entry: &Path, policy: &Policy, result: &mut Discovery) -> Result<()> {
    let base = if entry.is_dir() {
        entry
    } else {
        entry.parent().unwrap()
    };
    if let Some(remote) = &policy.remote {
        remote.require_analysis_tree(base)?;
    }
    let mut pending = vec![base.to_owned()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let directory = match policy.canonical(&directory) {
            Ok(path) => path,
            Err(error) => {
                result
                    .diagnostics
                    .push(format!("Skipped directory: {error:#}"));
                continue;
            }
        };
        result.metadata.insert(directory.clone());
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                result
                    .diagnostics
                    .push(format!("Cannot read {}: {error}", directory.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.diagnostics.push(error.to_string());
                    continue;
                }
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    result
                        .diagnostics
                        .push(format!("Cannot read {}: {error}", path.display()));
                    continue;
                }
            };
            if kind.is_dir() {
                let name = entry.file_name();
                if linked_worktree(&path) {
                    continue;
                }
                if crate::unity::assets::is_root(base) && crate::unity::assets::is_root(&path) {
                    continue;
                }
                if name == "Library" && base.join("Assets").is_dir() {
                    if path.join("PackageCache").is_dir()
                        && !linked_worktree(&path.join("PackageCache"))
                    {
                        pending.push(path.join("PackageCache"));
                    }
                } else if !crate::unity::ignored_name(&name)
                    && !matches!(
                        name.to_str(),
                        Some(
                            "target"
                                | "bin"
                                | "obj"
                                | "node_modules"
                                | "Temp"
                                | "Logs"
                                | "UserSettings"
                        )
                    )
                {
                    pending.push(path);
                }
            } else if kind.is_file() {
                let language = match path.extension().and_then(|e| e.to_str()) {
                    Some("cs") => Language::CSharp,
                    Some("rs") => Language::Rust,
                    _ => continue,
                };
                files.push((path, language));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for language in [Language::CSharp, Language::Rust] {
        let project = result.projects.len();
        let name = base
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .replace('-', "_");
        let sources: Vec<_> = files
            .iter()
            .filter(|(_, lang)| *lang == language)
            .map(|(path, _)| SourceInput {
                path: path.clone(),
                project,
                module: if language == Language::Rust {
                    std::iter::once(name.clone())
                        .chain(
                            path.strip_prefix(base)
                                .unwrap()
                                .with_extension("")
                                .components()
                                .filter_map(|c| {
                                    let part = c.as_os_str().to_string_lossy().into_owned();
                                    (!matches!(part.as_str(), "src" | "lib" | "main" | "mod"))
                                        .then_some(part)
                                }),
                        )
                        .collect::<Vec<_>>()
                        .join("::")
                } else {
                    String::new()
                },
                language,
                metadata: false,
            })
            .collect();
        if sources.is_empty() {
            continue;
        }
        result.projects.push(Project {
            identity: format!("fallback:{}:{language:?}", base.display()),
            origin: None,
            name,
            defines: Vec::new(),
            references: Vec::new(),
            assemblies: Vec::new(),
            edition: "2024".into(),
            compiler_options: Default::default(),
            source_roots: Vec::new(),
        });
        result.sources.extend(sources);
    }
    Ok(())
}

fn collect_entries(
    directory: &Path,
    policy: &Policy,
    inputs: &mut BTreeSet<PathBuf>,
    metadata: &mut BTreeSet<PathBuf>,
    diagnostics: &mut Vec<String>,
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
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(format!(
                    "Cannot read entry in {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                diagnostics.push(format!(
                    "Cannot inspect {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if kind.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | "bin" | "obj" | "node_modules" | ".vs")
            )
            && !linked_worktree(&path)
        {
            match policy.canonical(&path) {
                Ok(path) => children.push(path),
                Err(error) => {
                    diagnostics.push(format!("Skipped directory {}: {error:#}", path.display()))
                }
            }
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
        if let Err(error) = collect_entries(&child, policy, inputs, metadata, diagnostics) {
            diagnostics.push(format!(
                "Cannot discover projects in {}: {error:#}",
                child.display()
            ));
        }
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
    result.diagnostics.extend(snapshot.diagnostics);
    ensure!(
        !snapshot.projects.is_empty(),
        "No .NET projects could be evaluated"
    );
    let cache = cache.canonicalize()?;
    let mut approved = policy.clone();
    if let Some(remote) = &policy.remote {
        approved.roots.push(remote.writable.join("upper"));
        approved.roots.push(remote.writable.join("packages"));
    } else {
        approved.roots.push(cache.join("dotnet").canonicalize()?);
    }
    let policy = &approved;
    let source_projects: HashSet<_> = snapshot.projects.iter().map(|p| p.origin.clone()).collect();
    for project in snapshot.projects {
        let origin = match policy.canonical(&project.origin) {
            Ok(origin) => origin,
            Err(error) => {
                result.diagnostics.push(format!(
                    "Skipped .NET project {}: {error:#}",
                    project.origin.display()
                ));
                continue;
            }
        };
        result.metadata.insert(origin.clone());
        for path in project.imports {
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(error) => {
                    result.diagnostics.push(format!(
                        "Unavailable .NET import {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            result.dependencies.insert(path.clone());
            result.metadata.insert(path);
        }
        for directory in project.globs {
            if let Err(error) = watch_tree(&directory, policy, &mut result.metadata) {
                result.diagnostics.push(format!(
                    "Cannot watch .NET sources {}: {error:#}",
                    directory.display()
                ));
            }
        }
        if let Some(assets) = project
            .properties
            .get("ProjectAssetsFile")
            .filter(|p| !p.is_empty())
        {
            match policy.canonical(Path::new(assets)) {
                Ok(path) => {
                    result.metadata.insert(path);
                }
                Err(error) => result
                    .diagnostics
                    .push(format!("Unavailable .NET assets {assets}: {error:#}")),
            }
        }
        let index = result.projects.len();
        for source in project.sources {
            let source = match policy.canonical(&source) {
                Ok(source) => source,
                Err(error) => {
                    result
                        .diagnostics
                        .push(format!("Unavailable .NET source: {error:#}"));
                    continue;
                }
            };
            result.sources.push(SourceInput {
                path: source,
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
            let path = match assembly.path.canonicalize() {
                Ok(path) => path,
                Err(error) => {
                    result.diagnostics.push(format!(
                        "Unavailable .NET reference {}: {error}",
                        assembly.path.display()
                    ));
                    continue;
                }
            };
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
                .unwrap_or_else(|| {
                    vec![crate::model::SourceRoot {
                        physical: cache.join("dotnet"),
                        logical: ".sigla".into(),
                    }]
                }),
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
            && !linked_worktree(&entry.path())
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
                let directories = match member_dirs(base, member) {
                    Ok(directories) => directories,
                    Err(error) => {
                        result.diagnostics.push(format!(
                            "Unavailable Rust workspace member {member}: {error:#}"
                        ));
                        continue;
                    }
                };
                for dir in directories {
                    let relative = dir.strip_prefix(base).unwrap_or(&dir);
                    if excludes.iter().any(|e| {
                        globset::Glob::new(e).is_ok_and(|g| g.compile_matcher().is_match(relative))
                    }) {
                        continue;
                    }
                    if let Err(error) = load_cargo(&dir.join("Cargo.toml"), policy, result, seen) {
                        if error.downcast_ref::<RequiredInputs>().is_some() {
                            return Err(error);
                        }
                        result.diagnostics.push(format!(
                            "Incomplete Rust workspace member {}: {error:#}",
                            dir.display()
                        ));
                        if let Err(error) = fallback_sources(&dir, policy, result) {
                            result.diagnostics.push(format!(
                                "Cannot read Rust workspace member {}: {error:#}",
                                dir.display()
                            ));
                        }
                    }
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
    let edition = cargo_edition(package, base, policy, &mut result.metadata).unwrap_or_else(|error| {
        result.diagnostics.push(format!("Cannot determine Rust edition for {}: {error:#}. Parsing with the latest supported edition.", path.display()));
        "2024".into()
    });
    let mut references = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(key).and_then(toml::Value::as_table) {
            for dep in deps.values() {
                if let Some(p) = dep.get("path").and_then(toml::Value::as_str) {
                    match policy.canonical(&base.join(p).join("Cargo.toml")) {
                        Ok(path) => references.push(path),
                        Err(error) => result
                            .diagnostics
                            .push(format!("Unavailable Rust dependency: {error:#}")),
                    }
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
                } else if let Some(target_name) = t.get("name").and_then(toml::Value::as_str) {
                    let single = base.join("src/bin").join(format!("{target_name}.rs"));
                    let root = if target_name == name && base.join(default).is_file() {
                        base.join(default)
                    } else if single.is_file() {
                        single
                    } else {
                        base.join("src/bin").join(target_name).join("main.rs")
                    };
                    roots.push((root, name.clone()));
                }
            }
        } else if package
            .get(if kind == "lib" { "autolib" } else { "autobins" })
            .and_then(toml::Value::as_bool)
            != Some(false)
            && base.join(default).is_file()
            && !crosses_worktree(base, base.join(default).parent().unwrap())
        {
            roots.push((base.join(default), name.clone()));
        }
    }
    for (kind, dir, automatic) in [
        ("bin", "src/bin", "autobins"),
        ("test", "tests", "autotests"),
        ("example", "examples", "autoexamples"),
        ("bench", "benches", "autobenches"),
    ] {
        if kind != "bin"
            && let Some(targets) = value.get(kind).and_then(toml::Value::as_array)
        {
            for target in targets {
                if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                    roots.push((base.join(path), name.clone()));
                } else if let Some(target_name) = target.get("name").and_then(toml::Value::as_str) {
                    let single = base.join(dir).join(format!("{target_name}.rs"));
                    roots.push((
                        if single.is_file() {
                            single
                        } else {
                            base.join(dir).join(target_name).join("main.rs")
                        },
                        name.clone(),
                    ));
                }
            }
        }
        if package.get(automatic).and_then(toml::Value::as_bool) == Some(false) {
            continue;
        }
        if crosses_worktree(base, &base.join(dir)) {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(base.join(dir)) {
            for e in entries {
                let p = match e {
                    Ok(entry) => entry.path(),
                    Err(error) => {
                        result
                            .diagnostics
                            .push(format!("Cannot read Rust target in {dir}: {error}"));
                        continue;
                    }
                };
                if linked_worktree(&p) {
                    continue;
                }
                if p.extension().is_some_and(|e| e == "rs") {
                    roots.push((p, name.clone()));
                } else if p.join("main.rs").exists() {
                    roots.push((p.join("main.rs"), name.clone()));
                }
            }
        }
    }
    match package.get("build") {
        Some(toml::Value::Boolean(false)) => (),
        Some(toml::Value::String(path)) => roots.push((base.join(path), name.clone())),
        _ if base.join("build.rs").is_file() => roots.push((base.join("build.rs"), name.clone())),
        _ => (),
    }
    let mut target_paths = BTreeSet::new();
    for (root, module) in roots {
        let root = match policy.canonical(&root) {
            Ok(root) => root,
            Err(error) => {
                result
                    .diagnostics
                    .push(format!("Unavailable Rust source: {error:#}"));
                continue;
            }
        };
        if !target_paths.insert(root.clone()) {
            continue;
        }
        result.sources.push(SourceInput {
            path: root,
            project,
            module,
            language: Language::Rust,
            metadata: false,
        });
    }
    for reference in references {
        if let Err(error) = load_cargo(&reference, policy, result, seen) {
            if error.downcast_ref::<RequiredInputs>().is_some() {
                return Err(error);
            }
            result.diagnostics.push(format!(
                "Incomplete Rust dependency {}: {error:#}",
                reference.display()
            ));
            fallback_sources(&reference, policy, result)?;
        }
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
                if e.file_type()?.is_dir()
                    && matcher.is_match(e.file_name())
                    && !linked_worktree(&e.path())
                {
                    next.push(e.path());
                }
            }
        }
        dirs = next;
    }
    Ok(dirs)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn add_worktree(repository: &Path, worktree: &Path) {
        std::fs::create_dir_all(repository).unwrap();
        let git = |args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repository)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init".as_ref()]);
        git(&[
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "fixture".as_ref(),
        ]);
        git(&[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            worktree.as_os_str(),
        ]);
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn nested_worktrees_are_excluded_but_direct_searches_and_other_checkouts_work() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("workspace");
        let worktree = root.join("branches/feature");
        add_worktree(&fixture.path().join("repository"), &worktree);
        let marker = std::fs::read_to_string(worktree.join(".git")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            marker.replacen("gitdir: ", "gitdir:", 1),
        )
        .unwrap();
        assert!(!linked_worktree(&worktree));
        std::fs::write(worktree.join(".git"), marker).unwrap();
        write(&root, "Parent.cs", "class Parent {}");
        for checkout in ["ordinary", "submodule", "malformed"] {
            write(&root, &format!("{checkout}/Child.cs"), "class Child {}");
        }
        std::fs::create_dir(root.join("ordinary/.git")).unwrap();
        std::fs::create_dir(root.join("submodule-admin")).unwrap();
        write(&root, "submodule/.git", "gitdir: ../submodule-admin\n");
        write(&root, "malformed/.git", "gitdir: \n");
        write(&worktree, "Child.cs", "class WorktreeChild {}");
        write(&worktree, "Native.cpp", "void worktree() {}");
        write(&worktree, "Guide.md", "Worktree documentation");
        let policy = Policy::new(vec![fixture.path().into()]).unwrap();
        for relative in [false, true] {
            if relative {
                let marker = std::fs::read_to_string(worktree.join(".git")).unwrap();
                let gitdir = Path::new(marker.trim().strip_prefix("gitdir: ").unwrap());
                std::fs::write(
                    worktree.join(".git"),
                    format!(
                        "gitdir: ../../../{}\n",
                        gitdir.strip_prefix(fixture.path()).unwrap().display()
                    ),
                )
                .unwrap();
            }
            assert!(linked_worktree(&worktree));
            let found = discover(&root, &policy).unwrap();
            assert!(!found.sources.iter().any(|s| s.path.starts_with(&worktree)));
            assert!(!found.metadata.iter().any(|p| p.starts_with(&worktree)));
            for path in [
                "Parent.cs",
                "ordinary/Child.cs",
                "submodule/Child.cs",
                "malformed/Child.cs",
            ] {
                assert!(found.sources.iter().any(|s| s.path == root.join(path)));
            }
            let direct = discover(&worktree, &policy).unwrap();
            for path in ["Child.cs", "Native.cpp", "Guide.md"] {
                assert!(direct.sources.iter().any(|s| s.path == worktree.join(path)));
            }
            let mut watched = BTreeSet::new();
            watch_tree(&root, &policy, &mut watched).unwrap();
            assert!(!watched.iter().any(|p| p.starts_with(&worktree)));
        }
        write(
            &worktree,
            "Cargo.toml",
            "[package]\nname='child'\nversion='0.1.0'\n",
        );
        write(&worktree, "src/lib.rs", "pub fn child() {}");
        let parent = discover(&root, &policy).unwrap();
        assert!(
            !parent
                .projects
                .iter()
                .any(|p| p.origin.as_ref().is_some_and(|p| p.starts_with(&worktree)))
        );
        let direct = discover(&worktree, &policy).unwrap();
        assert!(
            direct
                .sources
                .iter()
                .any(|s| s.path == worktree.join("src/lib.rs"))
        );
        let selected_manifest = discover(&worktree.join("Cargo.toml"), &policy).unwrap();
        assert!(
            selected_manifest
                .sources
                .iter()
                .any(|s| s.path == worktree.join("src/lib.rs"))
        );
    }

    #[test]
    fn cargo_automatic_targets_and_member_globs_stop_at_worktrees() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("workspace");
        let worktree = root.join("src/bin/feature");
        add_worktree(&fixture.path().join("repository"), &worktree);
        write(
            &root,
            "Cargo.toml",
            "[package]\nname='parent'\nversion='0.1.0'\n",
        );
        write(&root, "src/lib.rs", "pub fn parent() {}");
        write(&root, "src/bin/ordinary/main.rs", "fn main() {}");
        write(&worktree, "main.rs", "fn main() {}");
        write(
            &worktree,
            "Cargo.toml",
            "[package]\nname='child'\nversion='0.1.0'\n",
        );
        write(&worktree, "src/lib.rs", "pub fn child() {}");
        let policy = Policy::new(vec![fixture.path().into()]).unwrap();
        let found = discover(&root, &policy).unwrap();
        assert!(
            found
                .sources
                .iter()
                .any(|s| s.path == root.join("src/bin/ordinary/main.rs"))
        );
        assert!(!found.sources.iter().any(|s| s.path.starts_with(&worktree)));
        assert!(!member_dirs(&root, "src/bin/*").unwrap().contains(&worktree));
        assert!(
            !member_dirs(&root, "src/bin/*/src")
                .unwrap()
                .contains(&worktree.join("src"))
        );
        assert!(
            member_dirs(&root, "src/bin/feature")
                .unwrap()
                .contains(&worktree)
        );
        write(
            &root,
            "Cargo.toml",
            "[package]\nname='parent'\nversion='0.1.0'\n[[bin]]\nname='explicit'\npath='src/bin/feature/main.rs'\n[dependencies]\nchild={path='src/bin/feature'}\n",
        );
        let explicit = discover(&root, &policy).unwrap();
        for path in ["main.rs", "src/lib.rs"] {
            assert!(
                explicit
                    .sources
                    .iter()
                    .any(|s| s.path == worktree.join(path))
            );
        }
    }

    #[test]
    fn rediscovery_removes_cached_worktree_sources_and_watches() {
        for cached in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let root = fixture.path().join("workspace");
            let worktree = root.join("branches/feature");
            add_worktree(&fixture.path().join("repository"), &worktree);
            write(&root, "Parent.cs", "class Parent {}");
            write(&worktree, "Child.cs", "class Child {}");
            let marker = worktree.join(".git");
            let hidden_marker = worktree.join(".git.disabled");
            std::fs::rename(&marker, &hidden_marker).unwrap();
            let policy = Policy::new(vec![fixture.path().into()]).unwrap();
            let monitor = std::sync::Arc::new(crate::watch::Monitor::default());
            let open = || {
                crate::workspace::Workspace::open(
                    root.clone(),
                    cache.path(),
                    policy.clone(),
                    &cache.path().join("analysis"),
                    None,
                    monitor.clone(),
                )
                .unwrap()
            };
            let mut workspace = open();
            workspace.refresh().unwrap();
            assert!(
                workspace
                    .manifest
                    .files
                    .values()
                    .any(|f| f.path.starts_with(&worktree))
            );
            std::fs::rename(&hidden_marker, &marker).unwrap();
            if cached {
                let mut stale = (*workspace.manifest).clone();
                stale.discovery_policy[0] ^= 1;
                for (path, stamp) in &mut stale.metadata {
                    *stamp = crate::workspace::Stamp::read(path).unwrap();
                }
                workspace.store.save_manifest(&stale).unwrap();
                drop(workspace);
                workspace = open();
            }
            workspace.refresh().unwrap();
            assert!(
                !workspace
                    .manifest
                    .files
                    .values()
                    .any(|f| f.path.starts_with(&worktree))
            );
            assert!(
                !workspace
                    .manifest
                    .metadata
                    .keys()
                    .any(|p| p.starts_with(&worktree))
            );
            let watches = workspace.watch_snapshot();
            write(&worktree, "Child.cs", "class ChangedChild {}");
            assert!(
                !watches.changed(),
                "Excluded worktree edits must not dirty the parent workspace"
            );
        }
    }

    #[test]
    fn cargo_custom_targets_and_build_scripts() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "src/lib.rs",
            "scripts/check.rs",
            "scripts/example.rs",
            "scripts/bench.rs",
            "scripts/build.rs",
            "build.rs",
            "tests/ignored.rs",
        ] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "pub fn fixture() {}").unwrap();
        }
        let policy = Policy::new(vec![root.path().into()]).unwrap();
        for (build, expected) in [
            ("build='scripts/build.rs'", Some("scripts/build.rs")),
            ("build=false", None),
            ("", Some("build.rs")),
        ] {
            std::fs::write(
                root.path().join("Cargo.toml"),
                format!(
                    r#"
[package]
name='fixture'
version='0.1.0'
autotests=false
{build}
[[test]]
name='check'
path='scripts/check.rs'
[[example]]
name='example'
path='scripts/example.rs'
[[bench]]
name='bench'
path='scripts/bench.rs'
"#
                ),
            )
            .unwrap();
            let found = discover(root.path(), &policy).unwrap();
            let paths: BTreeSet<_> = found
                .sources
                .iter()
                .map(|s| {
                    s.path
                        .strip_prefix(root.path())
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            for path in [
                "src/lib.rs",
                "scripts/check.rs",
                "scripts/example.rs",
                "scripts/bench.rs",
            ] {
                assert!(paths.contains(path), "{paths:?}");
            }
            assert!(!paths.contains("tests/ignored.rs"));
            for path in ["build.rs", "scripts/build.rs"] {
                assert_eq!(paths.contains(path), expected == Some(path), "{paths:?}");
            }
        }
    }
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
