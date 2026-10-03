//! Native Unity discovery. This module never starts Unity or a managed process.
pub mod acquire;
mod catalog;
mod graph;
mod package_acquire;
mod packages;
mod settings;
mod symbols;
mod version;
pub(crate) use graph::discover;
pub use version::{ReleaseBranch, UnityVersion};

pub(crate) fn ignored_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') || name.ends_with('~') || name == "CVS"
}

/// Enumerate readable entries without losing siblings to one filesystem error.
fn entries(
    directory: &std::path::Path,
    diagnostics: &mut Vec<String>,
) -> Vec<(std::path::PathBuf, std::fs::FileType)> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(format!(
                "Cannot read directory {}: {error}",
                directory.display()
            ));
            return result;
        }
    };
    for entry in entries {
        let entry = entry.and_then(|entry| Ok((entry.path(), entry.file_type()?)));
        match entry {
            Ok(entry) => result.push(entry),
            Err(error) => diagnostics.push(format!(
                "Cannot read entry in {}: {error}",
                directory.display()
            )),
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    clap::ValueEnum,
)]
pub enum Platform {
    #[default]
    #[value(name = "UNITY_EDITOR_LINUX")]
    EditorLinux,
    #[value(name = "UNITY_STANDALONE_LINUX")]
    StandaloneLinux,
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::EditorLinux => "UNITY_EDITOR_LINUX",
            Self::StandaloneLinux => "UNITY_STANDALONE_LINUX",
        }
    }
}
pub mod assets;
