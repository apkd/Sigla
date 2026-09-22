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
