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
    sync::{Arc, Condvar, LazyLock, Mutex, Weak},
};

pub type ObjectId = [u8; 32];
pub(crate) const FORMAT: &[u8] =
    concat!("sigla-shared-3:", env!("SIGLA_ANALYSIS_FINGERPRINT")).as_bytes();
const UNINDEXED: u64 = u64::MAX;
pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const BATCH: usize = 128;
pub(crate) const PRESSURE_BATCH: usize = 4096;
const UNUSED: &[u8] = b"unused:";
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
#[derive(Serialize, Deserialize)]
pub struct Encoded {
    pub names: Vec<u8>,
    pub records: BTreeMap<Vec<u8>, Vec<u8>>,
}

type EncodedObject = (Encoded, u64, Names);
const PARTITION: u64 = 256;
const POSTING_NAMES: &[u8] = b"posting-names";
const INPUT_ALIAS: &[u8] = b"input:";

pub(crate) struct PendingInstall {
    file: String,
    revision: Vec<u8>,
    object: ObjectId,
    now: u64,
    expected: Option<Binding>,
    encoded: Option<Arc<EncodedObject>>,
    input: Option<ObjectId>,
    index: Option<(Names, Vec<u8>)>,
}
impl PendingInstall {
    pub(crate) fn bytes(&self) -> u64 {
        self.encoded.as_ref().map_or(0, |encoded| encoded.1)
            + self
                .index
                .as_ref()
                .map_or(0, |(_, names)| names.len() as u64)
    }
    pub(crate) fn input(mut self, input: Option<ObjectId>) -> Self {
        self.input = input;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub object: ObjectId,
    pub revision: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Liveness {
    ordinal: u64,
    bindings: u64,
    unused_since: Option<u64>,
    encoded_bytes: u64,
}
#[derive(Serialize, Deserialize)]
struct LegacyLiveness {
    bindings: u64,
    unused_since: Option<u64>,
    encoded_bytes: u64,
}
fn decode_live(bytes: &[u8]) -> Result<Liveness> {
    if let Some(bytes) = bytes.strip_prefix(b"L2") {
        return decode(bytes);
    }
    let old: LegacyLiveness = decode(bytes)?;
    Ok(Liveness {
        ordinal: UNINDEXED,
        bindings: old.bindings,
        unused_since: old.unused_since,
        encoded_bytes: old.encoded_bytes,
    })
}
fn encode_live(live: &Liveness) -> Result<Vec<u8>> {
    let mut bytes = b"L2".to_vec();
    bytes.extend(encode(live)?);
    Ok(bytes)
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

pub(crate) type InspectionVisitor<'a> = dyn FnMut(&WorkspaceInfo, Option<&[u8]>) -> Result<()> + 'a;
fn inspect_env(
    env: &Env,
    visit: &mut InspectionVisitor<'_>,
) -> Result<crate::cache::compaction::Stats> {
    let stats = crate::cache::compaction::Stats {
        allocated: env.real_disk_size()?,
        live: env.compacted_size()?,
    };
    let tx = env.read_txn()?;
    let control = env
        .open_database::<Bytes, Bytes>(&tx, Some("control"))?
        .context("Missing analysis control table")?;
    let format = control.get(&tx, b"format")?;
    ensure!(
        format == Some(FORMAT) || format.is_some_and(super::compatibility::accepts),
        "Unknown analysis format"
    );
    let spaces = env
        .open_database::<Bytes, Bytes>(&tx, Some("workspaces"))?
        .context("Missing workspace table")?;
    for row in spaces.iter(&tx)? {
        let (_, value) = row?;
        let info: WorkspaceInfo = decode(value)?;
        visit(&info, control.get(&tx, &manifest_key(info.id))?)?;
    }
    Ok(stats)
}

#[cfg(test)]
pub(crate) fn snapshot_manifest(
    path: &Path,
    entry: &Path,
    accept: impl Fn(&[u8]) -> Result<bool>,
) -> Result<Vec<u8>> {
    // Register a reader so a live writer cannot recycle the snapshot's pages.
    let env = unsafe {
        EnvOpenOptions::new()
            .max_dbs(12)
            .flags(heed::EnvFlags::READ_ONLY)
            .open(path)
    }?;
    let mut manifest: Option<(u64, Phase, Vec<u8>)> = None;
    inspect_env(&env, &mut |info, bytes| {
        if info.entry == entry
            && let Some(bytes) = bytes
            && accept(bytes)?
            && manifest.as_ref().is_none_or(|(id, _, _)| info.id > *id)
        {
            manifest = Some((info.id, info.phase, bytes.to_vec()));
        }
        Ok(())
    })?;
    let (_, phase, bytes) = manifest.context("Workspace snapshot is unavailable")?;
    ensure!(phase == Phase::Clean, "Snapshot refresh has not finished");
    Ok(bytes)
}

/// The caller must hold exclusive service-cache ownership for the entire read.
pub(crate) fn inspect_idle(
    path: &Path,
    visit: &mut InspectionVisitor<'_>,
) -> Result<crate::cache::compaction::Stats> {
    let metadata = std::fs::symlink_metadata(path.join("data.mdb"))?;
    ensure!(metadata.is_file(), "Invalid analysis file");
    // No reader-table writes are needed while exclusive cache ownership excludes writers.
    let env = unsafe {
        EnvOpenOptions::new()
            .max_dbs(12)
            .flags(heed::EnvFlags::READ_ONLY | heed::EnvFlags::NO_LOCK)
            .open(path)
    }?;
    let result = inspect_env(&env, visit);
    env.prepare_for_closing().wait();
    result
}

pub struct Database {
    env: Env,
    control: Table<Bytes, Bytes>,
    spaces: Table<Bytes, Bytes>,
    bindings: Table<Bytes, Bytes>,
    declarations: Table<Bytes, Bytes>,
    occurrences: Table<Bytes, Bytes>,
    globals: Table<Bytes, Bytes>,
    members: Table<Bytes, Str>,
    legacy_postings: [Table<Bytes, Str>; 3],
    legacy_until: u64,
    objects: Table<Bytes, Bytes>,
    records: Table<Bytes, Bytes>,
    builds: Mutex<HashMap<ObjectId, Weak<Mutex<()>>>>,
    prepared: Mutex<HashMap<ObjectId, Weak<EncodedObject>>>,
    pins: Mutex<HashMap<ObjectId, usize>>,
    writers: Mutex<(u64, u64)>,
    writer_changed: Condvar,
}
struct WriteTurn<'a>(&'a Database);
impl Drop for WriteTurn<'_> {
    fn drop(&mut self) {
        self.0.writers.lock().unwrap().1 += 1;
        self.0.writer_changed.notify_all();
    }
}
struct Writer<'a> {
    tx: RwTxn<'a>,
    _turn: WriteTurn<'a>,
}
impl<'a> std::ops::Deref for Writer<'a> {
    type Target = RwTxn<'a>;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}
impl std::ops::DerefMut for Writer<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}
impl Writer<'_> {
    fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}
