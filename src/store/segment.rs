//! Portable immutable analysis tables. Offsets and ordinals stay inside storage.
use super::{ObjectId, format, payload, shared::Encoded};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAGIC: &[u8; 8] = b"SIGLAIX1";
const HEADER: usize = 92;
const OBJECT: usize = 64;
const ROW: usize = 28;

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
pub struct Mount {
    pub path: PathBuf,
    pub files: BTreeMap<String, u32>,
}

#[derive(Default)]
pub(super) struct Mounted {
    pub files: BTreeMap<String, (usize, u32)>,
    segments: Vec<(Arc<Segment>, Aliases)>,
}

type Aliases = BTreeMap<u32, Vec<String>>;

impl Mounted {
    pub fn open(mounts: &[Mount], cache: &Path) -> Result<Self> {
        let mut result = Self::default();
        for mount in mounts {
            let segment = Segment::open(&mount.path, Some(cache))?;
            let slot = result.segments.len();
            let mut members = BTreeMap::<_, Vec<_>>::new();
            for (file, ordinal) in &mount.files {
                segment.object(*ordinal)?;
                ensure!(
                    result
                        .files
                        .insert(file.clone(), (slot, *ordinal))
                        .is_none(),
                    "File appears in multiple immutable indexes"
                );
                members.entry(*ordinal).or_default().push(file.clone());
            }
            result.segments.push((segment, members));
        }
        Ok(result)
    }
    pub fn record(&self, file: &str, suffix: &[u8]) -> Result<Option<&[u8]>> {
        let Some((slot, ordinal)) = self.files.get(file) else {
            return Ok(None);
        };
        self.segments[*slot].0.record(*ordinal, suffix)
    }
    pub fn object(&self, file: &str) -> Result<Option<ObjectId>> {
        self.files
            .get(file)
            .map(|(slot, ordinal)| self.segments[*slot].0.object(*ordinal))
            .transpose()
    }
    pub fn candidates(
        &self,
        name: &str,
        loose: bool,
        occurrences: bool,
        result: &mut BTreeSet<String>,
    ) -> Result<()> {
        for (segment, members) in &self.segments {
            segment.candidates(
                name,
                !loose && !name.contains('*'),
                occurrences,
                |candidate| crate::query::name_rank(name, candidate, loose).is_some(),
                |ordinal| {
                    if let Some(files) = members.get(&ordinal) {
                        result.extend(files.iter().cloned());
                    }
                },
            )?;
        }
        Ok(())
    }
    pub fn globals(&self, result: &mut BTreeSet<String>) -> Result<()> {
        for (segment, members) in &self.segments {
            segment.globals(|ordinal| {
                if let Some(files) = members.get(&ordinal) {
                    result.extend(files.iter().cloned());
                }
            })?;
        }
        Ok(())
    }
    pub fn retained_bytes(&self) -> usize {
        self.files
            .keys()
            .map(|file| file.len() * 2 + 160)
            .sum::<usize>()
            + self
                .segments
                .iter()
                .map(|(_, members)| members.len() * 96 + 128)
                .sum::<usize>()
    }
}

pub(crate) struct Builder {
    body: tempfile::NamedTempFile,
    objects: Vec<(ObjectId, [u8; 32])>,
    ids: BTreeMap<ObjectId, u32>,
    rows: BTreeMap<Vec<u8>, (u64, u64)>,
    postings: BTreeMap<Vec<u8>, BTreeSet<u32>>,
}

fn key(tag: u8, suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + suffix.len());
    key.push(tag);
    key.extend_from_slice(suffix);
    key
}

