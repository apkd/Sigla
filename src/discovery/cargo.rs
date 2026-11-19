use super::{Policy, crosses_worktree, linked_worktree};
use anyhow::Result;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};
use toml::Value;

pub(super) fn roots(
    base: &Path,
    manifest: &Value,
    package_name: &str,
    edition: &str,
    policy: &Policy,
    diagnostics: &mut Vec<String>,
) -> Result<Vec<(PathBuf, String)>> {
    let package = &manifest["package"];
    let mut roots = Vec::new();
    let lib = manifest.get("lib");
    let default_lib = base.join("src/lib.rs");
    if lib.is_some()
        || (package.get("autolib").and_then(Value::as_bool) != Some(false)
            && !crosses_worktree(base, &base.join("src"))
            && policy.has_file(&default_lib)?)
    {
        let name = lib
            .and_then(|l| field(l, "name"))
            .unwrap_or(package_name)
            .replace('-', "_");
        let path = match lib.and_then(|l| field(l, "path")) {
            Some(path) => manifest_path(base, path),
            None if edition == "2015" && !policy.has_file(&default_lib)? => {
                base.join("src").join(format!("{name}.rs"))
            }
            None => default_lib,
        };
        roots.push((path, name));
    }
    let has_lib = !roots.is_empty();
    for (kind, dir, flag) in [
        ("bin", "src/bin", "autobins"),
        ("test", "tests", "autotests"),
        ("example", "examples", "autoexamples"),
        ("bench", "benches", "autobenches"),
    ] {
        let targets = manifest.get(kind).and_then(Value::as_array);
        let automatic = package
            .get(flag)
            .and_then(Value::as_bool)
            .unwrap_or(edition != "2015" || targets.is_none());
        let targets = targets.map(Vec::as_slice).unwrap_or_default();
        // A declaration without a path still needs inference when automatic targets are off.
        let mut inferred = BTreeMap::<String, Vec<PathBuf>>::new();
        if automatic || targets.iter().any(|t| field(t, "path").is_none()) {
            if kind == "bin" && !crosses_worktree(base, &base.join("src")) {
                let main = base.join("src/main.rs");
                if policy.has_file(&main)? {
                    inferred.insert(package_name.into(), vec![main]);
                }
            }
            infer_directory(base, dir, policy, &mut inferred, diagnostics)?;
        }
        let mut explicit_paths = HashSet::new();
        for target in targets {
            let Some(name) = field(target, "name") else {
                diagnostics.push(format!(
                    "Cargo {kind} target missing name in {}",
                    base.display()
                ));
                continue;
            };
            // Cargo excludes inferred targets of the same kind by either name or path.
            let candidates = inferred.remove(name).unwrap_or_default();
            let path = if let Some(path) = field(target, "path") {
                let path = manifest_path(base, path);
                explicit_paths.insert(path.clone());
                Some(path)
            } else if candidates.len() == 1 {
                candidates.into_iter().next()
            } else if edition == "2015" {
                legacy_path(base, kind, name, has_lib, policy)?
            } else {
                None
            };
            if let Some(path) = path {
                roots.push((path, name.replace('-', "_")));
            } else {
                diagnostics.push(format!(
                    "Cannot infer Cargo {kind} target {name} in {}",
                    base.display()
                ));
            }
        }
        if automatic {
            for (name, paths) in inferred {
                roots.extend(
                    paths
                        .into_iter()
                        .filter(|path| !explicit_paths.contains(path))
                        .map(|path| (path, name.replace('-', "_"))),
                );
            }
        }
    }
    let build = match package.get("build") {
        Some(Value::Boolean(false)) => None,
        Some(Value::String(path)) => Some(manifest_path(base, path)),
        _ if policy.has_file(&base.join("build.rs"))? => Some(base.join("build.rs")),
        _ => None,
    };
    if let Some(path) = build
        && let Some(stem) = path.file_stem().and_then(OsStr::to_str)
    {
        let name = format!("build_script_{}", stem.replace('-', "_"));
        roots.push((path, name));
    }
    Ok(roots)
}

fn field<'a>(table: &'a Value, key: &str) -> Option<&'a str> {
    table.get(key).and_then(Value::as_str)
}

fn manifest_path(base: &Path, path: &str) -> PathBuf {
    let mut normalized = PathBuf::new();
    for part in base.join(path).components() {
        if part == Component::ParentDir {
            normalized.pop();
        } else {
            normalized.push(part);
        }
    }
    normalized
}

