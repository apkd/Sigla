//! A workspace vocabulary points only to posting partitions it actually uses.
//! This bounds wildcard and common-name searches as the shared cache grows.
use super::shared::Names;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
};

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Vocabulary {
    pub declarations: BTreeMap<String, Vec<u64>>,
    pub occurrences: BTreeMap<String, Vec<u64>>,
    pub globals: Vec<u64>,
}
impl Vocabulary {
    /// Objects arrive in ordinal order, so each partition is appended once.
    pub fn add(&mut self, partition: u64, names: Names) {
        for (index, names) in [
            (&mut self.declarations, names.declarations),
            (&mut self.occurrences, names.occurrences),
        ] {
            for name in names {
                append(index.entry(name).or_default(), partition);
            }
        }
        if names.global_imports {
            append(&mut self.globals, partition);
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let plain = postcard::to_allocvec(self)?;
        let mut bytes = blake3::hash(&plain).as_bytes().to_vec();
        bytes.extend(zstd::stream::encode_all(plain.as_slice(), 1)?);
        Ok(bytes)
    }
}
fn append(partitions: &mut Vec<u64>, partition: u64) {
    if partitions.last() != Some(&partition) {
        partitions.push(partition);
    }
}
struct Cache {
    entries: lru::LruCache<[u8; 32], (Arc<Vocabulary>, usize)>,
    bytes: usize,
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    Mutex::new(Cache {
        entries: lru::LruCache::unbounded(),
        bytes: 0,
    })
});

pub(super) fn decode(bytes: &[u8]) -> Result<Arc<Vocabulary>> {
    let key: [u8; 32] = bytes
        .get(..32)
        .context("Missing vocabulary identity")?
        .try_into()?;
    {
        let mut cache = CACHE.lock().unwrap();
        if let Some((value, _)) = cache.entries.get(&key) {
            return Ok(value.clone());
        }
    }
    let plain = zstd::stream::decode_all(&bytes[32..])?;
    anyhow::ensure!(
        blake3::hash(&plain).as_bytes() == &key,
        "Corrupt workspace vocabulary"
    );
    let value = Arc::new(postcard::from_bytes::<Vocabulary>(&plain)?);
    let size = value
        .declarations
        .iter()
        .chain(&value.occurrences)
        .map(|(name, partitions)| 96 + name.capacity() + partitions.capacity() * 8)
        .sum::<usize>()
        + value.globals.capacity() * 8;
    let limit = crate::memory::cache_budget() / 8;
    if size <= limit {
        let mut cache = CACHE.lock().unwrap();
        let Cache { entries, bytes } = &mut *cache;
        if let Some((existing, _)) = entries.get(&key) {
            return Ok(existing.clone());
        }
        while *bytes + size > limit {
            if let Some((_, (_, removed))) = entries.pop_lru() {
                *bytes -= removed;
            } else {
                break;
            }
        }
        entries.put(key, (value.clone(), size));
        *bytes += size;
    }
    Ok(value)
}
