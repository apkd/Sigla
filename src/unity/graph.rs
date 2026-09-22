use super::{Platform, catalog::Editor, packages::Packages, settings::Settings};
use crate::{
    discovery::{Discovery, Policy},
    model::{Language, MetadataReference, Project, ProjectReference, SourceInput, SourceRoot},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    name: String,
    #[serde(default)]
    root_namespace: String,
    #[serde(default)]
    references: Vec<String>,
    #[serde(default = "yes")]
    auto_referenced: bool,
    #[serde(default)]
    override_references: bool,
    #[serde(default)]
    precompiled_references: Vec<String>,
    #[serde(default)]
    no_engine_references: bool,
    #[serde(default)]
    include_platforms: Vec<String>,
    #[serde(default)]
    exclude_platforms: Vec<String>,
    #[serde(default)]
    define_constraints: Vec<String>,
    #[serde(default)]
    version_defines: Vec<VersionDefine>,
    #[serde(default)]
    optional_unity_references: Vec<String>,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Deserialize)]
struct VersionDefine {
    name: String,
    expression: String,
    define: String,
}
#[derive(Deserialize)]
struct AssemblyReference {
    reference: String,
}

struct Scope {
    path: PathBuf,
    logical: String,
    package: bool,
    testable: bool,
}
struct Assembly {
    descriptor: Descriptor,
    origin: Option<PathBuf>,
    directory: PathBuf,
    identity: String,
    scope: usize,
    predefined: bool,
    editor_only: bool,
    sources: Vec<PathBuf>,
}

pub fn discover(root: &Path, policy: &Policy, cache: &Path, result: &mut Discovery) -> Result<()> {
    if let Some(remote) = &policy.remote {
        remote.require_analysis_tree(&root.join("Assets"))?;
    }
    let editor = match &policy.remote {
        Some(remote) => super::acquire::editor(root, &remote.shared)?,
        None => Editor::local(root)?,
    };
    discover_with_editor(root, policy, cache, result, &editor)
}

