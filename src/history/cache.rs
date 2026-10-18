//! Bounded, disposable history facts. Keys include repository and policy identity.
use super::{
    matching::{Mapping, State},
    regions::{Body, Source},
};
use crate::repository::history::Tree;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    ops::Deref,
    sync::{Arc, Mutex},
};

pub(super) type Key = [u8; 32];
const MATCHING_VERSION: u32 = 1;
const ENTRY_BYTES: usize = 128;

pub(super) fn key(scope: &Key, value: &impl Serialize) -> Key {
    let mut hash = blake3::Hasher::new();
    postcard::to_io(
        &(
            scope,
            super::regions::ADAPTER_VERSION,
            MATCHING_VERSION,
            value,
        ),
        &mut hash,
    )
    .expect("serializable history cache key");
    *hash.finalize().as_bytes()
}

pub(super) struct SourceTree {
    pub entries: Tree,
    pub excluded: BTreeSet<String>,
}
impl Deref for SourceTree {
    type Target = Tree;
    fn deref(&self) -> &Tree {
        &self.entries
    }
}
impl SourceTree {
    fn heap_bytes(&self) -> usize {
        // Charge shared object IDs in each snapshot: they can outlive the
        // decoded directories that originally allocated them.
        self.entries
            .keys()
            .map(|p| p.capacity() + 160)
            .sum::<usize>()
            + self
                .excluded
                .iter()
                .map(|p| p.capacity() + 80)
                .sum::<usize>()
    }
}

#[derive(Clone, Copy)]
enum Evidence {
    Body(Body),
    Anchors(bool),
}

#[derive(Clone)]
enum Match {
    Anchor(Arc<State>),
    Mapping(Arc<Mapping>),
}

struct Entry<V> {
    value: V,
    bytes: usize,
}
struct Lru<V> {
    entries: lru::LruCache<Key, Entry<V>>,
    bytes: usize,
    limit: usize,
    hits: usize,
    misses: usize,
}
impl<V: Clone> Lru<V> {
    fn new(limit: usize) -> Self {
        Self {
            entries: lru::LruCache::unbounded(),
            bytes: 0,
            limit,
            hits: 0,
            misses: 0,
        }
    }
    fn get(&mut self, key: &Key) -> Option<V> {
        if let Some(entry) = self.entries.get(key) {
            self.hits += 1;
            Some(entry.value.clone())
        } else {
            self.misses += 1;
            None
        }
    }
    fn insert(&mut self, key: Key, value: V, cost: usize) -> (V, Vec<V>) {
        // Concurrent misses may finish together. Keep the already published value.
        if let Some(entry) = self.entries.get(&key) {
            return (entry.value.clone(), vec![value]);
        }
        let cost = cost.saturating_add(ENTRY_BYTES);
        if cost > self.limit {
            return (value, Vec::new());
        }
        let mut retired = Vec::new();
        while self.bytes > self.limit - cost {
            let (_, entry) = self.entries.pop_lru().unwrap();
            self.bytes -= entry.bytes;
            retired.push(entry.value);
        }
        self.bytes += cost;
        self.entries.put(
            key,
            Entry {
                value: value.clone(),
                bytes: cost,
            },
        );
        (value, retired)
    }
}

