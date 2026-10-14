//! Reuse dependency license scans across ordinary source edits.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize, Serialize)]
struct Cache {
    key: String,
    files: BTreeMap<PathBuf, String>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_digest(path: &Path) -> Result<String> {
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn local_key(root: &Path, target: &Path) -> Result<String> {
    let mut pending = vec![root.to_owned()];
    let mut visited = BTreeSet::new();
    let mut entries = BTreeMap::<PathBuf, Option<String>>::new();
    while let Some(directory) = pending.pop() {
        entries.insert(directory.clone(), None);
        if !visited.insert(directory.canonicalize()?) {
            continue;
        }
        let mut children = fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort();
        for path in children {
            if path == target || path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            let metadata = fs::metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let hash = file_digest(&path)?;
                entries.insert(path, Some(hash));
            }
        }
    }
    Ok(digest(&serde_json::to_vec(&entries)?))
}

impl Cache {
    fn capture(key: String, paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let files = paths
            .into_iter()
            .map(|path| {
                let hash = file_digest(&path)?;
                Ok((path, hash))
            })
            .collect::<Result<_>>()?;
        Ok(Self { key, files })
    }

    fn valid(&self, key: &str) -> bool {
        self.key == key
            && self
                .files
                .iter()
                .all(|(path, hash)| file_digest(path).is_ok_and(|value| value == *hash))
    }

    fn watch(&self, report: &Path) {
        for path in self.files.keys().filter(|path| path.as_path() != report) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

pub fn collect(output: &Path) -> Result<Vec<u8>> {
    let cargo = env::var_os("CARGO").context("Missing CARGO")?;
    let target = env::var("TARGET")?;
    let metadata = Command::new(&cargo)
        .args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
        ])
        .arg(&target)
        .output()?;
    ensure!(
        metadata.status.success(),
        "Cannot inspect the dependency graph for license generation: {}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let graph: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let packages = graph["packages"]
        .as_array()
        .context("Missing dependency inventory")?;
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").context("Missing manifest directory")?)
            .join("Cargo.toml");
    // Registry checksums and Git revisions pin dependency sources. Include local
    // dependency contents and directory membership because their graph can stay unchanged.
    let target_directory = Path::new(
        graph["target_directory"]
            .as_str()
            .context("Missing build output directory")?,
    );
    let mut local = BTreeMap::new();
    let mut cacheable = true;
    for package in packages
        .iter()
        .filter(|package| package["source"].is_null())
    {
        let dependency = Path::new(
            package["manifest_path"]
                .as_str()
                .context("Missing crate manifest")?,
        );
        if dependency == manifest {
            continue;
        }
        let root = dependency.parent().context("Missing crate root")?;
        println!("cargo:rerun-if-changed={}", root.display());
        match local_key(root, target_directory) {
            Ok(key) => {
                local.insert(root, key);
            }
            // An incomplete inventory cannot justify reusing a previous scan.
            Err(_) => cacheable = false,
        }
    }
    let version = Command::new(&cargo).args(["about", "--version"]).output()?;
    ensure!(
        version.status.success(),
        "Cannot collect Cargo licenses; install cargo-about 0.9.2"
    );
    let mut hash = Sha256::new();
    for input in [
        metadata.stdout,
        version.stdout,
        target.into_bytes(),
        fs::read("Cargo.toml")?,
        fs::read("Cargo.lock")?,
        fs::read("about.toml")?,
        fs::read("LICENSE")?,
        fs::read("build.rs")?,
        fs::read("scripts/licenses.rs")?,
        fs::read("scripts/cargo_licenses.rs")?,
        serde_json::to_vec(&local)?,
    ] {
        hash.update((input.len() as u64).to_le_bytes());
        hash.update(input);
    }
    let key = format!("{:x}", hash.finalize());
    let json = output.join("cargo-licenses.json");
    let stamp = output.join("cargo-licenses-cache.json");
    if cacheable
        && let Some(cache) = fs::read(&stamp)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        && cache.valid(&key)
    {
        cache.watch(&json);
        return Ok(fs::read(json)?);
    }
    ensure!(
        Command::new(cargo)
            .args([
                "about", "generate", "--locked", "--fail", "--format", "json", "--target"
            ])
            .arg(env::var("TARGET")?)
            .arg("-o")
            .arg(&json)
            .env(
                "RAYON_NUM_THREADS",
                env::var_os("RAYON_NUM_THREADS").unwrap_or_else(|| "4".into())
            )
            .status()?
            .success(),
        "Cannot collect Cargo licenses; install cargo-about 0.9.2"
    );
    let bytes = fs::read(&json)?;
    let report: serde_json::Value = serde_json::from_slice(&bytes)?;
    let mut inputs = BTreeSet::from([json.clone()]);
    for license in report["licenses"]
        .as_array()
        .context("Missing license inventory")?
    {
        for usage in license["used_by"]
            .as_array()
            .context("Missing license users")?
        {
            let manifest = PathBuf::from(
                usage["crate"]["manifest_path"]
                    .as_str()
                    .context("Missing crate manifest")?,
            );
            let root = manifest.parent().context("Missing crate root")?;
            if let Some(source) = license["source_path"].as_str() {
                let path = root.join(source);
                // Git-only notices are pinned by the checksums in about.toml.
                if path.try_exists()? {
                    inputs.insert(path);
                }
            }
            inputs.insert(manifest);
        }
    }
    let cache = Cache::capture(key, inputs)?;
    cache.watch(&json);
    fs::write(stamp, serde_json::to_vec(&cache)?)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dependency_edits_additions_and_removals_invalidate_the_scan() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let source = root.path().join("lib.rs");
        fs::write(&source, "original source").unwrap();
        let initial = local_key(root.path(), &target).unwrap();
        fs::write(&source, "changed source").unwrap();
        assert_ne!(local_key(root.path(), &target).unwrap(), initial);
        fs::write(&source, "original source").unwrap();
        assert_eq!(local_key(root.path(), &target).unwrap(), initial);
        let notice = root.path().join("NOTICE");
        fs::write(&notice, "additional notice").unwrap();
        assert_ne!(local_key(root.path(), &target).unwrap(), initial);
        fs::remove_file(notice).unwrap();
        assert_eq!(local_key(root.path(), &target).unwrap(), initial);
        fs::create_dir(&target).unwrap();
        fs::write(target.join("generated"), "build output").unwrap();
        assert_eq!(local_key(root.path(), &target).unwrap(), initial);
    }

    #[test]
    fn cache_reuses_unchanged_inputs_and_rejects_changed_missing_or_corrupt_files() {
        let directory = tempfile::tempdir().unwrap();
        let notice = directory.path().join("LICENSE");
        let report = directory.path().join("report.json");
        fs::write(&notice, "original notice").unwrap();
        fs::write(&report, "generated report").unwrap();
        let cache =
            Cache::capture("graph and tool".into(), [notice.clone(), report.clone()]).unwrap();
        assert!(cache.valid("graph and tool"));
        assert!(!cache.valid("changed graph or tool"));
        fs::write(&notice, "revised notice").unwrap();
        assert!(!cache.valid("graph and tool"));
        fs::write(&notice, "original notice").unwrap();
        assert!(cache.valid("graph and tool"));
        fs::write(&report, "incomplete report").unwrap();
        assert!(!cache.valid("graph and tool"));
        fs::write(&report, "generated report").unwrap();
        fs::remove_file(&notice).unwrap();
        assert!(!cache.valid("graph and tool"));
    }
}
