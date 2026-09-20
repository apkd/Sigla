//! Bounded cross-query results. Targets are identities, never old source spans.
use super::types::DefinitionId;
use std::{
    collections::{HashMap, VecDeque},
    sync::{LazyLock, Mutex},
};

#[derive(Clone)]
pub(super) struct Target {
    pub id: DefinitionId,
    pub file: String,
    pub name: String,
    pub uncertain: bool,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<[u8; 32], (Vec<Target>, usize)>,
    order: VecDeque<[u8; 32]>,
    bytes: usize,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));

pub(super) fn get(key: &[u8; 32]) -> Option<Vec<Target>> {
    CACHE
        .lock()
        .unwrap()
        .entries
        .get(key)
        .map(|(value, _)| value.clone())
}

pub(super) fn put(key: [u8; 32], targets: Vec<Target>) {
    const LIMIT: usize = 16 * 1024 * 1024;
    let size = 128
        + targets
            .iter()
            .map(|t| {
                std::mem::size_of::<Target>()
                    + t.id.context.len()
                    + t.id.key.len()
                    + t.file.len()
                    + t.name.len()
            })
            .sum::<usize>();
    let mut cache = CACHE.lock().unwrap();
    if size > LIMIT || cache.entries.contains_key(&key) {
        return;
    }
    while cache.bytes + size > LIMIT {
        let Some(key) = cache.order.pop_front() else {
            break;
        };
        if let Some((_, bytes)) = cache.entries.remove(&key) {
            cache.bytes -= bytes;
        }
    }
    cache.bytes += size;
    cache.order.push_back(key);
    cache.entries.insert(key, (targets, size));
}
