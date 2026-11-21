use super::*;
#[cfg(test)]
#[path = "benchmark.rs"]
mod benchmark;
use crate::{
    acquisition,
    model::Language,
    store::{FileData, payload as records},
};
use std::{collections::BTreeSet, io::Read};

pub(super) struct Builder {
    work: tempfile::TempDir,
    pub manifest: Manifest,
    profiles: Vec<Vec<String>>,
    paths: BTreeMap<String, ([u8; 32], String)>,
    assemblies: BTreeMap<ObjectId, [u8; 32]>,
}
impl Builder {
    pub fn new(origin: Origin, profiles: Vec<Vec<String>>) -> Result<Self> {
        ensure!(!profiles.is_empty(), "Missing analysis profiles");
        Ok(Self {
            paths: BTreeMap::new(),
            assemblies: BTreeMap::new(),
            work: tempfile::tempdir()?,
            profiles,
            manifest: Manifest {
                origin,
                entries: vec![],
                packages: vec![],
                package_names: vec![],
                recommended: BTreeMap::new(),
            },
        })
    }
    pub(super) fn object(&mut self, path: String, bytes: &[u8], group: &str) -> Result<()> {
        safe(&path)?;
        let hash = sections::object_hash(bytes);
        if let Some((previous, previous_group)) = self.paths.get(&path) {
            ensure!(
                previous == &hash && previous_group == group,
                "Conflicting archive path {path}"
            );
            return Ok(());
        }
        self.paths.insert(path.clone(), (hash, group.into()));
        let object = self.work.path().join(hex(&hash));
        if !object.exists() {
            fs::write(object, bytes)?;
        }
        self.manifest.entries.push(Entry {
            path,
            hash,
            size: bytes.len() as u64,
            group: group.into(),
        });
        Ok(())
    }
    pub fn add(&mut self, path: String, bytes: Vec<u8>, group: &str) -> Result<()> {
        let file = Path::new(&path);
        if file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
        {
            let stem = file
                .file_stem()
                .context("Assembly name missing")?
                .to_str()
                .context("Invalid assembly name")?;
            let id = crate::store::metadata_id(&bytes, stem)?;
            let target = file.with_extension("sigla").to_string_lossy().into_owned();
            if let Some(hash) = self.assemblies.get(&id) {
                let encoded = read(&self.work.path().join(hex(hash)))?;
                return self.object(target, &encoded, group);
            }
            let data = crate::metadata::file_data_bytes(bytes, stem)
                .with_context(|| format!("Cannot analyze assembly {path}"))?;
            let analysis = Analysis {
                id,
                encoded: records::encode(data)?,
            };
            let encoded = envelope(&binary::encode(&analysis)?);
            self.assemblies.insert(id, sections::object_hash(&encoded));
            return self.object(target, &encoded, group);
        }
        let language = if file.extension().is_some_and(|e| e == "cs") {
            Some(Language::CSharp)
        } else {
            crate::native::language(file)
        };
        if let Some(language) = language {
            let source = crate::model::decode(&bytes, language)?;
            let mut analyses = Vec::new();
            let mut seen = BTreeSet::new();
            let profiles = if source.contains('#') {
                self.profiles.as_slice()
            } else {
                &self.profiles[..1]
            };
            for defines in profiles {
                let context = || format!("Cannot analyze source {path} with defines {defines:?}");
                let id = match crate::store::source_id(&source, language, defines, "") {
                    Ok(id) => id,
                    // A source file can require editor/platform symbols to be valid.
                    // Keep its source, but cache only profiles it can compile under.
                    Err(error) if error.is::<crate::extract::preprocess::InvalidDirectives>() => {
                        tracing::debug!(%path, ?defines, %error, "Skipping invalid source profile");
                        continue;
                    }
                    Err(error) => return Err(error).with_context(context),
                };
                if !seen.insert(id) {
                    continue;
                }
                let facts = crate::extract::extract(&source, language, defines, "")
                    .with_context(context)?;
                analyses.push((
                    id,
                    binary::encode(&records::encode(FileData {
                        source: source.clone(),
                        facts,
                        assembly: None,
                    })?)?,
                ));
            }
            self.object(
                format!("{path}.analysis"),
                &envelope(&binary::encode(&analyses)?),
                "analysis",
            )?;
        }
        self.object(path, &bytes, group)
    }
    pub fn finish(mut self, output: &Path) -> Result<Artifact> {
        fs::create_dir_all(output)?;
        self.manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
        self.manifest.package_names.sort();
        self.manifest.package_names.dedup();
        self.manifest.packages.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in self.manifest.packages.windows(2) {
            ensure!(
                pair[0].name != pair[1].name || pair[0] == pair[1],
                "Conflicting bundled package versions"
            );
        }
        self.manifest.packages.dedup();
        sections::write(self.manifest, self.work.path(), output)
    }
}

