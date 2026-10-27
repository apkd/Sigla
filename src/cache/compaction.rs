//! An atomic replacement of a closed LMDB file, with a durable rollback link.
use anyhow::{Result, ensure};
use heed::{
    Env, EnvFlags, EnvOpenOptions,
    types::{Bytes, Str},
};
use std::{
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const PENDING: &str = "data.mdb.compacting";
const PREVIOUS: &str = "data.mdb.previous";
const RETIRED: &str = "data.mdb.retired-";
const TABLES: &[&str] = &[
    "control",
    "workspaces",
    "bindings",
    "objects",
    "records",
    "declarations",
    "occurrences",
    "global-import-files",
    "members",
    "shared-declarations",
    "shared-occurrences",
    "shared-global-imports",
];
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub allocated: u64,
    pub live: u64,
}
impl Stats {
    pub fn temporary_bytes(self) -> u64 {
        self.live
            .saturating_add(self.live / 10)
            .saturating_add(64 * 1024 * 1024)
    }
    pub fn reclaimable(self) -> u64 {
        self.allocated.saturating_sub(self.live)
    }
    pub fn eligible(self) -> bool {
        self.reclaimable() >= 256 * 1024 * 1024 && self.reclaimable() >= self.allocated / 4
    }
}

fn validate(path: &Path) -> Result<Vec<(String, u64)>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "Invalid analysis file"
    );
    // The cache ownership lock and the service activity gate exclude other openers.
    let env = unsafe {
        EnvOpenOptions::new()
            .max_dbs(12)
            .flags(EnvFlags::READ_ONLY | EnvFlags::NO_SUB_DIR)
            .open(path)
    }?;
    let result = (|| -> Result<_> {
        let tx = env.read_txn()?;
        let control = env
            .open_database::<Bytes, Bytes>(&tx, Some("control"))?
            .ok_or_else(|| anyhow::anyhow!("Missing analysis control database"))?;
        let format = control.get(&tx, b"format")?;
        let legacy = format.is_some_and(crate::store::compatibility::accepts);
        ensure!(
            format == Some(crate::store::shared::FORMAT) || legacy,
            "Unknown analysis format"
        );
        let mut counts = Vec::new();
        for &name in TABLES {
            let Some(table) = env.open_database::<Bytes, Bytes>(&tx, Some(name))? else {
                ensure!(legacy, "Missing analysis table");
                continue;
            };
            counts.push((name.to_owned(), table.len(&tx)?));
        }
        Ok(counts)
    })();
    env.prepare_for_closing().wait();
    result
}