impl Builder {
    pub fn new(directory: &Path) -> Result<Self> {
        Ok(Self {
            body: tempfile::NamedTempFile::new_in(directory)?,
            objects: Vec::new(),
            ids: BTreeMap::new(),
            rows: BTreeMap::new(),
            postings: BTreeMap::new(),
        })
    }
    fn row(&mut self, key: Vec<u8>, bytes: &[u8]) -> Result<()> {
        ensure!(
            !self.rows.contains_key(&key),
            "Duplicate immutable index key"
        );
        let offset = self.body.stream_position()?;
        self.body.write_all(bytes)?;
        self.rows.insert(key, (offset, bytes.len() as u64));
        Ok(())
    }
    pub fn object(&mut self, id: ObjectId, encoded: &Encoded) -> Result<u32> {
        if let Some(ordinal) = self.ids.get(&id) {
            return Ok(*ordinal);
        }
        payload::validate_encoded(encoded)?;
        let ordinal =
            u32::try_from(self.objects.len()).context("Too many immutable analysis objects")?;
        let environment = encoded
            .records
            .get(payload::ENVIRONMENT)
            .context("Missing declaration fingerprint")?
            .as_slice()
            .try_into()?;
        self.objects.push((id, environment));
        self.ids.insert(id, ordinal);
        self.row(key(b'I', &id), &ordinal.to_le_bytes())?;
        self.row(key(b'N', &ordinal.to_be_bytes()), &encoded.names)?;
        for (suffix, bytes) in &encoded.records {
            if suffix == payload::ENVIRONMENT {
                continue;
            }
            let mut record = key(b'R', &ordinal.to_be_bytes());
            record.extend_from_slice(suffix);
            self.row(record, bytes)?;
        }
        let names = format::names(&encoded.names, |key| {
            Ok(encoded.records.get(key).map(Vec::as_slice))
        })?;
        for (tag, names) in [(b'D', names.declarations), (b'U', names.occurrences)] {
            for name in names {
                self.postings
                    .entry(key(tag, payload::posting_term(&name).as_bytes()))
                    .or_default()
                    .insert(ordinal);
            }
        }
        if names.global_imports {
            self.postings.entry(vec![b'G']).or_default().insert(ordinal);
        }
        Ok(ordinal)
    }
    pub fn file(&mut self, path: &str, ordinals: &[u32], invariant: bool) -> Result<()> {
        ensure!(
            !ordinals.is_empty() && (!invariant || ordinals.len() == 1),
            "Invalid file analysis profiles"
        );
        let mut bytes = vec![u8::from(invariant)];
        for ordinal in ordinals {
            bytes.extend_from_slice(&ordinal.to_le_bytes());
        }
        self.row(key(b'P', path.as_bytes()), &bytes)
    }
    pub fn finish(self, output: &mut File) -> Result<()> {
        let mut output = std::io::BufWriter::new(output);
        self.write_to(&mut output)?;
        output.flush()?;
        Ok(())
    }

    fn write_to(mut self, output: &mut impl Write) -> Result<()> {
        for (key, ordinals) in std::mem::take(&mut self.postings) {
            self.row(
                key,
                &ordinals
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )?;
        }
        let body_start = HEADER as u64 + self.objects.len() as u64 * OBJECT as u64;
        let keys_start = body_start + self.body.as_file().metadata()?.len();
        let index_start = keys_start + self.rows.keys().map(|key| key.len() as u64).sum::<u64>();
        output.write_all(MAGIC)?;
        output.write_all(payload::ANALYSIS_VERSION.as_bytes())?;
        output.write_all(&(self.objects.len() as u32).to_le_bytes())?;
        output.write_all(&(self.rows.len() as u64).to_le_bytes())?;
        output.write_all(&index_start.to_le_bytes())?;
        for (id, environment) in self.objects {
            output.write_all(&id)?;
            output.write_all(&environment)?;
        }
        std::io::copy(&mut File::open(self.body.path())?, output)?;
        for key in self.rows.keys() {
            output.write_all(key)?;
        }
        let mut key_offset = keys_start;
        for (key, (offset, size)) in self.rows {
            output.write_all(&key_offset.to_le_bytes())?;
            output.write_all(&u32::try_from(key.len())?.to_le_bytes())?;
            output.write_all(&(body_start + offset).to_le_bytes())?;
            output.write_all(&size.to_le_bytes())?;
            key_offset += key.len() as u64;
        }
        Ok(())
    }
}

pub(crate) struct Segment {
    bytes: Bytes,
    objects: usize,
    rows: usize,
    index: usize,
    keys: usize,
    _lease: Option<crate::cache::views::LeaseSet>,
}