pub(super) fn retained(path: &Path) -> bool {
    acquisition::analysis_input(path)
        || license(path)
        || path.extension().is_some_and(|e| {
            matches!(
                e.to_string_lossy().to_ascii_lowercase().as_str(),
                "inc" | "cg" | "raytrace" | "ush" | "usf" | "cs" | "dll"
            )
        })
}
pub(super) fn license(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.starts_with("license")
        || name.starts_with("licence")
        || name.starts_with("third party notices")
        || name.starts_with("third-party-notices")
        || name.starts_with("notice")
        || name.contains("third party")
}

#[cfg(test)]
pub(super) fn unpack(
    path: &Path,
    destination: &Path,
    expected: Option<&Artifact>,
) -> Result<Manifest> {
    inspect(path, Some(destination), expected)
}

fn inspect(
    path: &Path,
    destination: Option<&Path>,
    expected: Option<&Artifact>,
) -> Result<Manifest> {
    if let Some(expected) = expected {
        verify_digest(path, expected)?;
    }
    sections::inspect(fs::File::open(path)?, destination, expected, None)
}

/// Decode into a private tree while verifying the compressed transport. No
/// immutable links are published until this function has validated the EOF.
#[cfg(test)]
pub(super) fn unpack_stream(
    reader: impl Read,
    destination: &Path,
    expected: &Artifact,
    import: &mut crate::cache::blobs::OwnedTree<'_>,
) -> Result<Manifest> {
    sections::authenticated(reader, expected.size, &expected.hash, |reader| {
        sections::inspect(reader, Some(destination), Some(expected), Some(import))
    })
}

pub(super) fn inspect_section(
    input: impl Read,
    destination: Option<&Path>,
    expected: &Section,
    import: Option<&mut crate::cache::blobs::OwnedTree<'_>>,
) -> Result<sections::Frame> {
    inspect_section_with(input, destination, expected, import, |_, _| Ok(()))
}

