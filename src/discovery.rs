use crate::model::{Language, Project, SourceInput};
use anyhow::{Context, Result, bail, ensure};
use quick_xml::{Reader, events::Event};
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Policy {
    pub roots: Vec<PathBuf>,
}
impl Policy {
    pub fn new(roots: Vec<PathBuf>) -> Result<Self> {
        Ok(Self {
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
}

pub fn discover(entry: &Path, policy: &Policy) -> Result<Discovery> {
    let entry = policy.canonical(entry)?;
    let root = if entry.is_dir() {
        entry.clone()
    } else {
        entry.parent().unwrap().to_owned()
    };
    let mut d = Discovery {
        root,
        projects: Vec::new(),
        sources: Vec::new(),
        metadata: BTreeSet::new(),
    };
    d.metadata.insert(d.root.clone());
    let mut inputs = Vec::new();
    if entry.is_dir() {
        let mut entries = std::fs::read_dir(&entry)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        let solutions = entries
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "sln"))
            .cloned()
            .collect::<Vec<_>>();
        if solutions.is_empty() {
            inputs.extend(entries.into_iter().filter(|p| {
                p.extension().is_some_and(|e| e == "csproj")
                    || p.file_name().is_some_and(|n| n == "Cargo.toml")
            }));
        } else {
            inputs.extend(solutions);
            if entry.join("Cargo.toml").exists() {
                inputs.push(entry.join("Cargo.toml"));
            }
        }
    } else {
        inputs.push(entry);
    }
    ensure!(
        !inputs.is_empty(),
        "No `.sln`, `.csproj`, or `Cargo.toml` directly in {}",
        crate::render::inline(&d.root.to_string_lossy())
    );
    let mut seen = HashSet::new();
    for input in inputs {
        load(&input, policy, &mut d, &mut seen)?;
    }
    // workspace-inherited Cargo dependencies refer to the same source projects
    // already discovered through the workspace member list.
    let projects_by_name: std::collections::HashMap<_, _> = d
        .projects
        .iter()
        .map(|p| (p.name.clone(), p.path.clone()))
        .collect();
    for project in &mut d.projects {
        if project.path.file_name().is_some_and(|n| n == "Cargo.toml") {
            let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(&project.path)?)?;
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(deps) = manifest.get(section).and_then(toml::Value::as_table) {
                    for (alias, dep) in deps {
                        let name = dep
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(alias)
                            .replace('-', "_");
                        if dep.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                            && let Some(path) = projects_by_name.get(&name)
                        {
                            project.references.push(path.clone());
                        }
                    }
                }
            }
        }
        project.references = project
            .references
            .iter()
            .map(|p| policy.canonical(p))
            .collect::<Result<_>>()?;
    }
    d.sources
        .sort_by(|a, b| (&a.path, a.project, &a.module).cmp(&(&b.path, b.project, &b.module)));
    d.sources
        .dedup_by(|a, b| a.path == b.path && a.project == b.project && a.module == b.module);
    Ok(d)
}

fn load(
    path: &Path,
    policy: &Policy,
    d: &mut Discovery,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let path = policy.canonical(path)?;
    if !seen.insert(path.clone()) {
        return Ok(());
    }
    d.metadata.insert(path.clone());
    match path.extension().and_then(|x| x.to_str()) {
        Some("sln") => {
            let source = std::fs::read_to_string(&path)?;
            for line in source
                .lines()
                .filter(|l| l.trim_start().starts_with("Project("))
            {
                let parts: Vec<_> = line.split('"').collect();
                if let Some(project) = parts.get(5).filter(|p| p.ends_with(".csproj")) {
                    load(
                        &path.parent().unwrap().join(project.replace('\\', "/")),
                        policy,
                        d,
                        seen,
                    )?;
                }
            }
        }
        Some("csproj") => load_csharp(&path, policy, d, seen)?,
        Some("toml") if path.file_name().is_some_and(|n| n == "Cargo.toml") => {
            load_cargo(&path, policy, d, seen)?
        }
        _ => bail!(
            "Unsupported project entry {}",
            crate::render::inline(&path.to_string_lossy())
        ),
    }
    Ok(())
}

fn msbuild_paths(value: &str, base: &Path) -> Result<Vec<PathBuf>> {
    value
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| {
            ensure!(
                !s.contains("$(") && !s.contains("@(") && !s.contains('*') && !s.contains('?'),
                "Unsupported MSBuild source/path expression {}. Generate explicit project items.",
                crate::render::inline(s)
            );
            let mut bytes = Vec::new();
            let s = s.as_bytes();
            let mut i = 0;
            while i < s.len() {
                if s[i] == b'%'
                    && i + 2 < s.len()
                    && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&s[i + 1..i + 3])?, 16)
                {
                    bytes.push(v);
                    i += 3;
                    continue;
                }
                bytes.push(s[i]);
                i += 1;
            }
            Ok(base.join(String::from_utf8(bytes)?.replace('\\', "/")))
        })
        .collect()
}

