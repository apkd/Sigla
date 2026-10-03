//! One LMDB transaction domain for immutable analysis and workspace-owned indexes.
//! This module does not know about syntax or perform filesystem input extraction.
use anyhow::{Context, Result, ensure};
use heed::{
    Database as Table, DatabaseFlags, Env, EnvOpenOptions, RoTxn, RwTxn,
    types::{Bytes, Str},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Bound::{Excluded, Included, Unbounded},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

pub type ObjectId = [u8; 32];
const FORMAT: &[u8] = b"sigla-shared-analysis-1";
pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const BATCH: usize = 128;
static OPEN: LazyLock<Mutex<HashMap<PathBuf, Weak<Database>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Names {
    pub declarations: BTreeSet<String>,
    pub occurrences: BTreeSet<String>,
    pub global_imports: bool,
}

/// Local record keys must be nonempty. The empty key is reserved for Names.
/// Values are already encoded/compressed when they enter the writer.
pub struct Encoded {
    pub names: Names,
    pub records: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub object: ObjectId,
    pub revision: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Liveness {
    bindings: u64,
    unused_since: Option<u64>,
    encoded_bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Dirty,
    Clean,
    Deleting,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub entry: PathBuf,
    pub owner: Option<PathBuf>,
    pub phase: Phase,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Installed {
    pub built: bool,
    pub reused: bool,
    pub encoded_bytes: u64,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Collected {
    pub visited: usize,
    pub objects: usize,
    pub encoded_bytes: u64,
}

pub struct Database {
    env: Env,
    control: Table<Bytes, Bytes>,
    spaces: Table<Bytes, Bytes>,
    bindings: Table<Bytes, Bytes>,
    declarations: Table<Bytes, Str>,
    occurrences: Table<Bytes, Str>,
    globals: Table<Bytes, Str>,
    objects: Table<Bytes, Bytes>,
    records: Table<Bytes, Bytes>,
    builds: Mutex<HashMap<ObjectId, Weak<Mutex<()>>>>,
}
#[derive(Clone)]
pub struct Scope {
    pub database: Arc<Database>,
    pub fingerprint: [u8; 32],
    pub id: u64,
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    postcard::from_bytes(bytes).context("Invalid shared analysis record")
}
fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    Ok(postcard::to_allocvec(value)?)
}
fn record_key(id: &ObjectId, suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(32 + suffix.len());
    key.extend_from_slice(id);
    key.extend_from_slice(suffix);
    key
}
fn scope_key(scope: u64, suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(8 + suffix.len());
    key.extend_from_slice(&scope.to_be_bytes());
    key.extend_from_slice(suffix);
    key
}
fn manifest_key(scope: u64) -> Vec<u8> {
    let mut key = b"manifest:".to_vec();
    key.extend_from_slice(&scope.to_be_bytes());
    key
}

impl Database {
    /// The service's existing ownership lock must protect the cache root.
    /// No strong reference is stored in the process-wide open registry.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        match std::fs::symlink_metadata(path) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Invalid analysis directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)?,
            Err(e) => return Err(e.into()),
        }
        let path = std::fs::canonicalize(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        for name in ["data.mdb", "lock.mdb"] {
            match std::fs::symlink_metadata(path.join(name)) {
                Ok(m) => ensure!(
                    m.is_file() && !m.file_type().is_symlink(),
                    "Invalid LMDB cache file"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut opened = OPEN
            .lock()
            .map_err(|_| anyhow::anyhow!("Analysis registry poisoned"))?;
        if let Some(db) = opened.get(&path).and_then(Weak::upgrade) {
            return Ok(db);
        }
        if let Some(closing) = heed::env_closing_event(&path) {
            closing.wait();
        }
        opened.retain(|_, db| db.strong_count() != 0);
        // SAFETY: exclusive service-cache ownership; only LMDB modifies mapped files.
        // Never resize, truncate, or replace this environment while it is open.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(64usize * 1024 * 1024 * 1024)
                .max_dbs(8)
                .max_readers(128)
                .open(&path)
        }?;
        let mut tx = env.write_txn()?;
        let control: Table<Bytes, Bytes> = env.create_database(&mut tx, Some("control"))?;
        match control.get(&tx, b"format")? {
            Some(format) => ensure!(
                format == FORMAT,
                "Analysis format changed; perform cold cache migration"
            ),
            None => {
                control.put(&mut tx, b"format", FORMAT)?;
            }
        }
        let spaces = env.create_database(&mut tx, Some("workspaces"))?;
        let bindings = env.create_database(&mut tx, Some("bindings"))?;
        let objects = env.create_database(&mut tx, Some("objects"))?;
        let records = env.create_database(&mut tx, Some("records"))?;
        let declarations = env
            .database_options()
            .types::<Bytes, Str>()
            .name("declarations")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let occurrences = env
            .database_options()
            .types::<Bytes, Str>()
            .name("occurrences")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let globals = env
            .database_options()
            .types::<Bytes, Str>()
            .name("global-import-files")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        tx.commit()?;
        let db = Arc::new(Self {
            env,
            control,
            spaces,
            bindings,
            objects,
            records,
            declarations,
            occurrences,
            globals,
            builds: Mutex::new(HashMap::new()),
        });
        opened.insert(path, Arc::downgrade(&db));
        Ok(db)
    }

    pub fn workspace(
        self: &Arc<Self>,
        fingerprint: [u8; 32],
        entry: &Path,
        owner: Option<&Path>,
    ) -> Result<Scope> {
        let mut tx = self.env.write_txn()?;
        let info: WorkspaceInfo = match self.spaces.get(&tx, &fingerprint)? {
            Some(value) => {
                let info: WorkspaceInfo = decode(value)?;
                ensure!(
                    info.phase != Phase::Deleting,
                    "Workspace retirement is incomplete"
                );
                ensure!(
                    info.entry == entry && info.owner.as_deref() == owner,
                    "Workspace identity does not match persisted owner"
                );
                info
            }
            None => {
                let id = self
                    .control
                    .get(&tx, b"next-workspace")?
                    .map(decode::<u64>)
                    .transpose()?
                    .unwrap_or(1);
                ensure!(id > 0, "Invalid workspace namespace counter");
                let next = id.checked_add(1).context("Workspace namespace exhausted")?;
                let info = WorkspaceInfo {
                    id,
                    entry: entry.to_owned(),
                    owner: owner.map(Path::to_owned),
                    phase: Phase::Dirty,
                };
                self.control
                    .put(&mut tx, b"next-workspace", &encode(&next)?)?;
                self.spaces.put(&mut tx, &fingerprint, &encode(&info)?)?;
                info
            }
        };
        tx.commit()?;
        Ok(Scope {
            database: self.clone(),
            fingerprint,
            id: info.id,
        })
    }

    pub fn workspaces(&self) -> Result<Vec<([u8; 32], WorkspaceInfo)>> {
        let tx = self.env.read_txn()?;
        self.spaces
            .iter(&tx)?
            .map(|row| {
                let (key, value) = row?;
                Ok((key.try_into()?, decode(value)?))
            })
            .collect()
    }

    /// Used by lifecycle maintenance, under the owning workspace/selector gate.
    pub fn existing(self: &Arc<Self>, fingerprint: [u8; 32]) -> Result<Option<Scope>> {
        let tx = self.env.read_txn()?;
        self.spaces
            .get(&tx, &fingerprint)?
            .map(|bytes| -> Result<_> {
                let info: WorkspaceInfo = decode(bytes)?;
                Ok(Scope {
                    database: self.clone(),
                    fingerprint,
                    id: info.id,
                })
            })
            .transpose()
    }

    fn gate(&self, object: ObjectId) -> Result<Arc<Mutex<()>>> {
        let mut gates = self
            .builds
            .lock()
            .map_err(|_| anyhow::anyhow!("Object build registry poisoned"))?;
        if let Some(gate) = gates.get(&object).and_then(Weak::upgrade) {
            return Ok(gate);
        }
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = Arc::new(Mutex::new(()));
        gates.insert(object, Arc::downgrade(&gate));
        Ok(gate)
    }

    pub fn record<'t>(
        &self,
        tx: &'t RoTxn<'_>,
        id: &ObjectId,
        key: &[u8],
    ) -> Result<Option<&'t [u8]>> {
        Ok(self.records.get(tx, &record_key(id, key))?)
    }
    fn names(&self, tx: &RoTxn<'_>, id: &ObjectId) -> Result<Names> {
        decode(
            self.record(tx, id, &[])?
                .context("Object descriptor is missing")?,
        )
    }
    fn live(&self, tx: &RoTxn<'_>, id: &ObjectId) -> Result<Option<Liveness>> {
        self.objects.get(tx, id)?.map(decode).transpose()
    }
    fn adjust(&self, tx: &mut RwTxn<'_>, id: &ObjectId, add: bool, now: u64) -> Result<()> {
        let mut live = self
            .live(tx, id)?
            .context("Binding points to missing object")?;
        if add {
            live.bindings = live
                .bindings
                .checked_add(1)
                .context("Object binding count overflow")?;
            live.unused_since = None;
        } else {
            live.bindings = live
                .bindings
                .checked_sub(1)
                .context("Object binding count underflow")?;
            if live.bindings == 0 {
                live.unused_since = Some(now);
            }
        }
        self.objects.put(tx, id, &encode(&live)?)?;
        Ok(())
    }

    /// Bounded scan; the cursor persists across closing/reopening the environment.
    /// Old readers remain valid through LMDB's own MVCC, including after collection.
    pub fn collect(&self, now: u64, limit: usize) -> Result<Collected> {
        self.collect_with_retention(now, limit, RETENTION_MS)
    }
    pub fn collect_with_retention(
        &self,
        now: u64,
        limit: usize,
        retention: u64,
    ) -> Result<Collected> {
        ensure!(limit > 0, "Collection batch must be nonzero");
        let mut tx = self.env.write_txn()?;
        let cursor = self.control.get(&tx, b"gc-cursor")?.map(<[u8]>::to_vec);
        let start = cursor.as_deref().map_or(Unbounded, Excluded);
        let ids: Vec<ObjectId> = self
            .objects
            .range(&tx, &(start, Unbounded))?
            .take(limit)
            .map(|row| Ok(row?.0.try_into()?))
            .collect::<Result<_>>()?;
        let mut result = Collected {
            visited: ids.len(),
            ..Default::default()
        };
        for id in &ids {
            let live = self
                .live(&tx, id)?
                .context("Object disappeared inside transaction")?;
            if live.bindings != 0
                || !live
                    .unused_since
                    .is_some_and(|at| now >= at && now - at >= retention)
            {
                continue;
            }
            let keys: Vec<Vec<u8>> = self
                .records
                .prefix_iter(&tx, id)?
                .map(|row| Ok(row?.0.to_vec()))
                .collect::<Result<_>>()?;
            for key in keys {
                self.records.delete(&mut tx, &key)?;
            }
            self.objects.delete(&mut tx, id)?;
            result.objects += 1;
            result.encoded_bytes += live.encoded_bytes;
        }
        if ids.len() == limit {
            self.control
                .put(&mut tx, b"gc-cursor", ids.last().unwrap())?;
        } else {
            self.control.delete(&mut tx, b"gc-cursor")?;
        }
        tx.commit()?;
        Ok(result)
    }

    pub fn object_count(&self, tx: &RoTxn<'_>) -> Result<u64> {
        Ok(self.objects.len(tx)?)
    }
    pub fn collect_unused(&self, now: u64, pressure: bool) -> Result<()> {
        let mut tx = self.env.write_txn()?;
        self.control.delete(&mut tx, b"gc-cursor")?;
        tx.commit()?;
        let retention = if pressure { 0 } else { RETENTION_MS };
        while self.collect_with_retention(now, 1024, retention)?.visited == 1024 {}
        Ok(())
    }
    pub fn binding_count(&self, tx: &RoTxn<'_>, id: &ObjectId) -> Result<Option<u64>> {
        Ok(self.live(tx, id)?.map(|l| l.bindings))
    }
    pub fn disk_bytes(&self) -> Result<u64> {
        Ok(self.env.real_disk_size()?)
    }
    pub fn disk_stats(&self) -> Result<crate::cache::compaction::Stats> {
        Ok(crate::cache::compaction::Stats {
            allocated: self.env.real_disk_size()?,
            live: self.env.non_free_pages_size()?,
        })
    }
    pub fn compact(self: Arc<Self>, cache: &Path) -> Result<u64> {
        let database =
            Arc::try_unwrap(self).map_err(|_| anyhow::anyhow!("Analysis is still in use"))?;
        crate::cache::compaction::run(database.env, cache)
    }
}

impl Scope {
    fn info(&self, tx: &RoTxn<'_>) -> Result<WorkspaceInfo> {
        let bytes = self
            .database
            .spaces
            .get(tx, &self.fingerprint)?
            .context("Workspace was retired")?;
        let info: WorkspaceInfo = decode(bytes)?;
        ensure!(
            info.id > 0 && info.id < u64::MAX && info.id == self.id,
            "Workspace handle belongs to an invalid or retired namespace"
        );
        Ok(info)
    }
    fn writable(&self, tx: &RoTxn<'_>) -> Result<WorkspaceInfo> {
        let info = self.info(tx)?;
        ensure!(info.phase != Phase::Deleting, "Workspace is being retired");
        Ok(info)
    }
    fn dirty_in(&self, tx: &mut RwTxn<'_>) -> Result<()> {
        let mut info = self.writable(tx)?;
        if info.phase != Phase::Dirty {
            info.phase = Phase::Dirty;
            self.database
                .spaces
                .put(tx, &self.fingerprint, &encode(&info)?)?;
        }
        Ok(())
    }
    pub fn read(&self) -> Result<RoTxn<'_, heed::WithTls>> {
        Ok(self.database.env.read_txn()?)
    }
    pub fn query_read(&self) -> Result<RoTxn<'_, heed::WithTls>> {
        let tx = self.read()?;
        self.check_read(&tx)?;
        Ok(tx)
    }
    pub fn check_read(&self, tx: &RoTxn<'_>) -> Result<()> {
        ensure!(
            self.info(tx)?.phase == Phase::Clean,
            "Workspace needs a completed refresh"
        );
        Ok(())
    }
    pub fn begin_refresh(&self) -> Result<()> {
        let mut tx = self.database.env.write_txn()?;
        self.dirty_in(&mut tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn clean(&self) -> Result<bool> {
        let tx = self.read()?;
        Ok(self.info(&tx)?.phase == Phase::Clean)
    }
    pub fn binding(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<Binding>> {
        self.database
            .bindings
            .get(tx, &scope_key(self.id, file.as_bytes()))?
            .map(decode)
            .transpose()
    }
    pub fn object(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<ObjectId>> {
        Ok(self.binding(tx, file)?.map(|b| b.object))
    }
    pub fn record<'t>(
        &self,
        tx: &'t RoTxn<'_>,
        file: &str,
        key: &[u8],
    ) -> Result<Option<&'t [u8]>> {
        match self.object(tx, file)? {
            Some(id) => self.database.record(tx, &id, key),
            None => Ok(None),
        }
    }

    /// Call under the workspace publication gate. No caller may keep a read
    /// transaction on this thread while entering this method (heed WithTls).
    /// `verify` must validate the input stamp on hits AND misses, after waiting.
    pub fn install(
        &self,
        file: &str,
        revision: Vec<u8>,
        object: ObjectId,
        now: u64,
        mut verify: impl FnMut() -> Result<()>,
        build: impl FnOnce() -> Result<Encoded>,
    ) -> Result<Installed> {
        ensure!(
            !file.is_empty()
                && 8 + file.len() <= self.database.env.max_key_size()
                && file.len() <= self.database.env.max_key_size(),
            "File key exceeds LMDB limit"
        );
        let expected = {
            let tx = self.read()?;
            self.writable(&tx)?;
            self.binding(&tx, file)?
        };
        let gate = self.database.gate(object)?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Object build interrupted"))?;
        let mut build = Some(build);
        let mut encoded: Option<(Encoded, Vec<u8>, u64)> = None;
        loop {
            let present = {
                let tx = self.read()?;
                self.database.live(&tx, &object)?.is_some()
            };
            if !present && encoded.is_none() {
                let value = build.take().context("Object builder already consumed")?()?;
                ensure!(
                    value
                        .records
                        .keys()
                        .all(|k| !k.is_empty() && 32 + k.len() <= self.database.env.max_key_size()),
                    "Invalid analysis record key"
                );
                for name in value
                    .names
                    .declarations
                    .iter()
                    .chain(&value.names.occurrences)
                {
                    ensure!(
                        !name.is_empty()
                            && name.len() <= 480
                            && 8 + name.len() <= self.database.env.max_key_size(),
                        "Indexed name exceeds LMDB limit"
                    );
                }
                let names = encode(&value.names)?;
                let bytes = value
                    .records
                    .values()
                    .try_fold(names.len() as u64, |sum, v| {
                        sum.checked_add(v.len() as u64)
                            .context("Object size overflow")
                    })?;
                encoded = Some((value, names, bytes));
            }
            verify()?;
            let mut tx = self.database.env.write_txn()?;
            self.writable(&tx)?;
            let old = self.binding(&tx, file)?;
            // Another caller may already have installed exactly this revision.
            if old
                .as_ref()
                .is_some_and(|b| b.object == object && b.revision == revision)
            {
                let bytes = self
                    .database
                    .live(&tx, &object)?
                    .context("Missing bound object")?
                    .encoded_bytes;
                return Ok(Installed {
                    reused: true,
                    encoded_bytes: bytes,
                    ..Default::default()
                });
            }
            ensure!(
                old == expected,
                "File binding changed during extraction; retry refresh"
            );
            let absent = self.database.live(&tx, &object)?.is_none();
            if absent && encoded.is_none() {
                // A zero-reference hit was collected after our read. Build on retry.
                drop(tx);
                continue;
            }
            self.dirty_in(&mut tx)?;
            if absent {
                let (value, names, bytes) = encoded.as_ref().unwrap();
                self.database.records.put(&mut tx, &object, names)?;
                for (key, bytes) in &value.records {
                    self.database
                        .records
                        .put(&mut tx, &record_key(&object, key), bytes)?;
                }
                self.database.objects.put(
                    &mut tx,
                    &object,
                    &encode(&Liveness {
                        bindings: 0,
                        unused_since: Some(now),
                        encoded_bytes: *bytes,
                    })?,
                )?;
            }
            let changed = old.as_ref().is_none_or(|b| b.object != object);
            if changed {
                if let Some(old) = &old {
                    self.postings(&mut tx, file, &old.object, false)?;
                    self.database.adjust(&mut tx, &old.object, false, now)?;
                }
                self.postings(&mut tx, file, &object, true)?;
                self.database.adjust(&mut tx, &object, true, now)?;
            }
            let bytes = self.database.live(&tx, &object)?.unwrap().encoded_bytes;
            self.database.bindings.put(
                &mut tx,
                &scope_key(self.id, file.as_bytes()),
                &encode(&Binding {
                    object,
                    revision: revision.clone(),
                })?,
            )?;
            tx.commit()?;
            return Ok(Installed {
                built: absent,
                reused: !absent,
                encoded_bytes: bytes,
            });
        }
    }

    fn postings(&self, tx: &mut RwTxn<'_>, file: &str, object: &ObjectId, add: bool) -> Result<()> {
        let names = self.database.names(tx, object)?;
        for (table, names) in [
            (self.database.declarations, names.declarations),
            (self.database.occurrences, names.occurrences),
        ] {
            for name in names {
                let key = scope_key(self.id, name.as_bytes());
                if add {
                    table.put(tx, &key, file)?;
                } else {
                    table.delete_one_duplicate(tx, &key, file)?;
                }
            }
        }
        if names.global_imports {
            if add {
                self.database
                    .globals
                    .put(tx, &self.id.to_be_bytes(), file)?;
            } else {
                self.database
                    .globals
                    .delete_one_duplicate(tx, &self.id.to_be_bytes(), file)?;
            }
        }
        Ok(())
    }
    fn detach_in(&self, tx: &mut RwTxn<'_>, file: &str, now: u64) -> Result<()> {
        if let Some(binding) = self.binding(tx, file)? {
            self.postings(tx, file, &binding.object, false)?;
            self.database
                .bindings
                .delete(tx, &scope_key(self.id, file.as_bytes()))?;
            self.database.adjust(tx, &binding.object, false, now)?;
        }
        Ok(())
    }
    pub fn detach(&self, file: &str, now: u64) -> Result<()> {
        let mut tx = self.database.env.write_txn()?;
        self.dirty_in(&mut tx)?;
        self.detach_in(&mut tx, file, now)?;
        tx.commit()?;
        Ok(())
    }
    pub fn candidates(
        &self,
        tx: &RoTxn<'_>,
        name: &str,
        occurrences: bool,
        exact: bool,
        mut matches: impl FnMut(&str) -> bool,
    ) -> Result<BTreeSet<String>> {
        let table = if occurrences {
            self.database.occurrences
        } else {
            self.database.declarations
        };
        let mut result = BTreeSet::new();
        if exact && name.len() <= 480 {
            if let Some(rows) = table.get_duplicates(tx, &scope_key(self.id, name.as_bytes()))? {
                for row in rows {
                    result.insert(row?.1.to_owned());
                }
            }
        } else {
            for row in table.prefix_iter(tx, &self.id.to_be_bytes())? {
                let (key, file) = row?;
                if matches(std::str::from_utf8(&key[8..])?) {
                    result.insert(file.to_owned());
                }
            }
        }
        Ok(result)
    }
    pub fn global_files(&self, tx: &RoTxn<'_>) -> Result<Vec<String>> {
        match self
            .database
            .globals
            .get_duplicates(tx, &self.id.to_be_bytes())?
        {
            Some(rows) => rows.map(|row| Ok(row?.1.to_owned())).collect(),
            None => Ok(Vec::new()),
        }
    }
    fn files_after(
        &self,
        tx: &RoTxn<'_>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        let lower = after.map_or_else(
            || self.id.to_be_bytes().to_vec(),
            |file| scope_key(self.id, file.as_bytes()),
        );
        let upper = (self.id + 1).to_be_bytes(); // max namespace is u64::MAX - 1.
        let start = if after.is_some() {
            Excluded(lower.as_slice())
        } else {
            Included(lower.as_slice())
        };
        self.database
            .bindings
            .range(tx, &(start, Excluded(upper.as_slice())))?
            .take(limit)
            .map(|row| Ok(std::str::from_utf8(&row?.0[8..])?.to_owned()))
            .collect()
    }
    /// Must hold the workspace publication gate through pruning and final commit.
    pub fn finish_refresh(&self, manifest: &[u8], keep: &BTreeSet<String>, now: u64) -> Result<()> {
        self.begin_refresh()?;
        let mut cursor: Option<String> = None;
        loop {
            let files = {
                let tx = self.read()?;
                self.files_after(&tx, cursor.as_deref(), BATCH)?
            };
            if files.is_empty() {
                break;
            }
            cursor = files.last().cloned();
            let mut tx = self.database.env.write_txn()?;
            self.writable(&tx)?;
            for file in files {
                if !keep.contains(&file) {
                    self.detach_in(&mut tx, &file, now)?;
                }
            }
            tx.commit()?;
        }
        // Validate outside the writer. The caller's publication gate excludes changes
        // to this scope; other workspaces and collection cannot remove its live bindings.
        {
            let tx = self.read()?;
            for file in keep {
                ensure!(
                    self.binding(&tx, file)?.is_some(),
                    "Manifest file has no binding"
                );
            }
        }
        let mut tx = self.database.env.write_txn()?;
        let mut info = self.writable(&tx)?;
        self.database
            .control
            .put(&mut tx, &manifest_key(self.id), manifest)?;
        info.phase = Phase::Clean;
        self.database
            .spaces
            .put(&mut tx, &self.fingerprint, &encode(&info)?)?;
        tx.commit()?;
        Ok(())
    }
    pub fn manifest(&self) -> Result<Option<Vec<u8>>> {
        let tx = self.read()?;
        if self.info(&tx)?.phase != Phase::Clean {
            return Ok(None);
        }
        Ok(self
            .database
            .control
            .get(&tx, &manifest_key(self.id))?
            .map(<[u8]>::to_vec))
    }
    pub fn mark_deleting(&self) -> Result<()> {
        let mut tx = self.database.env.write_txn()?;
        let mut info = self.info(&tx)?;
        info.phase = Phase::Deleting;
        self.database
            .spaces
            .put(&mut tx, &self.fingerprint, &encode(&info)?)?;
        tx.commit()?;
        Ok(())
    }
    /// Returns true when namespace retirement is complete. Resume after a crash.
    pub fn delete_batch(&self, now: u64, limit: usize) -> Result<bool> {
        ensure!(limit > 0, "Deletion batch must be nonzero");
        let mut tx = self.database.env.write_txn()?;
        ensure!(
            self.info(&tx)?.phase == Phase::Deleting,
            "Namespace is not retiring"
        );
        let files = self.files_after(&tx, None, limit)?;
        for file in files {
            self.detach_in(&mut tx, &file, now)?;
        }
        let done = self.files_after(&tx, None, 1)?.is_empty();
        if done {
            self.database
                .control
                .delete(&mut tx, &manifest_key(self.id))?;
            self.database.spaces.delete(&mut tx, &self.fingerprint)?;
        }
        tx.commit()?;
        Ok(done)
    }
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
