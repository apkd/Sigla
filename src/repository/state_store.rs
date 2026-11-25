//! Atomic selector headers over immutable inventories. Usage is independent.
use super::{
    Identity,
    manager::State,
    materialize::{Prepared, Target},
};
use crate::cache::records;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const HEADER: &str = "state.bin";
const USAGE: &str = "usage.bin";
const MAX_HEADER: u64 = 1024 * 1024;
const MAX_INVENTORY: u64 = 256 * 1024 * 1024;
pub const USAGE_INTERVAL: u64 = 60_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Header {
    pub repository: Identity,
    pub transport: String,
    pub branch: String,
    pub target: Target,
    pub last_use: u64,
    pub refreshed: u64,
    pub policy: String,
    pub prepared: PreparedHeader,
    pub indexed_revision: Option<String>,
    pub repair: bool,
    inventory: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PreparedHeader {
    pub branch: Option<String>,
    pub revision: String,
    transfer_bytes: u64,
    transport: Option<String>,
    pub(super) resolved_target: Option<Target>,
}

#[derive(Serialize, Deserialize)]
struct Inventory {
    selected: BTreeMap<String, String>,
    tracked: BTreeMap<String, String>,
    directories: BTreeSet<String>,
    unavailable: BTreeSet<String>,
    omitted: BTreeMap<String, String>,
    deferred_assets: BTreeSet<String>,
    additional: BTreeSet<String>,
}

#[derive(Serialize)]
struct InventoryRef<'a> {
    selected: &'a BTreeMap<String, String>,
    tracked: &'a BTreeMap<String, String>,
    directories: &'a BTreeSet<String>,
    unavailable: &'a BTreeSet<String>,
    omitted: &'a BTreeMap<String, String>,
    deferred_assets: &'a BTreeSet<String>,
    additional: &'a BTreeSet<String>,
}

impl From<&State> for Header {
    fn from(state: &State) -> Self {
        Self {
            repository: state.repository.clone(),
            transport: state.transport.clone(),
            branch: state.branch.clone(),
            target: state.target.clone(),
            last_use: state.last_use,
            refreshed: state.refreshed,
            policy: state.policy.clone(),
            prepared: PreparedHeader {
                branch: state.prepared.branch.clone(),
                revision: state.prepared.revision.clone(),
                transfer_bytes: state.prepared.transfer_bytes,
                transport: state.prepared.transport.clone(),
                resolved_target: state.prepared.resolved_target.clone(),
            },
            indexed_revision: state.indexed_revision.clone(),
            repair: state.repair,
            inventory: String::new(),
        }
    }
}

fn inventory_path(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        name.starts_with("inventory-")
            && Path::new(name).file_name().is_some_and(|file| file == name),
        "Invalid inventory name"
    );
    Ok(root.join(name))
}

