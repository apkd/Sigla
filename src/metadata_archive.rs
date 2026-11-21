//! Offline production and online consumption of the same immutable analysis records.
mod bundle;
mod client;
mod packing;
mod producer;
mod registry;
mod release;
mod sections;
#[cfg(test)]
pub(crate) mod tests;

pub(crate) use client::configured;
pub(crate) use client::editor;
pub(crate) use client::{PackagePlan, materialize_packages, plan_package};
pub use client::{package, prefetch};
pub use producer::Command;

pub(crate) fn section_entries(root: &Path) -> Result<Vec<Entry>> {
    let frame: sections::Frame = binary::decode(payload(&read(&root.join("section.bin"))?)?)?;
    Ok(frame.entries)
}

/// Resolve each selected section once; raw sources retain a local parsing fallback.
pub(crate) struct Indexes {
    cache: PathBuf,
    sections: BTreeMap<PathBuf, Option<(PathBuf, std::sync::Arc<crate::store::segment::Segment>)>>,
}
impl Indexes {
    pub fn new(cache: &Path) -> Self {
        Self {
            cache: cache.into(),
            sections: BTreeMap::new(),
        }
    }
    pub fn file(
        &mut self,
        path: &Path,
        profile: impl FnOnce() -> Result<ObjectId>,
    ) -> Result<Option<(PathBuf, u32)>> {
        let Ok(relative) = path.strip_prefix(self.cache.join("unity-metadata")) else {
            return Ok(None);
        };
        let mut parts = relative.components();
        let Some(artifact) = parts.next() else {
            return Ok(None);
        };
        if parts
            .next()
            .is_none_or(|part| part.as_os_str() != "sections")
        {
            return Ok(None);
        }
        let Some(section) = parts.next() else {
            return Ok(None);
        };
        let root = self
            .cache
            .join("unity-metadata")
            .join(artifact)
            .join("sections")
            .join(section);
        if !self.sections.contains_key(&root) {
            let frame: sections::Frame =
                binary::decode(payload(&read(&root.join("section.bin"))?)?)?;
            let name = sections::index_path(&frame.role);
            let index = if frame.entries.iter().any(|entry| entry.path == name) {
                let path = root.join(name);
                Some((
                    path.clone(),
                    crate::store::segment::Segment::open(&path, Some(&self.cache))?,
                ))
            } else {
                None
            };
            self.sections.insert(root.clone(), index);
        }
        let Some((index, segment)) = &self.sections[&root] else {
            return Ok(None);
        };
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("Invalid indexed source path")?;
        match segment.file(relative, profile) {
            Ok(ordinal) => Ok(ordinal.map(|ordinal| (index.clone(), ordinal))),
            Err(error) if error.is::<crate::extract::preprocess::InvalidDirectives>() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

use crate::{
    binary,
    store::{ObjectId, shared::Encoded},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub const REPOSITORY: &str = "apkd/sigla-metadata";
const TAG: &str = "metadata-archive";
const MAGIC: &[u8] = b"SIGLABIN";
const HEADER: usize = 8 + 64 + 32;
const MAX_RECORD: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub revision: String,
    pub url: String,
    pub integrity: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub minimum_editor: Option<String>,
    pub dependencies: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub group: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub origin: Origin,
    pub entries: Vec<Entry>,
    pub packages: Vec<Package>,
    pub package_names: Vec<String>,
    pub recommended: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub release: String,
    pub manifest: Manifest,
    pub sections: Vec<Section>,
    pub header_size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Section {
    pub role: String,
    /// Compressed range, relative to the end of the container header.
    pub offset: u64,
    pub size: u64,
    pub hash: [u8; 32],
    pub manifest_hash: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pack {
    pub name: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub release: String,
    pub members: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub artifacts: Vec<Artifact>,
    pub unavailable: Vec<String>,
    pub packs: Vec<Pack>,
}

impl Catalog {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = b"SIGLACAT3".to_vec();
        bytes.extend(binary::encode(self)?);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        binary::decode(
            bytes
                .strip_prefix(b"SIGLACAT3")
                .context("Unsupported metadata catalog format")?,
        )
    }
}
#[derive(Serialize, Deserialize)]
pub struct Analysis {
    pub id: ObjectId,
    pub encoded: Encoded,
}

struct IndexReference {
    id: ObjectId,
}

impl IndexReference {
    const SIZE: usize = 40;
    fn encode(&self) -> Vec<u8> {
        let mut bytes = b"SIGLAREF".to_vec();
        bytes.extend(self.id);
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == Self::SIZE && bytes.starts_with(b"SIGLAREF"),
            "Invalid immutable assembly reference"
        );
        Ok(Self {
            id: bytes[8..].try_into()?,
        })
    }
}

fn envelope(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(crate::store::ANALYSIS_VERSION.as_bytes());
    bytes.extend_from_slice(blake3::hash(payload).as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}
fn payload(bytes: &[u8]) -> Result<&[u8]> {
    ensure!(
        bytes.len() >= HEADER && bytes.starts_with(MAGIC),
        "Not a Sigla binary record"
    );
    ensure!(
        &bytes[8..72] == crate::store::ANALYSIS_VERSION.as_bytes(),
        "Metadata archive is incompatible with this Sigla binary; update Sigla and rebuild the archive with the same release"
    );
    let payload = &bytes[HEADER..];
    ensure!(
        blake3::hash(payload).as_bytes() == &bytes[72..HEADER],
        "Corrupt Sigla binary record"
    );
    Ok(payload)
}
fn read(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD,
        "Binary record exceeds size bound"
    );
    Ok(bytes)
}
fn save(path: &Path, payload: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("Output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut stage = tempfile::NamedTempFile::new_in(parent)?;
    stage.write_all(&envelope(payload))?;
    stage.as_file().sync_all()?;
    stage.persist(path)?;
    Ok(())
}
fn safe(path: &str) -> Result<()> {
    crate::repository::selection::validate_path(path)?;
    ensure!(
        !path.contains('\\') && !path.is_empty(),
        "Invalid archive path"
    );
    Ok(())
}
fn hex(hash: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*hash).to_hex().to_string()
}
fn digest(path: &Path) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    hash.update_reader(fs::File::open(path)?)?;
    Ok(*hash.finalize().as_bytes())
}

pub(crate) fn assembly_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "sigla")
}
pub(crate) fn reference(path: PathBuf) -> PathBuf {
    if path.is_file() {
        return path;
    }
    let archived = path.with_extension("sigla");
    if archived.is_file() { archived } else { path }
}
pub(crate) fn logical_filename(path: &Path) -> String {
    if assembly_path(path) {
        path.with_extension("dll")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    } else {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}
pub(crate) fn load_analysis(path: &Path) -> Result<Analysis> {
    let bytes = read(path)?;
    ensure!(
        !bytes.starts_with(b"SIGLAREF"),
        "Assembly requires its verified metadata section index"
    );
    binary::decode(payload(&bytes)?)
}
pub(crate) fn manifest(path: &Path) -> Result<Manifest> {
    binary::decode(payload(&read(path)?)?)
}
pub(crate) fn source_analysis(path: &Path, id: &ObjectId) -> Result<Option<Encoded>> {
    let sidecar = PathBuf::from(format!("{}.analysis", path.display()));
    if !sidecar.is_file() {
        return Ok(None);
    }
    let bytes = read(&sidecar)?;
    let records = binary::decode::<Vec<(ObjectId, &[u8])>>(payload(&bytes)?)?;
    records
        .iter()
        .find(|record| &record.0 == id)
        .map(|record| binary::decode(record.1))
        .transpose()
}
