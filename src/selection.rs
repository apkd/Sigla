//! Retain the best N results and count distinct matches for omission messages.
use std::collections::{BTreeMap, BTreeSet};

pub struct Selection<K> {
    entries: BTreeMap<K, String>,
    seen: BTreeSet<K>,
    limit: usize,
}
impl<K: Ord + Clone> Selection<K> {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            seen: BTreeSet::new(),
            limit,
        }
    }
    pub fn accepts(&mut self, key: &K) -> bool {
        self.seen.insert(key.clone());
        if self.entries.contains_key(key) {
            return false;
        }
        if self.entries.len() >= self.limit
            && self
                .entries
                .last_key_value()
                .is_some_and(|(last, _)| key > last)
        {
            return false;
        }
        true
    }
    pub fn insert(&mut self, key: K, unit: String) {
        if !self.accepts(&key) {
            return;
        }
        self.entries.insert(key, unit);
        if self.entries.len() > self.limit {
            self.entries.pop_last();
        }
    }
    pub fn finish(self) -> String {
        self.finish_with(|_, unit| unit)
    }
    pub fn finish_with(self, mut transform: impl FnMut(K, String) -> String) -> String {
        crate::search::render_selected(
            self.entries
                .into_iter()
                .map(|(key, s)| transform(key, s))
                .collect(),
            self.seen.len(),
        )
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
                assert!(selected.entries.len() <= limit);
                assert_eq!(selected.finish(), expected);
            }
        }
    }
}
