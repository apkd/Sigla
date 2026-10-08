//! Bounded parallel loading of immutable serialized asset records.
use super::parse::{self, Object, Parsed};
use crate::workspace::Stamp;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

const VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
struct Cached {
    version: u32,
    stamp: Stamp,
    parsed: Parsed,
    unavailable: Option<String>,
}

pub(super) struct Record {
    pub objects: Vec<Arc<Object>>,
    pub content: Option<PathBuf>,
    pub unavailable: Option<String>,
}

fn load(cache: &Path, file: &Path, stamp: &Stamp, cancel: &CancellationToken) -> Result<Record> {
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    let _admission = crate::memory::admit_file(stamp.size, false);
    ensure!(
        Stamp::read(file)? == *stamp,
        "Asset changed before being read; retry query"
    );
    let record = cache.join(format!(
        "{}.zst",
        blake3::hash(&serde_json::to_vec(&(VERSION, file, stamp))?).to_hex()
    ));
    let cached = std::fs::File::open(&record)
        .ok()
        .and_then(|f| zstd::stream::decode_all(f).ok())
        .and_then(|bytes| postcard::from_bytes::<Cached>(&bytes).ok())
        .filter(|c| c.version == VERSION && c.stamp == *stamp);
    let cached = if let Some(cached) = cached {
        cached
    } else {
        let bytes = std::fs::read(file)?;
        let (parsed, unavailable) = match std::str::from_utf8(&bytes) {
            Ok(text)
                if !text.contains('\0') && !text.bytes().any(|c| c < 9 || (c > 13 && c < 32)) =>
            {
                match parse::parse(text) {
                    Ok(parsed) => (parsed, None),
                    Err(error) => (
                        Parsed::default(),
                        Some(format!("Unsupported serialized contents: {error}")),
                    ),
                }
            }
            _ => (
                Parsed::default(),
                Some(format!("Binary asset ({} bytes)", stamp.size)),
            ),
        };
        ensure!(
            Stamp::read(file)? == *stamp,
            "Asset changed while being read; retry query"
        );
        let cached = Cached {
            version: VERSION,
            stamp: stamp.clone(),
            parsed,
            unavailable,
        };
        let mut output = tempfile::NamedTempFile::new_in(cache)?;
        let bytes = postcard::to_allocvec(&cached)?;
        zstd::stream::copy_encode(bytes.as_slice(), &mut output, 3)?;
        output.persist(&record)?;
        cached
    };
    Ok(Record {
        objects: cached.parsed.objects.into_iter().map(Arc::new).collect(),
        content: cached.unavailable.is_none().then_some(record),
        unavailable: cached.unavailable,
    })
}

pub(super) fn load_all(
    cache: &Path,
    files: &BTreeMap<PathBuf, Stamp>,
    cancel: &CancellationToken,
) -> Result<BTreeMap<PathBuf, Record>> {
    let jobs: Vec<_> = files.iter().collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min(jobs.len());
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    while let Some((file, stamp)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let result = load(cache, file, stamp, cancel);
                        if result.is_err() {
                            next.store(jobs.len(), Ordering::Relaxed);
                        }
                        results.push(((*file).clone(), result));
                    }
                    results
                })
            })
            .collect();
        let mut records = BTreeMap::new();
        for handle in handles {
            for (file, record) in handle
                .join()
                .map_err(|_| anyhow::anyhow!("Asset record worker panicked"))?
            {
                records.insert(file, record?);
            }
        }
        Ok(records)
    })
}

pub(super) fn content(path: &Path) -> Result<Parsed> {
    let bytes = zstd::stream::decode_all(std::fs::File::open(path)?)?;
    let cached: Cached = postcard::from_bytes(&bytes)?;
    Ok(cached.parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_preserve_cold_and_cached_contents_and_reject_changed_inputs() {
        let source = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        for name in ["First", "Second"] {
            let path = source.path().join(format!("{name}.prefab"));
            std::fs::write(
                &path,
                format!("--- !u!1 &1\nGameObject:\n  m_Name: {name}\n"),
            )
            .unwrap();
            files.insert(path.clone(), Stamp::read(&path).unwrap());
        }
        let cancel = CancellationToken::new();
        for _ in 0..2 {
            let records = load_all(cache.path(), &files, &cancel).unwrap();
            assert_eq!(records.len(), files.len());
            for (path, record) in records {
                assert!(record.unavailable.is_none());
                assert_eq!(
                    record.objects[0].name,
                    path.file_stem().unwrap().to_str().unwrap()
                );
                let cached = content(record.content.as_ref().unwrap()).unwrap();
                assert_eq!(cached.objects[0].name, record.objects[0].name);
            }
        }
        let records = load_all(cache.path(), &files, &cancel).unwrap();
        std::fs::write(
            records.values().next().unwrap().content.as_ref().unwrap(),
            b"broken cache",
        )
        .unwrap();
        assert!(load_all(cache.path(), &files, &cancel).is_ok());
        std::fs::write(files.keys().next().unwrap(), b"changed").unwrap();
        assert!(load_all(cache.path(), &files, &cancel).is_err());
        cancel.cancel();
        assert!(load_all(cache.path(), &files, &cancel).is_err());
    }
}
