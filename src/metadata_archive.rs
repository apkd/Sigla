//! Offline production and online consumption of the same immutable analysis records.
mod bundle;
mod client;
mod producer;
mod registry;
mod release;
#[cfg(test)]
mod tests;

pub(crate) use client::configured;
pub(crate) use client::editor;
pub use client::{package, prefetch};
pub use producer::Command;

use crate::{
    binary,
    store::{ObjectId, shared::Encoded},
};
use anyhow::{Context, Result, ensure};
use rkyv::{Archive, Deserialize, Serialize};
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

#[derive(Clone, Debug, Archive, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub kind: String,
    pub name: String,
    pub version: String,
    pub revision: String,
    pub url: String,
    pub integrity: Option<String>,
}
#[derive(Clone, Debug, Archive, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub minimum_editor: Option<String>,
    pub dependencies: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Archive, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub group: String,
}
#[derive(Clone, Debug, Archive, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub origin: Origin,
    pub entries: Vec<Entry>,
    pub packages: Vec<Package>,
    pub package_names: Vec<String>,
    pub recommended: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Archive, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub release: String,
    pub manifest: Manifest,
}
#[derive(Clone, Debug, Default, Archive, Serialize, Deserialize)]
pub struct Catalog {
    pub artifacts: Vec<Artifact>,
    pub unavailable: Vec<String>,
}
#[derive(Archive, Serialize, Deserialize)]
pub struct Analysis {
    pub id: ObjectId,
    pub encoded: Encoded,
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
    let records = binary::view::<Vec<Analysis>>(payload(&bytes)?)?;
    records
        .iter()
        .find(|record| record.id == *id)
        .map(|record| {
            Ok(rkyv::deserialize::<Encoded, rkyv::rancor::Error>(
                &record.encoded,
            )?)
        })
        .transpose()
}
