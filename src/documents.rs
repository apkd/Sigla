//! Text inputs useful for understanding a project, without symbol extraction.
use crate::model::Language;
use std::path::Path;

pub fn language(path: &Path) -> Option<Language> {
    let name = path.file_name()?.to_str()?;
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "md" {
        return Some(Language::Markdown);
    }
    if matches!(
        extension.as_str(),
        "txt"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "ini"
            | "cfg"
            | "csproj"
            | "sln"
            | "slnx"
            | "props"
            | "targets"
            | "asmdef"
            | "asmref"
            | "rsp"
            | "sh"
            | "bash"
            | "fish"
            | "ps1"
            | "cmd"
            | "bat"
            | "cmake"
            | "nix"
            | "hlsl"
            | "hlsli"
            | "glsl"
            | "shader"
            | "surfshader"
            | "compute"
            | "cginc"
            | "uss"
            | "uxml"
    ) || matches!(
        name,
        "Cargo.lock"
            | "yarn.lock"
            | "Gemfile"
            | "Gemfile.lock"
            | "Makefile"
            | "GNUmakefile"
            | "Dockerfile"
            | "Containerfile"
            | "Justfile"
            | "justfile"
            | ".editorconfig"
            | ".gitignore"
            | ".gitattributes"
            | ".gitmodules"
            | ".dockerignore"
            | "LICENSE"
            | "LICENCE"
            | "COPYING"
            | "NOTICE"
            | "package.json"
            | "package-lock.json"
            | "packages-lock.json"
            | "packages.lock.json"
            | "manifest.json"
            | "global.json"
            | "tsconfig.json"
            | "jsconfig.json"
            | "deno.json"
            | "deno.jsonc"
            | "biome.json"
            | "biome.jsonc"
            | "composer.json"
            | "composer.lock"
            | "CMakePresets.json"
            | "flake.lock"
    ) || name.starts_with("Dockerfile.")
        || name.ends_with(".config.json")
        || name.starts_with("tsconfig.") && name.ends_with(".json")
        || name.starts_with("Containerfile.")
        || name.eq_ignore_ascii_case("nuget.config")
        || name == "packages.config"
    {
        Some(Language::Text)
    } else {
        None
    }
}