#[derive(Clone)]
pub struct Scope {
    pub database: Arc<Database>,
    pub fingerprint: [u8; 32],
    pub id: u64,
    snapshot: Option<Arc<Snapshot>>,
    partitions: Arc<Mutex<Option<Partitions>>>,
    previous: Vec<u64>,
}
type Partitions = (usize, Arc<BTreeSet<u64>>);

struct Snapshot {
    database: Arc<Database>,
    bindings: BTreeMap<String, Binding>,
    members: BTreeMap<u64, Vec<String>>,
    vocabulary: Option<Vec<u8>>,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        let mut pins = self.database.pins.lock().unwrap();
        for binding in self.bindings.values() {
            let count = pins.get_mut(&binding.object).unwrap();
            *count -= 1;
            if *count == 0 {
                pins.remove(&binding.object);
            }
        }
    }
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

fn unused_key(id: &ObjectId, at: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(UNUSED.len() + 8 + id.len());
    key.extend_from_slice(UNUSED);
    key.extend_from_slice(&at.to_be_bytes());
    key.extend_from_slice(id);
    key
}

impl Database {
    fn write(&self) -> Result<Writer<'_>> {
        let mut queue = self.writers.lock().unwrap();
        let position = queue.0;
        queue.0 += 1;
        while queue.1 != position {
            queue = self.writer_changed.wait(queue).unwrap();
        }
        drop(queue);
        let turn = WriteTurn(self);
        Ok(Writer {
            tx: self.env.write_txn()?,
            _turn: turn,
        })
    }
    pub(crate) fn opened(path: &Path) -> Option<Arc<Self>> {
        OPEN.lock().unwrap().get(path).and_then(Weak::upgrade)
    }
    pub(crate) fn inspection(
        &self,
        visit: &mut InspectionVisitor<'_>,
    ) -> Result<crate::cache::compaction::Stats> {
        inspect_env(&self.env, visit)
    }
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
                .max_dbs(12)
                .max_readers(128)
                // Data pages remain durable at each batch. A power failure may
                // lose the last metadata update, but cannot corrupt the cache.
                .flags(heed::EnvFlags::NO_META_SYNC)
                .open(&path)
        }?;
        let mut tx = env.write_txn()?;
        let control: Table<Bytes, Bytes> = env.create_database(&mut tx, Some("control"))?;
        let previous_format = control.get(&tx, b"format")?;
        let compatible = previous_format.is_some_and(super::compatibility::accepts);
        let obsolete = previous_format != Some(FORMAT) && !compatible;
        let spaces = env.create_database(&mut tx, Some("workspaces"))?;
        let bindings = env.create_database(&mut tx, Some("bindings"))?;
        let objects = env.create_database(&mut tx, Some("objects"))?;
        let records = env.create_database(&mut tx, Some("records"))?;
        let declarations = env
            .database_options()
            .types::<Bytes, Bytes>()
            .name("shared-declarations")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let occurrences = env
            .database_options()
            .types::<Bytes, Bytes>()
            .name("shared-occurrences")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let globals = env
            .database_options()
            .types::<Bytes, Bytes>()
            .name("shared-global-imports")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let members = env
            .database_options()
            .types::<Bytes, Str>()
            .name("members")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let mut legacy_postings = Vec::new();
        for name in ["declarations", "occurrences", "global-import-files"] {
            legacy_postings.push(
                env.database_options()
                    .types::<Bytes, Str>()
                    .name(name)
                    .flags(DatabaseFlags::DUP_SORT)
                    .create(&mut tx)?,
            );
        }
        let legacy_postings: [_; 3] = legacy_postings.try_into().ok().unwrap();
        if obsolete {
            control.clear(&mut tx)?;
            spaces.clear(&mut tx)?;
            bindings.clear(&mut tx)?;
            objects.clear(&mut tx)?;
            records.clear(&mut tx)?;
            declarations.clear(&mut tx)?;
            occurrences.clear(&mut tx)?;
            globals.clear(&mut tx)?;
            members.clear(&mut tx)?;
            for table in legacy_postings {
                table.clear(&mut tx)?;
            }
            control.put(&mut tx, b"format", FORMAT)?;
            control.put(&mut tx, b"unused-index-ready", b"1")?;
        }
        if compatible {
            let until = control
                .get(&tx, b"next-workspace")?
                .map(decode::<u64>)
                .transpose()?
                .unwrap_or(1);
            control.put(&mut tx, b"legacy-until", &encode(&until)?)?;
            control.put(&mut tx, b"format", FORMAT)?;
        }
        let legacy_until = control
            .get(&tx, b"legacy-until")?
            .map(decode::<u64>)
            .transpose()?
            .unwrap_or(0);
        let needs_unused_index = control.get(&tx, b"unused-index-ready")?.is_none();
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
            members,
            legacy_postings,
            legacy_until,
            builds: Mutex::new(HashMap::new()),
            prepared: Mutex::new(HashMap::new()),
            pins: Mutex::new(HashMap::new()),
            writers: Mutex::new((0, 0)),
            writer_changed: Condvar::new(),
        });
        if needs_unused_index {
            while !db.index_unused_batch(PRESSURE_BATCH)? {}
        }
        opened.insert(path, Arc::downgrade(&db));
        Ok(db)
    }

    pub fn workspace(
        self: &Arc<Self>,
        fingerprint: [u8; 32],
        entry: &Path,
        owner: Option<&Path>,
    ) -> Result<Scope> {
        // Reopening a completed workspace must not queue behind the writer.
        if let Some(scope) = self.existing(fingerprint)? {
            let tx = scope.read()?;
            let info = scope.writable(&tx)?;
            ensure!(
                info.entry == entry && info.owner.as_deref() == owner,
                "Workspace identity does not match persisted owner"
            );
            drop(tx);
            return Ok(scope);
        }
        let mut tx = self.write()?;
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
        let mut scope = Scope {
            database: self.clone(),
            fingerprint,
            id: info.id,
            snapshot: None,
            partitions: Default::default(),
            previous: Vec::new(),
        };
        let tx = self.env.read_txn()?;
        scope.previous = self.previous_scopes(&tx, entry, owner, scope.id)?;
        Ok(scope)
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
                    snapshot: None,
                    partitions: Default::default(),
                    previous: self.previous_scopes(
                        &tx,
                        &info.entry,
                        info.owner.as_deref(),
                        info.id,
                    )?,
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
    fn previous_scopes(
        &self,
        tx: &RoTxn<'_>,
        entry: &Path,
        owner: Option<&Path>,
        current: u64,
    ) -> Result<Vec<u64>> {
        if self.legacy_until == 0 || !super::compatibility::legacy_workspace() {
            return Ok(Vec::new());
        }
        let mut scopes = Vec::new();
        for row in self.spaces.iter(tx)? {
            let info: WorkspaceInfo = decode(row?.1)?;
            if info.id != current
                && info.id < self.legacy_until
                && info.phase == Phase::Clean
                && info.entry == entry
                && info.owner.as_deref() == owner
            {
                scopes.push(info.id);
            }
        }
        scopes.sort_unstable_by(|a, b| b.cmp(a));
        Ok(scopes)
    }

    pub fn record<'t>(
        &self,
        tx: &'t RoTxn<'_>,
        id: &ObjectId,
        key: &[u8],
    ) -> Result<Option<&'t [u8]>> {
        Ok(self.records.get(tx, &record_key(id, key))?)
    }
    fn live(&self, tx: &RoTxn<'_>, id: &ObjectId) -> Result<Option<Liveness>> {
        self.objects.get(tx, id)?.map(decode_live).transpose()
    }
    fn postings(&self, tx: &mut RwTxn<'_>, ordinal: u64, names: &Names, add: bool) -> Result<()> {
        let partition = ordinal / PARTITION;
        let value = ordinal.to_be_bytes();
        for (table, names) in [
            (self.declarations, &names.declarations),
            (self.occurrences, &names.occurrences),
        ] {
            for name in names {
                let key = scope_key(partition, name.as_bytes());
                if add {
                    table.put(tx, &key, &value)?;
                } else {
                    table.delete_one_duplicate(tx, &key, &value)?;
                }
            }
        }
        if names.global_imports {
            let key = partition.to_be_bytes();
            if add {
                self.globals.put(tx, &key, &value)?;
            } else {
                self.globals.delete_one_duplicate(tx, &key, &value)?;
            }
        }
        Ok(())
    }
    fn adjust(&self, tx: &mut RwTxn<'_>, id: &ObjectId, add: bool, now: u64) -> Result<()> {
        let mut live = self
            .live(tx, id)?
            .context("Binding points to missing object")?;
        if add {
            if let Some(at) = live.unused_since {
                self.control.delete(tx, &unused_key(id, at))?;
            }
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
                self.control.put(tx, &unused_key(id, now), &[])?;
            }
        }
        self.objects.put(tx, id, &encode_live(&live)?)?;
        Ok(())
    }

    /// Upgrade an existing database in bounded, restartable transactions.
    fn index_unused_batch(&self, limit: usize) -> Result<bool> {
        let mut tx = self.write()?;
        if self.control.get(&tx, b"unused-index-ready")?.is_some() {
            return Ok(true);
        }
        let cursor = self.control.get(&tx, b"unused-index-cursor")?;
        let start = cursor.map_or(Unbounded, Excluded);
        let rows: Vec<(ObjectId, Liveness)> = self
            .objects
            .range(&tx, &(start, Unbounded))?
            .take(limit)
            .map(|row| {
                let (id, live) = row?;
                Ok((id.try_into()?, decode_live(live)?))
            })
            .collect::<Result<_>>()?;
        for (id, live) in &rows {
            if live.bindings == 0 {
                let at = live
                    .unused_since
                    .context("Unused object has no retirement time")?;
                self.control.put(&mut tx, &unused_key(id, at), &[])?;
            }
        }
        let done = rows.len() < limit;
        if done {
            self.control.put(&mut tx, b"unused-index-ready", b"1")?;
            self.control.delete(&mut tx, b"unused-index-cursor")?;
            self.control.delete(&mut tx, b"gc-cursor")?;
        } else {
            self.control
                .put(&mut tx, b"unused-index-cursor", &rows.last().unwrap().0)?;
        }
        tx.commit()?;
        Ok(done)
    }

    /// Visit only expired unused objects; removing their index entries records progress.
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
        let Some(cutoff) = now.checked_sub(retention) else {
            return Ok(Collected::default());
        };
        let pins = self.pins.lock().unwrap();
        let mut tx = self.write()?;
        let end = unused_key(&[u8::MAX; 32], cutoff);
        let mut unused: Vec<(Vec<u8>, ObjectId)> = self
            .control
            .range(&tx, &(Included(UNUSED), Included(end.as_slice())))?
            .map(|row| {
                let key = row?.0;
                Ok((key.to_vec(), key[UNUSED.len() + 8..].try_into()?))
            })
            .filter(|row| row.as_ref().map_or(true, |(_, id)| !pins.contains_key(id)))
            .take(limit)
            .collect::<Result<_>>()?;
        // Object records are ordered by ID, independently of retirement time.
        unused.sort_unstable_by_key(|(_, id)| *id);
        let mut result = Collected {
            visited: unused.len(),
            ..Default::default()
        };
        for (key, id) in &unused {
            let live = self
                .live(&tx, id)?
                .context("Object disappeared inside transaction")?;
            if live.ordinal != UNINDEXED {
                let names: Names = decode(
                    self.record(&tx, id, POSTING_NAMES)?
                        .context("Missing posting names")?,
                )?;
                self.postings(&mut tx, live.ordinal, &names, false)?;
            }
            let prefix = record_key(id, INPUT_ALIAS);
            let aliases = self
                .records
                .prefix_iter(&tx, &prefix)?
                .map(|row| Ok(record_key(row?.0[prefix.len()..].try_into()?, b"input")))
                .collect::<Result<Vec<_>>>()?;
            for alias in aliases {
                if self.control.get(&tx, &alias)? == Some(id.as_slice()) {
                    self.control.delete(&mut tx, &alias)?;
                }
            }
            {
                let mut records = self.records.prefix_iter_mut(&mut tx, id)?;
                while records.next().transpose()?.is_some() {
                    // No borrowed key or value survives the cursor update.
                    unsafe { records.del_current()? };
                }
            }
            self.objects.delete(&mut tx, id)?;
            self.control.delete(&mut tx, key)?;
            result.objects += 1;
            result.encoded_bytes += live.encoded_bytes;
        }
        if !unused.is_empty() {
            tx.commit()?;
        }
        Ok(result)
    }

    pub fn object_count(&self, tx: &RoTxn<'_>) -> Result<u64> {
        Ok(self.objects.len(tx)?)
    }
    pub fn collect_unused(&self, now: u64, pressure: bool) -> Result<()> {
        let retention = if pressure { 0 } else { RETENTION_MS };
        while self
            .collect_with_retention(now, PRESSURE_BATCH, retention)?
            .visited
            == PRESSURE_BATCH
        {}
        Ok(())
    }
    pub fn binding_count(&self, tx: &RoTxn<'_>, id: &ObjectId) -> Result<Option<u64>> {
        Ok(self.live(tx, id)?.map(|l| l.bindings))
    }
    pub fn disk_bytes(&self) -> Result<u64> {
        Ok(self.env.real_disk_size()?)
    }
    pub fn checkpoint(&self) -> Result<()> {
        Ok(self.env.force_sync()?)
    }
    pub fn disk_stats(&self) -> Result<crate::cache::compaction::Stats> {
        Ok(crate::cache::compaction::Stats {
            allocated: self.env.real_disk_size()?,
            live: self.env.compacted_size()?,
        })
    }
    pub fn compact(self: Arc<Self>, cache: &Path) -> Result<u64> {
        let database =
            Arc::try_unwrap(self).map_err(|_| anyhow::anyhow!("Analysis is still in use"))?;
        crate::cache::compaction::run(database.env, cache)
    }
}

