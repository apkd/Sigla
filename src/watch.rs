//! shared, nonrecursive Linux directory watches with a metadata-poll fallback.
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub struct Monitor {
    state: Mutex<State>,
}
#[derive(Clone)]
pub(crate) struct Snapshot {
    monitor: Arc<Monitor>,
    paths: BTreeSet<PathBuf>,
    generation: u64,
}
impl Snapshot {
    pub fn changed(&self) -> bool {
        self.monitor.fence(&self.paths, self.generation).0
    }
    pub(crate) fn changes(&self) -> Changes {
        self.monitor.changes(&self.paths, self.generation).0
    }
}

/// A bounded event history. Missing coverage always requires full validation.
#[derive(Default)]
pub(crate) struct Changes {
    full: bool,
    paths: BTreeSet<PathBuf>,
    trees: BTreeSet<PathBuf>,
}
impl Changes {
    pub(crate) fn affects(&self, path: &Path) -> bool {
        self.full
            || self.paths.contains(path)
            || self.trees.iter().any(|tree| path.starts_with(tree))
    }
    pub(crate) fn any(&self) -> bool {
        self.full || !self.paths.is_empty() || !self.trees.is_empty()
    }
}
struct Change {
    directory: PathBuf,
    path: PathBuf,
    generation: u64,
    membership: bool,
    tree: bool,
}
const EVENT_LIMIT: usize = 8192;
struct State {
    inotify: Option<Inotify>,
    directories: HashMap<PathBuf, (WatchDescriptor, usize)>,
    paths: HashMap<WatchDescriptor, PathBuf>,
    events: VecDeque<Change>,
    coverage: u64,
    generation: u64,
    fallback: bool,
}
impl State {
    fn record(&mut self, event: Change) {
        self.events.push_back(event);
        while self.events.len() > EVENT_LIMIT {
            self.coverage = self
                .coverage
                .max(self.events.pop_front().unwrap().generation);
        }
    }
}
impl Default for Monitor {
    fn default() -> Self {
        let inotify = Inotify::init().ok();
        Self {
            state: Mutex::new(State {
                fallback: inotify.is_none(),
                inotify,
                directories: HashMap::new(),
                paths: HashMap::new(),
                events: VecDeque::new(),
                coverage: 0,
                generation: 0,
            }),
        }
    }
}
impl Monitor {
    pub(crate) fn snapshot(
        self: &Arc<Self>,
        paths: &BTreeSet<PathBuf>,
        generation: u64,
    ) -> Snapshot {
        Snapshot {
            monitor: self.clone(),
            paths: paths.clone(),
            generation,
        }
    }
    pub fn register(&self, path: &Path) {
        let mut state = self.state.lock().unwrap();
        if let Some((_, users)) = state.directories.get_mut(path) {
            *users += 1;
            return;
        }
        if state.fallback {
            return;
        }
        // Inherited writable handles can delay CLOSE_WRITE until the child execs.
        let mask = WatchMask::MODIFY
            | WatchMask::CLOSE_WRITE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::ATTRIB
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF;
        match state.inotify.as_mut().unwrap().watches().add(path, mask) {
            Ok(wd) => {
                state.paths.insert(wd.clone(), path.into());
                state.directories.insert(path.into(), (wd, 1));
            }
            Err(e) => {
                tracing::warn!(error=%e,"Directory watching unavailable; using metadata polling");
                state.fallback = true;
            }
        }
    }
    pub fn unregister(&self, path: &Path) {
        let mut state = self.state.lock().unwrap();
        let Some((_, users)) = state.directories.get_mut(path) else {
            return;
        };
        *users -= 1;
        if *users > 0 {
            return;
        }
        let (wd, _) = state.directories.remove(path).unwrap();
        state.paths.remove(&wd);
        // Snapshots can outlive the workspace that owned the watch.
        state.generation += 1;
        let generation = state.generation;
        state.record(Change {
            directory: path.into(),
            path: path.into(),
            generation,
            membership: false,
            tree: true,
        });
        if let Some(inotify) = state.inotify.as_mut() {
            let _ = inotify.watches().remove(wd);
        }
    }
    pub fn fence(&self, paths: &BTreeSet<PathBuf>, previous: u64) -> (bool, u64) {
        let (changes, generation) = self.changes(paths, previous);
        (changes.any(), generation)
    }
    pub(crate) fn changes(&self, paths: &BTreeSet<PathBuf>, previous: u64) -> (Changes, u64) {
        let mut state = self.state.lock().unwrap();
        let mut buffer = [0u8; 65536];
        for batch in 0..8 {
            let events = match state.inotify.as_mut().map(|w| w.read_events(&mut buffer)) {
                Some(Ok(events)) => events
                    .map(|e| (e.wd, e.mask, e.name.map(std::ffi::OsStr::to_owned)))
                    .collect::<Vec<_>>(),
                Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Some(Err(_)) | None => {
                    state.fallback = true;
                    break;
                }
            };
            if events.is_empty() {
                break;
            }
            state.generation += 1;
            let generation = state.generation;
            for (wd, mask, name) in events {
                if mask.contains(EventMask::Q_OVERFLOW)
                    || (state.paths.contains_key(&wd)
                        && mask.intersects(
                            EventMask::IGNORED | EventMask::DELETE_SELF | EventMask::MOVE_SELF,
                        ))
                {
                    state.fallback = true;
                }
                if let Some(path) = state.paths.get(&wd).cloned() {
                    let changed = name.map_or_else(|| path.clone(), |name| path.join(name));
                    state.record(Change {
                        directory: path,
                        path: changed,
                        generation,
                        membership: mask.intersects(
                            EventMask::CREATE
                                | EventMask::DELETE
                                | EventMask::MOVED_FROM
                                | EventMask::MOVED_TO,
                        ),
                        tree: mask.contains(EventMask::ISDIR),
                    });
                }
            }
            if batch == 7 {
                state.fallback = true;
            }
        }
        let mut changes = Changes {
            full: state.fallback || previous < state.coverage,
            ..Default::default()
        };
        if !changes.full {
            for event in state
                .events
                .iter()
                .rev()
                .take_while(|e| e.generation > previous)
                .filter(|e| paths.contains(&e.directory))
            {
                changes.paths.insert(event.path.clone());
                if event.membership {
                    changes.paths.insert(event.directory.clone());
                }
                if event.tree {
                    changes.trees.insert(event.path.clone());
                }
            }
        }
        (changes, state.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn named_changes_preserve_independent_readers_and_directory_membership() {
        let root = tempfile::tempdir().unwrap();
        let changed = root.path().join("changed.rs");
        let untouched = root.path().join("untouched.rs");
        fs::write(&changed, "old").unwrap();
        fs::write(&untouched, "old").unwrap();
        let monitor = Arc::new(Monitor::default());
        monitor.register(root.path());
        let paths = BTreeSet::from([root.path().into()]);
        let (_, generation) = monitor.fence(&paths, 0);
        let first = monitor.snapshot(&paths, generation);
        let second = first.clone();
        fs::write(&changed, "changed").unwrap();
        for reader in [&first, &second] {
            let changes = reader.changes();
            assert!(changes.affects(&changed));
            assert!(!changes.affects(&untouched));
            assert!(!changes.affects(root.path()));
        }
        let (_, generation) = monitor.fence(&paths, generation);
        let current = monitor.snapshot(&paths, generation);
        fs::rename(&changed, root.path().join("renamed.rs")).unwrap();
        let changes = current.changes();
        assert!(changes.affects(&changed));
        assert!(changes.affects(&root.path().join("renamed.rs")));
        assert!(changes.affects(root.path()));
        assert!(!changes.affects(&untouched));
        assert!(first.changes().affects(&changed));
        monitor.unregister(root.path());
        assert!(current.changes().affects(&untouched));
    }

    #[test]
    fn readers_older_than_retained_events_require_full_validation() {
        let root = tempfile::tempdir().unwrap();
        let monitor = Monitor::default();
        monitor.register(root.path());
        let paths = BTreeSet::from([root.path().into()]);
        let (_, before) = monitor.fence(&paths, 0);
        for i in 0..=EVENT_LIMIT {
            fs::write(root.path().join("file"), i.to_string()).unwrap();
            monitor.fence(&paths, before);
        }
        assert!(monitor.changes(&paths, before).0.full);
        let (_, now) = monitor.fence(&paths, before);
        assert!(!monitor.changes(&paths, now).0.any());
    }

    #[test]
    fn writes_are_visible_while_an_inherited_handle_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"old").unwrap();
        let monitor = Monitor::default();
        monitor.register(directory.path());
        let paths = BTreeSet::from([directory.path().to_owned()]);
        let (_, generation) = monitor.fence(&paths, 0);
        let mut writer = fs::OpenOptions::new().write(true).open(&source).unwrap();
        // A concurrent fork retains the writable file description until exec.
        let inherited = writer.try_clone().unwrap();
        writer.write_all(b"new").unwrap();
        drop(writer);
        assert!(monitor.fence(&paths, generation).0);
        drop(inherited);
    }
}