pub fn header(root: &Path) -> Result<Header> {
    let mut header: Header = records::read(&root.join(HEADER), MAX_HEADER)?;
    match records::read::<u64>(&root.join(USAGE), 32) {
        Ok(used) => header.last_use = header.last_use.max(used),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    Ok(header)
}

pub fn read(root: &Path) -> Result<State> {
    let directory = File::open(root)?;
    directory.lock_shared()?;
    let header = header(root)?;
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(inventory_path(root, &header.inventory)?)?;
    directory.unlock()?;
    let inventory: Inventory = records::read_file(file, MAX_INVENTORY)?;
    Ok(State {
        repository: header.repository,
        transport: header.transport,
        branch: header.branch,
        target: header.target,
        last_use: header.last_use,
        refreshed: header.refreshed,
        policy: header.policy,
        prepared: Prepared {
            selected: inventory.selected,
            tracked: inventory.tracked,
            directories: inventory.directories,
            unavailable: inventory.unavailable,
            omitted: inventory.omitted,
            deferred_assets: inventory.deferred_assets,
            branch: header.prepared.branch,
            revision: header.prepared.revision,
            transfer_bytes: header.prepared.transfer_bytes,
            transport: header.prepared.transport,
            resolved_target: header.prepared.resolved_target,
        },
        indexed_revision: header.indexed_revision,
        repair: header.repair,
        additional: inventory.additional,
    })
}

pub fn write(root: &Path, state: &State) -> Result<()> {
    publish(root, state, stage(root, state)?)
}

pub struct StagedInventory(tempfile::NamedTempFile);

pub fn stage(root: &Path, state: &State) -> Result<StagedInventory> {
    let prepared = &state.prepared;
    let inventory = records::stage_in(
        root,
        "inventory-",
        &InventoryRef {
            selected: &prepared.selected,
            tracked: &prepared.tracked,
            directories: &prepared.directories,
            unavailable: &prepared.unavailable,
            omitted: &prepared.omitted,
            deferred_assets: &prepared.deferred_assets,
            additional: &state.additional,
        },
    )?;
    Ok(StagedInventory(inventory))
}

pub fn publish(root: &Path, state: &State, inventory: StagedInventory) -> Result<()> {
    let inventory = inventory.0;
    let directory = File::open(root)?;
    directory.lock()?;
    let previous = match records::read::<Header>(&root.join(HEADER), MAX_HEADER) {
        Ok(header) => Some(inventory_path(root, &header.inventory)?),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let mut next = Header::from(state);
    next.inventory = inventory
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    inventory.keep()?;
    // Make the inventory durable before publishing a header that names it.
    directory.sync_all()?;
    records::write(&root.join(HEADER), &next)?;
    directory.sync_all()?;
    if let Some(previous) = previous {
        fs::remove_file(previous)?;
    }
    Ok(())
}

pub fn update(root: &Path, state: &State) -> Result<()> {
    let directory = File::open(root)?;
    directory.lock()?;
    let current: Header = records::read(&root.join(HEADER), MAX_HEADER)?;
    let next = Header {
        inventory: current.inventory,
        ..Header::from(state)
    };
    records::write(&root.join(HEADER), &next)?;
    directory.sync_all()?;
    Ok(())
}

pub fn record_use(root: &Path, last_use: u64) -> Result<()> {
    let directory = File::open(root)?;
    directory.lock()?;
    let previous = match records::read::<u64>(&root.join(USAGE), 32) {
        Ok(used) => used,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            0
        }
        Err(error) => return Err(error),
    };
    if last_use > previous {
        records::write(&root.join(USAGE), &last_use)?;
    }
    Ok(())
}

/// Remove unpublished inventories before requests start, under cache ownership.
pub(crate) fn recover(cache: &Path) -> Result<super::commits::Pins> {
    scan(cache, true)
}

pub(super) fn commit_pins(cache: &Path) -> Result<super::commits::Pins> {
    scan(cache, false)
}

fn scan(cache: &Path, recover: bool) -> Result<super::commits::Pins> {
    let mut pins = super::commits::Pins::default();
    let repositories = match fs::read_dir(cache.join("repositories")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(pins),
        Err(error) => return Err(error.into()),
    };
    for repository in repositories {
        let repository = repository?;
        ensure!(
            repository.file_type()?.is_dir(),
            "Invalid selector directory"
        );
        scan_selector(&repository.path(), recover, &mut pins)?;
    }
    pins.prefer_indexed();
    Ok(pins)
}

/// Retain repository inventories while requiring their graphs to be rebuilt.
pub(crate) fn invalidate_indexed(cache: &Path) -> Result<()> {
    let root = cache.join("repositories");
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(metadata.is_dir(), "Invalid repository cache directory");
    for repository in fs::read_dir(root)? {
        let repository = repository?;
        ensure!(
            repository.file_type()?.is_dir(),
            "Invalid selector directory"
        );
        let path = repository.path().join(HEADER);
        let mut header: Header = match records::read(&path, MAX_HEADER) {
            Ok(header) => header,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        if header.indexed_revision.take().is_some() {
            records::write(&path, &header)?;
            File::open(repository.path())?.sync_all()?;
        }
    }
    Ok(())
}

fn scan_selector(root: &Path, recover: bool, pins: &mut super::commits::Pins) -> Result<()> {
    let header = match records::read::<Header>(&root.join(HEADER), MAX_HEADER) {
        Ok(header) => Some(header),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    if recover {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| {
                name.starts_with("inventory-")
                    && Some(name) != header.as_ref().map(|h| h.inventory.as_str())
            }) {
                fs::remove_file(entry.path())?;
            }
        }
    }
    if let Some(header) = header {
        pins.restore_owner(root, header);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let repository = super::super::Repository::parse("https://github.com/test/repo")
            .unwrap()
            .unwrap();
        State {
            repository: repository.identity,
            transport: repository.transport,
            branch: "main".into(),
            target: Target::Branch("main".into()),
            last_use: 1,
            refreshed: 1,
            policy: "policy".into(),
            indexed_revision: None,
            repair: false,
            additional: BTreeSet::new(),
            prepared: Prepared {
                selected: BTreeMap::from([("File.cs".into(), "a".repeat(40))]),
                tracked: BTreeMap::new(),
                directories: BTreeSet::new(),
                unavailable: BTreeSet::new(),
                omitted: BTreeMap::new(),
                deferred_assets: BTreeSet::new(),
                branch: Some("main".into()),
                revision: "a".repeat(40),
                transfer_bytes: 0,
                transport: None,
                resolved_target: None,
            },
        }
    }

    #[test]
    fn small_updates_preserve_inventory_and_usage_survives_reopening() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state();
        write(root.path(), &state).unwrap();
        let inventory = header(root.path()).unwrap().inventory;
        let bytes = fs::read(root.path().join(&inventory)).unwrap();
        record_use(root.path(), 123).unwrap();
        state.refreshed += 1;
        state.indexed_revision = Some(state.prepared.revision.clone());
        update(root.path(), &state).unwrap();
        let saved = read(root.path()).unwrap();
        assert_eq!(saved.last_use, 123);
        assert_eq!(saved.refreshed, state.refreshed);
        assert_eq!(saved.indexed_revision, state.indexed_revision);
        assert_eq!(header(root.path()).unwrap().inventory, inventory);
        assert_eq!(fs::read(root.path().join(inventory)).unwrap(), bytes);
    }

    #[test]
    fn graph_reset_keeps_inventory_repair_and_usage_but_requires_indexing() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("repositories/selector");
        fs::create_dir_all(&root).unwrap();
        let mut state = state();
        state.indexed_revision = Some(state.prepared.revision.clone());
        state.repair = true;
        write(&root, &state).unwrap();
        record_use(&root, 123).unwrap();
        let inventory = header(&root).unwrap().inventory;
        let bytes = fs::read(root.join(&inventory)).unwrap();
        invalidate_indexed(cache.path()).unwrap();
        let restored = read(&root).unwrap();
        assert!(restored.indexed_revision.is_none());
        assert!(restored.repair);
        assert_eq!(restored.prepared.revision, state.prepared.revision);
        assert_eq!(restored.prepared.selected, state.prepared.selected);
        assert_eq!(restored.last_use, 123);
        assert_eq!(header(&root).unwrap().inventory, inventory);
        assert_eq!(fs::read(root.join(inventory)).unwrap(), bytes);
    }

    #[test]
    fn replacement_preserves_open_inventory_and_removes_old_generation() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state();
        write(root.path(), &state).unwrap();
        let old = inventory_path(root.path(), &header(root.path()).unwrap().inventory).unwrap();
        let reader = File::open(&old).unwrap();
        state.prepared.selected.clear();
        write(root.path(), &state).unwrap();
        assert!(!old.exists());
        assert_eq!(
            records::read_file::<Inventory>(reader, MAX_INVENTORY)
                .unwrap()
                .selected
                .len(),
            1
        );
        assert!(read(root.path()).unwrap().prepared.selected.is_empty());
    }

    #[test]
    fn concurrent_readers_observe_matching_header_and_inventory() {
        let root = tempfile::tempdir().unwrap();
        let first = state();
        let mut second = first.clone();
        second.prepared.revision = "b".repeat(40);
        second
            .prepared
            .selected
            .insert("File.cs".into(), second.prepared.revision.clone());
        write(root.path(), &first).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let barrier = &barrier;
            let root = root.path();
            let writer = scope.spawn(move || {
                let mut results = Vec::new();
                for round in 0..16 {
                    barrier.wait();
                    results.push(write(root, if round % 2 == 0 { &second } else { &first }));
                    barrier.wait();
                }
                results
            });
            let mut results = Vec::new();
            for _ in 0..16 {
                barrier.wait();
                results.push(read(root));
                barrier.wait();
            }
            for result in writer.join().unwrap() {
                result.unwrap();
            }
            for result in results {
                let current = result.unwrap();
                assert_eq!(
                    current.prepared.selected["File.cs"],
                    current.prepared.revision
                );
            }
        });
    }

    #[test]
    fn abandoned_staged_inventory_does_not_change_published_state() {
        let root = tempfile::tempdir().unwrap();
        let current = state();
        write(root.path(), &current).unwrap();
        let original = header(root.path()).unwrap().inventory;
        let staged = records::stage_in(root.path(), "inventory-", &"abandoned").unwrap();
        let (_, orphan) = staged.keep().unwrap();
        assert_ne!(orphan.file_name().unwrap(), original.as_str());
        let restored = read(root.path()).unwrap();
        assert_eq!(restored.prepared.revision, current.prepared.revision);
        assert_eq!(restored.prepared.selected, current.prepared.selected);
        assert_eq!(header(root.path()).unwrap().inventory, original);
        scan_selector(root.path(), true, &mut Default::default()).unwrap();
        assert!(!orphan.exists());
        assert_eq!(
            read(root.path()).unwrap().prepared.selected,
            current.prepared.selected
        );
    }

    #[test]
    fn usage_checkpoint_does_not_replace_inventory() {
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let current = state();
        write(root.path(), &current).unwrap();
        let inventory =
            inventory_path(root.path(), &header(root.path()).unwrap().inventory).unwrap();
        let before = fs::metadata(&inventory).unwrap();
        record_use(root.path(), current.last_use + 1).unwrap();
        record_use(root.path(), current.last_use).unwrap();
        assert_eq!(header(root.path()).unwrap().last_use, current.last_use + 1);
        let after = fs::metadata(&inventory).unwrap();
        assert_eq!(before.ino(), after.ino());
        assert_eq!(before.len(), after.len());
        assert_eq!(read(root.path()).unwrap().last_use, current.last_use + 1);
    }
}