fn remove(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Run before the normal environment is opened, under exclusive cache ownership.
pub fn recover(cache: &Path) -> Result<()> {
    let root = cache.join("analysis");
    if !root.is_dir() {
        return Ok(());
    }
    let current = root.join("data.mdb");
    let previous = root.join(PREVIOUS);
    if !previous.try_exists()? {
        return Ok(());
    }
    if validate(&current).is_err() {
        validate(&previous)?;
        fs::rename(&previous, &current)?;
        remove(&root.join("lock.mdb"))?;
    } else {
        // Disarm the rollback marker durably, without unlinking a possibly huge
        // obsolete environment on the startup path. Unique names survive retries.
        let (file, retired) = tempfile::Builder::new()
            .prefix(RETIRED)
            .tempfile_in(&root)?
            .keep()?;
        drop(file);
        fs::rename(&previous, retired)?;
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

/// Optional disposal after recovery. Hold cache ownership and at least a shared
/// activity guard, excluding compaction/publication. Never remove PREVIOUS here:
/// its presence means a publication still needs mandatory recovery.
pub fn cleanup(cache: &Path) -> Result<()> {
    let root = cache.join("analysis");
    if !root.is_dir() {
        return Ok(());
    }
    if root.join(PREVIOUS).try_exists()? {
        return Ok(());
    }
    let mut changed = false;
    for name in [
        PENDING,
        "data.mdb.compacting-lock",
        "data.mdb.previous-lock",
        "data.mdb-lock",
    ] {
        changed |= remove(&root.join(name))?;
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(RETIRED))
        {
            changed |= remove(&entry.path())?;
        }
    }
    if changed {
        File::open(root)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Generation {
    transaction: usize,
    device: u64,
    inode: u64,
}
impl Generation {
    fn read(env: &Env, root: &Path) -> Result<Self> {
        let metadata = fs::metadata(root.join("data.mdb"))?;
        Ok(Self {
            transaction: env.info().last_txn_id,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// The serial maintenance worker owns this copy until publication or disposal.
pub(crate) struct Candidate {
    root: PathBuf,
    generation: Generation,
    before: u64,
}
pub(crate) enum Preparation {
    Ready(Candidate),
    Changed,
    InsufficientSpace,
}
impl Candidate {
    pub(crate) fn current(&self, env: &Env) -> Result<bool> {
        Ok(self.generation == Generation::read(env, &self.root)?)
    }
}
impl Drop for Candidate {
    fn drop(&mut self) {
        for name in [PENDING, "data.mdb.compacting-lock"] {
            if let Err(error) = remove(&self.root.join(name)) {
                tracing::warn!(%error, "Cannot discard analysis compaction copy");
            }
        }
    }
}

/// Copy under shared service activity. A concurrent commit invalidates the copy.
pub(crate) fn prepare(env: &Env, cache: &Path) -> Result<Preparation> {
    let root = cache.join("analysis");
    ensure!(
        !root.join(PREVIOUS).try_exists()?,
        "Analysis compaction requires recovery"
    );
    cleanup(cache)?;
    let generation = Generation::read(env, &root)?;
    let before = env.real_disk_size()?;
    let live = env.compacted_size()?;
    let available = super::policy::Disk::read(cache)?.available;
    if available
        < (Stats {
            allocated: before,
            live,
        })
        .temporary_bytes()
    {
        return Ok(Preparation::InsufficientSpace);
    }
    let expected = {
        let tx = env.read_txn()?;
        let mut counts = Vec::new();
        for &name in TABLES {
            // Re-open raw bytes only in the isolated validator; heed tracks typed handles here.
            let count = if matches!(
                name,
                "members" | "declarations" | "occurrences" | "global-import-files"
            ) {
                env.open_database::<Bytes, Str>(&tx, Some(name))?
                    .unwrap()
                    .len(&tx)?
            } else {
                env.open_database::<Bytes, Bytes>(&tx, Some(name))?
                    .unwrap()
                    .len(&tx)?
            };
            counts.push((name.to_owned(), count));
        }
        counts
    };
    let candidate = Candidate {
        root,
        generation,
        before,
    };
    let pending = candidate.root.join(PENDING);
    let start = std::time::Instant::now();
    let file = env.copy_to_path(&pending, heed::CompactionOption::Enabled)?;
    file.sync_all()?;
    drop(file);
    let current = candidate.current(env)?;
    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        copied_bytes = fs::metadata(&pending)?.len(),
        source_growth_bytes = env.real_disk_size()?.saturating_sub(before),
        current,
        "Analysis compaction copy finished"
    );
    if !current {
        return Ok(Preparation::Changed);
    }
    ensure!(
        validate(&pending)? == expected,
        "Compacted analysis validation failed"
    );
    Ok(Preparation::Ready(candidate))
}

/// Consume the last environment handle under exclusive service activity.
pub(crate) fn publish(env: Env, candidate: Candidate, cache: &Path) -> Result<u64> {
    ensure!(
        candidate.current(&env)?,
        "Analysis changed during compaction"
    );
    let root = &candidate.root;
    let pending = root.join(PENDING);
    env.prepare_for_closing().wait();
    fs::hard_link(root.join("data.mdb"), root.join(PREVIOUS))?;
    File::open(root)?.sync_all()?;
    fs::rename(&pending, root.join("data.mdb"))?;
    File::open(root)?.sync_all()?;
    remove(&root.join("lock.mdb"))?;
    if let Err(error) = validate(&root.join("data.mdb")) {
        fs::rename(root.join(PREVIOUS), root.join("data.mdb"))?;
        File::open(root)?.sync_all()?;
        return Err(error);
    }
    let after = fs::metadata(root.join("data.mdb"))?.len();
    recover(cache)?;
    Ok(candidate.before.saturating_sub(after))
}

#[cfg(test)]
pub(crate) fn run(env: Env, cache: &Path) -> Result<u64> {
    let Preparation::Ready(candidate) = prepare(&env, cache)? else {
        anyhow::bail!("Exclusive compaction could not prepare a copy");
    };
    let reclaimed = publish(env, candidate, cache)?;
    cleanup(cache)?;
    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::shared::{Database, Encoded};
    use std::collections::BTreeMap;

    fn populated(cache: &Path) -> std::sync::Arc<Database> {
        let database = Database::open(&cache.join("analysis")).unwrap();
        let scope = database
            .workspace([1; 32], Path::new("/example"), None)
            .unwrap();
        for i in 0u32..100 {
            let object = *blake3::hash(&i.to_le_bytes()).as_bytes();
            scope
                .install(
                    &format!("file-{i}"),
                    vec![1],
                    object,
                    1,
                    || Ok(()),
                    || {
                        let mut records = BTreeMap::from([(vec![1], vec![42; 64 * 1024])]);
                        let names = crate::store::format::write(
                            &[],
                            None,
                            true,
                            &crate::store::shared::Names::default(),
                            &mut records,
                        )?;
                        Ok(Encoded { names, records })
                    },
                )
                .unwrap();
        }
        for i in 1..100 {
            scope.detach(&format!("file-{i}"), 2).unwrap();
        }
        database.collect_unused(3, true).unwrap();
        database
    }
    fn readable(cache: &Path) {
        let database = Database::open(&cache.join("analysis")).unwrap();
        let scope = database
            .workspace([1; 32], Path::new("/example"), None)
            .unwrap();
        let tx = scope.read().unwrap();
        assert!(scope.record(&tx, "file-0", &[1]).unwrap().is_some());
        assert!(scope.record(&tx, "file-1", &[1]).unwrap().is_none());
    }
    #[test]
    fn compact_copy_reclaims_space_and_preserves_live_records() {
        let cache = tempfile::tempdir().unwrap();
        let database = populated(cache.path());
        let expected = database.disk_stats().unwrap();
        let reclaimed = database.compact(cache.path()).unwrap();
        assert!(reclaimed > 0);
        assert_eq!(reclaimed, expected.reclaimable());
        assert_eq!(
            fs::metadata(cache.path().join("analysis/data.mdb"))
                .unwrap()
                .len(),
            expected.live
        );
        readable(cache.path());
    }
    #[test]
    fn concurrent_commit_discards_copy_and_preserves_new_records() {
        let cache = tempfile::tempdir().unwrap();
        let database = populated(cache.path());
        let Preparation::Ready(candidate) = database.prepare_compaction(cache.path()).unwrap()
        else {
            panic!("Could not prepare compaction");
        };
        let scope = database
            .workspace([1; 32], Path::new("/example"), None)
            .unwrap();
        scope
            .install(
                "new",
                vec![1],
                [99; 32],
                4,
                || Ok(()),
                || {
                    let mut records = BTreeMap::from([(vec![1], vec![99])]);
                    let names = crate::store::format::write(
                        &[],
                        None,
                        true,
                        &crate::store::shared::Names::default(),
                        &mut records,
                    )?;
                    Ok(Encoded { names, records })
                },
            )
            .unwrap();
        drop(scope);
        assert!(!database.compaction_current(&candidate).unwrap());
        assert!(
            database
                .publish_compaction(cache.path(), candidate)
                .is_err()
        );
        assert!(!cache.path().join("analysis").join(PENDING).exists());
        readable(cache.path());
        let database = Database::open(&cache.path().join("analysis")).unwrap();
        let scope = database
            .workspace([1; 32], Path::new("/example"), None)
            .unwrap();
        let tx = scope.read().unwrap();
        assert_eq!(scope.record(&tx, "new", &[1]).unwrap(), Some(&[99][..]));
    }
    #[test]
    fn interrupted_replacement_restores_a_valid_backup() {
        let cache = tempfile::tempdir().unwrap();
        drop(populated(cache.path()));
        let root = cache.path().join("analysis");
        fs::hard_link(root.join("data.mdb"), root.join(PREVIOUS)).unwrap();
        fs::write(root.join(PENDING), b"incomplete").unwrap();
        fs::rename(root.join(PENDING), root.join("data.mdb")).unwrap();
        recover(cache.path()).unwrap();
        readable(cache.path());
    }
    #[test]
    fn recovery_disarms_rollback_without_disposing_of_retained_data() {
        use std::os::unix::fs::MetadataExt;
        let cache = tempfile::tempdir().unwrap();
        drop(populated(cache.path()));
        let root = cache.path().join("analysis");
        let current = root.join("data.mdb");
        let inode = fs::metadata(&current).unwrap().ino();
        fs::hard_link(&current, root.join(PREVIOUS)).unwrap();
        fs::write(root.join(PENDING), b"interrupted copy").unwrap();
        recover(cache.path()).unwrap();
        assert_eq!(fs::metadata(&current).unwrap().ino(), inode);
        assert!(!root.join(PREVIOUS).exists());
        assert!(root.join(PENDING).exists());
        let retired = || {
            fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with(RETIRED)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(retired().len(), 1);
        // A subsequent startup must neither re-promote nor unlink this garbage.
        recover(cache.path()).unwrap();
        assert_eq!(retired().len(), 1);
        readable(cache.path());
        cleanup(cache.path()).unwrap();
        cleanup(cache.path()).unwrap();
        assert!(retired().is_empty());
        assert!(!root.join(PENDING).exists());
        readable(cache.path());
    }

    #[test]
    fn recovery_restores_a_missing_current_file() {
        let cache = tempfile::tempdir().unwrap();
        drop(populated(cache.path()));
        let root = cache.path().join("analysis");
        fs::rename(root.join("data.mdb"), root.join(PREVIOUS)).unwrap();
        recover(cache.path()).unwrap();
        readable(cache.path());
    }

    #[test]
    fn failed_recovery_keeps_both_candidates_and_cleanup_never_removes_the_backup() {
        let cache = tempfile::tempdir().unwrap();
        let root = cache.path().join("analysis");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("data.mdb"), b"invalid current").unwrap();
        fs::write(root.join(PREVIOUS), b"invalid backup").unwrap();
        fs::write(root.join(PENDING), b"unpublished copy").unwrap();
        assert!(recover(cache.path()).is_err());
        cleanup(cache.path()).unwrap();
        assert_eq!(fs::read(root.join("data.mdb")).unwrap(), b"invalid current");
        assert_eq!(fs::read(root.join(PREVIOUS)).unwrap(), b"invalid backup");
        assert_eq!(fs::read(root.join(PENDING)).unwrap(), b"unpublished copy");
        assert!(recover(cache.path()).is_err());
    }

    #[test]
    fn outstanding_database_handles_prevent_compaction() {
        let cache = tempfile::tempdir().unwrap();
        let database = populated(cache.path());
        let held = database.clone();
        assert!(database.compact(cache.path()).is_err());
        drop(held);
        readable(cache.path());
    }
}
