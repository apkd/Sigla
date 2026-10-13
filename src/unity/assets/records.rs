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

const VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
struct Cached {
    version: u32,
    parsed: Parsed,
    unavailable: Option<String>,
}

pub(super) struct Record {
    pub objects: Vec<Arc<Object>>,
    pub content: Option<PathBuf>,
    pub unavailable: Option<String>,
}

fn load(
    cache: &Path,
    shared: Option<&crate::cache::artifacts::Store>,
    file: &Path,
    stamp: &Stamp,
    cancel: &CancellationToken,
) -> Result<Record> {
    ensure!(
        !cancel.is_cancelled(),
        "Asset indexing cancelled or superseded"
    );
    ensure!(
        Stamp::read(file)? == *stamp,
        "Asset changed before being read; retry query"
    );
    let mut source = None;
    let identity = if let Some(id) = stamp.immutable() {
        *id
    } else {
        let bytes = std::fs::read(file)?;
        let id = *blake3::hash(&bytes).as_bytes();
        source = Some(bytes);
        id
    };
    let key = blake3::hash(&postcard::to_allocvec(&(
        VERSION,
        env!("SIGLA_ASSET_RECORD_FINGERPRINT"),
        identity,
    ))?)
    .to_hex();
    let record = cache.join(format!("{key}.summary.zst"));
    let text = cache.join(format!("{key}.text.zst"));
    if let Some(shared) = shared {
        shared.restore(&text)?;
        shared.restore(&record)?;
    }
    let cached = std::fs::File::open(&record)
        .ok()
        .and_then(|f| zstd::stream::decode_all(f).ok())
        .and_then(|bytes| postcard::from_bytes::<Cached>(&bytes).ok())
        .filter(|c| c.version == VERSION && (c.unavailable.is_some() || text.is_file()));
    let cached = if let Some(cached) = cached {
        cached
    } else {
        let _admission = crate::memory::admit_file(stamp.size, false);
        let _worker = crate::memory::admit_worker();
        let bytes = source.map_or_else(|| std::fs::read(file), Ok)?;
        let (mut parsed, unavailable) = match std::str::from_utf8(&bytes) {
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
        if unavailable.is_none() {
            let content = Parsed {
                objects: Vec::new(),
                fragments: std::mem::take(&mut parsed.fragments),
                omissions: std::mem::take(&mut parsed.omissions),
                bytes: parsed.bytes,
            };
            save(&text, &content)?;
            if let Some(shared) = shared {
                shared.publish(&text)?;
            }
        }
        let cached = Cached {
            version: VERSION,
            parsed,
            unavailable,
        };
        save(&record, &cached)?;
        if let Some(shared) = shared {
            shared.publish(&record)?;
        }
        cached
    };
    Ok(Record {
        objects: cached.parsed.objects.into_iter().map(Arc::new).collect(),
        content: cached.unavailable.is_none().then_some(text),
        unavailable: cached.unavailable,
    })
}

pub(super) fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut output = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let mut encoder = zstd::stream::write::Encoder::new(&mut output, 3)?;
    encoder.include_checksum(true)?;
    postcard::to_io(value, &mut encoder)?;
    encoder.finish()?;
    output.persist(path)?;
    Ok(())
}

pub(super) fn load_all(
    cache: &Path,
    shared: Option<&crate::cache::artifacts::Store>,
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
                        let result = load(cache, shared, file, stamp, cancel);
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
    let mut parsed: Parsed = postcard::from_bytes(&bytes)?;
    let name = path.file_name().unwrap().to_string_lossy();
    let summary = path.with_file_name(name.replace(".text.zst", ".summary.zst"));
    let bytes = zstd::stream::decode_all(std::fs::File::open(summary)?)?;
    let cached: Cached = postcard::from_bytes(&bytes)?;
    parsed.objects = cached.parsed.objects;
    Ok(parsed)
}

pub(super) fn available(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_string_lossy();
    path.is_file()
        && path
            .with_file_name(name.replace(".text.zst", ".summary.zst"))
            .is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_records_are_shared_across_views_and_collected_after_last_owner() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let shared = crate::cache::artifacts::Store::open(root.path()).unwrap();
        let first = tempfile::tempdir_in(root.path()).unwrap();
        let second = tempfile::tempdir_in(root.path()).unwrap();
        let cancel = CancellationToken::new();
        let mut paths = Vec::new();
        for view in [&first, &second] {
            let source = view.path().join("Object.prefab");
            std::fs::write(&source, "--- !u!1 &1\nGameObject:\n  m_Name: Example\n").unwrap();
            let record = load(
                view.path(),
                Some(&shared),
                &source,
                &Stamp::read(&source).unwrap(),
                &cancel,
            )
            .unwrap();
            paths.push(record.content.unwrap());
        }
        assert_eq!(
            std::fs::metadata(&paths[0]).unwrap().ino(),
            std::fs::metadata(&paths[1]).unwrap().ino()
        );
        assert_eq!(shared.collect(true).unwrap(), 0);
        drop(first);
        assert_eq!(shared.collect(true).unwrap(), 0);
        assert_eq!(content(&paths[1]).unwrap().objects[0].name, "Example");
        drop(second);
        assert!(shared.collect(true).unwrap() > 0);
    }

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
            let records = load_all(cache.path(), None, &files, &cancel).unwrap();
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
        let records = load_all(cache.path(), None, &files, &cancel).unwrap();
        std::fs::write(
            records.values().next().unwrap().content.as_ref().unwrap(),
            b"broken cache",
        )
        .unwrap();
        assert!(load_all(cache.path(), None, &files, &cancel).is_ok());
        std::fs::write(files.keys().next().unwrap(), b"changed").unwrap();
        assert!(load_all(cache.path(), None, &files, &cancel).is_err());
        cancel.cancel();
        assert!(load_all(cache.path(), None, &files, &cancel).is_err());
    }
}
