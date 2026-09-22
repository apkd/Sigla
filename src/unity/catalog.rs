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
}

impl References {
    pub fn read(
        data: &Path,
        version: UnityVersion,
        platform: Platform,
        backend: u32,
    ) -> Result<Self> {
        ensure!(
            supported(version),
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
        let records: Vec<Module> =
            serde_yaml::from_value(document["PlatformModuleSetup"]["modules"].clone())?;
        let mut modules = BTreeMap::new();
        for module in records {
            ensure!(
                module.controlled_by_builtin_package <= 1 && !module.name.is_empty(),
                "Invalid Unity module descriptor"
            );
            ensure!(
                modules
                    .insert(module.name, module.controlled_by_builtin_package == 1)
                    .is_none(),
                "Duplicate Unity module descriptor"
            );
        }
        ensure!(
            modules.contains_key("Core"),
            "Unity module catalog lacks Core"
        );
        let mut watched = BTreeSet::from([module_path]);
        let mut directory = |relative: &str| -> Result<Vec<PathBuf>> {
            let path = data.join(relative);
            watched.insert(path.clone());
            let mut files = Vec::new();
            for entry in std::fs::read_dir(&path)
                .with_context(|| format!("Missing Unity reference directory {}", path.display()))?
            {
                let entry = entry?;
                if entry.path().extension().is_some_and(|e| e == "dll") {
                    ensure!(
                        entry.file_type()?.is_file(),
                        "Unity reference is not a regular file: {}",
                        entry.path().display()
                    );
                    files.push(entry.path());
                }
            }
            ensure!(
                !files.is_empty(),
                "Empty Unity reference directory {}",
                path.display()
            );
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
            ensure!(
                path.is_file(),
                "Missing Unity framework reference {}",
                path.display()
            );
        }
        ensure!(
            engine.iter().any(|p| p
                .file_name()
                .is_some_and(|n| n == "UnityEngine.CoreModule.dll")),
            "Unity reference layout lacks CoreModule"
        );
        Ok(Self {
            engine,
            standard,
            framework,
            modules,
            watched,
        })
    }
}

pub(super) fn validate_references(data: &Path, version: UnityVersion) -> Result<()> {
    References::read(data, version, Platform::EditorLinux, 0)?;
    References::read(data, version, Platform::StandaloneLinux, 0)?;
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
}
pub struct AssemblyContext {
    pub predefined: bool,
    pub editor_only: bool,
    pub tests: bool,
    pub no_engine: bool,
    pub editor_compatible: bool,
}

impl Editor {
    pub fn local(project: &Path) -> Result<Self> {
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
        let home = std::env::var_os("HOME")
            .context("Cannot locate installed Unity editors without the user's home directory")?;
        Self::from_installations(
            project,
            &PathBuf::from(home).join("Unity/Hub/Editor"),
            declared,
            revision,
        )
    }

    fn from_installations(
        _project: &Path,
        installations: &Path,
        declared: UnityVersion,
        revision: Option<String>,
    ) -> Result<Self> {
        let mut candidates = Vec::new();
        if let Ok(entries) = std::fs::read_dir(installations) {
            for entry in entries {
                let entry = entry?;
                let Ok(selected) = entry.file_name().to_string_lossy().parse::<UnityVersion>()
                else {
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
        if !no_engine {
            for package in modules {
                let module = layout
                    .modules
                    .keys()
                    .find(|name| {
                        package == &format!("com.unity.modules.{}", name.to_ascii_lowercase())
                    })
                    .with_context(|| {
                        format!("Unity editor has no module descriptor for {package}")
                    })?;
                let filename = format!("UnityEngine.{module}Module.dll");
                ensure!(
                    layout
                        .engine
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == filename.as_str())),
                    "Unity editor lacks the managed reference for {package}"
                );
            }
            for path in &layout.engine {
                let name = path.file_name().unwrap().to_string_lossy();
                if let Some(module) = name
                    .strip_prefix("UnityEngine.")
                    .and_then(|n| n.strip_suffix("Module.dll"))
                {
                    let controlled = layout.modules.get(module).with_context(|| {
                        format!("Unity module {module} is absent from modules.asset")
                    })?;
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
                references.extend(EDITOR_PRECOMPILED.iter().map(|p| self.data.join(p)));
            }
        }
        for path in &references {
            ensure!(
                path.is_file(),
                "Unity editor bundle lacks required reference {}",
                path.display()
            );
        }
        Ok(CompilationInputs {
            defines,
            references: references.into_iter().collect(),
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
