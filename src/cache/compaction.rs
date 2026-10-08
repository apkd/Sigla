//! An atomic replacement of a closed LMDB file, with a durable rollback link.
use anyhow::{Result, ensure};
use heed::{
    Env, EnvFlags, EnvOpenOptions,
    types::{Bytes, Str},
};
use std::{
    fs::{self, File},
    path::Path,
};

const PENDING: &str = "data.mdb.compacting";
const PREVIOUS: &str = "data.mdb.previous";
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub allocated: u64,
    pub live: u64,
}
impl Stats {
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
            .max_dbs(8)
            .flags(EnvFlags::READ_ONLY | EnvFlags::NO_SUB_DIR)
            .open(path)
    }?;
    let result = (|| -> Result<_> {
        let tx = env.read_txn()?;
        let control = env
            .open_database::<Bytes, Bytes>(&tx, Some("control"))?
            .ok_or_else(|| anyhow::anyhow!("Missing analysis control database"))?;
        ensure!(
            control.get(&tx, b"format")? == Some(crate::store::ANALYSIS_VERSION.as_bytes()),
            "Unknown analysis format"
        );
        let mut counts = Vec::new();
        for name in [
            "control",
            "workspaces",
            "bindings",
            "objects",
            "records",
            "declarations",
            "occurrences",
            "global-import-files",
        ] {
            let table = env
                .open_database::<Bytes, Bytes>(&tx, Some(name))?
                .ok_or_else(|| anyhow::anyhow!("Missing analysis table"))?;
            counts.push((name.to_owned(), table.len(&tx)?));
        }
        Ok(counts)
    })();
    env.prepare_for_closing().wait();
    result
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
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
    if previous.exists() {
        if validate(&current).is_err() {
            validate(&previous)?;
            fs::rename(&previous, &current)?;
            remove(&root.join("lock.mdb"))?;
        } else {
            remove(&previous)?;
        }
    }
    for name in [
        PENDING,
        "data.mdb.compacting-lock",
        "data.mdb.previous-lock",
        "data.mdb-lock",
    ] {
        remove(&root.join(name))?;
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

/// Consumes the last environment handle. Callers must exclude work for the full operation.
pub fn run(env: Env, cache: &Path) -> Result<u64> {
    let root = cache.join("analysis");
    let before = env.real_disk_size()?;
    let live = env.non_free_pages_size()?;
    let available = super::policy::Disk::read(cache)?.available;
    ensure!(
        available
            >= live
                .saturating_add(live / 10)
                .saturating_add(64 * 1024 * 1024),
        "Insufficient temporary space for analysis compaction"
    );
    let pending = root.join(PENDING);
    let file = env.copy_to_path(&pending, heed::CompactionOption::Enabled)?;
    file.sync_all()?;
    drop(file);
    let expected = {
        let tx = env.read_txn()?;
        let mut counts = Vec::new();
        for name in [
            "control",
            "workspaces",
            "bindings",
            "objects",
            "records",
            "declarations",
            "occurrences",
            "global-import-files",
        ] {
            // Re-open raw bytes only in the isolated validator; heed tracks typed handles here.
            let count = if matches!(name, "declarations" | "occurrences" | "global-import-files") {
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
    ensure!(
        validate(&pending)? == expected,
        "Compacted analysis validation failed"
    );
    env.prepare_for_closing().wait();
    fs::hard_link(root.join("data.mdb"), root.join(PREVIOUS))?;
    File::open(&root)?.sync_all()?;
    fs::rename(&pending, root.join("data.mdb"))?;
    File::open(&root)?.sync_all()?;
    remove(&root.join("lock.mdb"))?;
    if let Err(error) = validate(&root.join("data.mdb")) {
        fs::rename(root.join(PREVIOUS), root.join("data.mdb"))?;
        File::open(&root)?.sync_all()?;
        return Err(error);
    }
    let after = fs::metadata(root.join("data.mdb"))?.len();
    recover(cache)?;
    Ok(before.saturating_sub(after))
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
                        Ok(Encoded {
                            names: crate::binary::encode(&crate::store::shared::Names::default())
                                .unwrap(),
                            records: BTreeMap::from([(vec![1], vec![42; 64 * 1024])]),
                        })
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
        assert!(database.compact(cache.path()).unwrap() > 0);
        readable(cache.path());
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
    fn outstanding_database_handles_prevent_compaction() {
        let cache = tempfile::tempdir().unwrap();
        let database = populated(cache.path());
        let held = database.clone();
        assert!(database.compact(cache.path()).is_err());
        drop(held);
        readable(cache.path());
    }
}
