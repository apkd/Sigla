//! Lazy indexes over one immutable published manifest. Cloning starts a new snapshot.
use super::Manifest;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

type Navigation = BTreeMap<String, String>;

struct CachedNavigation {
    root: PathBuf,
    paths: Arc<Navigation>,
    heap_bytes: u64,
}

struct QueryManifest {
    sources: Vec<Weak<Manifest>>,
    manifest: Arc<Manifest>,
    heap_bytes: u64,
}

#[derive(Default)]
pub struct Derived {
    navigation: Mutex<Vec<CachedNavigation>>,
    search: OnceLock<Paths>,
    rendering: OnceLock<crate::render::Inventory>,
    query: Mutex<Vec<QueryManifest>>,
    pub(crate) asset_generation: OnceLock<Result<[u8; 32], String>>,
    pub(crate) source_generations: [OnceLock<Result<[u8; 32], String>>; 2],
}

impl Clone for Derived {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Derived {
    /// Two source groups have three nonempty combinations. Retain their derived
    /// paths and rendering inventories within the workspace heap budget.
    pub(crate) fn query(base: &Arc<Manifest>, sources: &[Arc<Manifest>]) -> Arc<Manifest> {
        if sources.is_empty() {
            return base.clone();
        }
        let mut cached = base.derived.query.lock().unwrap();
        if let Some(index) = cached.iter().position(|saved| {
            saved.sources.len() == sources.len()
                && saved
                    .sources
                    .iter()
                    .zip(sources)
                    .all(|(left, right)| left.ptr_eq(&Arc::downgrade(right)))
        }) {
            let saved = cached.remove(index);
            let manifest = saved.manifest.clone();
            cached.push(saved);
            return manifest;
        }
        let mut manifest = (**base).clone();
        for source in sources {
            manifest.files.extend(source.files.clone());
        }
        let heap_bytes = crate::memory::encoded_size(&manifest).saturating_mul(3) as u64;
        let manifest = Arc::new(manifest);
        cached.retain(|saved| saved.sources.iter().all(|source| source.strong_count() > 0));
        if cached.len() == 3 {
            cached.remove(0);
        }
        cached.push(QueryManifest {
            sources: sources.iter().map(Arc::downgrade).collect(),
            manifest: manifest.clone(),
            heap_bytes,
        });
        manifest
    }

    pub(crate) fn navigation(
        &self,
        root: &Path,
        build: impl FnOnce() -> Navigation,
    ) -> Arc<Navigation> {
        let mut cached = self.navigation.lock().unwrap();
        if let Some(saved) = cached.iter().find(|saved| saved.root == root) {
            return saved.paths.clone();
        }
        let paths = Arc::new(build());
        let heap_bytes = (root.as_os_str().len()
            + paths
                .iter()
                .map(|(path, key)| path.capacity() + key.capacity() + 80)
                .sum::<usize>()) as u64;
        // Local and remote callers may use the canonical or supplied root.
        if cached.len() == 2 {
            cached.remove(0);
        }
        cached.push(CachedNavigation {
            root: root.to_owned(),
            paths: paths.clone(),
            heap_bytes,
        });
        paths
    }

    pub(crate) fn cached_paths(&self) -> Option<&Paths> {
        self.search.get()
    }

    pub(crate) fn paths(&self, manifest: &Manifest) -> &Paths {
        self.search.get_or_init(|| Paths::new(manifest))
    }

    pub(crate) fn rendering(&self, manifest: &Manifest) -> &crate::render::Inventory {
        self.rendering.get_or_init(|| {
            crate::render::Inventory::new(
                manifest
                    .files
                    .iter()
                    .chain(
                        manifest
                            .deferred
                            .iter()
                            .filter(|(key, _)| !manifest.files.contains_key(*key)),
                    )
                    .map(|(_, file)| file)
                    .filter(|file| !file.metadata)
                    .flat_map(|file| {
                        file.memberships
                            .iter()
                            .map(|membership| manifest.display(file, membership).into_owned())
                    }),
            )
        })
    }