enum Bytes {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

fn number<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    Ok(bytes
        .get(offset..offset.checked_add(N).context("Index offset overflow")?)
        .context("Truncated immutable index")?
        .try_into()?)
}

impl Segment {
    pub fn compatible(path: &Path, size: u64) -> Result<bool> {
        use std::io::Read;
        let mut file = File::open(path)?;
        if file.metadata()?.len() != size {
            return Ok(false);
        }
        let mut header = [0; HEADER];
        file.read_exact(&mut header)?;
        let objects = u32::from_le_bytes(number(&header, 72)?) as u64;
        let rows = u64::from_le_bytes(number(&header, 76)?);
        let index = u64::from_le_bytes(number(&header, 84)?);
        let valid = &header[..8] == MAGIC
            && &header[8..72] == payload::ANALYSIS_VERSION.as_bytes()
            && HEADER as u64 + objects * OBJECT as u64 <= index
            && rows
                .checked_mul(ROW as u64)
                .and_then(|n| index.checked_add(n))
                == Some(size);
        if !valid || rows == 0 {
            return Ok(valid);
        }
        file.seek(std::io::SeekFrom::Start(index))?;
        let mut first = [0; 8];
        file.read_exact(&mut first)?;
        let keys = u64::from_le_bytes(first);
        Ok(HEADER as u64 + objects * OBJECT as u64 <= keys && keys <= index)
    }
    pub fn open(path: &Path, cache: Option<&Path>) -> Result<Arc<Self>> {
        static OPEN: std::sync::LazyLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<Segment>>>,
        > = std::sync::LazyLock::new(Default::default);
        let lease = cache.map(crate::cache::views::LeaseSet::new).transpose()?;
        if let Some(lease) = &lease {
            lease.retain(path)?;
        }
        let file = File::open(path)?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() <= 1024 * 1024 * 1024,
            "Invalid immutable index file"
        );
        if cache.is_some()
            && let Some(segment) = OPEN
                .lock()
                .unwrap()
                .get(path)
                .and_then(std::sync::Weak::upgrade)
        {
            return Ok(segment);
        }
        let bytes = if let Some(cache) = cache {
            ensure!(
                path.starts_with(cache.join("unity-metadata")),
                "Index is outside immutable metadata storage"
            );
            // Metadata publication only renames stopped, verified trees. The lease
            // protects the backing files until the last reader releases this map.
            Bytes::Mapped(unsafe { memmap2::MmapOptions::new().map(&file)? })
        } else {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024 * 1024,
                "Index exceeds size bound"
            );
            Bytes::Owned(bytes)
        };
        let segment = Self::parse(bytes, lease)?;
        if cache.is_some() {
            let mut open = OPEN.lock().unwrap();
            if let Some(existing) = open.get(path).and_then(std::sync::Weak::upgrade) {
                return Ok(existing);
            }
            if open.len() == open.capacity() {
                open.retain(|_, segment| segment.strong_count() > 0);
            }
            open.insert(path.into(), Arc::downgrade(&segment));
        }
        Ok(segment)
    }
    /// The caller owns this private file and must prevent writes until the map drops.
    pub unsafe fn staged(path: &Path) -> Result<Arc<Self>> {
        let file = File::open(path)?;
        ensure!(
            file.metadata()?.len() <= 1024 * 1024 * 1024,
            "Index exceeds size bound"
        );
        let bytes = Bytes::Mapped(unsafe { memmap2::MmapOptions::new().map(&file)? });
        Self::parse(bytes, None)
    }
    fn parse(bytes: Bytes, lease: Option<crate::cache::views::LeaseSet>) -> Result<Arc<Self>> {
        ensure!(
            bytes.get(..8) == Some(MAGIC)
                && bytes.get(8..72) == Some(payload::ANALYSIS_VERSION.as_bytes()),
            "Incompatible immutable analysis index"
        );
        let objects = u32::from_le_bytes(number(&bytes, 72)?) as usize;
        let rows = usize::try_from(u64::from_le_bytes(number(&bytes, 76)?))?;
        let index = usize::try_from(u64::from_le_bytes(number(&bytes, 84)?))?;
        ensure!(
            HEADER
                .checked_add(objects.checked_mul(OBJECT).context("Index size overflow")?)
                .is_some_and(|n| n <= index)
                && index.checked_add(rows.checked_mul(ROW).context("Index size overflow")?)
                    == Some(bytes.len()),
            "Invalid immutable index directory"
        );
        let keys = if rows == 0 {
            index
        } else {
            usize::try_from(u64::from_le_bytes(number(&bytes, index)?))?
        };
        ensure!(
            HEADER + objects * OBJECT <= keys && keys <= index,
            "Invalid immutable key extent"
        );
        Ok(Arc::new(Self {
            bytes,
            objects,
            rows,
            index,
            keys,
            _lease: lease,
        }))
    }
    fn slice(&self, offset: u64, size: u64) -> Result<&[u8]> {
        let offset = usize::try_from(offset)?;
        let end = offset
            .checked_add(usize::try_from(size)?)
            .context("Index range overflow")?;
        ensure!(
            offset >= HEADER + self.objects * OBJECT && end <= self.index,
            "Immutable index range overlaps its directory"
        );
        self.bytes
            .get(offset..end)
            .context("Immutable index range is outside the file")
    }
    fn row(&self, row: usize) -> Result<(&[u8], &[u8])> {
        ensure!(row < self.rows, "Immutable index row is out of range");
        let at = self.index + row * ROW;
        let key_offset = u64::from_le_bytes(number(&self.bytes, at)?);
        let value_offset = u64::from_le_bytes(number(&self.bytes, at + 12)?);
        let value_size = u64::from_le_bytes(number(&self.bytes, at + 20)?);
        ensure!(
            key_offset >= self.keys as u64
                && value_offset
                    .checked_add(value_size)
                    .is_some_and(|end| end <= self.keys as u64),
            "Invalid immutable key/value range"
        );
        Ok((
            self.slice(
                key_offset,
                u32::from_le_bytes(number(&self.bytes, at + 8)?) as u64,
            )?,
            self.slice(value_offset, value_size)?,
        ))
    }
    fn lower_bound(&self, key: &[u8]) -> Result<usize> {
        let (mut low, mut high) = (0, self.rows);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.row(mid)?.0 < key {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }
    fn get(&self, key: &[u8]) -> Result<Option<&[u8]>> {
        let row = self.lower_bound(key)?;
        if row == self.rows {
            return Ok(None);
        }
        let (found, value) = self.row(row)?;
        Ok((found == key).then_some(value))
    }
    fn visit(
        &self,
        prefix: &[u8],
        mut visitor: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        for row in self.lower_bound(prefix)?..self.rows {
            let (key, value) = self.row(row)?;
            let Some(suffix) = key.strip_prefix(prefix) else {
                break;
            };
            visitor(suffix, value)?;
        }
        Ok(())
    }
    pub fn object(&self, ordinal: u32) -> Result<ObjectId> {
        ensure!(
            (ordinal as usize) < self.objects,
            "Immutable object ordinal is out of range"
        );
        number(&self.bytes, HEADER + ordinal as usize * OBJECT)
    }
    pub fn record(&self, ordinal: u32, suffix: &[u8]) -> Result<Option<&[u8]>> {
        self.object(ordinal)?;
        if suffix == payload::ENVIRONMENT {
            let at = HEADER + ordinal as usize * OBJECT + 32;
            return Ok(Some(&self.bytes[at..at + 32]));
        }
        let mut key = key(b'R', &ordinal.to_be_bytes());
        key.extend_from_slice(suffix);
        self.get(&key)
    }
    pub fn source(&self, ordinal: u32) -> Result<&str> {
        payload::source(
            self.record(ordinal, payload::DATA)?
                .context("Indexed source has no text")?,
        )
    }
    #[cfg(test)]
    pub fn find(&self, id: &ObjectId) -> Result<Option<u32>> {
        self.get(&key(b'I', id))?
            .map(|bytes| Ok(u32::from_le_bytes(bytes.try_into()?)))
            .transpose()
    }
    pub fn file(
        &self,
        path: &str,
        profile: impl FnOnce() -> Result<ObjectId>,
    ) -> Result<Option<u32>> {
        let Some(bytes) = self.get(&key(b'P', path.as_bytes()))? else {
            return Ok(None);
        };
        let (&invariant, variants) = bytes
            .split_first()
            .context("Missing immutable file profile")?;
        ensure!(
            !variants.is_empty()
                && variants.len().is_multiple_of(4)
                && invariant <= 1
                && (invariant == 0 || variants.len() == 4),
            "Invalid immutable file profiles"
        );
        if invariant == 1 {
            return Ok(Some(u32::from_le_bytes(variants[..4].try_into()?)));
        }
        let wanted = profile()?;
        for ordinal in variants.as_chunks::<4>().0 {
            let ordinal = u32::from_le_bytes(*ordinal);
            if self.object(ordinal)? == wanted {
                return Ok(Some(ordinal));
            }
        }
        Ok(None)
    }
    pub fn encoded(&self, ordinal: u32) -> Result<Encoded> {
        let names = self
            .get(&key(b'N', &ordinal.to_be_bytes()))?
            .context("Missing immutable posting names")?
            .to_vec();
        let mut records = BTreeMap::new();
        self.visit(&key(b'R', &ordinal.to_be_bytes()), |key, value| {
            records.insert(key.to_vec(), value.to_vec());
            Ok(())
        })?;
        records.insert(
            payload::ENVIRONMENT.to_vec(),
            self.record(ordinal, payload::ENVIRONMENT)?
                .unwrap()
                .to_vec(),
        );
        Ok(Encoded { names, records })
    }
    pub fn files(&self, mut visit: impl FnMut(&str, bool, u32) -> Result<()>) -> Result<()> {
        self.visit(b"P", |path, bytes| {
            let path = std::str::from_utf8(path)?;
            ensure!(
                bytes.len() > 1 && bytes[0] <= 1 && bytes[1..].len().is_multiple_of(4),
                "Invalid immutable file profile"
            );
            let invariant = bytes[0] == 1;
            for bytes in bytes[1..].as_chunks::<4>().0 {
                visit(path, invariant, u32::from_le_bytes(*bytes))?;
            }
            Ok(())
        })
    }
    fn ordinals(&self, bytes: &[u8], mut visit: impl FnMut(u32)) -> Result<()> {
        ensure!(bytes.len().is_multiple_of(4), "Truncated immutable posting");
        for bytes in bytes.as_chunks::<4>().0 {
            let ordinal = u32::from_le_bytes(*bytes);
            self.object(ordinal)?;
            visit(ordinal);
        }
        Ok(())
    }
    pub fn candidates(
        &self,
        name: &str,
        exact: bool,
        occurrences: bool,
        matches: impl Fn(&str) -> bool,
        mut visit: impl FnMut(u32),
    ) -> Result<()> {
        let tag = if occurrences { b'U' } else { b'D' };
        if exact {
            if let Some(bytes) = self.get(&key(tag, payload::posting_term(name).as_bytes()))? {
                self.ordinals(bytes, visit)?;
            }
        } else {
            self.visit(&[tag], |name, bytes| {
                let name = std::str::from_utf8(name)?;
                if name == payload::OVERSIZED_TERM || matches(name) {
                    self.ordinals(bytes, &mut visit)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
    pub fn globals(&self, visit: impl FnMut(u32)) -> Result<()> {
        if let Some(bytes) = self.get(b"G")? {
            self.ordinals(bytes, visit)?;
        }
        Ok(())
    }
    /// Validate once in private staging, before the section becomes immutable.
    pub fn validate(&self, semantics: bool) -> Result<()> {
        let mut previous = None;
        let mut key_end = self.keys;
        let mut value_bytes = 0usize;
        let (mut identities, mut names, mut roots) = (0, 0, 0);
        for index in 0..self.rows {
            let (key, value) = self.row(index)?;
            ensure!(
                u64::from_le_bytes(number(&self.bytes, self.index + index * ROW)?)
                    == key_end as u64,
                "Noncontiguous immutable keys"
            );
            key_end = key_end
                .checked_add(key.len())
                .context("Key extent overflow")?;
            value_bytes = value_bytes
                .checked_add(value.len())
                .context("Value extent overflow")?;
            ensure!(
                value_bytes <= self.keys - HEADER - self.objects * OBJECT,
                "Immutable values exceed body extent"
            );
            ensure!(
                previous.is_none_or(|previous: &[u8]| previous < key),
                "Unsorted immutable index"
            );
            previous = Some(key);
            match key.first() {
                Some(b'D' | b'U') => {
                    ensure!(key.len() > 1, "Empty posting name");
                    std::str::from_utf8(&key[1..])?;
                    self.posting(value)?;
                }
                Some(b'G') => {
                    ensure!(key.len() == 1, "Invalid global posting key");
                    self.posting(value)?;
                }
                Some(b'I') => {
                    ensure!(
                        key.len() == 33 && value.len() == 4,
                        "Invalid object directory row"
                    );
                    ensure!(
                        self.object(u32::from_le_bytes(value.try_into()?))?
                            .as_slice()
                            == &key[1..],
                        "Object directory mismatch"
                    );
                    identities += 1;
                }
                Some(b'N' | b'R') => {
                    ensure!(
                        if key[0] == b'N' {
                            key.len() == 5
                        } else {
                            key.len() > 5 && &key[5..] != payload::ENVIRONMENT
                        },
                        "Invalid object record key"
                    );
                    self.object(u32::from_be_bytes(key[1..5].try_into()?))?;
                    names += usize::from(key[0] == b'N');
                    roots += usize::from(key[0] == b'R' && &key[5..] == payload::DATA);
                }
                Some(b'P') => {
                    crate::repository::selection::validate_path(std::str::from_utf8(&key[1..])?)?;
                    ensure!(
                        value.len() > 1 && value[0] <= 1 && (value[0] == 0 || value.len() == 5),
                        "Invalid file profile"
                    );
                    self.ordinals(&value[1..], |_| {})?;
                }
                _ => anyhow::bail!("Unknown immutable index table"),
            }
        }
        ensure!(
            key_end == self.index && value_bytes == self.keys - HEADER - self.objects * OBJECT,
            "Immutable table extents disagree"
        );
        ensure!(
            identities == self.objects && names == self.objects && roots == self.objects,
            "Missing immutable object records"
        );
        let mut postings = BTreeMap::<Vec<u8>, BTreeSet<u32>>::new();
        if semantics {
            for ordinal in 0..self.objects as u32 {
                let encoded = self.encoded(ordinal)?;
                payload::validate_encoded(&encoded)?;
                let names = format::names(&encoded.names, |key| {
                    Ok(encoded.records.get(key).map(Vec::as_slice))
                })?;
                for (tag, names) in [(b'D', names.declarations), (b'U', names.occurrences)] {
                    for name in names {
                        postings
                            .entry(key(tag, payload::posting_term(&name).as_bytes()))
                            .or_default()
                            .insert(ordinal);
                    }
                }
                if names.global_imports {
                    postings.entry(vec![b'G']).or_default().insert(ordinal);
                }
            }
            for tag in *b"DUG" {
                self.visit(&[tag], |suffix, bytes| {
                    let expected = postings
                        .remove(&key(tag, suffix))
                        .context("Unexpected immutable posting")?;
                    ensure!(
                        expected
                            .into_iter()
                            .flat_map(u32::to_le_bytes)
                            .eq(bytes.iter().copied()),
                        "Immutable postings disagree with analysis records"
                    );
                    Ok(())
                })?;
            }
            ensure!(postings.is_empty(), "Missing immutable posting");
        }
        Ok(())
    }
    fn posting(&self, bytes: &[u8]) -> Result<()> {
        ensure!(!bytes.is_empty(), "Empty immutable posting");
        let mut previous = None;
        let mut ordered = true;
        self.ordinals(bytes, |ordinal| {
            ordered &= previous.is_none_or(|previous| previous < ordinal);
            previous = Some(ordinal);
        })?;
        ensure!(ordered, "Unsorted immutable posting");
        Ok(())
    }
}
