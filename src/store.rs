//! persistent file replacements. lmdb owns immutable reader snapshots and atomic publication.
use crate::model::Facts;
use anyhow::{Context, Result, ensure};
use heed::{
    Database, DatabaseFlags, Env, EnvOpenOptions, RoTxn,
    types::{Bytes, Str},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    io::Read,
    path::Path,
    sync::{Arc, LazyLock, Mutex, Weak},
};

const FORMAT: &[u8] = b"sigla-facts-11";
pub const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;

// Shared across every workspace; immutable record bytes are the revision key.
// Locations therefore refresh even when semantic headers remain equivalent.
static DECODED: LazyLock<Mutex<DecodedCache>> =
    LazyLock::new(|| Mutex::new(DecodedCache::default()));
#[derive(Clone)]
enum Decoded {
    Headers(Arc<crate::csharp::syntax::DeclarationFile>),
    Body(Arc<crate::csharp::syntax::BodyFile>),
}
#[derive(Default)]
struct DecodedCache {
    entries: HashMap<[u8; 32], (Decoded, usize)>,
    order: VecDeque<[u8; 32]>,
    bytes: usize,
}
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

#[derive(Serialize, Deserialize)]
pub struct FileData {
    pub source: String,
    pub facts: Facts,
    pub assembly: Option<String>,
}

