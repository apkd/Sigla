//! Weak Git-object aliases to immutable files. Alias records never retain blobs.
use super::materialize::{Metrics, Prepared, Request};
use crate::cache::blobs::{Id, Linked, Store};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = MAX_BYTES as usize / 192;

#[derive(Serialize, Deserialize)]
struct Catalog {
    version: u32,
    entries: BTreeMap<String, String>,
    checksum: String,
}

fn checksum(entries: &BTreeMap<String, String>) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    serde_json::to_writer(&mut hash, entries)?;
    Ok(hash.finalize().to_hex().to_string())
}

fn key(oid: &str, mode: u32) -> String {
    let format = if oid.len() == 64 { "sha256" } else { "sha1" };
    format!("{format}/{oid}/{mode:o}")
}

fn decode(key: &str, value: &str) -> Option<(Id, u32)> {
    let mut parts = key.split('/');
    let width = match parts.next()? {
        "sha1" => 40,
        "sha256" => 64,
        _ => return None,
    };
    let oid = parts.next()?;
    let mode_text = parts.next()?;
    if mode_text.len() > 3 {
        return None;
    }
    let mode = u32::from_str_radix(mode_text, 8).ok()?;
    if oid.len() != width
        || !oid.bytes().all(|b| b.is_ascii_hexdigit())
        || mode > 0o777
        || parts.next().is_some()
    {
        return None;
    }
    Some((*blake3::Hash::from_hex(value).ok()?.as_bytes(), mode))
}

fn read(root: &Path) -> BTreeMap<String, String> {
    let load = || -> Result<Catalog> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("files.json"))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_BYTES,
            "Invalid Git file aliases"
        );
        let catalog: Catalog = serde_json::from_reader(std::io::BufReader::new(file))?;
        ensure!(
            catalog.version == 1 && catalog.entries.len() <= MAX_ENTRIES,
            "Unsupported Git file aliases"
        );
        ensure!(
            catalog.checksum == checksum(&catalog.entries)?,
            "Corrupt Git file aliases"
        );
        Ok(catalog)
    };
    load().map(|catalog| catalog.entries).unwrap_or_default()
}

fn writer(root: &Path) -> Result<File> {
    Ok(File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("files.lock"))?)
}

fn publish(root: &Path, mut entries: BTreeMap<String, String>) -> Result<()> {
    entries.retain(|key, value| decode(key, value).is_some());
    while entries.len() > MAX_ENTRIES {
        entries.pop_first();
    }
    super::manager::write_json(
        &root.join("files.json"),
        &Catalog {
            version: 1,
            checksum: checksum(&entries)?,
            entries,
        },
    )?;
    File::open(root)?.sync_all()?;
    Ok(())
}

pub(super) fn reuse(
    request: &Request,
    cache: &Path,
    changed: &mut BTreeMap<String, String>,
    omitted: &mut BTreeMap<String, String>,
    metrics: &mut Metrics,
) -> Result<()> {
    let aliases = read(request.store.parent().context("Git store has no owner")?);
    if aliases.is_empty() || changed.is_empty() {
        return Ok(());
    }
    let files = Store::open(cache)?;
    let _lease = files.lease()?;
    fs::create_dir_all(&request.staging)?;
    let mode = crate::cache::blobs::creation_mode(&request.staging)?;
    let mut reused = Vec::new();
    for (path, oid) in changed.iter() {
        let key = key(oid, mode);
        let Some((id, mode)) = aliases.get(&key).and_then(|value| decode(&key, value)) else {
            continue;
        };
        let limit = (super::selection::asset(Path::new(path))
            || super::selection::visual_graph(Path::new(path)))
        .then_some(super::selection::ASSET_LIMIT);
        match files.link_cached(&id, mode, limit, &request.staging.join(path))? {
            Linked::Missing => {}
            Linked::Oversized(size) => {
                omitted.insert(
                    path.clone(),
                    format!("Exceeds remote asset size limit ({size} bytes)"),
                );
                reused.push(path.clone());
            }
            Linked::File(size) => {
                metrics.reused_files += 1;
                metrics.reused_bytes += size;
                reused.push(path.clone());
            }
        }
    }
    for path in reused {
        changed.remove(&path);
    }
    Ok(())
}

