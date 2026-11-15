//! Retain the best N results and count distinct matches for omission messages.
use std::collections::{BTreeMap, BTreeSet};

pub struct Selection<K, V = String> {
    entries: Top<K, V>,
    seen: BTreeSet<K>,
}
impl<K: Ord + Clone, V> Selection<K, V> {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: Top::new(limit),
            seen: BTreeSet::new(),
        }
    }
    pub fn accepts(&mut self, key: &K) -> bool {
        self.seen.insert(key.clone());
        if self.entries.get_mut(key).is_some() {
            return false;
        }
        self.entries.boundary().is_none_or(|last| key <= last)
    }
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key)
    }
    pub fn insert(&mut self, key: K, unit: V) {
        if !self.accepts(&key) {
            return;
        }
        self.entries.insert(key, unit);
    }
    pub fn into_parts(self) -> (BTreeMap<K, V>, usize) {
        (self.entries.into_entries(), self.seen.len())
    }
}

/// Keep the best results without retaining every discarded rank.
pub(crate) struct Top<K, V> {
    entries: BTreeMap<K, V>,
    limit: usize,
}

impl<K: Ord, V> Top<K, V> {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            limit,
        }
    }

    pub fn boundary(&self) -> Option<&K> {
        (self.entries.len() >= self.limit)
            .then(|| self.entries.last_key_value().map(|(key, _)| key))
            .flatten()
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key)
    }

    pub fn insert(&mut self, key: K, value: V) {
        if self.boundary().is_some_and(|last| &key > last) {
            return;
        }
        self.entries.insert(key, value);
        if self.entries.len() > self.limit {
            self.entries.pop_last();
        }
    }

    pub fn into_entries(self) -> BTreeMap<K, V> {
        self.entries
    }
}

#[cfg(test)]
impl<K: Ord + Clone> Selection<K> {
    pub fn finish(self) -> String {
        let (entries, total) = self.into_parts();
        crate::search::render_selected(entries.into_values().collect(), total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_selection_matches_full_sort_for_any_input_order() {
        let units: Vec<_> = (0..200)
            .map(|i| format!("Result {i}: {}", "λ".repeat(i % 13 + 3)))
            .collect();
        for limit in [1, 5, 33, 200, 1000] {
            let expected = crate::search::render(units.clone(), limit);
            for reverse in [false, true] {
                let mut selected = Selection::new(limit);
                for i in 0..units.len() {
                    let i = if reverse { units.len() - 1 - i } else { i };
                    selected.insert(i, units[i].clone());
                    selected.insert(i, units[i].clone());
                }
                assert!(selected.entries.entries.len() <= limit);
                assert_eq!(selected.finish(), expected);
            }
        }
    }
}