fn load_csharp(
    path: &Path,
    policy: &Policy,
    d: &mut Discovery,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let source = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&source);
    reader.config_mut().trim_text(true);
    reader.config_mut().expand_empty_elements = true;
    let base = path.parent().unwrap();
    let mut project = Project {
        path: path.into(),
        name: path.file_stem().unwrap().to_string_lossy().into(),
        defines: Vec::new(),
        references: Vec::new(),
        assemblies: Vec::new(),
        edition: String::new(),
        compiler_options: Default::default(),
    };
    let mut files = BTreeSet::new();
    let mut stack = Vec::<String>::new();
    let mut reference = None;
    let mut reference_enabled = true;
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let mut include = None;
                let mut remove = None;
                for a in e.attributes() {
                    let a = a?;
                    let value = a.decode_and_unescape_value(reader.decoder())?.into_owned();
                    match a.key.local_name().as_ref() {
                        b"Include" => include = Some(value),
                        b"Remove" => remove = Some(value),
                        _ => {}
                    }
                }
                if tag == "Compile" {
                    if let Some(v) = include {
                        for p in msbuild_paths(&v, base)? {
                            files.insert(p);
                        }
                    }
                    if let Some(v) = remove {
                        for p in msbuild_paths(&v, base)? {
                            files.remove(&p);
                        }
                    }
                } else if tag == "ProjectReference" {
                    reference = include
                        .map(|v| msbuild_paths(&v, base))
                        .transpose()?
                        .and_then(|v| v.into_iter().next());
                    reference_enabled = true;
                }
                stack.push(tag);
            }
            Event::Text(t) => {
                let value = t.decode()?.into_owned();
                let value = quick_xml::escape::unescape(&value)?.into_owned();
                match stack.last().map(String::as_str) {
                    Some("AssemblyName") => project.name = value,
                    Some(
                        option @ ("LangVersion"
                        | "Nullable"
                        | "AllowUnsafeBlocks"
                        | "CheckForOverflowUnderflow"
                        | "TargetFramework"
                        | "TargetFrameworkVersion"),
                    ) => {
                        project.compiler_options.insert(option.into(), value);
                    }
                    Some("DefineConstants") if project.defines.is_empty() => {
                        project.defines = value
                            .split(';')
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    }
                    Some("HintPath") => project.assemblies.extend(msbuild_paths(&value, base)?),
                    Some("ReferenceOutputAssembly") => {
                        reference_enabled = !value.trim().eq_ignore_ascii_case("false")
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if tag == "ProjectReference"
                    && let Some(p) = reference.take().filter(|_| reference_enabled)
                {
                    project.references.push(p);
                }
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let index = d.projects.len();
    let refs = project.references.clone();
    d.projects.push(project);
    for p in files {
        d.sources.push(SourceInput {
            path: policy.canonical(&p)?,
            project: index,
            module: String::new(),
            language: Language::CSharp,
            metadata: false,
        });
    }
    for p in refs {
        load(&p, policy, d, seen)?;
    }
    Ok(())
}

fn load_cargo(
    path: &Path,
    policy: &Policy,
    d: &mut Discovery,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
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
                    load(&dir.join("Cargo.toml"), policy, d, seen)?;
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
        .ok_or_else(|| anyhow::anyhow!("Cargo package missing name"))?
        .replace('-', "_");
    let edition = cargo_edition(package, base, policy, &mut d.metadata)?;
    let mut references = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(key).and_then(toml::Value::as_table) {
            for dep in deps.values() {
                if let Some(p) = dep.get("path").and_then(toml::Value::as_str) {
                    references.push(base.join(p).join("Cargo.toml"));
                }
            }
        }
    }
    let project = d.projects.len();
    d.projects.push(Project {
        path: path.into(),
        name: name.clone(),
        defines: Vec::new(),
        references: references.clone(),
        assemblies: Vec::new(),
        edition,
        compiler_options: Default::default(),
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
        d.sources.push(SourceInput {
            path: policy.canonical(&root)?,
            project,
            module,
            language: Language::Rust,
            metadata: false,
        });
    }
    for p in references {
        load(&p, policy, d, seen)?;
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
    let explicit = package
        .get("workspace")
        .and_then(toml::Value::as_str)
        .map(|p| base.join(p));
    let candidates: Vec<_> = explicit
        .map(|p| vec![p])
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
        let pattern = component.as_os_str().to_string_lossy();
        let matcher = globset::Glob::new(&pattern)?.compile_matcher();
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
    fn escape_order() {
        let p = msbuild_paths("A&B%3bC.cs;Percent%253B.cs", Path::new("/tmp")).unwrap();
        assert_eq!(
            p,
            vec![
                PathBuf::from("/tmp/A&B;C.cs"),
                PathBuf::from("/tmp/Percent%3B.cs")
            ]
        );
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
        let default: toml::Value = toml::from_str("name='sample'").unwrap();
        assert_eq!(
            cargo_edition(&default, &child, &policy, &mut metadata).unwrap(),
            "2015"
        );
        let inherited: toml::Value = toml::from_str("edition.workspace=true").unwrap();
        assert_eq!(
            cargo_edition(&inherited, &child, &policy, &mut metadata).unwrap(),
            "2021"
        );
        assert!(metadata.contains(&root.path().join("Cargo.toml")));
    }
}