/// Call after importing raw Git files and before replacing any LFS pointers.
pub(super) fn record(root: &Path, cache: &Path, prepared: &Prepared, staging: &Path) -> Result<()> {
    let files = Store::open(cache)?;
    let mut additions = BTreeMap::new();
    for (path, oid) in &prepared.selected {
        if let Some((id, mode)) = files.file_identity(&staging.join(path))? {
            additions.insert(
                key(oid, mode),
                blake3::Hash::from_bytes(id).to_hex().to_string(),
            );
        }
    }
    if additions.is_empty() {
        return Ok(());
    }
    let _writer = writer(root)?;
    _writer.lock()?;
    let mut entries = read(root);
    if additions
        .iter()
        .all(|(key, value)| entries.get(key) == Some(value))
    {
        return Ok(());
    }
    entries.extend(additions);
    publish(root, entries)
}

pub(super) fn prune(root: &Path, cache: &Path) -> Result<()> {
    if !root.join("files.json").is_file() {
        return Ok(());
    }
    let files = Store::open(cache)?;
    let _writer = writer(root)?;
    match _writer.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    let entries = read(root);
    let mut retained = BTreeMap::new();
    for (key, value) in &entries {
        if let Some((id, mode)) = decode(key, value)
            && files.cached_size(&id, mode)?.is_some()
        {
            retained.insert(key.clone(), value.clone());
        }
    }
    if retained.len() != entries.len() {
        publish(root, retained)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_publishers_merge_and_invalid_catalogs_are_cache_misses() {
        let root = tempfile::tempdir().unwrap();
        let files = Store::open(root.path()).unwrap();
        let mut jobs = Vec::new();
        for name in ["a", "b"] {
            let stage = root.path().join(name);
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("file.cs"), name).unwrap();
            files.import_owned_tree(&stage).unwrap();
            let prepared = Prepared {
                selected: BTreeMap::from([("file.cs".into(), name.repeat(40))]),
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                branch: None,
                revision: String::new(),
                tracked: Default::default(),
                directories: Default::default(),
            };
            jobs.push((stage, prepared));
        }
        let barrier = std::sync::Barrier::new(jobs.len());
        std::thread::scope(|scope| {
            for (stage, prepared) in &jobs {
                let barrier = &barrier;
                let root = root.path();
                scope.spawn(move || {
                    barrier.wait();
                    record(root, root, prepared, stage).unwrap();
                });
            }
        });
        let entries = read(root.path());
        assert_eq!(entries.len(), jobs.len());
        for (stage, prepared) in &jobs {
            let (id, mode) = files
                .file_identity(&stage.join("file.cs"))
                .unwrap()
                .unwrap();
            assert_eq!(
                entries[&key(&prepared.selected["file.cs"], mode)],
                blake3::Hash::from_bytes(id).to_hex().as_str()
            );
        }
        let mut corrupt: Catalog =
            serde_json::from_slice(&fs::read(root.path().join("files.json")).unwrap()).unwrap();
        let values: Vec<_> = corrupt.entries.values().cloned().collect();
        for (entry, value) in corrupt.entries.values_mut().zip(values.into_iter().rev()) {
            *entry = value;
        }
        super::super::manager::write_json(&root.path().join("files.json"), &corrupt).unwrap();
        assert!(read(root.path()).is_empty());
        fs::write(
            root.path().join("files.json"),
            br#"{"version":999,"entries":{}}"#,
        )
        .unwrap();
        assert!(read(root.path()).is_empty());
        File::create(root.path().join("files.json"))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(read(root.path()).is_empty());
    }
}
