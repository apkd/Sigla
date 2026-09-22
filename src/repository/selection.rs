//! Selection concerns tracked paths, never Git checkout rules or attributes.
use anyhow::{Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Component, Path};

pub struct Selection {
    includes: GlobSet,
    excludes: GlobSet,
    pub identity: String,
    pub patterns: (Vec<String>, Vec<String>),
}

impl Selection {
    pub fn new(includes: &[String], excludes: &[String]) -> Result<Self> {
        fn compile(patterns: &[String]) -> Result<GlobSet> {
            let mut builder = GlobSetBuilder::new();
            for pattern in patterns {
                ensure!(
                    !pattern.is_empty()
                        && !Path::new(pattern).is_absolute()
                        && !pattern.split('/').any(|c| c == "..")
                        && !pattern.contains('\\'),
                    "Repository file globs must be relative paths"
                );
                builder.add(
                    GlobBuilder::new(pattern)
                        .literal_separator(true)
                        .backslash_escape(false)
                        .build()?,
                );
            }
            Ok(builder.build()?)
        }
        Ok(Self {
            includes: compile(includes)?,
            excludes: compile(excludes)?,
            identity: blake3::hash(&serde_json::to_vec(&(2u32, includes, excludes))?)
                .to_hex()
                .to_string(),
            patterns: (includes.to_vec(), excludes.to_vec()),
        })
    }

    pub fn selected(&self, path: &str) -> bool {
        self.selected_in(path, &[])
    }

    pub fn selected_in(&self, path: &str, unity_roots: &[std::path::PathBuf]) -> bool {
        let generated = unity_roots.iter().any(|root| {
            Path::new(path)
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.components().next())
                .is_some_and(|p| {
                    matches!(
                        p.as_os_str().to_str(),
                        Some("Library" | "Temp" | "Logs" | "Build" | "Builds")
                    )
                })
        });
        (baseline(path) && !generated || self.includes.is_match(path))
            && !self.excludes.is_match(path)
    }

    pub fn require(&self, path: &str) -> Result<()> {
        validate_path(path)?;
        ensure!(
            !self.excludes.is_match(path),
            "Required discovery input was excluded: {path}"
        );
        Ok(())
    }
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && !path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == ".." || c.eq_ignore_ascii_case(".git")),
        "Invalid tracked repository path"
    );
    Ok(())
}

fn baseline(path: &str) -> bool {
    let path = Path::new(path);
    if path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git" | "obj" | "bin" | "target" | "node_modules" | ".vs")
        )
    }) {
        return false;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(
        path.extension().and_then(|s| s.to_str()),
        Some(
            "cs" | "rs"
                | "csproj"
                | "sln"
                | "slnx"
                | "props"
                | "targets"
                | "asmdef"
                | "asmref"
                | "rsp"
        )
    ) {
        return true;
    }
    if matches!(
        name,
        "global.json"
            | "packages.config"
            | "packages.lock.json"
            | "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
    ) || name.eq_ignore_ascii_case("nuget.config")
    {
        return true;
    }
    if matches!(name, "ProjectVersion.txt" | "ProjectSettings.asset")
        && path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "ProjectSettings"))
    {
        return true;
    }
    if matches!(name, "manifest.json" | "packages-lock.json")
        && path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "Packages"))
    {
        return true;
    }
    if matches!(name, "config" | "config.toml")
        && path
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == ".cargo"))
    {
        return true;
    }
    [".asmdef.meta", ".asmref.meta", ".dll.meta", ".rsp.meta"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_discovery_inputs_are_selected_without_assets() {
        let selection = Selection::new(&[], &[]).unwrap();
        for path in [
            "game/Assets/Code.cs",
            "game/Packages/packages-lock.json",
            "game/Packages/manifest.json",
            "game/ProjectSettings/ProjectVersion.txt",
            "game/ProjectSettings/ProjectSettings.asset",
            "game/Assets/Runtime.asmdef.meta",
            "dotnet/NuGet.Config",
            "dotnet/Directory.Packages.props",
            "rust/.cargo/config.toml",
        ] {
            assert!(selection.selected(path), "{path}");
        }
        for path in [
            "game/Assets/Texture.png",
            "game/Assets/Texture.png.meta",
            "game/Assets/Scene.unity",
            "game/Assets/Plugin.dll",
            "rust/target/Generated.rs",
            "data/arbitrary.json",
        ] {
            assert!(!selection.selected(path), "{path}");
        }
        assert!(!selection.selected_in("game/Library/Generated.cs", &["game".into()]));
        assert!(selection.selected("dotnet/Library/Code.cs"));
    }
    #[test]
    fn overrides_extend_baseline_but_cannot_exclude_required_inputs_silently() {
        let selection =
            Selection::new(&["assets/**/*.txt".into()], &["**/secret/**".into()]).unwrap();
        assert!(selection.selected("assets/notes/readme.txt"));
        assert!(selection.selected("game/Code.cs"));
        assert!(!selection.selected("assets/secret/readme.txt"));
        assert!(selection.require("assets/secret/readme.txt").is_err());
        assert!(selection.require("imports/custom.xml").is_ok());
        for path in [
            "../escape.cs",
            "/absolute.cs",
            "a/../../x",
            "a/.git/config",
            "a\\b.cs",
        ] {
            assert!(validate_path(path).is_err());
        }
    }
}
