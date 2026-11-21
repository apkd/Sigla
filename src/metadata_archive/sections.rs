//! One release asset containing independently authenticated compressed sections.
use super::*;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
};

const MAGIC: &[u8; 8] = b"SIGLASE1";
const MAX_HEADER: u64 = 4 * 1024 * 1024;
pub(super) const OBJECT_MODE: u32 = 0o600;

#[derive(Serialize, Deserialize)]
struct Header {
    manifest: Manifest,
    sections: Vec<Section>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Frame {
    pub role: String,
    pub entries: Vec<Entry>,
    pub package: Option<Package>,
}

pub(super) fn object_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hash = crate::cache::file_id::hasher(OBJECT_MODE);
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

pub(super) fn index_path(role: &str) -> String {
    format!(
        "indexes/{}.sigla-index",
        blake3::hash(role.as_bytes()).to_hex()
    )
}

fn indexed(role: &str, entries: Vec<Entry>, spool: &Path) -> Result<Vec<Entry>> {
    let mut builder = crate::store::segment::Builder::new(spool)?;
    let sources: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut result = Vec::new();
    let mut objects = false;
    for entry in &entries {
        if entry.path.ends_with(".sigla") {
            let bytes = read(&spool.join(hex(&entry.hash)))?;
            let analysis: Analysis = binary::decode(payload(&bytes)?)?;
            let ordinal = builder.object(analysis.id, &analysis.encoded)?;
            builder.file(&entry.path, &[ordinal], true)?;
            let reference = IndexReference { id: analysis.id };
            let bytes = reference.encode();
            let hash = object_hash(&bytes);
            fs::write(spool.join(hex(&hash)), &bytes)?;
            result.push(Entry {
                hash,
                size: bytes.len() as u64,
                ..entry.clone()
            });
            objects = true;
        } else if let Some(path) = entry.path.strip_suffix(".cs.analysis") {
            let path = format!("{path}.cs");
            let bytes = read(&spool.join(hex(&entry.hash)))?;
            let mut ordinals = Vec::new();
            for (id, bytes) in binary::decode::<Vec<(ObjectId, &[u8])>>(payload(&bytes)?)? {
                ordinals.push(builder.object(id, &binary::decode(bytes)?)?);
            }
            if !ordinals.is_empty() {
                let source = sources
                    .get(path.as_str())
                    .context("Analysis is missing its source")?;
                let source = crate::model::decode(
                    &read(&spool.join(hex(&source.hash)))?,
                    crate::model::Language::CSharp,
                )?;
                builder.file(&path, &ordinals, !source.contains('#'))?;
                objects = true;
            }
        } else {
            result.push(entry.clone());
        }
    }
    if objects {
        let mut file = tempfile::NamedTempFile::new_in(spool)?;
        builder.finish(file.as_file_mut())?;
        let size = file.as_file().metadata()?.len();
        ensure!(
            size <= MAX_RECORD,
            "Immutable section index exceeds size bound"
        );
        let mut hash = crate::cache::file_id::hasher(OBJECT_MODE);
        hash.update_reader(fs::File::open(file.path())?)?;
        let hash = *hash.finalize().as_bytes();
        file.persist(spool.join(hex(&hash)))?;
        result.push(Entry {
            path: index_path(role),
            hash,
            size,
            group: "index".into(),
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

pub(super) fn role(entry: &Entry, groups: &BTreeMap<&str, &str>) -> Result<String> {
    for prefix in [
        "packages/",
        "Editor/Data/Resources/PackageManager/BuiltInPackages/",
    ] {
        if let Some(relative) = entry.path.strip_prefix(prefix) {
            let (name, _) = relative
                .split_once('/')
                .context("Invalid package file path")?;
            return Ok(format!("package/{name}"));
        }
    }
    if entry.path.ends_with(".sigla")
        && let Some(relative) = entry.path.strip_prefix("Editor/Data/")
        && !crate::unity::references::default_reference(&entry.group, Path::new(relative))
    {
        return Ok(assembly_role(&entry.group, &entry.path));
    }
    Ok(if entry.group == "analysis" {
        *groups
            .get(
                entry
                    .path
                    .strip_suffix(".analysis")
                    .context("Invalid analysis path")?,
            )
            .context("Analysis has no source file")?
    } else {
        &entry.group
    }
    .to_owned())
}

pub(super) fn assembly_role(group: &str, path: &str) -> String {
    format!("assembly/{group}/{path}")
}

pub(super) fn validate(sections: &[Section], header_size: u64, size: u64) -> Result<()> {
    ensure!(
        (16..=MAX_HEADER + 16).contains(&header_size),
        "Invalid metadata header size"
    );
    ensure!(
        !sections.is_empty() && sections.len() <= 4096,
        "Invalid metadata section count"
    );
    let mut end = 0u64;
    let mut roles = BTreeSet::new();
    for section in sections {
        safe(&section.role)?;
        ensure!(
            roles.insert(&section.role) && section.offset == end && section.size > 0,
            "Invalid metadata section directory"
        );
        end = end
            .checked_add(section.size)
            .context("Metadata section range overflow")?;
    }
    ensure!(
        header_size.checked_add(end) == Some(size),
        "Metadata sections do not cover the asset"
    );
    Ok(())
}

pub(super) fn write(mut manifest: Manifest, spool: &Path, output: &Path) -> Result<Artifact> {
    let groups: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.group.as_str()))
        .collect();
    let mut selected = BTreeMap::<String, Vec<Entry>>::new();
    for entry in &manifest.entries {
        selected
            .entry(role(entry, &groups)?)
            .or_default()
            .push(entry.clone());
    }
    if selected.is_empty() {
        selected.insert("empty".into(), Vec::new());
    }
    let mut frames = Vec::new();
    let mut sections = Vec::new();
    let mut offset = 0u64;
    for (role, entries) in selected {
        let frame = Frame {
            role: role.clone(),
            entries: indexed(&role, entries, spool)?,
            package: role
                .strip_prefix("package/")
                .and_then(|name| {
                    manifest
                        .packages
                        .iter()
                        .find(|package| package.name == name)
                })
                .cloned(),
        };
        let bytes = envelope(&binary::encode(&frame)?);
        let mut file = tempfile::NamedTempFile::new_in(spool)?;
        {
            let mut compressed = zstd::stream::write::Encoder::new(file.as_file_mut(), 22)?;
            compressed.window_log(27)?;
            compressed.long_distance_matching(true)?;
            let mut tar = tar::Builder::new(compressed);
            fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, name, bytes)?;
                Ok(())
            }
            append(&mut tar, "section.bin", &bytes)?;
            for id in frame
                .entries
                .iter()
                .map(|entry| entry.hash)
                .collect::<BTreeSet<_>>()
            {
                let file = fs::File::open(spool.join(hex(&id)))?;
                let mut header = tar::Header::new_gnu();
                header.set_size(file.metadata()?.len());
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, format!("objects/{}", hex(&id)), file)?;
            }
            tar.into_inner()?.finish()?;
        }
        let size = file.as_file().metadata()?.len();
        sections.push(Section {
            role,
            offset,
            size,
            hash: digest(file.path())?,
            manifest_hash: *blake3::hash(&bytes).as_bytes(),
        });
        offset = offset
            .checked_add(size)
            .context("Metadata asset size overflow")?;
        frames.push(file);
    }
    manifest.entries.clear();
    let header = envelope(&binary::encode(&Header {
        manifest: manifest.clone(),
        sections: sections.clone(),
    })?);
    ensure!(
        header.len() as u64 <= MAX_HEADER,
        "Metadata header exceeds size bound"
    );
    let name = format!(
        "{}-{}.sigla",
        manifest.origin.kind,
        blake3::hash(&header).to_hex()
    );
    let path = output.join(&name);
    let mut file = tempfile::NamedTempFile::new_in(output)?;
    file.write_all(MAGIC)?;
    file.write_all(&(header.len() as u64).to_le_bytes())?;
    file.write_all(&header)?;
    for frame in frames {
        std::io::copy(&mut fs::File::open(frame.path())?, &mut file)?;
    }
    file.as_file().sync_all()?;
    file.persist(&path)?;
    let artifact = Artifact {
        name,
        hash: digest(&path)?,
        size: fs::metadata(&path)?.len(),
        release: TAG.into(),
        manifest,
        sections,
        header_size: 16 + header.len() as u64,
    };
    bundle::verify(&path, Some(&artifact))?;
    save(
        &path.with_extension("sigla.bin"),
        &binary::encode(&artifact)?,
    )?;
    Ok(artifact)
}

