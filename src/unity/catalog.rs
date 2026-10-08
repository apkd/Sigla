use super::symbols::symbols;
use super::{
    Platform, UnityVersion,
    settings::{self, Settings},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

fn supported(version: UnityVersion) -> bool {
    matches!(
        (version.branch.major, version.branch.minor),
        (2022, 3) | (6000, 3)
    )
}

/// Read the installed editor's metadata once for an entire discovery graph.
/// Reference groups remain separate even though acquisition retains extra DLLs.
pub struct References {
    engine: Vec<PathBuf>,
    standard: Vec<PathBuf>,
    framework: Vec<PathBuf>,
    modules: BTreeMap<String, bool>,
    pub watched: BTreeSet<PathBuf>,
    pub diagnostics: Vec<String>,
}

impl References {
    pub fn read(
        data: &Path,
        version: UnityVersion,
        platform: Platform,
        backend: u32,
    ) -> Result<Self> {
        ensure!(
            supported(version) || data.join("manifest.bin").is_file(),
            "Unsupported Unity editor reference layout {version}"
        );
        let module_path = data.join("Resources/modules.asset");
        let document = settings::yaml(&module_path)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Module {
            name: String,
            controlled_by_builtin_package: u8,
        }
        let records: Vec<serde_yaml::Value> =
            serde_yaml::from_value(document["PlatformModuleSetup"]["modules"].clone())?;
        let mut modules = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for record in records {
            let parsed = serde_yaml::from_value::<Module>(record)
                .map_err(anyhow::Error::from)
                .and_then(|module| {
                    ensure!(
                        module.controlled_by_builtin_package <= 1 && !module.name.is_empty(),
                        "Invalid Unity module descriptor"
                    );
                    ensure!(
                        !modules.contains_key(&module.name),
                        "Duplicate Unity module descriptor {}",
                        module.name
                    );
                    Ok(module)
                });
            match parsed {
                Ok(module) => {
                    modules.insert(module.name, module.controlled_by_builtin_package == 1);
                }
                Err(error) => {
                    diagnostics.push(format!("Excluded Unity module descriptor: {error:#}"))
                }
            }
        }
        ensure!(
            modules.contains_key("Core"),
            "Unity module catalog lacks Core"
        );
        let mut watched = BTreeSet::from([module_path]);
        if data.join("manifest.bin").is_file() {
            watched.insert(data.join("manifest.bin"));
            let manifest = crate::metadata_archive::manifest(&data.join("manifest.bin"))?;
            let group = if platform == Platform::EditorLinux {
                "editor"
            } else if backend == 0 {
                "player-mono"
            } else {
                "player-il2cpp"
            };
            let paths = |group: &str| -> Vec<PathBuf> {
                manifest
                    .entries
                    .iter()
                    .filter(|e| e.group == group && e.path.ends_with(".sigla"))
                    .filter_map(|e| e.path.strip_prefix("Editor/Data/").map(|p| data.join(p)))
                    .collect()
            };
            let engine: Vec<_> = paths(group)
                .into_iter()
                .filter(|p| {
                    platform != Platform::EditorLinux
                        || p.starts_with(data.join("Managed/UnityEngine"))
                })
                .collect();
            let standard = paths("standard")
                .into_iter()
                .filter(|p| {
                    let path = p.to_string_lossy();
                    path.contains("/ref/2.1.0/")
                        || path.contains("/compat/2.1.0/")
                        || path.contains("/Extensions/2.0.0/")
                })
                .collect();
            let framework = paths("framework")
                .into_iter()
                .filter(|p| {
                    p.parent().is_some_and(|p| p.ends_with("Facades"))
                        || FRAMEWORK
                            .split_whitespace()
                            .any(|name| p.file_stem().is_some_and(|n| n == name))
                })
                .collect();
            if engine.is_empty() {
                diagnostics.push(format!("Archived editor has no {group} references"));
            }
            return Ok(Self {
                engine,
                standard,
                framework,
                modules,
                watched,
                diagnostics,
            });
        }
        let mut directory = |relative: &str| -> Result<Vec<PathBuf>> {
            let path = data.join(relative);
            watched.insert(path.clone());
            let mut files = Vec::new();
            for (path, kind) in super::entries(&path, &mut diagnostics) {
                if path.extension().is_some_and(|e| e == "dll") {
                    if kind.is_file() {
                        files.push(path);
                    } else {
                        diagnostics.push(format!(
                            "Skipped non-regular Unity reference {}",
                            path.display()
                        ));
                    }
                }
            }
            if files.is_empty() {
                diagnostics.push(format!("No references available in {}", path.display()));
            }
            files.sort();
            Ok(files)
        };
        let engine = directory(if platform == Platform::EditorLinux {
            "Managed/UnityEngine"
        } else if backend == 0 {
            "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed"
        } else {
            "PlaybackEngines/LinuxStandaloneSupport/Variations/il2cpp/Managed"
        })?;
        let mut standard = Vec::new();
        for group in [
            "NetStandard/ref/2.1.0",
            "NetStandard/compat/2.1.0/shims/netfx",
            "NetStandard/compat/2.1.0/shims/netstandard",
            "NetStandard/Extensions/2.0.0",
        ] {
            standard.extend(directory(group)?);
        }
        let mut framework = directory("UnityReferenceAssemblies/unity-4.8-api/Facades")?;
        framework.extend(FRAMEWORK.split_whitespace().map(|name| {
            data.join("UnityReferenceAssemblies/unity-4.8-api")
                .join(format!("{name}.dll"))
        }));
        for path in standard.iter().chain(&framework) {
            if !path.is_file() {
                diagnostics.push(format!(
                    "Missing Unity framework reference {}",
                    path.display()
                ));
            }
        }
        if !engine.iter().any(|p| {
            p.file_name()
                .is_some_and(|n| n == "UnityEngine.CoreModule.dll")
        }) {
            diagnostics.push("Unity reference layout lacks CoreModule".into());
        }
        Ok(Self {
            engine,
            standard,
            framework,
            modules,
            watched,
            diagnostics,
        })
    }
}

pub(super) fn validate_references(data: &Path, version: UnityVersion) -> Result<()> {
    for platform in [Platform::EditorLinux, Platform::StandaloneLinux] {
        let references = References::read(data, version, platform, 0)?;
        ensure!(
            references.diagnostics.is_empty(),
            "Incomplete Unity editor bundle: {}",
            references.diagnostics.join("; ")
        );
    }
    for path in EDITOR_PRECOMPILED {
        ensure!(
            data.join(path).is_file(),
            "Incomplete Unity editor bundle: missing {path}"
        );
    }
    Ok(())
}

pub struct Editor {
    pub declared: UnityVersion,
    pub selected: UnityVersion,
    pub data: PathBuf,
    pub declared_revision: Option<String>,
    pub selected_revision: Option<String>,
}

pub struct CompilationInputs {
    pub defines: BTreeSet<String>,
    pub references: Vec<PathBuf>,
    pub diagnostics: Vec<String>,
}
pub struct AssemblyContext {
    pub predefined: bool,
    pub editor_only: bool,
    pub tests: bool,
    pub no_engine: bool,
    pub editor_compatible: bool,
}

impl Editor {
    pub fn local(project: &Path, installations: Option<&Path>) -> Result<Self> {
        let version = std::fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?;
        let declared: UnityVersion = version
            .lines()
            .find_map(|l| l.strip_prefix("m_EditorVersion:"))
            .context("Unity project has no declared editor version")?
            .trim()
            .parse()?;
        let revision = version
            .lines()
            .find_map(|l| l.strip_prefix("m_EditorVersionWithRevision:"))
            .map(|s| s.trim().to_owned());
        let default;
        let installations = match installations {
            Some(path) => path,
            None => {
                let home = std::env::var_os("HOME")
                    .context("Set --unity-editors when the user's home directory is unavailable")?;
                default = PathBuf::from(home).join("Unity/Hub/Editor");
                &default
            }
        };
        Self::from_installations(project, installations, declared, revision)
    }

    fn from_installations(
        _project: &Path,
        installations: &Path,
        declared: UnityVersion,
        revision: Option<String>,
    ) -> Result<Self> {
        let mut candidates = Vec::new();
        let entries = std::fs::read_dir(installations).with_context(|| format!(
            "Cannot read Unity editors at {}; set --unity-editors to a readable installation directory",
            installations.display()
        ))?;
        for entry in entries {
            let entry = entry?;
            let Ok(selected) = entry.file_name().to_string_lossy().parse::<UnityVersion>() else {
                continue;
            };
            let data = entry.path().join("Editor/Data");
            if supported(selected)
                && data
                    .join("Managed/UnityEngine/UnityEngine.CoreModule.dll")
                    .is_file()
                && data.join("NetStandard/ref/2.1.0/netstandard.dll").is_file()
                && data
                    .join("UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll")
                    .is_file()
            {
                candidates.push((selected == declared, selected, data));
            }
        }
        let (_, selected, data) = candidates
            .into_iter()
            .max()
            .context("No usable installed Unity editor with a supported reference catalog")?;
        Ok(Self {
            declared,
            selected,
            data: data.canonicalize()?,
            selected_revision: (declared == selected).then(|| revision.clone()).flatten(),
            declared_revision: revision,
        })
    }

    pub fn compilation(
        &self,
        layout: &References,
        platform: Platform,
        settings: &Settings,
        assembly: AssemblyContext,
        modules: &BTreeSet<String>,
    ) -> Result<CompilationInputs> {
        let AssemblyContext {
            predefined,
            editor_only,
            tests,
            no_engine,
            editor_compatible,
        } = assembly;
        let api = if editor_only {
            // Unity 6 serializes Default=1, NET_Unity_4_8=2, NET_Standard=3.
            // Earlier supported editors use the default Framework profile.
            if settings.editor_api == 3 { 6 } else { 3 }
        } else {
            settings.api
        };
        let mut defines = symbols(self.selected, platform, api, editor_only);
        let version = self.selected;
        defines.extend([
            format!("UNITY_{}", version.branch.major),
            format!("UNITY_{}", version.branch.to_string().replace('.', "_")),
            format!(
                "UNITY_{}_{}",
                version.branch.to_string().replace('.', "_"),
                version.patch
            ),
        ]);
        defines.insert(
            if platform == Platform::EditorLinux || settings.backend == 0 {
                "ENABLE_MONO"
            } else {
                "ENABLE_IL2CPP"
            }
            .into(),
        );
        if settings.input != 1 {
            defines.insert("ENABLE_LEGACY_INPUT_MANAGER".into());
        }
        if settings.input != 0 {
            defines.insert("ENABLE_INPUT_SYSTEM".into());
        }
        if tests && platform == Platform::EditorLinux {
            defines.insert("UNITY_INCLUDE_TESTS".into());
        }
        defines.extend(settings.defines.iter().cloned());
        let mut references: BTreeSet<_> = if api == 6 {
            &layout.standard
        } else {
            &layout.framework
        }
        .iter()
        .cloned()
        .collect();
        let mut diagnostics = Vec::new();
        if !no_engine {
            for package in modules {
                let module = layout.modules.keys().find(|name| {
                    package == &format!("com.unity.modules.{}", name.to_ascii_lowercase())
                });
                let Some(module) = module else {
                    diagnostics.push(format!(
                        "Unity editor has no module descriptor for {package}"
                    ));
                    continue;
                };
                let filename = format!("UnityEngine.{module}Module.dll");
                if !layout
                    .engine
                    .iter()
                    .any(|p| crate::metadata_archive::logical_filename(p) == filename)
                {
                    diagnostics.push(format!(
                        "Unity editor lacks the managed reference for {package}"
                    ));
                }
            }
            for path in &layout.engine {
                let name = crate::metadata_archive::logical_filename(path);
                if let Some(module) = name
                    .strip_prefix("UnityEngine.")
                    .and_then(|n| n.strip_suffix("Module.dll"))
                {
                    let Some(controlled) = layout.modules.get(module) else {
                        diagnostics.push(format!(
                            "Excluded Unity module {module}: absent from modules.asset"
                        ));
                        continue;
                    };
                    // AR and Insights are excluded from runtime code by Unity,
                    // independently of package control (observed in both layouts).
                    if !editor_only
                        && (matches!(module, "AR" | "Insights")
                            || *controlled
                                && !modules.contains(&format!(
                                    "com.unity.modules.{}",
                                    module.to_ascii_lowercase()
                                )))
                    {
                        continue;
                    }
                } else if name != "UnityEngine.dll"
                    && !(platform == Platform::EditorLinux
                        && (name == "UnityEditor.dll"
                            || name.starts_with("UnityEditor.") && name.ends_with("Module.dll")))
                {
                    continue;
                }
                references.insert(path.clone());
            }
            if platform == Platform::EditorLinux
                && (editor_only || !predefined && editor_compatible)
            {
                references.extend(
                    EDITOR_PRECOMPILED
                        .iter()
                        .map(|p| crate::metadata_archive::reference(self.data.join(p))),
                );
            }
        }
        references.retain(|path| {
            if path.is_file() {
                true
            } else {
                diagnostics.push(format!("Unavailable Unity reference {}", path.display()));
                false
            }
        });
        Ok(CompilationInputs {
            defines,
            references: references.into_iter().collect(),
            diagnostics,
        })
    }
}

// Editor-only precompiled references observed in both supported Linux layouts.
// Unity tests custom-assembly compatibility with response-file defines, but
// without the compilation's general/version defines (TargetAssembly.editorCompatibility).
const EDITOR_PRECOMPILED: &[&str] = &[
    "Managed/UnityEditor.Graphs.dll",
    "PlaybackEngines/LinuxStandaloneSupport/UnityEditor.LinuxStandalone.Extensions.dll",
];

// Unity's default .NET Framework references. Other DLLs in this directory
// remain available to explicit references; they are not automatic imports.
const FRAMEWORK: &str = "
Microsoft.CSharp System.ComponentModel.Composition System.Core System.Data.DataSetExtensions
System.Data System.Drawing System.IO.Compression.FileSystem System.IO.Compression
System.Net.Http System.Numerics.Vectors System.Numerics System.Runtime.Serialization
System.Transactions System.Xml.Linq System.Xml System mscorlib
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_installations_select_the_projects_editor() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let installations = root.path().join("editors");
        std::fs::create_dir_all(project.join("ProjectSettings")).unwrap();
        let version = "6000.3.10f1";
        std::fs::write(
            project.join("ProjectSettings/ProjectVersion.txt"),
            format!("m_EditorVersion: {version}\n"),
        )
        .unwrap();
        let data = installations.join(version).join("Editor/Data");
        for reference in [
            "Managed/UnityEngine/UnityEngine.CoreModule.dll",
            "NetStandard/ref/2.1.0/netstandard.dll",
            "UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll",
        ] {
            let path = data.join(reference);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "fixture").unwrap();
        }
        let editor = Editor::local(&project, Some(&installations)).unwrap();
        assert_eq!(editor.selected, editor.declared);
        assert_eq!(editor.data, data.canonicalize().unwrap());
        assert!(Editor::local(&project, Some(&root.path().join("missing"))).is_err());
    }
}
