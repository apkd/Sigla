//! Workspace views over shared immutable analysis. See store/shared.rs for ownership.
mod payload;
pub mod shared;

use crate::{
    csharp::syntax::{BodyFile, DeclarationFile},
    model::{Facts, ModuleFile},
};
use anyhow::{Context, Result};
use heed::RoTxn;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

pub use payload::{ANALYSIS_VERSION, canonical_defines, metadata_id, source_id};
pub use shared::{Database, Installed, ObjectId};
pub const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MANIFEST_PREFIX: &[u8] = b"sigla-manifest-3\0";

pub(crate) fn inspect_manifest(bytes: &[u8]) -> Result<Option<crate::workspace::Manifest>> {
    bytes
        .strip_prefix(MANIFEST_PREFIX)
        .map(|bytes| Ok(postcard::from_bytes(bytes)?))
        .transpose()
}

#[derive(Serialize, Deserialize)]
pub struct FileData {
    pub source: String,
    pub facts: Facts,
    pub assembly: Option<String>,
}

pub struct Store {
    pub scope: shared::Scope,
}

// Existing bounded cross-workspace decode cache, not another unbounded FileData cache.
#[derive(Clone)]
enum Decoded {
    Headers(Arc<DeclarationFile>),
    Body(Arc<BodyFile>),
}
#[derive(Default)]
struct DecodedCache {
    entries: HashMap<[u8; 32], (Decoded, usize)>,
    order: VecDeque<[u8; 32]>,
    bytes: usize,
}
static DECODED: LazyLock<Mutex<DecodedCache>> =
    LazyLock::new(|| Mutex::new(DecodedCache::default()));