pub struct Store {
    refresh: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    env: Env,
    files: Database<Str, Bytes>,
    summaries: Database<Str, Bytes>,
    headers: Database<Str, Bytes>,
    bodies: Database<Str, Bytes>,
    declarations: Database<Str, Str>,
    occurrences: Database<Str, Str>,
    metadata: Database<Str, Bytes>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        // the cache directory is private. only LMDB changes mapped pages, using its
        // transaction protocol; Sigla never truncates or replaces an open environment.
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(64usize * 1024 * 1024 * 1024)
                .max_dbs(7)
                .max_readers(128)
                .open(path)
        }?;
        let mut tx = env.write_txn()?;
        let files = env.create_database(&mut tx, Some("files"))?;
        let summaries = env.create_database(&mut tx, Some("summaries"))?;
        let headers = env.create_database(&mut tx, Some("csharp-headers"))?;
        let bodies = env.create_database(&mut tx, Some("csharp-bodies"))?;
        let metadata: Database<Str, Bytes> = env.create_database(&mut tx, Some("metadata"))?;
        let declarations = env
            .database_options()
            .types::<Str, Str>()
            .name("declarations")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        let occurrences = env
            .database_options()
            .types::<Str, Str>()
            .name("occurrences")
            .flags(DatabaseFlags::DUP_SORT)
            .create(&mut tx)?;
        if metadata.get(&tx, "format")? != Some(FORMAT) {
            files.clear(&mut tx)?;
            summaries.clear(&mut tx)?;
            headers.clear(&mut tx)?;
            bodies.clear(&mut tx)?;
            declarations.clear(&mut tx)?;
            occurrences.clear(&mut tx)?;
            metadata.clear(&mut tx)?;
            metadata.put(&mut tx, "format", FORMAT)?;
        }
        tx.commit()?;
        Ok(Self {
            refresh: Mutex::new(HashMap::new()),
            env,
            files,
            summaries,
            headers,
            bodies,
            declarations,
            occurrences,
            metadata,
        })
    }
    pub fn read(&self) -> Result<RoTxn<'_, heed::WithTls>> {
        Ok(self.env.read_txn()?)
    }
    pub fn load(&self, tx: &RoTxn<'_>, key: &str) -> Result<Option<FileData>> {
        self.files
            .get(tx, key)?
            .map(|bytes| {
                let mut decoded = Vec::new();
                zstd::stream::read::Decoder::new(bytes)?
                    .take((MAX_SOURCE_BYTES * 32 + 1) as u64)
                    .read_to_end(&mut decoded)?;
                ensure!(
                    decoded.len() <= MAX_SOURCE_BYTES * 32,
                    "Cache record exceeds safety bound"
                );
                let data: FileData =
                    postcard::from_bytes(&decoded).context("Invalid cached facts")?;
                validate(&data)?;
                Ok(data)
            })
            .transpose()
    }
    pub fn replace(&self, key: &str, data: &FileData, revision: &impl Serialize) -> Result<()> {
        let encoded = zstd::stream::encode_all(postcard::to_allocvec(data)?.as_slice(), 1)?;
        let summary = zstd::stream::encode_all(
            postcard::to_allocvec(&data.facts.declarations)?.as_slice(),
            1,
        )?;
        let mut tx = self.env.write_txn()?;
        if let Some(old) = self.load(&tx, key)? {
            self.remove_names(&mut tx, key, &old.facts)?;
        }
        self.files.put(&mut tx, key, &encoded)?;
        self.summaries.put(&mut tx, key, &summary)?;
        let environment = declaration_revision(data)?;
        self.metadata
            .put(&mut tx, &format!("environment:{key}"), &environment)?;
        if let Some(syntax) = &data.facts.csharp {
            self.metadata.put(
                &mut tx,
                &format!("forwarders:{key}"),
                &postcard::to_allocvec(&syntax.forwarders)?,
            )?;
            let global: Vec<_> = syntax.imports.iter().filter(|i| i.global).collect();
            self.metadata.put(
                &mut tx,
                &format!("global:{key}"),
                &postcard::to_allocvec(&global)?,
            )?;
            // Postcard encodes tuples and records identically. Borrow the arrays
            // while serializing rather than cloning an entire source file's facts.
            if data.assembly.is_some() {
                // Metadata has no bodies. Read only declarations sharing a lookup
                // name, with an index entry for following containing definitions.
                let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
                for (index, declaration) in data.facts.declarations.iter().enumerate() {
                    groups.entry(&declaration.name).or_default().push(index);
                    self.headers.put(
                        &mut tx,
                        &format!("index:{key}:{index}"),
                        declaration.name.as_bytes(),
                    )?;
                }
                for (name, indices) in groups {
                    let declarations: Vec<_> = indices
                        .iter()
                        .map(|i| &data.facts.declarations[*i])
                        .collect();
                    let headers: Vec<_> = indices.iter().map(|i| &syntax.headers[*i]).collect();
                    let bytes = zstd::stream::encode_all(
                        postcard::to_allocvec(&(declarations, headers, &syntax.imports))?
                            .as_slice(),
                        1,
                    )?;
                    self.headers
                        .put(&mut tx, &format!("name:{key}:{name}"), &bytes)?;
                }
            } else {
                let headers = zstd::stream::encode_all(
                    postcard::to_allocvec(&(
                        &data.facts.declarations,
                        &syntax.headers,
                        &syntax.imports,
                    ))?
                    .as_slice(),
                    1,
                )?;
                self.headers.put(&mut tx, key, &headers)?;
                self.remove_bodies(&mut tx, key)?;
                let groups = crate::csharp::syntax::split_bodies(
                    syntax,
                    &data.facts.declarations,
                    data.source.len(),
                )?;
                let mut index = Vec::new();
                for (ordinal, (range, group)) in groups.into_iter().enumerate() {
                    let group_key = format!("group:{key}:{ordinal}");
                    let encoded =
                        zstd::stream::encode_all(postcard::to_allocvec(&group)?.as_slice(), 1)?;
                    self.bodies.put(&mut tx, &group_key, &encoded)?;
                    index.push((range, group_key));
                }
                self.bodies
                    .put(&mut tx, key, &postcard::to_allocvec(&index)?)?;
            }
        } else {
            self.headers.delete(&mut tx, key)?;
            self.remove_bodies(&mut tx, key)?;
        }
        if let Some(name) = &data.assembly {
            self.metadata
                .put(&mut tx, &format!("assembly:{key}"), name.as_bytes())?;
        }
        self.metadata.put(
            &mut tx,
            &format!("revision:{key}"),
            &postcard::to_allocvec(revision)?,
        )?;
        for name in data
            .facts
            .declarations
            .iter()
            .map(|d| d.name.as_str())
            .collect::<BTreeSet<_>>()
        {
            if !name.is_empty() && name.len() <= 480 {
                self.declarations.put(&mut tx, name, key)?;
            }
        }
        for name in data
            .facts
            .occurrences
            .iter()
            .map(|d| d.name.as_str())
            .chain(data.facts.imports.iter().map(|i| i.alias.as_str()))
            .collect::<BTreeSet<_>>()
        {
            if !name.is_empty() && name.len() <= 480 {
                self.occurrences.put(&mut tx, name, key)?;
            }
        }
        for name in relationship_names(&data.facts) {
            self.declarations.put(&mut tx, &name, key)?;
        }
        tx.commit()?;
        Ok(())
    }
    /// share a build across all callers of this cache. publish its revision with its facts.
    pub fn ensure_revision<R: Serialize + serde::de::DeserializeOwned + PartialEq>(
        &self,
        key: &str,
        revision: &R,
        build: impl FnOnce() -> Result<FileData>,
    ) -> Result<(bool, Vec<crate::model::ModuleFile>)> {
        let gate = {
            let mut builds = self
                .refresh
                .lock()
                .map_err(|_| anyhow::anyhow!("Cache build registry was poisoned"))?;
            if let Some(gate) = builds.get(key).and_then(Weak::upgrade) {
                gate
            } else {
                builds.retain(|_, gate| gate.strong_count() > 0);
                let gate = Arc::new(Mutex::new(()));
                builds.insert(key.into(), Arc::downgrade(&gate));
                gate
            }
        };
        let _guard = gate
            .lock()
            .map_err(|_| anyhow::anyhow!("Cache build was interrupted"))?;
        if let Some((old, modules)) = self.revision::<(R, Vec<crate::model::ModuleFile>)>(key)?
            && &old == revision
        {
            return Ok((false, modules));
        }
        let data = build()?;
        self.replace(key, &data, &(revision, &data.facts.modules))?;
        Ok((true, data.facts.modules))
    }
    fn remove_names(&self, tx: &mut heed::RwTxn<'_>, key: &str, f: &Facts) -> Result<()> {
        for name in relationship_names(f) {
            self.declarations.delete_one_duplicate(tx, &name, key)?;
        }
        for name in f
            .declarations
            .iter()
            .map(|d| d.name.as_str())
            .collect::<BTreeSet<_>>()
        {
            if !name.is_empty() && name.len() <= 480 {
                self.declarations.delete_one_duplicate(tx, name, key)?;
            }
        }
        for name in f
            .occurrences
            .iter()
            .map(|o| o.name.as_str())
            .chain(f.imports.iter().map(|i| i.alias.as_str()))
            .collect::<BTreeSet<_>>()
        {
            if !name.is_empty() && name.len() <= 480 {
                self.occurrences.delete_one_duplicate(tx, name, key)?;
            }
        }
        Ok(())
    }
    pub fn remove(&self, key: &str) -> Result<()> {
        let mut tx = self.env.write_txn()?;
        if let Some(old) = self.load(&tx, key)? {
            self.remove_names(&mut tx, key, &old.facts)?;
        }
        self.files.delete(&mut tx, key)?;
        self.summaries.delete(&mut tx, key)?;
        self.headers.delete(&mut tx, key)?;
        self.remove_bodies(&mut tx, key)?;
        self.metadata.delete(&mut tx, &format!("revision:{key}"))?;
        self.metadata.delete(&mut tx, &format!("global:{key}"))?;
        tx.commit()?;
        Ok(())
    }
    pub fn declarations(&self, key: &str) -> Result<Vec<crate::model::Declaration>> {
        let tx = self.read()?;
        self.declarations_in(&tx, key)
    }
    pub fn csharp_headers(
        &self,
        tx: &RoTxn<'_>,
        key: &str,
    ) -> Result<Option<Arc<crate::csharp::syntax::DeclarationFile>>> {
        let Some(bytes) = self.headers.get(tx, key)? else {
            return Ok(None);
        };
        let key = *blake3::hash(bytes).as_bytes();
        if let Some(Decoded::Headers(value)) = DECODED.lock().unwrap().get(&key) {
            return Ok(Some(value));
        }
        let (data, size) = decode_record::<crate::csharp::syntax::DeclarationFile>(bytes)?;
        let data = Arc::new(data);
        DECODED
            .lock()
            .unwrap()
            .put(key, Decoded::Headers(data.clone()), size * 4);
        Ok(Some(data))
    }
    pub fn csharp_members(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        name: &str,
    ) -> Result<Option<Arc<crate::csharp::syntax::DeclarationFile>>> {
        self.csharp_headers(tx, &format!("name:{file}:{name}"))
    }
    pub fn csharp_global_imports(
        &self,
        tx: &RoTxn<'_>,
    ) -> Result<Vec<(String, Vec<crate::csharp::syntax::Import>)>> {
        let mut result = Vec::new();
        for row in self.metadata.prefix_iter(tx, "global:")? {
            let (key, value) = row?;
            let imports: Vec<_> = postcard::from_bytes(value)?;
            if !imports.is_empty() {
                result.push((key[7..].into(), imports));
            }
        }
        Ok(result)
    }
    pub fn csharp_declaration(
        &self,
        tx: &RoTxn<'_>,
        file: &str,
        index: u32,
    ) -> Result<Option<Arc<crate::csharp::syntax::DeclarationFile>>> {
        let Some(name) = self.headers.get(tx, &format!("index:{file}:{index}"))? else {
            return Ok(None);
        };
        self.csharp_members(tx, file, std::str::from_utf8(name)?)
    }
    pub fn csharp_body(
        &self,
        tx: &RoTxn<'_>,
        key: &str,
        position: usize,
    ) -> Result<Option<Arc<crate::csharp::syntax::BodyFile>>> {
        let Some(index) = self.bodies.get(tx, key)? else {
            return Ok(None);
        };
        let index: Vec<(std::ops::Range<usize>, String)> = postcard::from_bytes(index)?;
        let Some((_, key)) = index
            .iter()
            .filter(|(range, _)| range.contains(&position))
            .min_by_key(|(range, _)| range.len())
        else {
            return Ok(None);
        };
        let bytes = self
            .bodies
            .get(tx, key)?
            .ok_or_else(|| anyhow::anyhow!("Missing C# body group"))?;
        let key = *blake3::hash(bytes).as_bytes();
        if let Some(Decoded::Body(value)) = DECODED.lock().unwrap().get(&key) {
            return Ok(Some(value));
        }
        let (data, size) = decode_record::<crate::csharp::syntax::BodyFile>(bytes)?;
        let data = Arc::new(data);
        DECODED
            .lock()
            .unwrap()
            .put(key, Decoded::Body(data.clone()), size * 4);
        Ok(Some(data))
    }
    fn remove_bodies(&self, tx: &mut heed::RwTxn<'_>, file: &str) -> Result<()> {
        if let Some(index) = self.bodies.get(tx, file)? {
            let index: Vec<(std::ops::Range<usize>, String)> = postcard::from_bytes(index)?;
            for (_, key) in index {
                self.bodies.delete(tx, &key)?;
            }
        }
        self.bodies.delete(tx, file)?;
        Ok(())
    }
    pub fn declarations_in(
        &self,
        tx: &RoTxn<'_>,
        key: &str,
    ) -> Result<Vec<crate::model::Declaration>> {
        let bytes = self
            .summaries
            .get(tx, key)?
            .ok_or_else(|| anyhow::anyhow!("Missing declaration cache record"))?;
        let mut decoded = Vec::new();
        zstd::stream::read::Decoder::new(bytes)?
            .take((MAX_SOURCE_BYTES * 16 + 1) as u64)
            .read_to_end(&mut decoded)?;
        ensure!(
            decoded.len() <= MAX_SOURCE_BYTES * 16,
            "Declaration record exceeds safety bound"
        );
        Ok(postcard::from_bytes(&decoded)?)
    }
    pub fn revision<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let tx = self.read()?;
        self.metadata
            .get(&tx, &format!("revision:{key}"))?
            .map(|b| postcard::from_bytes(b).map_err(Into::into))
            .transpose()
    }
    pub fn declaration_revision(&self, key: &str) -> Result<[u8; 32]> {
        let tx = self.read()?;
        let bytes = self
            .metadata
            .get(&tx, &format!("environment:{key}"))?
            .ok_or_else(|| anyhow::anyhow!("Missing declaration revision"))?;
        Ok(bytes.try_into()?)
    }
    pub fn assembly_name(&self, key: &str) -> Result<Option<String>> {
        let tx = self.read()?;
        self.assembly_name_in(&tx, key)
    }
    pub fn assembly_name_in(&self, tx: &RoTxn<'_>, key: &str) -> Result<Option<String>> {
        self.metadata
            .get(tx, &format!("assembly:{key}"))?
            .map(|bytes| Ok(std::str::from_utf8(bytes)?.to_owned()))
            .transpose()
    }
    pub fn assembly_forwarders(&self, tx: &RoTxn<'_>, key: &str) -> Result<Vec<(String, String)>> {
        self.metadata
            .get(tx, &format!("forwarders:{key}"))?
            .map(|bytes| postcard::from_bytes(bytes).map_err(Into::into))
            .transpose()
            .map(|value| value.unwrap_or_default())
    }
    pub fn candidates(
        &self,
        tx: &RoTxn<'_>,
        name: &str,
        loose: bool,
        occurrences: bool,
    ) -> Result<BTreeSet<String>> {
        let db = if occurrences {
            self.occurrences
        } else {
            self.declarations
        };
        let mut result = BTreeSet::new();
        if !loose && !name.contains('*') && name.len() <= 480 {
            if let Some(iter) = db.get_duplicates(tx, name)? {
                for row in iter {
                    result.insert(row?.1.to_owned());
                }
            }
        } else {
            for row in db.iter(tx)? {
                let (candidate, file) = row?;
                if crate::query::name_rank(name, candidate, loose).is_some() {
                    result.insert(file.to_owned());
                }
            }
        }
        Ok(result)
    }
    pub fn get_manifest<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>> {
        let tx = self.read()?;
        self.metadata
            .get(&tx, "manifest")?
            .map(|b| postcard::from_bytes(b).map_err(Into::into))
            .transpose()
    }
    pub fn manifest_current(&self) -> Result<bool> {
        let tx = self.read()?;
        Ok(self.metadata.get(&tx, "manifest_revision")? == Some(b"3".as_slice()))
    }
    pub fn save_manifest<T: Serialize>(&self, manifest: &T) -> Result<()> {
        let bytes = postcard::to_allocvec(manifest)?;
        let mut tx = self.env.write_txn()?;
        self.metadata.put(&mut tx, "manifest", &bytes)?;
        self.metadata.put(&mut tx, "manifest_revision", b"3")?;
        tx.commit()?;
        Ok(())
    }
}