fn header(reader: &mut impl Read, expected: Option<&Artifact>) -> Result<Header> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "Unsupported metadata container");
    let mut length = [0; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    ensure!(length <= MAX_HEADER, "Metadata header exceeds size bound");
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes)?;
    let header: Header = binary::decode(payload(&bytes)?)?;
    ensure!(
        header.manifest.entries.is_empty(),
        "Container header contains file inventories"
    );
    let size = header
        .sections
        .last()
        .and_then(|section| section.offset.checked_add(section.size))
        .and_then(|size| size.checked_add(16 + length))
        .context("Invalid metadata section extent")?;
    validate(&header.sections, 16 + length, size)?;
    if let Some(expected) = expected {
        ensure!(
            expected.manifest == header.manifest
                && expected.sections == header.sections
                && expected.size == size
                && expected.header_size == 16 + length
                && expected.name
                    == format!(
                        "{}-{}.sigla",
                        header.manifest.origin.kind,
                        blake3::hash(&bytes).to_hex()
                    ),
            "Catalog and metadata container disagree"
        );
    }
    Ok(header)
}

#[cfg(test)]
pub(super) fn artifact(path: &Path) -> Result<Artifact> {
    use std::io::Seek;
    let mut file = fs::File::open(path)?;
    let header = header(&mut file, None)?;
    Ok(Artifact {
        name: path
            .file_name()
            .context("Missing artifact name")?
            .to_str()
            .context("Invalid artifact name")?
            .into(),
        hash: digest(path)?,
        size: file.metadata()?.len(),
        release: TAG.into(),
        manifest: header.manifest,
        sections: header.sections,
        header_size: file.stream_position()?,
    })
}