impl DecodedCache {
    fn get(&mut self, key: &[u8; 32]) -> Option<Decoded> {
        let value = self.entries.get(key)?.0.clone();
        self.order.retain(|k| k != key);
        self.order.push_back(*key);
        Some(value)
    }
    fn put(&mut self, key: [u8; 32], value: Decoded, bytes: usize) {
        const LIMIT: usize = 64 * 1024 * 1024;
        if bytes > LIMIT || self.entries.contains_key(&key) {
            return;
        }
        while self.bytes + bytes > LIMIT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = self.entries.remove(&oldest) {
                self.bytes -= bytes;
            }
        }
        self.bytes += bytes;
        self.order.push_back(key);
        self.entries.insert(key, (value, bytes));
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
impl Store {
    pub fn open_workspace(
        analysis: &Path,
        fingerprint: [u8; 32],
        entry: &Path,
        owner: Option<&Path>,
    ) -> Result<Arc<Self>> {
        let database = Database::open(analysis)?;
        Ok(Arc::new(Self {
            scope: database.workspace(fingerprint, entry, owner)?,
        }))
    }
    pub fn read(&self) -> Result<RoTxn<'_, heed::WithTls>> {
        self.scope.read()
    }
    pub fn query_read(&self) -> Result<RoTxn<'_, heed::WithTls>> {
        self.scope.query_read()
    }
    pub fn check_read(&self, tx: &RoTxn<'_>) -> Result<()> {
        self.scope.check_read(tx)
    }
    pub fn begin_refresh(&self) -> Result<()> {
        self.scope.begin_refresh()
    }
    pub fn manifest_current(&self) -> Result<bool> {
        self.scope.clean()
    }
    pub fn get_manifest<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>> {
        self.scope
            .manifest()?
            .and_then(|bytes| bytes.strip_prefix(MANIFEST_PREFIX).map(<[u8]>::to_vec))
            .map(|bytes| Ok(postcard::from_bytes(&bytes)?))
            .transpose()
    }
    /// Reconcile against persisted bindings, not only the previous in-memory manifest.
    pub fn save_manifest(&self, manifest: &crate::workspace::Manifest) -> Result<()> {
        let keep = manifest.files.keys().cloned().collect();
        let mut bytes = MANIFEST_PREFIX.to_vec();
        bytes.extend(postcard::to_allocvec(manifest)?);
        self.scope.finish_refresh(&bytes, &keep, now())
    }
    pub fn remove(&self, file: &str) -> Result<()> {
        self.scope.detach(file, now())
    }
    pub fn current<R: Serialize>(
        &self,
        file: &str,
        revision: &R,
    ) -> Result<Option<Vec<ModuleFile>>> {
        let tx = self.read()?;
        let Some(binding) = self.scope.binding(&tx, file)? else {
            return Ok(None);
        };
        if binding.revision != postcard::to_allocvec(revision)? {
            return Ok(None);
        }
        let bytes = self
            .scope
            .database
            .record(&tx, &binding.object, payload::MODULES)?
            .context("Bound object is missing its module list")?;
        Ok(Some(postcard::from_bytes(bytes)?))
    }
    pub fn install<R: Serialize>(
        &self,
        file: &str,
        revision: &R,
        id: ObjectId,
        verify: impl FnMut() -> Result<()>,
        build: impl FnOnce() -> Result<FileData>,
    ) -> Result<Installed> {
        self.scope.install(
            file,
            postcard::to_allocvec(revision)?,
            id,
            now(),
            verify,
            || payload::encode(build()?),
        )
    }
    pub fn modules(&self, file: &str) -> Result<Vec<ModuleFile>> {
        let tx = self.read()?;
        Ok(postcard::from_bytes(
            self.scope
                .record(&tx, file, payload::MODULES)?
                .context("Missing module record")?,
        )?)
    }
    pub fn load(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<FileData>> {
        self.scope
            .record(tx, file, payload::DATA)?
            .map(|bytes| {
                let (data, _) = payload::decode::<FileData>(bytes, 32)?;
                payload::validate(&data)?;
                Ok(data)
            })
            .transpose()
    }
    pub fn declarations(&self, file: &str) -> Result<Vec<crate::model::Declaration>> {
        let tx = self.read()?;
        self.declarations_in(&tx, file)
    }
    pub fn declarations_in(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
    ) -> Result<Vec<crate::model::Declaration>> {
        let bytes = self
            .scope
            .record(tx, file, payload::SUMMARY)?
            .context("Missing declaration record")?;
        Ok(payload::decode(bytes, 16)?.0)
    }
    fn headers_record(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        key: &[u8],
    ) -> Result<Option<Arc<DeclarationFile>>> {
        let Some(bytes) = self.scope.record(tx, file, key)? else {
            return Ok(None);
        };
        let key = *blake3::hash(bytes).as_bytes();
        if let Some(Decoded::Headers(value)) = DECODED.lock().unwrap().get(&key) {
            return Ok(Some(value));
        }
        let (data, size) = payload::decode::<DeclarationFile>(bytes, 16)?;
        let data = Arc::new(data);
        DECODED
            .lock()
            .unwrap()
            .put(key, Decoded::Headers(data.clone()), size.saturating_mul(4));
        Ok(Some(data))
    }
    pub fn csharp_headers(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
    ) -> Result<Option<Arc<DeclarationFile>>> {
        self.headers_record(tx, file, payload::HEADERS)
    }
    pub fn csharp_members(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        name: &str,
    ) -> Result<Option<Arc<DeclarationFile>>> {
        self.headers_record(tx, file, &payload::member_key(name))
    }
    pub fn csharp_declaration(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        index: u32,
    ) -> Result<Option<Arc<DeclarationFile>>> {
        let Some(name) = self
            .scope
            .record(tx, file, &payload::declaration_key(index))?
        else {
            return Ok(None);
        };
        self.csharp_members(tx, file, std::str::from_utf8(name)?)
    }
    pub fn csharp_body(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        position: usize,
    ) -> Result<Option<Arc<BodyFile>>> {
        let Some(bytes) = self.scope.record(tx, file, payload::BODY_INDEX)? else {
            return Ok(None);
        };
        let index: Vec<(std::ops::Range<usize>, u32)> = postcard::from_bytes(bytes)?;
        let Some((_, ordinal)) = index
            .iter()
            .filter(|(r, _)| r.contains(&position))
            .min_by_key(|(r, _)| r.len())
        else {
            return Ok(None);
        };
        let bytes = self
            .scope
            .record(tx, file, &payload::body_key(*ordinal))?
            .context("Missing body group")?;
        let key = *blake3::hash(bytes).as_bytes();
        if let Some(Decoded::Body(value)) = DECODED.lock().unwrap().get(&key) {
            return Ok(Some(value));
        }
        let (data, size) = payload::decode::<BodyFile>(bytes, 16)?;
        let data = Arc::new(data);
        DECODED
            .lock()
            .unwrap()
            .put(key, Decoded::Body(data.clone()), size.saturating_mul(4));
        Ok(Some(data))
    }
    pub fn csharp_global_imports(
        &self,
        tx: &RoTxn<'_>,
    ) -> Result<Vec<(String, Vec<crate::csharp::syntax::Import>)>> {
        self.scope
            .global_files(tx)?
            .into_iter()
            .map(|file| {
                let imports = postcard::from_bytes(
                    self.scope
                        .record(tx, &file, payload::GLOBALS)?
                        .context("Missing global-import record")?,
                )?;
                Ok((file, imports))
            })
            .collect()
    }
    pub fn declaration_revision(&self, file: &str) -> Result<[u8; 32]> {
        let tx = self.read()?;
        Ok(self
            .scope
            .record(&tx, file, payload::ENVIRONMENT)?
            .context("Missing declaration fingerprint")?
            .try_into()?)
    }
    pub fn assembly_name(&self, file: &str) -> Result<Option<String>> {
        let tx = self.read()?;
        self.assembly_name_in(&tx, file)
    }
    pub fn assembly_name_in(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<String>> {
        self.scope
            .record(tx, file, payload::ASSEMBLY)?
            .map(|b| Ok(std::str::from_utf8(b)?.to_owned()))
            .transpose()
    }
    pub fn assembly_forwarders(&self, tx: &RoTxn<'_>, file: &str) -> Result<Vec<(String, String)>> {
        self.scope
            .record(tx, file, payload::FORWARDERS)?
            .map(|b| Ok(postcard::from_bytes(b)?))
            .transpose()
            .map(|v| v.unwrap_or_default())
    }
    pub fn candidates(
        &self,
        tx: &RoTxn<'_>,
        name: &str,
        loose: bool,
        occurrences: bool,
    ) -> Result<BTreeSet<String>> {
        self.scope.candidates(
            tx,
            name,
            occurrences,
            !loose && !name.contains('*'),
            |candidate| crate::query::name_rank(name, candidate, loose).is_some(),
        )
    }
}