pub(super) fn inspect_section_with(
    input: impl Read,
    destination: Option<&Path>,
    expected: &Section,
    mut import: Option<&mut crate::cache::blobs::OwnedTree<'_>>,
    begin: impl FnOnce(&sections::Frame, u64) -> Result<()>,
) -> Result<sections::Frame> {
    let mut reader = zstd::stream::read::Decoder::new(input)?;
    reader.window_log_max(27)?;
    let mut archive = tar::Archive::new(reader);
    // Extensions can allocate unbounded payloads before our size/type checks.
    // The bundle format uses only short, regular entries.
    let mut entries = archive.entries()?.raw(true);
    let mut first = entries.next().context("Empty metadata bundle")??;
    ensure!(
        first.path()?.as_ref() == Path::new("section.bin") && first.header().entry_type().is_file(),
        "Bundle must start with its manifest"
    );
    ensure!(
        first.size() <= 64 * 1024 * 1024,
        "Manifest exceeds size bound"
    );
    let mut bytes = Vec::new();
    first.read_to_end(&mut bytes)?;
    ensure!(
        blake3::hash(&bytes).as_bytes() == &expected.manifest_hash,
        "Section manifest digest mismatch"
    );
    let manifest: sections::Frame = binary::decode(payload(&bytes)?)?;
    let manifest_size = bytes.len() as u64;
    drop(bytes);
    ensure!(manifest.role == expected.role, "Section role mismatch");
    let index_path = sections::index_path(&manifest.role);
    let has_index = manifest
        .entries
        .iter()
        .any(|entry| entry.path == index_path);
    let verification = if has_index && destination.is_none() {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let mut references = Vec::new();
    let mut index_disk = None;
    let mut sources = BTreeMap::new();
    let groups: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.group.as_str()))
        .collect();
    let mut package_verified = manifest.package.is_none();
    let mut pending: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    for entry in &manifest.entries {
        safe(&entry.path)?;
        ensure!(
            if entry.group == "index" {
                entry.path == index_path
            } else {
                sections::role(entry, &groups)? == manifest.role
            },
            "File belongs to another metadata section"
        );
        ensure!(
            !has_index
                || !entry.path.ends_with(".sigla")
                || entry.size == IndexReference::SIZE as u64,
            "Invalid assembly reference size"
        );
        ensure!(
            entry.size <= MAX_RECORD && paths.insert(entry.path.as_str()),
            "Invalid bundle inventory"
        );
        pending
            .entry(format!("objects/{}", hex(&entry.hash)))
            .or_default()
            .push(entry);
    }
    for path in &paths {
        ensure!(
            !Path::new(path)
                .ancestors()
                .skip(1)
                .any(|parent| paths.contains(parent.to_str().unwrap())),
            "Bundle file is also used as a directory"
        );
    }
    begin(&manifest, manifest_size)?;
    if let Some(destination) = destination {
        fs::create_dir_all(destination)?;
    }
    for item in entries {
        let mut item = item?;
        ensure!(
            item.header().entry_type().is_file(),
            "Unexpected non-file bundle entry"
        );
        let path = item.path()?.to_string_lossy().into_owned();
        let targets = pending
            .remove(&path)
            .context("Unexpected or duplicate bundle object")?;
        ensure!(
            targets.iter().all(|e| e.size == item.size()),
            "Object size mismatch"
        );
        if destination.is_none() && targets.iter().any(|entry| entry.path == index_path) {
            let path = verification.as_ref().unwrap().path().join(&index_path);
            fs::create_dir_all(path.parent().unwrap())?;
            let mut file = fs::File::create(&path)?;
            std::io::copy(&mut item, &mut file)?;
            let mut hash = crate::cache::file_id::hasher(sections::OBJECT_MODE);
            hash.update_reader(fs::File::open(&path)?)?;
            ensure!(
                hash.finalize().as_bytes() == &targets[0].hash,
                "Index digest mismatch"
            );
            index_disk = Some(path);
            continue;
        }
        if let Some(destination) = destination {
            let first = destination.join(&targets[0].path);
            if let Some(import) = import.as_deref_mut() {
                ensure!(
                    import.write_identified(
                        &first,
                        0o666,
                        &mut item,
                        (sections::OBJECT_MODE, targets[0].hash)
                    )? == targets[0].size,
                    "Truncated bundle object"
                );
                if has_index {
                    for target in &targets {
                        if target.path == index_path {
                            index_disk = Some(first.clone());
                        }
                        if target.path.ends_with(".sigla") {
                            references.push((
                                target.path.clone(),
                                IndexReference::decode(&read(&first)?)?,
                            ));
                        }
                    }
                }
                for target in &targets[1..] {
                    import.alias(&first, &destination.join(&target.path))?;
                }
                continue;
            }
            fs::create_dir_all(first.parent().unwrap())?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&first)?;
            ensure!(
                std::io::copy(&mut item, &mut output)? == targets[0].size,
                "Truncated bundle object"
            );
            drop(output);
            if has_index {
                for target in &targets {
                    if target.path == index_path {
                        index_disk = Some(first.clone());
                    }
                    if target.path.ends_with(".sigla") {
                        references
                            .push((target.path.clone(), IndexReference::decode(&read(&first)?)?));
                    }
                }
            }
            ensure!(
                sections::object_hash(&read(&first)?) == targets[0].hash,
                "Object digest mismatch"
            );
            for target in &targets[1..] {
                let path = destination.join(&target.path);
                fs::create_dir_all(path.parent().unwrap())?;
                // Independent inodes can be adopted by the owned-tree importer.
                let mut copy = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                std::io::copy(&mut fs::File::open(&first)?, &mut copy)?;
            }
            continue;
        }
        let mut bytes = Vec::new();
        item.read_to_end(&mut bytes)?;
        ensure!(
            sections::object_hash(&bytes) == targets[0].hash,
            "Object digest mismatch"
        );
        for target in targets {
            if let Some(package) = &manifest.package
                && [
                    "packages",
                    "Editor/Data/Resources/PackageManager/BuiltInPackages",
                ]
                .iter()
                .any(|prefix| target.path == format!("{prefix}/{}/package.json", package.name))
            {
                ensure!(
                    &registry::package(&serde_json::from_slice(&bytes)?)? == package,
                    "Package contents disagree with discovery inventory"
                );
                package_verified = true;
            }
            if target.path.ends_with(".cs") && has_index {
                let source = crate::model::decode(&bytes, Language::CSharp)?;
                sources.insert(
                    target.path.clone(),
                    (
                        *blake3::hash(source.as_bytes()).as_bytes(),
                        source.contains('#'),
                    ),
                );
            }
            if target.path.ends_with(".sigla") {
                if has_index {
                    references.push((target.path.clone(), IndexReference::decode(&bytes)?));
                } else {
                    validate_analysis(&binary::decode::<Analysis>(payload(&bytes)?)?)?;
                }
            } else if target.path.ends_with(".analysis") {
                for (id, encoded) in binary::decode::<Vec<(ObjectId, &[u8])>>(payload(&bytes)?)? {
                    validate_analysis(&Analysis {
                        id,
                        encoded: binary::decode(encoded)?,
                    })?;
                }
            }
        }
    }
    ensure!(pending.is_empty(), "Bundle is missing objects");
    ensure!(
        destination.is_some() || package_verified,
        "Package section is missing its package manifest"
    );
    // The iterator consumes the first zero block. Only the writer's second
    // block may follow; never expand an unchecked tail before authentication.
    let mut reader = archive.into_inner();
    let mut padding = [0; 512];
    reader.read_exact(&mut padding)?;
    ensure!(padding == [0; 512], "Invalid bundle padding");
    ensure!(reader.read(&mut [0])? == 0, "Unexpected data after bundle");
    if has_index {
        // Extraction has stopped. The private output remains owned here until
        // this map drops, before publication or removal of the staging tree.
        let segment = unsafe {
            crate::store::segment::Segment::staged(
                index_disk.as_deref().context("Missing section index")?,
            )?
        };
        segment.validate(destination.is_none())?;
        segment.files(|path, invariant, ordinal| {
            ensure!(
                paths.contains(path),
                "Indexed file is absent from its section"
            );
            if destination.is_none() && path.ends_with(".cs") {
                let (hash, directives) = sources.get(path).context("Indexed source is missing")?;
                let source = segment.source(ordinal)?;
                ensure!(
                    blake3::hash(source.as_bytes()).as_bytes() == hash,
                    "Source profile belongs to a different file"
                );
                if invariant {
                    ensure!(
                        !directives
                            && segment.object(ordinal)?
                                == crate::store::source_id(source, Language::CSharp, &[], "")?,
                        "Conditional source marked as invariant"
                    );
                }
            }
            Ok(())
        })?;
        for (path, reference) in references {
            let ordinal = segment
                .file(&path, || Ok(reference.id))?
                .context("Assembly has no immutable profile")?;
            ensure!(
                segment.object(ordinal)? == reference.id,
                "Assembly reference identity mismatch"
            );
        }
    }
    Ok(manifest)
}
pub(super) fn verify(path: &Path, expected: Option<&Artifact>) -> Result<Manifest> {
    inspect(path, None, expected)
}

pub(super) fn verify_digest(path: &Path, expected: &Artifact) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == expected.size && digest(path)? == expected.hash,
        "Bundle digest or size mismatch"
    );
    Ok(())
}
fn validate_analysis(analysis: &Analysis) -> Result<()> {
    records::validate_encoded(&analysis.encoded)
}