pub(super) fn authenticated<R: Read, T>(
    reader: R,
    size: u64,
    hash: &[u8; 32],
    consume: impl FnOnce(&mut dyn Read) -> Result<T>,
) -> Result<T> {
    struct Checked<R> {
        reader: R,
        size: u64,
        hash: blake3::Hasher,
        limit: u64,
    }
    impl<R: Read> Read for Checked<R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let count = self.reader.read(bytes)?;
            self.size = self
                .size
                .checked_add(count as u64)
                .ok_or_else(|| std::io::Error::other("Metadata size overflow"))?;
            if self.size > self.limit {
                return Err(std::io::Error::other("Metadata exceeds expected size"));
            }
            self.hash.update(&bytes[..count]);
            Ok(count)
        }
    }
    let mut reader = Checked {
        reader,
        size: 0,
        hash: blake3::Hasher::new(),
        limit: size,
    };
    let value = consume(&mut reader)?;
    std::io::copy(&mut reader, &mut std::io::sink())?;
    ensure!(
        reader.size == size && reader.hash.finalize().as_bytes() == hash,
        "Metadata digest or size mismatch"
    );
    Ok(value)
}

pub(super) fn inspect(
    mut reader: impl Read,
    destination: Option<&Path>,
    expected: Option<&Artifact>,
    mut import: Option<&mut crate::cache::blobs::OwnedTree<'_>>,
) -> Result<Manifest> {
    let Header {
        mut manifest,
        sections,
    } = header(&mut reader, expected)?;
    let mut packages = BTreeSet::new();
    for section in sections {
        let frame = authenticated(
            reader.by_ref().take(section.size),
            section.size,
            &section.hash,
            |reader| bundle::inspect_section(reader, destination, &section, import.as_deref_mut()),
        )?;
        let advertised = frame.role.strip_prefix("package/").and_then(|name| {
            manifest
                .packages
                .iter()
                .find(|package| package.name == name)
        });
        ensure!(
            frame.package.as_ref() == advertised,
            "Package section disagrees with container inventory"
        );
        if let Some(package) = advertised {
            packages.insert(package.name.clone());
        }
        manifest.entries.extend(frame.entries);
    }
    ensure!(
        manifest
            .packages
            .iter()
            .all(|package| packages.contains(&package.name)),
        "Advertised package has no metadata section"
    );
    ensure!(
        reader.read(&mut [0])? == 0,
        "Unexpected data after metadata container"
    );
    let paths: BTreeSet<_> = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    ensure!(
        paths.len() == manifest.entries.len(),
        "Duplicate path across metadata sections"
    );
    for path in &paths {
        ensure!(
            !Path::new(path)
                .ancestors()
                .skip(1)
                .any(|parent| paths.contains(parent.to_str().unwrap())),
            "Metadata file is also a directory"
        );
    }
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(destination) = destination {
        let bytes = binary::encode(&manifest)?;
        if let Some(import) = import {
            import.write(
                &destination.join("manifest.bin"),
                0o600,
                envelope(&bytes).as_slice(),
            )?;
        } else {
            save(&destination.join("manifest.bin"), &bytes)?;
        }
    }
    Ok(manifest)
}
