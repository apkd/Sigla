//! Bounded parallel loading of immutable serialized asset records.
use super::parse::{self, Object, Parsed};
use crate::{cancellation::Cancellation, workspace::Stamp};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
struct Cached {
    parsed: Parsed,
    unavailable: Option<String>,
}

pub(super) struct Record {
    pub objects: Arc<Vec<Arc<Object>>>,
    pub content: Option<PathBuf>,
    pub unavailable: Option<String>,
}

type Objects = Arc<Vec<Arc<Object>>>;
type ObjectCache = lru::LruCache<std::ffi::OsString, Weak<Vec<Arc<Object>>>>;
pub(super) fn empty() -> Objects {
    static EMPTY: LazyLock<Objects> = LazyLock::new(|| Arc::new(Vec::new()));
    EMPTY.clone()
}
// Weak Vec owners release the object array when the last index retires. Only
// small weak entries remain, bounded independently of the live graph budget.
static OBJECTS: LazyLock<Mutex<ObjectCache>> = LazyLock::new(|| {
    Mutex::new(lru::LruCache::new(
        std::num::NonZeroUsize::new(16_384).unwrap(),
    ))
});

fn shared(path: &Path) -> Option<Objects> {
    OBJECTS
        .lock()
        .unwrap()
        .get(path.file_name().unwrap())
        .and_then(Weak::upgrade)
}

pub(super) fn intern(path: &Path, objects: Objects) -> Objects {
    if objects.is_empty() {
        return empty();
    }
    let existing = {
        let mut cache = OBJECTS.lock().unwrap();
        let key = path.file_name().unwrap();
        match cache.get(key).and_then(Weak::upgrade) {
            Some(existing) => Some(existing),
            None => {
                cache.put(key.to_owned(), Arc::downgrade(&objects));
                None
            }
        }
    };
    // Release a racing duplicate graph after unlocking the cache.
    existing.unwrap_or(objects)
}

pub(super) fn load(
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
    let _admission = crate::memory::admit_file(stamp.size, false, cancel)?;
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
    if record.is_file()
        && text.is_file()
        && let Some(objects) = self::shared(&text)
    {
        cancel.check()?;
        return Ok(Record {
            objects,
            content: Some(text),
            unavailable: None,
        });
    }
    let cached = read::<Cached>(&record, cancel)
        .ok()
        .filter(|c| c.unavailable.is_some() || text.is_file());
    cancel.check()?;
    let cached = if let Some(cached) = cached {
        cached
    } else {
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
            parsed,
            unavailable,
        };
        save(&record, &cached)?;
        if let Some(shared) = shared {
            shared.publish(&record)?;
        }
        cached
    };
    let objects = Arc::new(cached.parsed.objects.into_iter().map(Arc::new).collect());
    let objects = if cached.unavailable.is_none() {
        intern(&text, objects)
    } else if objects.is_empty() {
        empty()
    } else {
        objects
    };
    Ok(Record {
        objects,
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

pub(super) fn content(path: &Path, cancel: &dyn Cancellation) -> Result<Parsed> {
    // Object summaries are already held by the index; content consumers use
    // only fragments, omissions, and the original byte count.
    read(path, cancel)
}

pub(super) fn read<T: DeserializeOwned>(path: &Path, cancel: &dyn Cancellation) -> Result<T> {
    crate::binary::decode_reader(
        zstd::stream::read::Decoder::new(std::fs::File::open(path)?)?,
        512 * 1024 * 1024,
        cancel,
    )
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
        let mut objects = Vec::new();
        let text = format!(
            "--- !u!1 &1\nGameObject:\n  m_Name: {}\n",
            root.path().display()
        );
        for view in [&first, &second] {
            let source = view.path().join("Object.prefab");
            std::fs::write(&source, &text).unwrap();
            let record = load(
                view.path(),
                Some(&shared),
                &source,
                &Stamp::read(&source).unwrap(),
                &cancel,
            )
            .unwrap();
            paths.push(record.content.unwrap());
            objects.push(record.objects);
        }
        assert!(Arc::ptr_eq(&objects[0], &objects[1]));
        assert_ne!(paths[0], paths[1]);
        let weak = Arc::downgrade(&objects[0]);
        drop(objects);
        assert!(
            weak.upgrade().is_none(),
            "the shared cache must not retain objects"
        );
        assert_eq!(
            std::fs::metadata(&paths[0]).unwrap().ino(),
            std::fs::metadata(&paths[1]).unwrap().ino()
        );
        assert_eq!(shared.collect(true).unwrap(), 0);
        drop(first);
        assert_eq!(shared.collect(true).unwrap(), 0);
        let cached = content(&paths[1], &()).unwrap();
        assert_eq!(
            cached
                .fragments
                .iter()
                .map(|f| f.text.as_str())
                .collect::<String>(),
            text
        );
        drop(second);
        assert!(shared.collect(true).unwrap() > 0);
    }

    #[test]
    fn concurrent_record_publications_share_objects() {
        let cache = tempfile::tempdir().unwrap();
        let key = cache.path().file_name().unwrap();
        let barrier = std::sync::Barrier::new(4);
        let objects = std::thread::scope(|scope| {
            (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let objects = Arc::new(
                            parse::parse("--- !u!1 &1\nGameObject:\n  m_Name: Shared\n")
                                .unwrap()
                                .objects
                                .into_iter()
                                .map(Arc::new)
                                .collect(),
                        );
                        barrier.wait();
                        intern(Path::new(key), objects)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(objects.iter().all(|value| Arc::ptr_eq(value, &objects[0])));
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
                let cached = content(record.content.as_ref().unwrap(), &()).unwrap();
                assert_eq!(
                    cached
                        .fragments
                        .iter()
                        .map(|f| f.text.as_str())
                        .collect::<String>(),
                    std::fs::read_to_string(path).unwrap()
                );
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