    pub(crate) fn heap_bytes(&self) -> u64 {
        let navigation: u64 = self
            .navigation
            .lock()
            .unwrap()
            .iter()
            .map(|cached| cached.heap_bytes)
            .sum();
        let query: u64 = self
            .query
            .lock()
            .unwrap()
            .iter()
            .map(|query| query.heap_bytes + query.manifest.derived.heap_bytes())
            .sum();
        navigation
            + query
            + self.search.get().map_or(0, |paths| paths.heap_bytes)
            + self
                .rendering
                .get()
                .map_or(0, |inventory| inventory.heap_bytes())
    }
}

pub(crate) struct Paths {
    by_path: BTreeMap<Arc<str>, Vec<Arc<str>>>,
    by_file: BTreeMap<Arc<str>, Vec<Arc<str>>>,
    heap_bytes: u64,
}

impl Paths {
    fn new(manifest: &Manifest) -> Self {
        let mut by_path: BTreeMap<Arc<str>, Vec<Arc<str>>> = BTreeMap::new();
        let mut by_file = BTreeMap::new();
        let mut heap_bytes = 0;
        // Ready native snapshots duplicate deferred entries in query manifests.
        // Their path and membership identity is the same, so index each key once.
        for (key, file) in manifest.files.iter().chain(
            manifest
                .deferred
                .iter()
                .filter(|(key, _)| !manifest.files.contains_key(*key)),
        ) {
            let key: Arc<str> = Arc::from(key.as_str());
            heap_bytes += key.len() + 32;
            let mut displays = Vec::with_capacity(file.memberships.len());
            for item in &file.memberships {
                let display = manifest.display(file, item);
                let path: Arc<str> = by_path
                    .get_key_value(display.as_ref())
                    .map_or_else(|| Arc::from(display.as_ref()), |(path, _)| path.clone());
                displays.push(path.clone());
                by_path.entry(path).or_default().push(key.clone());
            }
            heap_bytes += 80 + displays.capacity() * std::mem::size_of::<Arc<str>>();
            by_file.insert(key, displays);
        }
        heap_bytes += by_path
            .iter()
            .map(|(path, rows)| path.len() + 96 + rows.capacity() * std::mem::size_of::<Arc<str>>())
            .sum::<usize>();
        Self {
            by_path,
            by_file,
            heap_bytes: heap_bytes as u64,
        }
    }

    pub fn display(&self, file: &str, membership: usize) -> &str {
        &self.by_file[file][membership]
    }

    /// A literal filter can name a directory. Stop as soon as it spans two rows.
    pub(crate) fn at_most_one_file(&self, literal: &str) -> bool {
        self.ordered(Some(literal))
            .flat_map(|(_, files)| files)
            .nth(1)
            .is_none()
    }

    pub fn matching(
        &self,
        literal: Option<&str>,
        matches: impl Fn(&str) -> bool,
    ) -> BTreeSet<String> {
        let mut files = BTreeSet::new();
        self.visit(literal, |path, rows| {
            if matches(path) {
                files.extend(rows.iter().map(|file| file.to_string()));
            }
        });
        files
    }

    pub fn visit(&self, literal: Option<&str>, mut visit: impl FnMut(&str, &[Arc<str>])) {
        for (path, rows) in self.ordered(literal) {
            visit(path, rows);
        }
    }