fn decode_record<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<(T, usize)> {
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(bytes)?
        .take((MAX_SOURCE_BYTES * 16 + 1) as u64)
        .read_to_end(&mut decoded)?;
    ensure!(
        decoded.len() <= MAX_SOURCE_BYTES * 16,
        "Semantic record exceeds safety bound"
    );
    Ok((postcard::from_bytes(&decoded)?, decoded.len()))
}

fn declaration_revision(data: &FileData) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    if let Some(syntax) = &data.facts.csharp {
        for header in &syntax.headers {
            let declaration = &data.facts.declarations[header.declaration as usize];
            if header.local {
                continue;
            }
            hash.update(&postcard::to_allocvec(&(
                &declaration.name,
                &declaration.qualified,
                &declaration.owner,
                &declaration.kind,
                &declaration.access,
                &declaration.modifiers,
                &declaration.attributes,
                &header.ty,
                &header.parameters,
                &header.generics,
                &header.bases,
                &header.explicit_interface,
                &header.implementations,
                &header.constant,
                &header.accessors,
            ))?);
            if declaration.kind == "const" {
                hash.update(data.source[declaration.header.clone()].as_bytes());
            }
        }
        for import in &syntax.imports {
            let namespace = data
                .facts
                .declarations
                .iter()
                .filter(|d| {
                    d.kind == "namespace"
                        && d.span.start <= import.scope.start
                        && d.span.end >= import.scope.end
                })
                .min_by_key(|d| d.span.len())
                .map(|d| d.qualified.as_str());
            hash.update(&postcard::to_allocvec(&(
                &import.kind,
                &import.ty,
                import.global,
                namespace,
            ))?);
        }
        // A damaged header must never retain a semantic cache generation by accident.
        if data.facts.errors {
            hash.update(data.source.as_bytes());
        }
    } else {
        hash.update(data.source.as_bytes());
    }
    Ok(*hash.finalize().as_bytes())
}

fn relationship_names(facts: &Facts) -> BTreeSet<String> {
    let simple = |name: &str| {
        name.split('<')
            .next()
            .unwrap_or(name)
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(name)
            .split('`')
            .next()
            .unwrap_or(name)
            .to_owned()
    };
    facts
        .declarations
        .iter()
        .flat_map(|d| &d.bases)
        .map(|base| format!("@base:{}", simple(base)))
        .chain(
            facts
                .imports
                .iter()
                .filter(|i| !i.alias.is_empty())
                .map(|i| format!("@alias:{}", simple(&i.path))),
        )
        .filter(|name| name.len() <= 480)
        .collect()
}

fn validate(data: &FileData) -> Result<()> {
    let valid = |r: &std::ops::Range<usize>| {
        r.start <= r.end
            && r.end <= data.source.len()
            && data.source.is_char_boundary(r.start)
            && data.source.is_char_boundary(r.end)
    };
    for d in &data.facts.declarations {
        ensure!(
            valid(&d.span) && valid(&d.name_span) && valid(&d.header) && valid(&d.scope),
            "Invalid declaration span in cache"
        );
    }
    for o in &data.facts.occurrences {
        ensure!(valid(&o.span), "Invalid occurrence span in cache");
    }
    Ok(())
}