fn infer_directory(
    base: &Path,
    dir: &str,
    policy: &Policy,
    inferred: &mut BTreeMap<String, Vec<PathBuf>>,
    diagnostics: &mut Vec<String>,
) -> Result<()> {
    let directory = base.join(dir);
    if crosses_worktree(base, &directory) {
        return Ok(());
    }
    let mut add = |entry: &OsStr, path: PathBuf| {
        if let Some(name) = entry.to_str().filter(|s| !s.starts_with('.')) {
            inferred.entry(name.into()).or_default().push(path);
        }
    };
    if let Some(remote) = &policy.remote
        && !remote.tracked.is_empty()
        && let Ok(relative) = directory.strip_prefix(&remote.workspace)
    {
        for key in remote.tracked_files_in(&directory)? {
            let path = Path::new(key);
            let parent = path.parent().unwrap();
            if parent == relative && path.extension().is_some_and(|e| e == "rs") {
                add(path.file_stem().unwrap(), remote.workspace.join(path));
            } else if parent.parent() == Some(relative)
                && path.file_name().is_some_and(|f| f == "main.rs")
                && !linked_worktree(&remote.workspace.join(parent))
            {
                add(parent.file_name().unwrap(), remote.workspace.join(path));
            }
        }
    } else if let Ok(entries) = directory.read_dir() {
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(format!(
                        "Cannot read Cargo target in {}: {error}",
                        directory.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let main = path.join("main.rs");
                if main.is_file() && !linked_worktree(&path) {
                    add(&entry.file_name(), main);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                add(path.file_stem().unwrap(), path.clone());
            }
        }
    }
    Ok(())
}

// Cargo still accepts these paths for explicit Rust 2015 targets without a unique match.
fn legacy_path(
    base: &Path,
    kind: &str,
    name: &str,
    has_lib: bool,
    policy: &Policy,
) -> Result<Option<PathBuf>> {
    let src = base.join("src");
    let candidates = match kind {
        "bin" => [
            (!has_lib).then(|| src.join(format!("{name}.rs"))),
            Some(src.join("main.rs")),
            Some(src.join("bin/main.rs")),
        ],
        "bench" if name == "bench" => [Some(src.join("bench.rs")), None, None],
        _ => return Ok(None),
    };
    for path in candidates.into_iter().flatten() {
        if policy.has_file(&path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, fs, process::Command};

    #[test]
    fn declared_and_inferred_targets_match_cargo() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        for path in [
            "src/main.rs",
            "src/lib.rs",
            "commands/custom.rs",
            "build.rs",
            "src/bin/tool.rs",
            "src/bin/other/main.rs",
            "src/bin/.scratch.rs",
            "tests/tool/main.rs",
            "tests/extra.rs",
            "examples/tool.rs",
            "examples/extra/main.rs",
            "benches/tool.rs",
            "benches/extra.rs",
        ] {
            let path = base.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        let policy = Policy::new(vec![base.into()]).unwrap();
        let declarations = "
            [[bin]]
            name='tool'
            path='commands/custom.rs'
            [[test]]
            name='tool'
            [[example]]
            name='renamed'
            path='./examples/tool.rs'
            [[bench]]
            name='tool'
            path='commands/custom.rs'
        ";
        for (edition, flags, targets) in [
            ("2015", "", declarations),
            ("2021", "", declarations),
            ("2015", "autobins=true\nautotests=true", declarations),
            (
                "2024",
                "autobins=false\nautotests=false\nautobenches=false",
                declarations,
            ),
            (
                "2021",
                "",
                "[[bin]]\nname='my-app'\npath='commands/custom.rs'",
            ),
            ("2021", "autobins=false", "[[bin]]\nname='my-app'"),
            ("2021", "autolib=false\nbuild=false", ""),
            (
                "2021",
                "build='commands/custom.rs'",
                "[lib]\nname='shared_lib'\npath='src/main.rs'",
            ),
        ] {
            let manifest = format!(
                "[package]\nname='my-app'\nversion='0.1.0'\nedition='{edition}'\n{flags}\n{targets}"
            );
            fs::write(base.join("Cargo.toml"), &manifest).unwrap();
            let output = Command::new(env!("CARGO"))
                .current_dir(base)
                .args(["metadata", "--offline", "--no-deps", "--format-version=1"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let expected: BTreeSet<_> = metadata["packages"][0]["targets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    (
                        PathBuf::from(t["src_path"].as_str().unwrap()),
                        t["name"].as_str().unwrap().replace('-', "_"),
                    )
                })
                .collect();
            let mut diagnostics = Vec::new();
            let actual: BTreeSet<_> = roots(
                base,
                &toml::from_str(&manifest).unwrap(),
                "my-app",
                edition,
                &policy,
                &mut diagnostics,
            )
            .unwrap()
            .into_iter()
            .collect();
            assert!(diagnostics.is_empty(), "{manifest}: {diagnostics:?}");
            assert_eq!(actual, expected, "{manifest}");
        }
    }
}