fn discover_with_editor(
    root: &Path,
    policy: &Policy,
    cache: &Path,
    result: &mut Discovery,
    editor: &Editor,
) -> Result<()> {
    let settings = Settings::read(&root.join("ProjectSettings/ProjectSettings.asset"))?;
    let references = super::catalog::References::read(
        &editor.data,
        editor.selected,
        policy.unity_platform,
        settings.backend,
    )?;
    result.metadata.extend(references.watched.iter().cloned());
    let packages = Packages::local(root, policy, editor, cache)?;
    result.metadata.extend(packages.watched);
    result.metadata.extend([
        root.join("ProjectSettings/ProjectSettings.asset"),
        root.join("ProjectSettings/ProjectVersion.txt"),
    ]);
    result.diagnostics.extend(packages.diagnostics);
    if editor.declared != editor.selected {
        result.diagnostics.push(format!(
            "Unity editor: declared {}; selected {}.",
            editor.declared, editor.selected
        ));
    }
    let prefix = root
        .strip_prefix(&result.root)
        .unwrap_or(root)
        .to_string_lossy();
    let logical = |tail: &str| {
        if prefix.is_empty() {
            tail.to_owned()
        } else {
            format!("{prefix}/{tail}")
        }
    };
    let mut scopes = vec![Scope {
        path: policy.canonical(&root.join("Assets"))?,
        logical: logical("Assets"),
        package: false,
        testable: true,
    }];
    let mut versions = BTreeMap::from([("Unity".to_owned(), editor.selected.to_string())]);
    let mut modules = BTreeSet::new();
    let mut provenance = BTreeMap::new();
    for package in packages.selected {
        versions.insert(package.manifest.name.clone(), package.manifest.version);
        if package.manifest.name.starts_with("com.unity.modules.") {
            modules.insert(package.manifest.name.clone());
        }
        provenance.insert(package.manifest.name.clone(), package.identity);
        scopes.push(Scope {
            path: package.root,
            logical: logical(&format!("Packages/{}", package.manifest.name)),
            package: true,
            testable: package.testable,
        });
    }
    let mappings: Vec<_> = scopes
        .iter()
        .map(|s| SourceRoot {
            physical: s.path.clone(),
            logical: s.logical.clone(),
        })
        .collect();
    let mut files = Vec::new();
    for (scope, location) in scopes.iter().enumerate() {
        if let Some(remote) = &policy.remote {
            remote.require_analysis_tree(&location.path)?;
        }
        scan(&location.path, scope, &mut files, &mut result.metadata)?;
    }
    let mut plugins = Vec::new();
    for (_, path) in &files {
        if path.extension().is_some_and(|e| e == "dll") && crate::metadata::is_managed(path)? {
            let meta = PathBuf::from(format!("{}.meta", path.display()));
            let settings = if meta.is_file() {
                result.metadata.insert(meta.clone());
                Some(super::settings::yaml(&meta)?)
            } else {
                None
            };
            plugins.push((path.clone(), settings));
        }
    }
    let mut assemblies = Vec::new();
    let mut boundaries = BTreeMap::new();
    let mut names = BTreeMap::new();
    let mut guids = BTreeMap::new();
    for (scope, file) in &files {
        if file.extension().is_none_or(|x| x != "asmdef") {
            continue;
        }
        let descriptor: Descriptor = serde_json::from_slice(&std::fs::read(file)?)
            .with_context(|| format!("Malformed assembly definition {}", file.display()))?;
        ensure!(
            !descriptor.name.is_empty() && !PREDEFINED.contains(&descriptor.name.as_str()),
            "Invalid custom assembly name {}",
            descriptor.name
        );
        ensure!(
            descriptor.include_platforms.is_empty() || descriptor.exclude_platforms.is_empty(),
            "Assembly {} has contradictory platform lists",
            descriptor.name
        );
        let index = assemblies.len();
        ensure!(
            names.insert(descriptor.name.clone(), index).is_none(),
            "Duplicate Unity assembly {}",
            descriptor.name
        );
        let directory = file.parent().unwrap().to_owned();
        ensure!(
            boundaries.insert(directory.clone(), index).is_none(),
            "Multiple assembly boundaries in {}",
            directory.display()
        );
        let meta = PathBuf::from(format!("{}.meta", file.display()));
        let guid = if meta.is_file() {
            result.metadata.insert(meta.clone());
            let yaml = super::settings::yaml(&meta)?;
            let guid = yaml["guid"]
                .as_str()
                .context("Assembly metadata has no GUID")?
                .to_owned();
            ensure!(
                guid.len() == 32 && guid.bytes().all(|c| c.is_ascii_hexdigit()),
                "Invalid assembly GUID"
            );
            ensure!(
                guids.insert(guid.clone(), index).is_none(),
                "Duplicate assembly GUID {guid}"
            );
            Some(guid)
        } else {
            None
        };
        let identity = format!(
            "unity:{}:{}:{}",
            root.display(),
            guid.unwrap_or_else(|| file
                .strip_prefix(root)
                .unwrap_or(file)
                .to_string_lossy()
                .into_owned()),
            policy.unity_platform.name()
        );
        let editor_only =
            descriptor.include_platforms.len() == 1 && descriptor.include_platforms[0] == "Editor";
        result.metadata.insert(file.clone());
        assemblies.push(Assembly {
            descriptor,
            origin: Some(file.clone()),
            directory,
            identity,
            scope: *scope,
            predefined: false,
            editor_only,
            sources: Vec::new(),
        });
    }
    let resolve = |reference: &str| {
        if let Some(guid) = reference.strip_prefix("GUID:") {
            guids.get(guid).copied()
        } else {
            names.get(reference).copied()
        }
    };
    for (_, file) in &files {
        if file.extension().is_some_and(|x| x == "asmref") {
            let reference: AssemblyReference = serde_json::from_slice(&std::fs::read(file)?)?;
            let target = resolve(&reference.reference).with_context(|| {
                format!(
                    "Unresolved assembly ownership reference {}",
                    reference.reference
                )
            })?;
            ensure!(
                boundaries
                    .insert(file.parent().unwrap().to_owned(), target)
                    .is_none(),
                "Conflicting assembly boundaries in {}",
                file.display()
            );
            result.metadata.insert(file.clone());
        }
    }
    let custom_count = assemblies.len();
    for name in PREDEFINED {
        let index = assemblies.len();
        names.insert(name.into(), index);
        assemblies.push(Assembly {
            descriptor: serde_json::from_value(serde_json::json!({"name": name}))?,
            origin: None,
            directory: root.join("Assets"),
            identity: format!(
                "unity:{}:{name}:{}",
                root.display(),
                policy.unity_platform.name()
            ),
            scope: 0,
            predefined: true,
            editor_only: name.contains("Editor"),
            sources: Vec::new(),
        });
    }
    for (scope, file) in &files {
        if file.extension().is_none_or(|e| e != "cs") {
            continue;
        }
        let owner = file
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(&scopes[*scope].path))
            .find_map(|p| boundaries.get(p).copied());
        let owner = if let Some(owner) = owner {
            owner
        } else if scopes[*scope].package {
            continue;
        } else {
            let relative = file.strip_prefix(root.join("Assets"))?;
            let first = relative.components().next().unwrap().as_os_str();
            let first_pass = matches!(
                first.to_str(),
                Some("Plugins" | "Standard Assets" | "Pro Standard Assets")
            );
            let editor_only = relative.components().any(|c| c.as_os_str() == "Editor");
            custom_count
                + match (first_pass, editor_only) {
                    (true, false) => 0,
                    (true, true) => 1,
                    (false, false) => 2,
                    (false, true) => 3,
                }
        };
        assemblies[owner].sources.push(file.clone());
    }
    let mut active = BTreeMap::new();
    for (index, assembly) in assemblies.iter().enumerate() {
        if assembly.sources.is_empty()
            || assembly.editor_only && policy.unity_platform != Platform::EditorLinux
        {
            continue;
        }
        let platform = if policy.unity_platform == Platform::EditorLinux {
            "Editor"
        } else {
            "LinuxStandalone64"
        };
        let descriptor = &assembly.descriptor;
        if !descriptor.include_platforms.is_empty()
            && !descriptor.include_platforms.iter().any(|p| p == platform)
            || descriptor.exclude_platforms.iter().any(|p| p == platform)
        {
            continue;
        }
        let legacy_test = descriptor
            .optional_unity_references
            .iter()
            .any(|r| r == "TestAssemblies");
        let testable = scopes[assembly.scope].testable;
        if legacy_test && (!testable || policy.unity_platform != Platform::EditorLinux) {
            continue;
        }
        let response = assembly.directory.join("csc.rsp");
        let mut response_refs = Vec::new();
        let mut response_defines = BTreeSet::new();
        if response.is_file() {
            result.metadata.insert(response.clone());
            response_file(
                &response,
                root,
                policy,
                &mut response_defines,
                &mut response_refs,
            )?;
        }
        let mut inputs = editor.compilation(
            &references,
            policy.unity_platform,
            &settings,
            super::catalog::AssemblyContext {
                predefined: assembly.predefined,
                editor_only: assembly.editor_only,
                tests: true,
                no_engine: descriptor.no_engine_references,
                editor_compatible: constraints(&descriptor.define_constraints, &response_defines)?,
            },
            &modules,
        )?;
        for define in &descriptor.version_defines {
            if let Some(version) = versions.get(&define.name)
                && version_matches(version, &define.expression, define.name == "Unity")?
            {
                inputs.defines.insert(define.define.clone());
            }
        }
        inputs.defines.extend(response_defines);
        // Package testability controls activation. Unity still passes the Editor
        // test symbol to participating runtime assemblies in unlisted packages.
        let mut activation_defines = inputs.defines.clone();
        if !testable {
            activation_defines.remove("UNITY_INCLUDE_TESTS");
        }
        if !constraints(&descriptor.define_constraints, &activation_defines)? {
            continue;
        }
        let mut metadata: Vec<_> = inputs
            .references
            .into_iter()
            .map(|path| MetadataReference {
                path,
                aliases: Vec::new(),
                provenance: format!("Unity editor {}", editor.selected),
            })
            .collect();
        metadata.extend(response_refs.into_iter().map(|path| MetadataReference {
            path,
            aliases: Vec::new(),
            provenance: "csc.rsp".into(),
        }));
        let mut selected_plugins = BTreeSet::new();
        for (path, settings) in &plugins {
            if !plugin_enabled(settings.as_ref(), policy.unity_platform, &inputs.defines)? {
                continue;
            }
            let explicit = settings
                .as_ref()
                .is_some_and(|s| s["PluginImporter"]["isExplicitlyReferenced"].as_i64() == Some(1));
            let filename = path.file_name().unwrap().to_string_lossy();
            let selected = if descriptor.override_references {
                descriptor
                    .precompiled_references
                    .iter()
                    .any(|r| r == &filename)
            } else {
                !explicit
            };
            if selected {
                selected_plugins.insert(filename.into_owned());
                metadata.push(MetadataReference {
                    path: path.canonicalize()?,
                    aliases: Vec::new(),
                    provenance: "Unity managed plugin".into(),
                });
            }
        }
        if descriptor.override_references {
            for reference in &descriptor.precompiled_references {
                if !selected_plugins.contains(reference) {
                    result.diagnostics.push(format!(
                        "Unresolved managed plugin {reference} in assembly {}.",
                        descriptor.name
                    ));
                }
            }
        }
        let mut options = BTreeMap::from([
            ("rootNamespace".into(), descriptor.root_namespace.clone()),
            ("UnityVersion".into(), editor.selected.to_string()),
            ("DeclaredUnityVersion".into(), editor.declared.to_string()),
            ("UnityPlatform".into(), policy.unity_platform.name().into()),
            ("UnityPackages".into(), serde_json::to_string(&provenance)?),
        ]);
        if let Some(revision) = &editor.declared_revision {
            options.insert("DeclaredUnityRevision".into(), revision.clone());
        }
        if let Some(revision) = &editor.selected_revision {
            options.insert("UnityRevision".into(), revision.clone());
        }
        for reference in &metadata {
            result.dependencies.insert(reference.path.clone());
        }
        active.insert(index, result.projects.len());
        result.projects.push(Project {
            identity: assembly.identity.clone(),
            origin: assembly.origin.clone(),
            name: descriptor.name.clone(),
            defines: inputs.defines.into_iter().collect(),
            references: Vec::new(),
            assemblies: metadata,
            edition: String::new(),
            compiler_options: options,
            source_roots: mappings.clone(),
        });
        for path in &assembly.sources {
            result.sources.push(SourceInput {
                path: path.canonicalize()?,
                project: result.projects.len() - 1,
                module: String::new(),
                language: Language::CSharp,
                metadata: false,
            });
        }
    }
    for (&index, &project) in &active {
        let assembly = &assemblies[index];
        let mut references = BTreeSet::new();
        if assembly.predefined {
            for (&target, &target_project) in &active {
                let other = &assemblies[target];
                if other.predefined || !other.descriptor.auto_referenced {
                    continue;
                }
                references.insert(result.projects[target_project].identity.clone());
            }
            let needed: &[usize] = match index - custom_count {
                0 => &[],
                1 | 2 => &[0],
                3 => &[0, 1, 2],
                _ => unreachable!(),
            };
            for target in needed {
                if let Some(target) = active.get(&(custom_count + target)) {
                    references.insert(result.projects[*target].identity.clone());
                }
            }
        } else {
            // Both supported editors implicitly reference UI, except from UI
            // itself and assemblies that suppress engine references.
            if !assembly.descriptor.no_engine_references
                && !matches!(
                    assembly.descriptor.name.as_str(),
                    "UnityEngine.UI" | "UnityEditor.UI"
                )
            {
                for name in ["UnityEngine.UI", "UnityEditor.UI"] {
                    if let Some(target) = names.get(name).and_then(|i| active.get(i)) {
                        references.insert(result.projects[*target].identity.clone());
                    }
                }
            }
            for reference in &assembly.descriptor.references {
                ensure!(
                    !PREDEFINED.contains(&reference.as_str()),
                    "Custom assembly cannot reference predefined assembly {reference}"
                );
                let target = reference
                    .strip_prefix("GUID:")
                    .and_then(|g| guids.get(g))
                    .or_else(|| names.get(reference));
                references.insert(
                    target
                        .and_then(|i| active.get(i))
                        .map(|i| result.projects[*i].identity.clone())
                        .unwrap_or_else(|| format!("unresolved-unity:{reference}")),
                );
            }
        }
        result.projects[project].references = references
            .into_iter()
            .map(|target| ProjectReference {
                target,
                aliases: Vec::new(),
            })
            .collect();
    }
    Ok(())
}

