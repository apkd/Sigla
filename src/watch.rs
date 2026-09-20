//! shared, nonrecursive Linux directory watches with a metadata-poll fallback.
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct Monitor {
    state: Mutex<State>,
}
struct State {
    inotify: Option<Inotify>,
    directories: HashMap<PathBuf, (WatchDescriptor, usize)>,
    paths: HashMap<WatchDescriptor, PathBuf>,
    changed: HashMap<PathBuf, u64>,
    generation: u64,
    fallback: bool,
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
                changed: HashMap::new(),
                generation: 0,
            }),
        }
    }
}
impl Monitor {
    pub fn register(&self, path: &Path) {
        let mut state = self.state.lock().unwrap();
        if let Some((_, users)) = state.directories.get_mut(path) {
            *users += 1;
            return;
        }
        if state.fallback {
            return;
        }
        let mask = WatchMask::CLOSE_WRITE
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
        state.changed.remove(path);
        if let Some(inotify) = state.inotify.as_mut() {
            let _ = inotify.watches().remove(wd);
        }
    }
    pub fn fence(&self, paths: &BTreeSet<PathBuf>, previous: u64) -> (bool, u64) {
        let mut state = self.state.lock().unwrap();
        let mut buffer = [0u8; 65536];
        for batch in 0..8 {
            let events = match state.inotify.as_mut().map(|w| w.read_events(&mut buffer)) {
                Some(Ok(events)) => events.map(|e| (e.wd, e.mask)).collect::<Vec<_>>(),
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
            for (wd, mask) in events {
                if mask.contains(EventMask::Q_OVERFLOW)
                    || (state.paths.contains_key(&wd)
                        && mask.intersects(
                            EventMask::IGNORED | EventMask::DELETE_SELF | EventMask::MOVE_SELF,
                        ))
                {
                    state.fallback = true;
                }
                if let Some(path) = state.paths.get(&wd).cloned() {
                    state.changed.insert(path, generation);
                }
            }
            if batch == 7 {
                state.fallback = true;
            }
        }
        (
            state.fallback
                || paths
                    .iter()
                    .any(|p| state.changed.get(p).is_some_and(|&g| g > previous)),
            state.generation,
        )
    }
}