fn insert<V: Clone>(cache: &Mutex<Lru<V>>, key: Key, value: V, cost: usize) -> V {
    let (value, retired) = cache.lock().unwrap().insert(key, value, cost);
    // Large source records can release substantial heap. Drop them after unlocking.
    drop(retired);
    value
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Stats {
    pub bytes: usize,
    pub entries: usize,
    pub hits: [usize; 4],
    pub misses: [usize; 4],
}

pub(crate) struct Cache {
    sources: Mutex<Lru<Arc<Source>>>,
    trees: Mutex<Lru<Arc<SourceTree>>>,
    matches: Mutex<Lru<Match>>,
    evidence: Mutex<Lru<Evidence>>,
}
impl Cache {
    pub fn new(bytes: usize) -> Self {
        // Separate partitions keep large tree inventories from evicting every
        // compact relationship needed by a repeated request.
        let limit = bytes / 4;
        Self {
            sources: Mutex::new(Lru::new(limit)),
            trees: Mutex::new(Lru::new(limit)),
            matches: Mutex::new(Lru::new(limit)),
            evidence: Mutex::new(Lru::new(limit)),
        }
    }
    pub fn stats(&self) -> Stats {
        fn record<V>(cache: &Mutex<Lru<V>>, stats: &mut Stats, index: usize) {
            let cache = cache.lock().unwrap();
            stats.bytes += cache.bytes;
            stats.entries += cache.entries.len();
            stats.hits[index] = cache.hits;
            stats.misses[index] = cache.misses;
        }
        let mut stats = Stats::default();
        record(&self.sources, &mut stats, 0);
        record(&self.trees, &mut stats, 1);
        record(&self.matches, &mut stats, 2);
        record(&self.evidence, &mut stats, 3);
        stats
    }
    pub(super) fn source(&self, key: &Key) -> Option<Arc<Source>> {
        self.sources.lock().unwrap().get(key)
    }
    pub(super) fn put_source(&self, key: Key, source: Arc<Source>) -> Arc<Source> {
        let cost = source.heap_bytes();
        insert(&self.sources, key, source, cost)
    }
    pub(super) fn tree(&self, key: &Key) -> Option<Arc<SourceTree>> {
        self.trees.lock().unwrap().get(key)
    }
    pub(super) fn put_tree(&self, key: Key, tree: Arc<SourceTree>) -> Arc<SourceTree> {
        let cost = tree.heap_bytes();
        insert(&self.trees, key, tree, cost)
    }
    pub(super) fn mapping(&self, key: &Key) -> Option<Arc<Mapping>> {
        self.matches.lock().unwrap().get(key).map(|value| {
            let Match::Mapping(mapping) = value else {
                unreachable!()
            };
            mapping
        })
    }
    pub(super) fn put_mapping(&self, key: Key, mapping: &Mapping) {
        // A partial candidate search must never become a cached absence.
        if !mapping.state.uncertain {
            insert(
                &self.matches,
                key,
                Match::Mapping(Arc::new(mapping.clone())),
                mapping.heap_bytes(),
            );
        }
    }
    pub(super) fn anchor(&self, key: &Key) -> Option<Arc<State>> {
        self.matches.lock().unwrap().get(key).map(|value| {
            let Match::Anchor(state) = value else {
                unreachable!()
            };
            state
        })
    }
    pub(super) fn put_anchor(&self, key: Key, state: &State) {
        if !state.uncertain {
            insert(
                &self.matches,
                key,
                Match::Anchor(Arc::new(state.clone())),
                std::mem::size_of::<State>() + state.heap_bytes(),
            );
        }
    }
    pub(super) fn body(&self, key: Key, compute: impl FnOnce() -> Body) -> Body {
        if let Some(value) = self.evidence.lock().unwrap().get(&key) {
            let Evidence::Body(body) = value else {
                unreachable!()
            };
            return body;
        }
        let body = compute();
        insert(
            &self.evidence,
            key,
            Evidence::Body(body),
            std::mem::size_of::<Evidence>(),
        );
        body
    }
    pub(super) fn anchors(&self, key: Key, a: &str, b: &str) -> bool {
        if let Some(value) = self.evidence.lock().unwrap().get(&key) {
            let Evidence::Anchors(anchors) = value else {
                unreachable!()
            };
            return anchors;
        }
        let anchors = super::regions::ordered_body_anchors(a, b);
        insert(
            &self.evidence,
            key,
            Evidence::Anchors(anchors),
            std::mem::size_of::<Evidence>(),
        );
        anchors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::history::Store;
    use std::{
        path::Path,
        process::Command,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    fn git(root: &Path, args: &[&str], input: &[u8]) -> String {
        let output = crate::process::capture(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "History Test")
                .env("GIT_AUTHOR_EMAIL", "history@example.invalid")
                .env("GIT_COMMITTER_NAME", "History Test")
                .env("GIT_COMMITTER_EMAIL", "history@example.invalid"),
            Duration::from_secs(20),
            Some(input.to_vec()),
            None,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim_end().into()
    }
    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(
            root.path(),
            &["init", "--bare", "--quiet", "--template="],
            b"",
        );
        root
    }
    fn commit(root: &Path, parent: Option<&str>, text: &str) -> String {
        commit_file(root, parent, "code.rs", text)
    }
    fn commit_file(root: &Path, parent: Option<&str>, path: &str, text: &str) -> String {
        let blob = git(root, &["hash-object", "-w", "--stdin"], text.as_bytes());
        let tree = git(
            root,
            &["mktree"],
            format!("100644 blob {blob}\t{path}\n").as_bytes(),
        );
        let mut args = vec!["commit-tree", &tree, "-m", "Fixture"];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        git(root, &args, b"")
    }
    fn history(root: &Path, at: &str, query: &str, cache: &Arc<Cache>) -> anyhow::Result<String> {
        let store = Store::local(root, Arc::new(AtomicBool::new(false)))?;
        super::super::run_cached(store, Some(at), query, cache.clone())
    }

    #[test]
    fn source_batches_preserve_order_facts_errors_and_cancellation() {
        use super::super::regions::{Region, Sources};
        let root = repository();
        let inputs: [(&str, &[u8]); 7] = [
            (
                "C.cs",
                b"class C {\n#if A\nint Run() => Read();\n#else\nlong Run() => Read();\n#endif\n}",
            ),
            ("R.rs", b"fn run() -> i32 { read() + 1 }"),
            ("C.java", b"class C { int run() { return read(); } }"),
            ("C.txt", b"class C { int run() { return read(); } }"),
            ("broken.cs", b"class C { void Run( }"),
            ("binary.cs", b"class C {}\0"),
            ("invalid.cs", &[0xff]),
        ];
        let blobs: Vec<_> = inputs
            .iter()
            .map(|(_, text)| git(root.path(), &["hash-object", "-w", "--stdin"], text))
            .collect();
        let paths: Vec<_> = inputs
            .iter()
            .zip(&blobs)
            .map(|((path, _), blob)| (*path, blob.as_str()))
            .collect();
        let mut store = Store::local(root.path(), Arc::new(AtomicBool::new(false))).unwrap();
        let parallel = Sources::default();
        let serial = Sources::default();
        let mut index = 0;
        while index < paths.len() {
            let batch = parallel.batch(&mut store, paths[index..].iter().copied());
            assert!(!batch.is_empty());
            for result in batch {
                let (path, blob) = paths[index];
                match (result, serial.get(&mut store, path, blob)) {
                    (Ok(actual), Ok(expected)) => {
                        assert_eq!(actual.text, expected.text);
                        assert_eq!(actual.incomplete, expected.incomplete);
                        let declarations = |source: &Source| {
                            source
                                .declarations
                                .iter()
                                .map(|d| Region::declaration(path, blob, d, source))
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(declarations(&actual), declarations(&expected));
                        let calls = |source: &Source| {
                            source
                                .calls
                                .iter()
                                .map(|c| {
                                    (
                                        c.span.clone(),
                                        c.name.clone(),
                                        c.arguments,
                                        c.receiver.clone(),
                                    )
                                })
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(calls(&actual), calls(&expected));
                        assert!(Arc::ptr_eq(
                            &actual,
                            &parallel.get(&mut store, path, blob).unwrap()
                        ));
                    }
                    (Err(actual), Err(expected)) => {
                        assert_eq!(actual.to_string(), expected.to_string())
                    }
                    _ => panic!("Batched and serial source outcomes differ"),
                }
                index += 1;
            }
        }
        store.cancel.store(true, Ordering::Release);
        assert!(
            parallel
                .batch(&mut store, paths.iter().copied())
                .iter()
                .all(Result::is_err)
        );
    }

    #[test]
    fn repeated_concurrent_and_uncached_queries_preserve_selected_history() {
        let root = repository();
        let base = commit(
            root.path(),
            None,
            "pub fn first() -> i32 { 11 }\npub fn second() -> i32 { 29 }\n",
        );
        let first = commit(
            root.path(),
            Some(&base),
            "pub fn first() -> i32 { 31 }\npub fn second() -> i32 { 29 }\n",
        );
        let tip = commit(
            root.path(),
            Some(&first),
            "pub fn first() -> i32 { 31 }\npub fn second() -> i32 { 47 }\n",
        );
        let queries = [
            "log function:first path:code.rs".to_owned(),
            "log function:second path:code.rs".to_owned(),
            format!("diff {base}..{tip} function:first path:code.rs"),
            format!("diff {tip}..{base} function:first path:code.rs"),
        ];
        let cache = Arc::new(Cache::new(4 * 1024 * 1024));
        let cold: Vec<_> = queries
            .iter()
            .map(|q| history(root.path(), &tip, q, &cache).unwrap())
            .collect();
        assert!(cold[0].lines().any(|line| line.starts_with(&first)));
        assert!(!cold[0].lines().any(|line| line.starts_with(&tip)));
        assert!(cold[1].lines().any(|line| line.starts_with(&tip)));
        assert!(!cold[1].lines().any(|line| line.starts_with(&first)));
        let populated = cache.stats();
        for (query, expected) in queries.iter().zip(&cold) {
            assert_eq!(
                history(root.path(), &tip, query, &cache).unwrap(),
                *expected
            );
            assert_eq!(
                history(root.path(), &tip, query, &Arc::new(Cache::new(0))).unwrap(),
                *expected
            );
        }
        let warm = cache.stats();
        assert_eq!(
            warm.misses[0], populated.misses[0],
            "Repeated queries reparsed source"
        );
        assert!(
            warm.hits[2] > populated.hits[2],
            "Completed matches were not reused"
        );
        let concurrent = Arc::new(Cache::new(4 * 1024 * 1024));
        std::thread::scope(|threads| {
            for _ in 0..4 {
                threads.spawn(|| {
                    for (query, expected) in queries.iter().zip(&cold) {
                        assert_eq!(
                            history(root.path(), &tip, query, &concurrent).unwrap(),
                            *expected
                        );
                    }
                });
            }
        });
    }

    #[test]
    fn cached_anchors_skip_object_reads_and_preserve_selection_and_cancellation() {
        let root = repository();
        let at = commit(root.path(), None, "pub fn first() {}\npub fn second() {}\n");
        let cache = Arc::new(Cache::new(1024 * 1024));
        let first = super::super::query::Request::parse("log function:first")
            .unwrap()
            .selector;
        let second = super::super::query::Request::parse("log function:second")
            .unwrap()
            .selector;
        let cancel = Arc::new(AtomicBool::new(false));
        let store = Store::local(root.path(), cancel.clone()).unwrap();
        let mut tracker = super::super::matching::Tracker::new(store, cache.clone());
        let commit = tracker.store.commit(&at).unwrap();
        let expected = tracker.anchor(&commit, &first).unwrap();
        let reads = tracker.store.reads;
        assert!(
            tracker
                .anchor(&commit, &first)
                .unwrap()
                .same_occurrences(&expected)
        );
        assert_eq!(tracker.store.reads, reads, "Anchor rescanned Git objects");
        assert!(
            !tracker
                .anchor(&commit, &second)
                .unwrap()
                .same_occurrences(&expected)
        );
        cancel.store(true, Ordering::Release);
        assert!(tracker.anchor(&commit, &first).is_err());
    }

    #[test]
    fn cached_facts_do_not_supply_objects_missing_from_another_repository() {
        let private = repository();
        let public = repository();
        let at = commit(private.path(), None, "pub fn secret() -> i32 { 73 }\n");
        let cache = Arc::new(Cache::new(1024 * 1024));
        let query = "log function:secret path:code.rs";
        assert!(history(private.path(), &at, query, &cache).is_ok());
        // A forged commit can name an object held only by the other repository.
        let body = git(private.path(), &["cat-file", "commit", &at], b"") + "\n";
        assert_eq!(
            git(
                public.path(),
                &["hash-object", "-t", "commit", "-w", "--stdin"],
                body.as_bytes()
            ),
            at
        );
        assert!(history(public.path(), &at, "log limit:1", &cache).is_ok());
        assert!(history(public.path(), &at, query, &cache).is_err());
        let blob = git(
            private.path(),
            &["rev-parse", &format!("{at}:code.rs")],
            b"",
        );
        let sources = super::super::regions::Sources::new(cache);
        let mut store = Store::local(public.path(), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(sources.get(&mut store, "code.rs", &blob).is_err());
    }

    #[test]
    fn conditional_matches_survive_eviction_and_reparsing_of_source_facts() {
        let root = repository();
        let source = "class Example {\npublic int Stable(int value) { int doubled = value * 2; int adjusted = doubled + 7; return adjusted * 3; }\n#if A\npublic int Value(int value) { return value; }\n#else\npublic string Value(string value) { return value; }\n#endif\n}\n";
        let base = commit_file(root.path(), None, "code.cs", source);
        let changed = source.replace("doubled + 7", "doubled + 11").replacen(
            "return value;",
            "return value + 1;",
            1,
        );
        let tip = commit_file(root.path(), Some(&base), "code.cs", &changed);
        let cache = Arc::new(Cache::new(1024 * 1024));
        let query = format!("show {tip} method:Example.Stable(int) path:code.cs");
        let conditional_query = format!("show {tip} method:Example.Value(int) path:code.cs");
        let cold = history(root.path(), &tip, &query, &cache).unwrap();
        let conditional = history(root.path(), &tip, &conditional_query, &cache).unwrap();
        assert!(!cold.contains("incomplete:"), "{cold}");
        let sources = super::super::regions::Sources::new(cache.clone());
        let mut store = Store::local(root.path(), Arc::new(AtomicBool::new(false))).unwrap();
        let size = cache.sources.lock().unwrap().limit / 2;
        for index in 0..3 {
            let text = format!("{index}{}", "x".repeat(size));
            let blob = git(
                root.path(),
                &["hash-object", "-w", "--stdin"],
                text.as_bytes(),
            );
            sources.get(&mut store, "notes.md", &blob).unwrap();
        }
        let before = cache.stats();
        assert_eq!(history(root.path(), &tip, &query, &cache).unwrap(), cold);
        let after = cache.stats();
        assert!(
            after.misses[0] > before.misses[0],
            "Source facts were not evicted"
        );
        assert!(
            after.hits[2] > before.hits[2],
            "Completed matches were not reused"
        );
        assert_eq!(
            history(root.path(), &tip, &conditional_query, &cache).unwrap(),
            conditional
        );
        assert!(
            cache
                .matches
                .lock()
                .unwrap()
                .entries
                .iter()
                .all(|(_, entry)| {
                    match &entry.value {
                        Match::Mapping(mapping) => !mapping.state.uncertain,
                        Match::Anchor(_) => true,
                    }
                }),
            "Uncertain matches were retained"
        );
        assert_eq!(
            history(root.path(), &tip, &query, &Arc::new(Cache::new(0))).unwrap(),
            cold
        );
    }

    #[test]
    fn incomplete_history_is_retried_when_a_missing_parent_returns() {
        let root = repository();
        let base = commit(root.path(), None, "pub fn value() -> i32 { 11 }\n");
        let tip = commit(root.path(), Some(&base), "pub fn value() -> i32 { 29 }\n");
        let body = git(root.path(), &["cat-file", "commit", &base], b"") + "\n";
        std::fs::remove_file(
            root.path()
                .join("objects")
                .join(&base[..2])
                .join(&base[2..]),
        )
        .unwrap();
        let cache = Arc::new(Cache::new(1024 * 1024));
        let query = "log function:value path:code.rs";
        let incomplete = history(root.path(), &tip, query, &cache).unwrap();
        assert!(incomplete.contains("incomplete:"), "{incomplete}");
        assert_eq!(
            git(
                root.path(),
                &["hash-object", "-t", "commit", "-w", "--stdin"],
                body.as_bytes()
            ),
            base
        );
        let complete = history(root.path(), &tip, query, &cache).unwrap();
        assert!(!complete.contains("incomplete:"), "{complete}");
        assert_eq!(
            complete,
            history(root.path(), &tip, query, &Arc::new(Cache::new(0))).unwrap()
        );
    }

    #[test]
    fn cached_sources_still_honor_cancellation() {
        let root = repository();
        let blob = git(
            root.path(),
            &["hash-object", "-w", "--stdin"],
            b"pub fn value() {}\n",
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let mut store = Store::local(root.path(), cancel.clone()).unwrap();
        let sources = super::super::regions::Sources::default();
        let first = sources.get(&mut store, "code.rs", &blob).unwrap();
        cancel.store(true, Ordering::Release);
        assert!(sources.get(&mut store, "code.rs", &blob).is_err());
        cancel.store(false, Ordering::Release);
        assert!(Arc::ptr_eq(
            &first,
            &sources.get(&mut store, "code.rs", &blob).unwrap()
        ));
    }

    #[test]
    fn uncertain_absence_is_never_reused_as_a_completed_search() {
        let cache = Cache::new(1024 * 1024);
        let mut mapping = Mapping::absent();
        mapping.state.uncertain = true;
        cache.put_mapping([1; 32], &mapping);
        assert!(cache.mapping(&[1; 32]).is_none());
    }

    #[test]
    fn eviction_preserves_active_values_and_oversized_values_are_not_retained() {
        let mut cache = Lru::new(ENTRY_BYTES + 4);
        let first = Arc::new("first".to_owned());
        cache.insert([1; 32], first.clone(), 4);
        cache.insert([2; 32], Arc::new("second".to_owned()), 4);
        assert!(cache.get(&[1; 32]).is_none());
        assert_eq!(first.as_str(), "first");
        assert!(cache.get(&[2; 32]).is_some());
        cache.insert([3; 32], Arc::new("oversized".to_owned()), 5);
        assert!(cache.get(&[3; 32]).is_none());
        assert!(cache.get(&[2; 32]).is_some());
        assert!(cache.bytes <= cache.limit);
    }
}
