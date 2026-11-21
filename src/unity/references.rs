//! Reference selection shared by editor discovery and archive production.
use std::path::{Path, PathBuf};

pub(super) const STANDARD_DIRS: &[&str] = &[
    "NetStandard/ref/2.1.0",
    "NetStandard/compat/2.1.0/shims/netfx",
    "NetStandard/compat/2.1.0/shims/netstandard",
    "NetStandard/Extensions/2.0.0",
];
pub(super) const FRAMEWORK_DIR: &str = "UnityReferenceAssemblies/unity-4.8-api";
pub(super) const FACADES_DIR: &str = "UnityReferenceAssemblies/unity-4.8-api/Facades";

// Editor-only precompiled references observed in both supported Linux layouts.
pub(super) const EDITOR_PRECOMPILED: &[&str] = &[
    "Managed/UnityEditor.Graphs.dll",
    "PlaybackEngines/LinuxStandaloneSupport/UnityEditor.LinuxStandalone.Extensions.dll",
];

// Other framework assemblies remain available to explicit references.
pub(super) const FRAMEWORK: &str = "
Microsoft.CSharp System.ComponentModel.Composition System.Core System.Data.DataSetExtensions
System.Data System.Drawing System.IO.Compression.FileSystem System.IO.Compression
System.Net.Http System.Numerics.Vectors System.Numerics System.Runtime.Serialization
System.Transactions System.Xml.Linq System.Xml System mscorlib
";

/// Assemblies used automatically by at least one supported compilation.
pub(crate) fn default_reference(group: &str, relative: &Path) -> bool {
    let dll = relative.with_extension("dll");
    let name = dll.file_name().and_then(|name| name.to_str()).unwrap_or("");
    match group {
        "editor" => {
            dll.starts_with("Managed/UnityEngine") && engine_reference(name, true)
                || EDITOR_PRECOMPILED.iter().any(|path| dll == Path::new(path))
        }
        "player-mono" | "player-il2cpp" => engine_reference(name, false),
        "standard" => STANDARD_DIRS
            .iter()
            .any(|directory| dll.starts_with(directory)),
        "framework" => {
            dll.parent().is_some_and(|path| path.ends_with("Facades"))
                || FRAMEWORK
                    .split_whitespace()
                    .any(|name| dll.file_stem().is_some_and(|stem| stem == name))
        }
        _ => true,
    }
}

pub(super) fn engine_reference(name: &str, editor: bool) -> bool {
    name == "UnityEngine.dll"
        || name.starts_with("UnityEngine.") && name.ends_with("Module.dll")
        || editor
            && (name == "UnityEditor.dll"
                || name.starts_with("UnityEditor.") && name.ends_with("Module.dll"))
}

pub(crate) fn profile_paths(name: &str, api: u32) -> Vec<PathBuf> {
    if Path::new(name).file_name().and_then(|name| name.to_str()) != Some(name)
        || name.contains('\\')
    {
        return Vec::new();
    }
    let filename = format!("{}.dll", name.strip_suffix(".dll").unwrap_or(name));
    let directories = if api == 6 {
        STANDARD_DIRS
    } else {
        &[FRAMEWORK_DIR, FACADES_DIR]
    };
    directories
        .iter()
        .map(|directory| Path::new(directory).join(&filename))
        .collect()
}