const PREDEFINED: [&str; 4] = [
    "Assembly-CSharp-firstpass",
    "Assembly-CSharp-Editor-firstpass",
    "Assembly-CSharp",
    "Assembly-CSharp-Editor",
];

fn scan(
    directory: &Path,
    scope: usize,
    files: &mut Vec<(usize, PathBuf)>,
    watched: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    watched.insert(directory.to_owned());
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if super::ignored_name(&name) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            scan(&entry.path(), scope, files, watched)?;
        } else if entry.file_type()?.is_file()
            && entry.path().extension().is_some_and(|e| {
                matches!(e.to_str(), Some("cs" | "asmdef" | "asmref" | "dll" | "rsp"))
            })
        {
            files.push((scope, entry.path()));
        }
    }
    Ok(())
}

fn constraints(entries: &[String], defines: &BTreeSet<String>) -> Result<bool> {
    let mut enabled = true;
    for entry in entries {
        let mut any = false;
        for alternative in entry.split("||") {
            let value = alternative.trim();
            let (negative, name) = value
                .strip_prefix('!')
                .map_or((false, value), |v| (true, v.trim()));
            ensure!(
                !name.is_empty()
                    && name.chars().enumerate().all(|(i, c)| c == '_'
                        || c.is_alphabetic()
                        || i != 0 && c.is_ascii_digit()),
                "Invalid Unity define constraint {entry}"
            );
            any |= defines.contains(name) != negative;
        }
        enabled &= any;
    }
    Ok(enabled)
}

