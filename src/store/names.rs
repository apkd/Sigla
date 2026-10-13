//! Packed, query-side vocabularies shared by active posting partitions and
//! their generations. No workspace rebuilds, compresses, or publishes a copy.
use super::{Database, scope_key};
use anyhow::{Context, Result};
use heed::{RoTxn, RwTxn};
use std::{
    cmp::Reverse,
    collections::{BTreeSet, BinaryHeap},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

// Reopening an environment gets a new namespace, even at the same path.
pub(super) fn namespace() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .expect("Dictionary namespaces exhausted")
}
fn generation_key(partition: u64) -> Vec<u8> {
    scope_key(partition, b"posting-generation")
}
pub(super) fn changed(database: &Database, tx: &mut RwTxn<'_>, partition: u64) -> Result<()> {
    let key = generation_key(partition);
    let generation = (tx.id() as u64).to_be_bytes();
    // At most one tiny control write per touched partition per transaction.
    if database.control.get(tx, &key)? != Some(generation.as_slice()) {
        database.control.put(tx, &key, &generation)?;
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Dictionary {
    text: String,
    term_ends: Vec<u32>,
    partitions: Vec<u64>,
    posting_ends: Vec<u32>,
    postings: Vec<u32>,
}
impl Dictionary {
    pub(super) fn terms(&self) -> impl Iterator<Item = (&str, &[u32])> {
        let mut text = 0;
        let mut posting = 0;
        self.term_ends
            .iter()
            .zip(&self.posting_ends)
            .map(move |(&end, &last)| {
                let term = &self.text[text..end as usize];
                let partitions = &self.postings[posting..last as usize];
                text = end as usize;
                posting = last as usize;
                (term, partitions)
            })
    }
    pub(super) fn partition(&self, index: u32) -> u64 {
        self.partitions[index as usize]
    }
    fn bytes(&self) -> usize {
        self.text.capacity()
            + (self.term_ends.capacity() + self.posting_ends.capacity() + self.postings.capacity())
                * 4
            + self.partitions.capacity() * 8
            + std::mem::size_of::<Self>()
            + 128
    }
}
struct Cache {
    // None remembers an oversized vocabulary: subsequent queries stream keys
    // instead of repeatedly attempting an allocation that cannot be cached.
    entries: lru::LruCache<[u8; 32], (Option<Arc<Dictionary>>, usize)>,
    bytes: usize,
}
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| {
    Mutex::new(Cache {
        entries: lru::LruCache::unbounded(),
        bytes: 0,
    })
});

pub(super) fn dictionary(
    database: &Database,
    tx: &RoTxn<'_>,
    partitions: &BTreeSet<u64>,
    occurrences: bool,
) -> Result<Option<Arc<Dictionary>>> {
    let mut key = blake3::Hasher::new();
    key.update(&database.dictionary_namespace.to_be_bytes());
    key.update(&[u8::from(occurrences)]);
    for &partition in partitions {
        key.update(&partition.to_be_bytes());
        let generation = database.control.get(tx, &generation_key(partition))?;
        let generation = generation.unwrap_or(&[0; 8]);
        if generation.len() != 8 {
            // This is derived acceleration metadata, not index authority.
            // Keep queries correct by streaming the committed posting keys.
            return Ok(None);
        }
        key.update(generation);
    }
    let key = *key.finalize().as_bytes();
    if let Some((value, _)) = CACHE.lock().unwrap().entries.get(&key) {
        return Ok(value.clone());
    }
    let limit = crate::memory::cache_budget() / 8;
    let value = build(database, tx, partitions, occurrences, limit)?.map(Arc::new);
    let size = value.as_ref().map_or(128, |value| value.bytes());
    let mut cache = CACHE.lock().unwrap();
    if let Some((value, _)) = cache.entries.get(&key) {
        return Ok(value.clone());
    }
    while cache.bytes.saturating_add(size) > limit {
        let Some((_, (_, bytes))) = cache.entries.pop_lru() else {
            break;
        };
        cache.bytes -= bytes;
    }
    cache.bytes += size;
    cache.entries.put(key, (value.clone(), size));
    Ok(value)
}

fn build(
    database: &Database,
    tx: &RoTxn<'_>,
    partitions: &BTreeSet<u64>,
    occurrences: bool,
    limit: usize,
) -> Result<Option<Dictionary>> {
    // Bound cursor/heap overhead before opening a cursor per active partition.
    if partitions.len().saturating_mul(256) > limit || partitions.len() > u32::MAX as usize {
        return Ok(None);
    }
    let table = if occurrences {
        database.occurrences
    } else {
        database.declarations
    };
    let prefixes: Vec<_> = partitions.iter().map(|p| p.to_be_bytes()).collect();
    let mut cursors: Vec<_> = prefixes
        .iter()
        .map(|prefix| -> Result<_> { Ok(table.prefix_iter(tx, prefix)?.move_between_keys()) })
        .collect::<Result<_>>()?;
    let mut heap = BinaryHeap::new();
    for (index, cursor) in cursors.iter_mut().enumerate() {
        if let Some((key, _)) = cursor.next().transpose()? {
            heap.push(Reverse((std::str::from_utf8(&key[8..])?, index)));
        }
    }
    let mut value = Dictionary {
        partitions: partitions.iter().copied().collect(),
        ..Default::default()
    };
    let mut previous = None;
    // Merge sorted distinct keys directly from LMDB. The packed result tests
    // each unique term once, retaining the existing warm broad-query algorithm.
    // No object descriptor or POSTING_NAMES payload is decoded, even on a miss.
    while let Some(Reverse((term, index))) = heap.pop() {
        if value.bytes().saturating_add(term.len() + 12) > limit {
            return Ok(None);
        }
        if previous != Some(term) {
            let Some(end) = value
                .text
                .len()
                .checked_add(term.len())
                .and_then(|end| u32::try_from(end).ok())
            else {
                return Ok(None);
            };
            value.text.push_str(term);
            value.term_ends.push(end);
            value.posting_ends.push(0);
            previous = Some(term);
        }
        value.postings.push(index as u32);
        *value
            .posting_ends
            .last_mut()
            .context("Missing dictionary term")? = u32::try_from(value.postings.len())?;
        // Amortized growth can exceed the limit by one buffer growth step,
        // never by an unbounded vocabulary. Oversized results are discarded.
        if value.bytes() > limit {
            return Ok(None);
        }
        if let Some((key, _)) = cursors[index].next().transpose()? {
            heap.push(Reverse((std::str::from_utf8(&key[8..])?, index)));
        }
    }
    Ok(Some(value))
}
