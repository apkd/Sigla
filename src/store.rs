//! Workspace views over shared immutable analysis. See store/shared.rs for ownership.
pub(crate) mod format;
pub(crate) mod payload;
pub mod shared;

use crate::{
    csharp::syntax::{BodyFile, DeclarationFile},
    model::{Facts, ModuleFile},
};
use anyhow::{Context, Result};
use heed::RoTxn;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

pub(crate) use payload::DeclarationLookup;
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
    Shared(Arc<format::Shared>),
    Directory(Arc<format::Directory>),
    Page(Arc<format::PageIndex>),
}
struct DecodedCache {
    entries: lru::LruCache<[u8; 32], (Decoded, usize)>,
    bytes: usize,
}
impl Default for DecodedCache {
    fn default() -> Self {
        Self {
            entries: lru::LruCache::unbounded(),
            bytes: 0,
        }
    }
}
static DECODED: LazyLock<Mutex<DecodedCache>> =
    LazyLock::new(|| Mutex::new(DecodedCache::default()));
impl DecodedCache {
    fn get(&mut self, key: &[u8; 32]) -> Option<Decoded> {
        self.entries.get(key).map(|entry| entry.0.clone())
    }
    fn put(&mut self, key: [u8; 32], value: Decoded, bytes: usize) {
        const LIMIT: usize = 64 * 1024 * 1024;
        // Include the Arc and LRU entry, in addition to the decoded allocation.
        let bytes = bytes.saturating_add(128);
        if bytes > LIMIT || self.entries.contains(&key) {
            return;
        }
        while self.bytes + bytes > LIMIT {
            let Some((_, (_, bytes))) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= bytes;
        }
        self.bytes += bytes;
        self.entries.put(key, (value, bytes));
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
        Ok(Some(crate::binary::decode(bytes)?))
    }
    pub fn install<R: Serialize>(
        &self,
        file: &str,
        revision: &R,
        id: ObjectId,
        verify: impl FnMut() -> Result<()>,
        build: impl FnOnce() -> Result<FileData>,
    ) -> Result<Installed> {
        self.install_encoded(file, revision, id, verify, || payload::encode(build()?))
    }
    pub(crate) fn install_encoded<R: Serialize>(
        &self,
        file: &str,
        revision: &R,
        id: ObjectId,
        verify: impl FnMut() -> Result<()>,
        build: impl FnOnce() -> Result<shared::Encoded>,
    ) -> Result<Installed> {
        self.scope.install(
            file,
            postcard::to_allocvec(revision)?,
            id,
            now(),
            verify,
            build,
        )
    }
    pub(crate) fn prepare_install<R: Serialize>(
        &self,
        file: &str,
        revision: &R,
        id: ObjectId,
        build: impl FnOnce() -> Result<shared::Encoded>,
    ) -> Result<shared::PendingInstall> {
        self.scope
            .prepare_install(file, postcard::to_allocvec(revision)?, id, now(), build)
    }
    pub fn modules(&self, file: &str) -> Result<Vec<ModuleFile>> {
        let tx = self.read()?;
        crate::binary::decode(
            self.scope
                .record(&tx, file, payload::MODULES)?
                .context("Missing module record")?,
        )
    }
    pub fn load(&self, tx: &RoTxn<'_>, file: &str) -> Result<Option<FileData>> {
        self.scope
            .record(tx, file, payload::DATA)?
            .map(|bytes| {
                let (mut data, _) = payload::decode::<FileData>(bytes, 32)?;
                data.facts.declarations = self.declarations_in(tx, file)?;
                data.facts.modules = crate::binary::decode(
                    self.scope
                        .record(tx, file, payload::MODULES)?
                        .context("Missing module record")?,
                )?;
                data.assembly = self.assembly_name_in(tx, file)?;
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
        format::Reader::new(|key| self.scope.record(tx, file, key))?.declarations()
    }
    fn headers_record(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        key: &[u8],
    ) -> Result<Option<Arc<DeclarationFile>>> {
        let Some(binding) = self.scope.binding(tx, file)? else {
            return Ok(None);
        };
        let mut hash = blake3::Hasher::new();
        hash.update(&binding.object);
        hash.update(key);
        let cache_key = *hash.finalize().as_bytes();
        if let Some(Decoded::Headers(value)) = DECODED.lock().unwrap().get(&cache_key) {
            return Ok(Some(value));
        }
        let mut reader =
            format::Reader::for_analysis(|key| self.scope.record(tx, file, key), binding.object)?;
        if !reader.directory.csharp {
            return Ok(None);
        }
        let data = if key == payload::HEADERS {
            if !reader.directory.source {
                return Ok(None);
            }
            reader.all()?
        } else {
            let index = u32::from_be_bytes(key[1..].try_into()?);
            if index >= reader.directory.count {
                return Ok(None);
            }
            let (declaration, header) = reader.declaration(index)?;
            DeclarationFile {
                declarations: vec![declaration],
                headers: vec![header],
                imports: Vec::new(),
            }
        };
        let size = format::declaration_bytes(&data);
        let data = Arc::new(data);
        DECODED
            .lock()
            .unwrap()
            .put(cache_key, Decoded::Headers(data.clone()), size);
        Ok(Some(data))
    }
    pub fn csharp_headers(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
    ) -> Result<Option<Arc<DeclarationFile>>> {
        self.headers_record(tx, file, payload::HEADERS)
    }
    pub(crate) fn csharp_lookup(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        name: &str,
        kind: DeclarationLookup,
    ) -> Result<Vec<u32>> {
        let Some(bytes) = self
            .scope
            .record(tx, file, &payload::lookup_key(kind, name))?
        else {
            return Ok(Vec::new());
        };
        Ok(payload::decode(bytes, 16)?.0)
    }
    pub(crate) fn csharp_imports(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
    ) -> Result<Vec<crate::csharp::syntax::Import>> {
        let bytes = self
            .scope
            .record(tx, file, payload::IMPORTS)?
            .context("Missing C# imports")?;
        Ok(payload::decode(bytes, 16)?.0)
    }
    pub fn csharp_declaration(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        index: u32,
    ) -> Result<Option<Arc<DeclarationFile>>> {
        self.headers_record(tx, file, &payload::declaration_key(index))
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
        let index = crate::binary::decode::<Vec<(std::ops::Range<usize>, u32)>>(bytes)?;
        let Some(entry) = index
            .iter()
            .filter(|v| v.0.start <= position && position < v.0.end)
            .min_by_key(|v| v.0.end - v.0.start)
        else {
            return Ok(None);
        };
        let bytes = self
            .scope
            .record(tx, file, &payload::body_key(entry.1))?
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
            .put(key, Decoded::Body(data.clone()), size);
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
                let imports = self
                    .csharp_imports(tx, &file)?
                    .into_iter()
                    .filter(|import| import.global)
                    .collect();
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
            .map(crate::binary::decode)
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