fn plugin_enabled(
    settings: Option<&serde_yaml::Value>,
    platform: Platform,
    defines: &BTreeSet<String>,
) -> Result<bool> {
    let Some(settings) = settings else {
        return Ok(true);
    };
    let importer = &settings["PluginImporter"];
    ensure!(
        importer.is_mapping(),
        "Managed plugin metadata has no PluginImporter"
    );
    let constraints_list: Vec<String> = importer["defineConstraints"]
        .as_sequence()
        .into_iter()
        .flatten()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .context("Invalid plugin define constraint")
        })
        .collect::<Result<_>>()?;
    if !constraints(&constraints_list, defines)? {
        return Ok(false);
    }
    let Some(entries) = importer["platformData"].as_sequence() else {
        return Ok(true);
    };
    let any = entries
        .iter()
        .find(|entry| entry["first"].get("Any").is_some());
    let specific = entries.iter().find(|entry| {
        if platform == Platform::EditorLinux {
            entry["first"].get("Editor").is_some()
        } else {
            matches!(
                entry["first"]["Standalone"].as_str(),
                Some("Linux64" | "LinuxStandalone64")
            )
        }
    });
    let all = any.is_some_and(|entry| entry["second"]["enabled"].as_i64() == Some(1));
    if all {
        let excluded = if platform == Platform::EditorLinux {
            "Exclude Editor"
        } else {
            "Exclude Linux64"
        };
        if any.unwrap()["second"]["settings"][excluded].as_i64() == Some(1) {
            return Ok(false);
        }
    } else if specific.is_none_or(|entry| entry["second"]["enabled"].as_i64() != Some(1)) {
        return Ok(false);
    }
    if platform == Platform::EditorLinux
        && let Some(specific) = specific
    {
        let options = &specific["second"]["settings"];
        if options["OS"]
            .as_str()
            .is_some_and(|os| !matches!(os, "AnyOS" | "Linux"))
        {
            return Ok(false);
        }
        if options["CPU"]
            .as_str()
            .is_some_and(|cpu| !matches!(cpu, "AnyCPU" | "x86_64"))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Version {
    Unity(super::UnityVersion),
    Package(semver::Version),
}
fn version(value: &str, unity: bool) -> Result<Version> {
    if unity && let Ok(version) = value.parse() {
        return Ok(Version::Unity(version));
    }
    let mut normalized = value.to_owned();
    while normalized.split('.').count() < 3 {
        normalized.push_str(".0");
    }
    if unity {
        Ok(Version::Unity(format!("{normalized}f0").parse()?))
    } else {
        Ok(Version::Package(normalized.parse()?))
    }
}
fn version_matches(actual: &str, expression: &str, unity: bool) -> Result<bool> {
    let actual = version(actual, unity)?;
    if expression.starts_with(['[', '(']) {
        ensure!(
            expression.ends_with([']', ')']),
            "Invalid Unity version expression {expression}"
        );
        let inner = &expression[1..expression.len() - 1];
        if let Some((low, high)) = inner.split_once(',') {
            let low = low.trim();
            let high = high.trim();
            let above = low.is_empty()
                || if expression.starts_with('[') {
                    actual >= version(low, unity)?
                } else {
                    actual > version(low, unity)?
                };
            let below = high.is_empty()
                || if expression.ends_with(']') {
                    actual <= version(high, unity)?
                } else {
                    actual < version(high, unity)?
                };
            Ok(above && below)
        } else {
            ensure!(
                expression.starts_with('[') && expression.ends_with(']'),
                "Invalid exact version expression"
            );
            Ok(actual == version(inner.trim(), unity)?)
        }
    } else {
        Ok(actual >= version(expression.trim(), unity)?)
    }
}

fn response_file(
    path: &Path,
    root: &Path,
    policy: &Policy,
    defines: &mut BTreeSet<String>,
    references: &mut Vec<PathBuf>,
) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut quoted = false;
    let mut token = String::new();
    let mut tokens = Vec::new();
    for character in text.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace() && !quoted {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(character);
        }
    }
    ensure!(
        !quoted,
        "Unclosed quote in response file {}",
        path.display()
    );
    if !token.is_empty() {
        tokens.push(token);
    }
    for token in tokens {
        let token = token.trim_start_matches(['-', '/']);
        if let Some(value) = token
            .strip_prefix("define:")
            .or_else(|| token.strip_prefix("d:"))
        {
            defines.extend(
                value
                    .split([';', ','])
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
            );
        } else if let Some(value) = token
            .strip_prefix("reference:")
            .or_else(|| token.strip_prefix("r:"))
        {
            let relative = root.join(value);
            let path = if relative.is_file() {
                relative
            } else {
                path.parent().unwrap().join(value)
            };
            references.push(policy.canonical(&path)?);
        } else if token.starts_with('@') {
            bail!("Nested Unity response files are not supported");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_graph_matches_observed_unity_6000_3_compilations() {
        compare_observed("6000.3.10f1");
    }
    #[test]
    fn native_graph_matches_observed_unity_2022_3_compilations() {
        compare_observed("2022.3.62f3");
    }
    fn compare_observed(version: &str) {
        let archive = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/unity-reference")
            .join(format!("{version}.tar.zst"));
        let extracted = tempfile::tempdir().unwrap();
        tar::Archive::new(zstd::Decoder::new(std::fs::File::open(archive).unwrap()).unwrap())
            .unpack(extracted.path())
            .unwrap();
        let inventory: Vec<String> = serde_json::from_slice(
            &std::fs::read(extracted.path().join("editor-inventory.json")).unwrap(),
        )
        .unwrap();
        for scope in ["", "advanced", "compatibility"] {
            let references = extracted.path().join(scope);
            for filename in [
                "editor-standard.json",
                "editor-framework.json",
                "player-standard.json",
                "player-framework.json",
            ] {
                let snapshot: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(references.join(filename)).unwrap())
                        .unwrap();
                let root = tempfile::tempdir().unwrap();
                let editor_root = tempfile::tempdir().unwrap();
                let cache = tempfile::tempdir().unwrap();
                for input in snapshot["inputs"].as_array().unwrap() {
                    let path = root.path().join(input["path"].as_str().unwrap());
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    if let Some(encoded) = input["base64"].as_str().filter(|s| !s.is_empty()) {
                        use base64::Engine;
                        std::fs::write(
                            path,
                            base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .unwrap(),
                        )
                        .unwrap();
                    } else {
                        std::fs::write(path, input["contents"].as_str().unwrap()).unwrap();
                    }
                }
                // Reference contents are not read by graph reconstruction. Actual
                // paths and expected reference sets come from the recorded editor.
                for path in &inventory {
                    let path = editor_root.path().join(path);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, []).unwrap();
                }
                std::fs::create_dir_all(editor_root.path().join("Resources")).unwrap();
                std::fs::copy(
                    extracted.path().join("modules.asset"),
                    editor_root.path().join("Resources/modules.asset"),
                )
                .unwrap();
                for entry in std::fs::read_dir(extracted.path().join("builtins")).unwrap() {
                    let entry = entry.unwrap();
                    let target = editor_root
                        .path()
                        .join("Resources/PackageManager/BuiltInPackages")
                        .join(entry.file_name());
                    std::fs::create_dir_all(&target).unwrap();
                    std::fs::copy(
                        entry.path().join("package.json"),
                        target.join("package.json"),
                    )
                    .unwrap();
                }
                let mut policy = Policy::new(vec![root.path().into()]).unwrap();
                policy.unity_platform = if filename.starts_with("player") {
                    Platform::StandaloneLinux
                } else {
                    Platform::EditorLinux
                };
                let selected = snapshot["version"].as_str().unwrap().parse().unwrap();
                let editor = Editor {
                    declared: selected,
                    selected,
                    data: editor_root.path().into(),
                    declared_revision: None,
                    selected_revision: None,
                };
                if scope.is_empty() && filename == "editor-standard.json" {
                    check_module_selection(&editor, root.path());
                }
                let mut actual = Discovery {
                    root: root.path().into(),
                    projects: Vec::new(),
                    sources: Vec::new(),
                    metadata: BTreeSet::new(),
                    dependencies: BTreeSet::new(),
                    diagnostics: Vec::new(),
                };
                discover_with_editor(root.path(), &policy, cache.path(), &mut actual, &editor)
                    .unwrap();
                let expected = snapshot["assemblies"].as_array().unwrap();
                assert_eq!(
                    actual
                        .projects
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<BTreeSet<_>>(),
                    expected
                        .iter()
                        .map(|a| a["name"].as_str().unwrap())
                        .collect::<BTreeSet<_>>(),
                    "{scope}/{filename}"
                );
                for (index, project) in actual.projects.iter().enumerate() {
                    let expected = expected
                        .iter()
                        .find(|a| a["name"].as_str() == Some(&project.name))
                        .unwrap();
                    let strings = |field: &str| {
                        expected[field]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|v| v.as_str().unwrap().to_owned())
                            .collect::<BTreeSet<_>>()
                    };
                    let sources = actual
                        .sources
                        .iter()
                        .filter(|s| s.project == index)
                        .map(|s| {
                            let mapping = project
                                .source_roots
                                .iter()
                                .filter(|r| s.path.starts_with(&r.physical))
                                .max_by_key(|r| r.physical.components().count())
                                .unwrap();
                            format!(
                                "{}/{}",
                                mapping.logical,
                                s.path.strip_prefix(&mapping.physical).unwrap().display()
                            )
                        })
                        .collect::<BTreeSet<_>>();
                    assert_eq!(
                        sources,
                        strings("sources"),
                        "{filename}: {} sources",
                        project.name
                    );
                    assert_eq!(
                        project.defines.iter().cloned().collect::<BTreeSet<_>>(),
                        strings("defines"),
                        "{filename}: {} defines",
                        project.name
                    );
                    let references = project
                        .assemblies
                        .iter()
                        .map(|r| {
                            if let Ok(path) = r.path.strip_prefix(editor_root.path()) {
                                format!("<EDITOR_DATA>/{}", path.display())
                            } else {
                                r.path
                                    .strip_prefix(root.path())
                                    .unwrap()
                                    .to_string_lossy()
                                    .into_owned()
                            }
                        })
                        .collect::<BTreeSet<_>>();
                    assert_eq!(
                        references,
                        strings("references"),
                        "{filename}: {} references",
                        project.name
                    );
                    let projects = project
                        .references
                        .iter()
                        .map(|r| {
                            actual
                                .projects
                                .iter()
                                .find(|p| p.identity == r.target)
                                .unwrap()
                                .name
                                .clone()
                        })
                        .collect::<BTreeSet<_>>();
                    assert_eq!(
                        projects,
                        strings("projects"),
                        "{filename}: {} projects",
                        project.name
                    );
                }
            }
        }
    }

    fn check_module_selection(editor: &Editor, root: &Path) {
        let settings = Settings::read(&root.join("ProjectSettings/ProjectSettings.asset")).unwrap();
        let layout = super::super::catalog::References::read(
            &editor.data,
            editor.selected,
            Platform::EditorLinux,
            0,
        )
        .unwrap();
        let compile = |modules: &BTreeSet<String>, no_engine| {
            editor.compilation(
                &layout,
                Platform::EditorLinux,
                &settings,
                super::super::catalog::AssemblyContext {
                    predefined: false,
                    editor_only: false,
                    tests: false,
                    no_engine,
                    editor_compatible: true,
                },
                modules,
            )
        };
        let physics = editor
            .data
            .join("Managed/UnityEngine/UnityEngine.PhysicsModule.dll");
        let module = BTreeSet::from(["com.unity.modules.physics".into()]);
        assert!(
            !compile(&BTreeSet::new(), false)
                .unwrap()
                .references
                .contains(&physics)
        );
        assert!(
            compile(&module, false)
                .unwrap()
                .references
                .contains(&physics)
        );
        assert!(
            !compile(&module, true)
                .unwrap()
                .references
                .contains(&physics)
        );
        // A selected module must not silently disappear from an incomplete bundle.
        std::fs::remove_file(&physics).unwrap();
        assert!(compile(&module, false).is_err());
        std::fs::write(&physics, []).unwrap();

        // The editor's descriptor, rather than a compiled package list, controls
        // whether the module is automatically referenced.
        let path = editor.data.join("Resources/modules.asset");
        let original = std::fs::read(&path).unwrap();
        let mut document = super::super::settings::yaml(&path).unwrap();
        let physics = document["PlatformModuleSetup"]["modules"]
            .as_sequence_mut()
            .unwrap()
            .iter_mut()
            .find(|m| m["name"].as_str() == Some("Physics"))
            .unwrap();
        physics["controlledByBuiltinPackage"] = serde_yaml::Value::Number(0.into());
        std::fs::write(&path, serde_yaml::to_string(&document).unwrap()).unwrap();
        let updated = super::super::catalog::References::read(
            &editor.data,
            editor.selected,
            Platform::EditorLinux,
            0,
        )
        .unwrap();
        let automatic = editor
            .compilation(
                &updated,
                Platform::EditorLinux,
                &settings,
                super::super::catalog::AssemblyContext {
                    predefined: false,
                    editor_only: false,
                    tests: false,
                    no_engine: false,
                    editor_compatible: true,
                },
                &BTreeSet::new(),
            )
            .unwrap();
        assert!(
            automatic
                .references
                .iter()
                .any(|p| p.file_name().unwrap() == "UnityEngine.PhysicsModule.dll")
        );
        std::fs::write(path, original).unwrap();
    }

    #[test]
    fn version_constraints_use_ranges_negation_and_disjunction() {
        let defines = BTreeSet::from(["AVAILABLE".into()]);
        assert!(
            constraints(
                &["!MISSING".into(), "MISSING || AVAILABLE".into()],
                &defines
            )
            .unwrap()
        );
        assert!(!constraints(&["!AVAILABLE".into()], &defines).unwrap());
        assert!(constraints(&["AVAILABLE && OTHER".into()], &defines).is_err());
        assert!(version_matches("1.5.0", "[1.0,2.0)", false).unwrap());
        assert!(!version_matches("2.0.0", "[1.0,2.0)", false).unwrap());
        assert!(version_matches("6000.3.10f1", "[6000.3,6000.4)", true).unwrap());
    }
}