    pub fn ordered(&self, literal: Option<&str>) -> impl Iterator<Item = (&str, &[Arc<str>])> {
        let exact = literal.and_then(|path| self.by_path.get_key_value(path));
        let prefix = literal.map(|path| format!("{}/", path.trim_end_matches('/')));
        let start = prefix.as_deref().map_or(Bound::Unbounded, Bound::Included);
        let range = self
            .by_path
            .range::<str, _>((start, Bound::Unbounded))
            .take_while(move |(path, _)| {
                prefix
                    .as_ref()
                    .is_none_or(|prefix| path.starts_with(prefix))
            });
        exact
            .into_iter()
            .chain(range)
            .map(|(path, rows)| (path.as_ref(), rows.as_slice()))
    }
}

pub(crate) fn literal_filter(query: &crate::query::Query) -> Option<&str> {
    query
        .filters
        .iter()
        .find(|filter| {
            filter.key == "path"
                && !filter.negate
                && !filter.value.contains(['*', '?', '[', '{', '}', '\\'])
        })
        .map(|filter| filter.value.as_str())
}

pub(crate) fn candidate_files<'a>(
    files: &'a BTreeMap<String, super::FileEntry>,
    selected: Option<&'a BTreeSet<String>>,
) -> impl Iterator<Item = (&'a String, &'a super::FileEntry)> {
    let all = selected
        .is_none()
        .then(|| files.iter())
        .into_iter()
        .flatten();
    let selected = selected
        .into_iter()
        .flat_map(|keys| keys.iter().filter_map(|key| files.get_key_value(key)));
    all.chain(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternating_groups_retain_rendering_and_account_for_memory() {
        let root = tempfile::tempdir().unwrap();
        let make = |path: &str| {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "source").unwrap();
            let mut manifest = Manifest::default();
            manifest.projects.push(crate::model::Project {
                identity: "project".into(),
                origin: None,
                name: "project".into(),
                defines: vec![],
                references: vec![],
                assemblies: vec![],
                edition: String::new(),
                compiler_options: BTreeMap::new(),
                source_roots: vec![],
            });
            manifest.files.insert(
                path.into(),
                super::super::FileEntry {
                    path: file.clone(),
                    display: path.into(),
                    stamp: super::super::Stamp::read(&file).unwrap(),
                    language: crate::model::Language::Rust,
                    memberships: vec![super::super::Membership {
                        project: 0,
                        module: String::new(),
                    }],
                    modules: vec![],
                    metadata: false,
                },
            );
            Arc::new(manifest)
        };
        let base = make("code/shared.rs");
        let native = make("native/shared.rs");
        let shaders = make("shaders/shared.rs");
        let groups = [
            vec![native.clone()],
            vec![shaders.clone()],
            vec![native, shaders],
        ];
        let combinations: Vec<_> = groups
            .iter()
            .map(|sources| Derived::query(&base, sources))
            .collect();
        let before = base.derived.heap_bytes();
        for manifest in &combinations {
            manifest.derived.rendering(manifest);
        }
        assert!(base.derived.heap_bytes() > before);
        for manifest in &combinations {
            let mut paths = crate::render::Paths::new(manifest.derived.rendering(manifest));
            assert_eq!(paths.display("code/shared.rs"), "code/shared.rs");
            assert_eq!(paths.display("code/shared.rs"), "code/shared.rs");
        }
        for _ in 0..2 {
            for (sources, previous) in groups.iter().zip(&combinations) {
                let current = Derived::query(&base, sources);
                assert!(Arc::ptr_eq(previous, &current));
                assert!(std::ptr::eq(
                    previous.derived.rendering(previous),
                    current.derived.rendering(&current)
                ));
            }
        }
        let changed = make("replacement/new.rs");
        let replacement = Derived::query(&base, &[changed]);
        assert!(replacement.files.contains_key("replacement/new.rs"));
        assert!(!replacement.files.contains_key("native/shared.rs"));
        assert!(combinations[0].files.contains_key("native/shared.rs"));
        assert!(!Arc::ptr_eq(
            &combinations[0],
            &Derived::query(&base, &groups[0])
        ));
    }

    #[test]
    fn query_manifest_reuses_only_the_same_immutable_sources() {
        let base = Arc::new(Manifest::default());
        let source = Arc::new(Manifest::default());
        let first = Derived::query(&base, std::slice::from_ref(&source));
        let repeated = Derived::query(&base, std::slice::from_ref(&source));
        assert!(Arc::ptr_eq(&first, &repeated));
        assert!(base.derived.heap_bytes() > 0);
        assert!(Arc::ptr_eq(&base, &Derived::query(&base, &[])));
        let replacement = Arc::new((*source).clone());
        let changed = Derived::query(&base, &[replacement]);
        assert!(!Arc::ptr_eq(&first, &changed));
        let cloned = Arc::new((*base).clone());
        assert_eq!(cloned.derived.heap_bytes(), 0);
        assert!(!Arc::ptr_eq(&changed, &Derived::query(&cloned, &[source])));
    }
}