impl Scope {
    pub(crate) fn previous_binding(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        revision: &[u8],
    ) -> Result<Option<Binding>> {
        for &scope in &self.previous {
            if let Some(value) = self
                .database
                .bindings
                .get(tx, &scope_key(scope, file.as_bytes()))?
            {
                let binding: Binding = decode(value)?;
                if binding.revision == revision {
                    return Ok(Some(binding));
                }
            }
        }
        Ok(None)
    }
    pub(crate) fn previous_manifest(
        &self,
        accept: impl Fn(&[u8]) -> Result<bool>,
    ) -> Result<Option<Vec<u8>>> {
        let tx = self.read()?;
        for &scope in &self.previous {
            if let Some(bytes) = self.database.control.get(&tx, &manifest_key(scope))?
                && accept(bytes)?
            {
                return Ok(Some(bytes.to_vec()));
            }
        }
        Ok(None)
    }
    /// Pin immutable objects, not LMDB pages. Reads through this view can use
    /// short transactions while another generation replaces the workspace.
    pub fn snapshot(&self) -> Result<Self> {
        if self.snapshot.is_some() {
            return Ok(self.clone());
        }
        let mut pins = self.database.pins.lock().unwrap();
        let tx = self.query_read()?;
        let mut bindings = BTreeMap::new();
        let mut members = BTreeMap::<u64, Vec<String>>::new();
        for row in self
            .database
            .bindings
            .prefix_iter(&tx, &self.id.to_be_bytes())?
        {
            let (key, value) = row?;
            let file = std::str::from_utf8(&key[8..])?.to_owned();
            let binding: Binding = decode(value)?;
            let live = self
                .database
                .live(&tx, &binding.object)?
                .context("Missing bound object")?;
            members.entry(live.ordinal).or_default().push(file.clone());
            bindings.insert(file, binding);
        }
        let vocabulary = self
            .database
            .control
            .get(&tx, &scope_key(self.id, b"vocabulary"))?
            .map(<[u8]>::to_vec);
        for binding in bindings.values() {
            *pins.entry(binding.object).or_default() += 1;
        }
        Ok(Self {
            snapshot: Some(Arc::new(Snapshot {
                database: self.database.clone(),
                bindings,
                members,
                vocabulary,
            })),
            partitions: Default::default(),
            ..self.clone()
        })
    }
    pub(crate) fn input(&self, identity: &ObjectId) -> Result<Option<ObjectId>> {
        let tx = self.read()?;
        let key = record_key(identity, b"input");
        let Some(bytes) = self.database.control.get(&tx, &key)? else {
            return Ok(None);
        };
        let id: ObjectId = bytes.try_into()?;
        Ok(self.database.live(&tx, &id)?.map(|_| id))
    }
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
        ensure!(
            self.snapshot.is_none(),
            "Cannot write through an immutable snapshot"
        );
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
        if self.snapshot.is_some() {
            return Ok(());
        }
        let phase = self.info(tx)?.phase;
        ensure!(
            self.shared_ready(tx)?,
            crate::diagnostics::RefreshRequired(
                "Workspace index upgrade needs a completed refresh"
            )
        );
        ensure!(
            phase != Phase::Dirty,
            crate::diagnostics::RefreshRequired("Workspace needs a completed refresh")
        );
        ensure!(phase == Phase::Clean, "Workspace is being retired");
        Ok(())
    }
    pub fn begin_refresh(&self) -> Result<()> {
        {
            let tx = self.read()?;
            if self.writable(&tx)?.phase == Phase::Dirty {
                return Ok(());
            }
        }
        let mut tx = self.database.write()?;
        self.dirty_in(&mut tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn clean(&self) -> Result<bool> {
        let tx = self.read()?;
        Ok(self.info(&tx)?.phase == Phase::Clean && self.shared_ready(&tx)?)
    }
    fn shared_ready(&self, tx: &RoTxn<'_>) -> Result<bool> {
        Ok(self.id >= self.database.legacy_until
            || self
                .database
                .control
                .get(tx, &scope_key(self.id, b"shared-ready"))?
                .is_some())
    }
    pub(crate) fn matches(&self, tx: &RoTxn<'_>, file: &str, revision: &[u8]) -> Result<bool> {
        let Some(binding) = self.binding(tx, file)? else {
            return Ok(false);
        };
        Ok(binding.revision == revision
            && (self.id >= self.database.legacy_until
                || self.indexed(tx, file, &binding.object)?))
    }
    pub fn binding(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<Binding>> {
        if let Some(snapshot) = &self.snapshot {
            return Ok(snapshot.bindings.get(file).cloned());
        }
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
        let gate = self.database.gate(object)?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Object build interrupted"))?;
        let mut build = Some(build);
        let mut pending =
            self.prepare_install_inner(file, revision, object, now, || build.take().unwrap()())?;
        loop {
            verify()?;
            let mut tx = self.database.write()?;
            if let Some(installed) = self.install_in(&mut tx, &pending)? {
                tx.commit()?;
                return Ok(installed);
            }
            // A zero-reference hit was collected after preparation. Build outside the writer.
            drop(tx);
            pending.encoded = Some(Arc::new(
                self.encode_object(build.take().context("Object builder already consumed")?()?)?,
            ));
        }
    }

    pub(crate) fn prepare_install(
        &self,
        file: &str,
        revision: Vec<u8>,
        object: ObjectId,
        now: u64,
        build: impl FnOnce() -> Result<Encoded>,
    ) -> Result<PendingInstall> {
        let gate = self.database.gate(object)?;
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Object build interrupted"))?;
        self.prepare_install_inner(file, revision, object, now, build)
    }

    fn prepare_install_inner(
        &self,
        file: &str,
        revision: Vec<u8>,
        object: ObjectId,
        now: u64,
        build: impl FnOnce() -> Result<Encoded>,
    ) -> Result<PendingInstall> {
        ensure!(
            !file.is_empty()
                && 8 + file.len() <= self.database.env.max_key_size()
                && file.len() <= self.database.env.max_key_size(),
            "File key exceeds LMDB limit"
        );
        let (expected, present, index) = {
            let tx = self.read()?;
            self.writable(&tx)?;
            let live = self.database.live(&tx, &object)?;
            let index = if live.as_ref().is_some_and(|live| live.ordinal == UNINDEXED) {
                let descriptor = self
                    .database
                    .record(&tx, &object, &[])?
                    .context("Missing object descriptor")?;
                let names = super::format::names(descriptor, |key| {
                    self.database.record(&tx, &object, key)
                })?;
                let bytes = encode(&names)?;
                Some((names, bytes))
            } else {
                None
            };
            (self.binding(&tx, file)?, live.is_some(), index)
        };
        Ok(PendingInstall {
            file: file.to_owned(),
            revision,
            object,
            now,
            expected,
            input: None,
            index,
            encoded: if present {
                None
            } else {
                let cached = self
                    .database
                    .prepared
                    .lock()
                    .unwrap()
                    .get(&object)
                    .and_then(Weak::upgrade);
                Some(if let Some(cached) = cached {
                    cached
                } else {
                    let value = Arc::new(self.encode_object(build()?)?);
                    let mut prepared = self.database.prepared.lock().unwrap();
                    prepared.retain(|_, object| object.strong_count() != 0);
                    prepared.insert(object, Arc::downgrade(&value));
                    value
                })
            },
        })
    }

    fn encode_object(&self, mut value: Encoded) -> Result<EncodedObject> {
        ensure!(
            value.records.keys().all(|k| !k.is_empty()
                && k != POSTING_NAMES
                && !k.starts_with(INPUT_ALIAS)
                && 32 + k.len() <= self.database.env.max_key_size()),
            "Invalid analysis record key"
        );
        let decoded_names = super::format::names(&value.names, |key| {
            Ok(value.records.get(key).map(Vec::as_slice))
        })?;
        for name in decoded_names
            .declarations
            .iter()
            .chain(decoded_names.occurrences.iter())
        {
            ensure!(
                !name.is_empty()
                    && name.len() <= 480
                    && 8 + name.len() <= self.database.env.max_key_size(),
                "Indexed name exceeds LMDB limit"
            );
        }
        value
            .records
            .insert(POSTING_NAMES.to_vec(), encode(&decoded_names)?);
        let bytes = value
            .records
            .values()
            .try_fold(value.names.len() as u64, |sum, v| {
                sum.checked_add(v.len() as u64)
                    .context("Object size overflow")
            })?;
        Ok((value, bytes, decoded_names))
    }

    /// Every object and binding in a batch becomes durable in one transaction.
    /// None means a prepared cache hit was collected and must be rebuilt.
    pub(crate) fn install_batch(
        &self,
        pending: &[PendingInstall],
        mut verify: impl FnMut(usize) -> Result<()>,
    ) -> Result<Vec<Option<Installed>>> {
        // Filesystem work never belongs under the database writer. The caller
        // validates the generation again before publishing a clean manifest.
        for index in 0..pending.len() {
            verify(index)?;
        }
        let queued = std::time::Instant::now();
        let mut tx = self.database.write()?;
        let wait = queued.elapsed();
        let applying = std::time::Instant::now();
        let mut installed = Vec::with_capacity(pending.len());
        for pending in pending {
            let result = self.install_in(&mut tx, pending)?;
            if result.is_some()
                && let Some(input) = pending.input
            {
                let key = record_key(&input, b"input");
                if self.database.control.get(&tx, &key)? != Some(pending.object.as_slice()) {
                    self.database.control.put(&mut tx, &key, &pending.object)?;
                    let mut key = record_key(&pending.object, INPUT_ALIAS);
                    key.extend_from_slice(&input);
                    self.database.records.put(&mut tx, &key, &[])?;
                }
            }
            installed.push(result);
        }
        let apply = applying.elapsed();
        let committing = std::time::Instant::now();
        tx.commit()?;
        tracing::debug!(
            workspace = self.id,
            files = pending.len(),
            wait_ms = wait.as_millis(),
            apply_ms = apply.as_millis(),
            commit_ms = committing.elapsed().as_millis(),
            "analysis batch published"
        );
        Ok(installed)
    }

    fn install_in(
        &self,
        tx: &mut RwTxn<'_>,
        pending: &PendingInstall,
    ) -> Result<Option<Installed>> {
        let PendingInstall {
            file,
            revision,
            object,
            now,
            expected,
            encoded,
            input: _,
            index,
        } = pending;
        let now = *now;
        self.writable(tx)?;
        let old = self.binding(tx, file)?;
        // Another caller may already have installed exactly this revision.
        if old
            .as_ref()
            .is_some_and(|b| &b.object == object && &b.revision == revision)
            && self.indexed(tx, file, object)?
        {
            let bytes = self
                .database
                .live(tx, object)?
                .context("Missing bound object")?
                .encoded_bytes;
            return Ok(Some(Installed {
                reused: true,
                encoded_bytes: bytes,
                ..Default::default()
            }));
        }
        ensure!(
            &old == expected,
            "File binding changed during extraction; retry refresh"
        );
        let absent = self.database.live(tx, object)?.is_none();
        if absent && encoded.is_none() {
            return Ok(None);
        }
        self.dirty_in(tx)?;
        let mut live = if absent {
            let (value, bytes, _) = encoded.as_deref().unwrap();
            self.database.records.put(tx, object, &value.names)?;
            for (key, bytes) in &value.records {
                self.database
                    .records
                    .put(tx, &record_key(object, key), bytes)?;
            }
            Liveness {
                ordinal: UNINDEXED,
                bindings: 0,
                unused_since: Some(now),
                encoded_bytes: *bytes,
            }
        } else {
            self.database.live(tx, object)?.unwrap()
        };
        if live.ordinal == UNINDEXED {
            let names = if let Some((_, _, names)) = encoded.as_deref() {
                names
            } else {
                &index.as_ref().context("Posting names were not prepared")?.0
            };
            let ordinal = self
                .database
                .control
                .get(tx, b"next-object")?
                .map(decode::<u64>)
                .transpose()?
                .unwrap_or(0);
            ensure!(ordinal < UNINDEXED, "Object IDs exhausted");
            self.database.control.put(
                tx,
                b"next-object",
                &encode(&ordinal.checked_add(1).context("Object IDs exhausted")?)?,
            )?;
            if let Some((_, bytes)) = index {
                self.database
                    .records
                    .put(tx, &record_key(object, POSTING_NAMES), bytes)?;
            }
            self.database.postings(tx, ordinal, names, true)?;
            live.ordinal = ordinal;
            self.database
                .objects
                .put(tx, object, &encode_live(&live)?)?;
        }
        let changed = old.as_ref().is_none_or(|b| &b.object != object);
        if changed {
            if let Some(old) = &old {
                self.member(tx, file, &old.object, false)?;
                self.database.adjust(tx, &old.object, false, now)?;
            }
            self.database.adjust(tx, object, true, now)?;
        }
        self.member(tx, file, object, true)?;
        let bytes = self.database.live(tx, object)?.unwrap().encoded_bytes;
        self.database.bindings.put(
            tx,
            &scope_key(self.id, file.as_bytes()),
            &encode(&Binding {
                object: *object,
                revision: revision.clone(),
            })?,
        )?;
        Ok(Some(Installed {
            built: absent,
            reused: !absent,
            encoded_bytes: bytes,
        }))
    }

    fn indexed(&self, tx: &RoTxn<'_>, file: &str, object: &ObjectId) -> Result<bool> {
        let ordinal = self
            .database
            .live(tx, object)?
            .context("Missing bound object")?
            .ordinal;
        if ordinal == UNINDEXED {
            return Ok(false);
        }
        if let Some(rows) = self
            .database
            .members
            .get_duplicates(tx, &scope_key(self.id, &ordinal.to_be_bytes()))?
        {
            for row in rows {
                if row?.1 == file {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    fn member(&self, tx: &mut RwTxn<'_>, file: &str, object: &ObjectId, add: bool) -> Result<()> {
        let ordinal = self
            .database
            .live(tx, object)?
            .context("Missing bound object")?
            .ordinal;
        let key = scope_key(self.id, &ordinal.to_be_bytes());
        if add {
            self.database.members.put(tx, &key, file)?;
        } else {
            self.database.members.delete_one_duplicate(tx, &key, file)?;
        }
        Ok(())
    }
    fn detach_in(&self, tx: &mut RwTxn<'_>, file: &str, now: u64) -> Result<()> {
        if let Some(binding) = self.binding(tx, file)? {
            self.member(tx, file, &binding.object, false)?;
            self.database
                .bindings
                .delete(tx, &scope_key(self.id, file.as_bytes()))?;
            self.database.adjust(tx, &binding.object, false, now)?;
        }
        Ok(())
    }
    pub fn detach(&self, file: &str, now: u64) -> Result<()> {
        let mut tx = self.database.write()?;
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
        // Exact lookups use the small partition set directly. A large vocabulary
        // may exceed its decoded cache budget; decoding it for every type lookup
        // would repeatedly rebuild the entire workspace dictionary.
        if !exact && let Some(vocabulary) = self.vocabulary(tx)? {
            let terms = if occurrences {
                &vocabulary.occurrences
            } else {
                &vocabulary.declarations
            };
            for (term, partitions) in terms.iter().filter(|(term, _)| matches(term)) {
                for &partition in partitions {
                    if let Some(rows) =
                        table.get_duplicates(tx, &scope_key(partition, term.as_bytes()))?
                    {
                        for row in rows {
                            self.member_files(tx, row?.1, &mut result)?;
                        }
                    }
                }
            }
            return Ok(result);
        }
        for &partition in self.partitions(tx)?.iter() {
            if exact && name.len() <= 480 {
                if let Some(rows) =
                    table.get_duplicates(tx, &scope_key(partition, name.as_bytes()))?
                {
                    for row in rows {
                        self.member_files(tx, row?.1, &mut result)?;
                    }
                }
            } else {
                for row in table.prefix_iter(tx, &partition.to_be_bytes())? {
                    let (key, ordinal) = row?;
                    if matches(std::str::from_utf8(&key[8..])?) {
                        self.member_files(tx, ordinal, &mut result)?;
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn global_files(&self, tx: &RoTxn<'_>) -> Result<Vec<String>> {
        let mut files = BTreeSet::new();
        for &partition in self.partitions(tx)?.iter() {
            if let Some(rows) = self
                .database
                .globals
                .get_duplicates(tx, &partition.to_be_bytes())?
            {
                for row in rows {
                    self.member_files(tx, row?.1, &mut files)?;
                }
            }
        }
        Ok(files.into_iter().collect())
    }
    fn vocabulary(&self, tx: &RoTxn<'_>) -> Result<Option<Arc<super::names::Vocabulary>>> {
        let bytes = if let Some(snapshot) = &self.snapshot {
            snapshot.vocabulary.as_deref()
        } else if self.info(tx)?.phase == Phase::Clean {
            self.database
                .control
                .get(tx, &scope_key(self.id, b"vocabulary"))?
        } else {
            None
        };
        bytes.map(super::names::decode).transpose()
    }
    fn partitions(&self, tx: &RoTxn<'_>) -> Result<Arc<BTreeSet<u64>>> {
        let mut cached = self.partitions.lock().unwrap();
        if let Some((generation, partitions)) = cached.as_ref()
            && (*generation == tx.id() || self.snapshot.is_some())
        {
            return Ok(partitions.clone());
        }
        if let Some(snapshot) = &self.snapshot {
            let partitions = Arc::new(snapshot.members.keys().map(|id| id / PARTITION).collect());
            *cached = Some((tx.id(), Arc::clone(&partitions)));
            return Ok(partitions);
        }
        let partitions = Arc::new(
            self.database
                .members
                .prefix_iter(tx, &self.id.to_be_bytes())?
                .move_between_keys()
                .map(|row| Ok(u64::from_be_bytes(row?.0[8..].try_into()?) / PARTITION))
                .collect::<Result<_>>()?,
        );
        *cached = Some((tx.id(), Arc::clone(&partitions)));
        Ok(partitions)
    }
    fn member_files(
        &self,
        tx: &RoTxn<'_>,
        ordinal: &[u8],
        result: &mut BTreeSet<String>,
    ) -> Result<()> {
        if let Some(snapshot) = &self.snapshot {
            if let Some(files) = snapshot
                .members
                .get(&u64::from_be_bytes(ordinal.try_into()?))
            {
                result.extend(files.iter().cloned());
            }
        } else if let Some(rows) = self
            .database
            .members
            .get_duplicates(tx, &scope_key(self.id, ordinal))?
        {
            for row in rows {
                result.insert(row?.1.to_owned());
            }
        }
        Ok(())
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
        if self.clean()? && self.manifest()?.as_deref() == Some(manifest) {
            return Ok(());
        }
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
            let removed: Vec<_> = files
                .into_iter()
                .filter(|file| !keep.contains(file))
                .collect();
            if removed.is_empty() {
                continue;
            }
            let mut tx = self.database.write()?;
            self.writable(&tx)?;
            for file in removed {
                self.detach_in(&mut tx, &file, now)?;
            }
            tx.commit()?;
        }
        // Validate outside the writer. The caller's publication gate excludes changes
        // to this scope; other workspaces and collection cannot remove its live bindings.
        let vocabulary = {
            let tx = self.read()?;
            for file in keep {
                ensure!(
                    self.binding(&tx, file)?.is_some(),
                    "Manifest file has no binding"
                );
            }
            let mut vocabulary = super::names::Vocabulary::default();
            for row in self
                .database
                .members
                .prefix_iter(&tx, &self.id.to_be_bytes())?
                .move_between_keys()
            {
                let (key, file) = row?;
                let ordinal = u64::from_be_bytes(key[8..].try_into()?);
                let object = self.object(&tx, file)?.context("Missing member binding")?;
                let names = self
                    .database
                    .record(&tx, &object, POSTING_NAMES)?
                    .context("Missing posting names")?;
                vocabulary.add(ordinal / PARTITION, decode(names)?);
            }
            vocabulary.encode()?
        };
        let mut tx = self.database.write()?;
        let mut info = self.writable(&tx)?;
        self.database
            .control
            .put(&mut tx, &manifest_key(self.id), manifest)?;
        self.database
            .control
            .put(&mut tx, &scope_key(self.id, b"vocabulary"), &vocabulary)?;
        if self.id < self.database.legacy_until {
            self.database
                .control
                .put(&mut tx, &scope_key(self.id, b"shared-ready"), b"1")?;
        }
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
    /// Retire older versions of this logical workspace after its replacement
    /// is complete. Payloads stay live through the replacement's bindings.
    pub(crate) fn retire_previous(&self, equivalent: impl Fn(&[u8]) -> Result<bool>) -> Result<()> {
        let keys = {
            let tx = self.read()?;
            let current = self.info(&tx)?;
            let mut keys = Vec::new();
            for row in self.database.spaces.iter(&tx)? {
                let (key, value) = row?;
                let info: WorkspaceInfo = decode(value)?;
                if info.id < self.id
                    && info.entry == current.entry
                    && info.owner == current.owner
                    && info.phase != Phase::Deleting
                    && let Some(manifest) =
                        self.database.control.get(&tx, &manifest_key(info.id))?
                    && equivalent(manifest)?
                {
                    keys.push(key.to_vec());
                }
            }
            keys
        };
        if keys.is_empty() {
            return Ok(());
        }
        let mut tx = self.database.write()?;
        ensure!(
            self.writable(&tx)?.phase == Phase::Clean,
            "Replacement workspace is incomplete"
        );
        for key in keys {
            if let Some(bytes) = self.database.spaces.get(&tx, &key)? {
                let mut info: WorkspaceInfo = decode(bytes)?;
                info.phase = Phase::Deleting;
                self.database.spaces.put(&mut tx, &key, &encode(&info)?)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn mark_deleting(&self) -> Result<()> {
        let mut tx = self.database.write()?;
        let mut info = self.info(&tx)?;
        if info.phase == Phase::Deleting {
            return Ok(());
        }
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
        let mut tx = self.database.write()?;
        ensure!(
            self.info(&tx)?.phase == Phase::Deleting,
            "Namespace is not retiring"
        );
        // Shared postings survive workspace retirement. Remove only membership
        // lists; remaining rows are the persistent resume cursor.
        let mut remaining = limit;
        let mut tables = vec![self.database.members];
        if self.id < self.database.legacy_until {
            tables.extend(self.database.legacy_postings);
        }
        for table in tables {
            let keys: Vec<_> = table
                .prefix_iter(&tx, &self.id.to_be_bytes())?
                .move_between_keys()
                .take(remaining)
                .map(|row| Ok(row?.0.to_vec()))
                .collect::<Result<_>>()?;
            for key in &keys {
                table.delete(&mut tx, key)?;
            }
            remaining -= keys.len();
            if remaining == 0 {
                tx.commit()?;
                return Ok(false);
            }
        }
        let files = self.files_after(&tx, None, remaining)?;
        for file in files {
            let binding = self
                .binding(&tx, &file)?
                .context("Binding disappeared inside transaction")?;
            self.database
                .bindings
                .delete(&mut tx, &scope_key(self.id, file.as_bytes()))?;
            self.database.adjust(&mut tx, &binding.object, false, now)?;
        }
        let done = self.files_after(&tx, None, 1)?.is_empty();
        if done {
            self.database
                .control
                .delete(&mut tx, &scope_key(self.id, b"vocabulary"))?;
            self.database
                .control
                .delete(&mut tx, &scope_key(self.id, b"shared-ready"))?;
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
