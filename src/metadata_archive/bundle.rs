use super::*;
use crate::{
    acquisition,
    model::Language,
    store::{FileData, payload as records},
};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
};

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
    fn object(&mut self, path: String, bytes: &[u8], group: &str) -> Result<()> {
        safe(&path)?;
        let hash = *blake3::hash(bytes).as_bytes();
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
            self.assemblies
                .insert(id, *blake3::hash(&encoded).as_bytes());
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
                analyses.push(Analysis {
                    id,
                    encoded: records::encode(FileData {
                        source: source.clone(),
                        facts,
                        assembly: None,
                    })?,
                });
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
        let bytes = envelope(&binary::encode(&self.manifest)?);
        let name = format!(
            "{}-{}.tar.zst",
            self.manifest.origin.kind,
            blake3::hash(&bytes).to_hex()
        );
        let path = output.join(&name);
        let mut temporary = tempfile::NamedTempFile::new_in(output)?;
        {
            let compressed = zstd::stream::write::Encoder::new(temporary.as_file_mut(), 12)?;
            let mut tar = tar::Builder::new(compressed);
            fn append<W: Write>(tar: &mut tar::Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, path, bytes)?;
                Ok(())
            }
            append(&mut tar, "manifest.bin", &bytes)?;
            let unique: BTreeSet<_> = self.manifest.entries.iter().map(|e| e.hash).collect();
            for hash in unique {
                let bytes = read(&self.work.path().join(hex(&hash)))?;
                append(&mut tar, &format!("objects/{}", hex(&hash)), &bytes)?;
            }
            tar.into_inner()?.finish()?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(&path)?;
        self.manifest.entries.clear();
        let artifact = Artifact {
            name,
            hash: digest(&path)?,
            size: fs::metadata(&path)?.len(),
            release: TAG.into(),
            manifest: self.manifest,
        };
        verify(&path, Some(&artifact))?;
        save(&path.with_extension("bin"), &binary::encode(&artifact)?)?;
        Ok(artifact)
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

pub(super) fn unpack(
    path: &Path,
    destination: &Path,
    expected: Option<&Artifact>,
) -> Result<Manifest> {
    if let Some(expected) = expected {
        ensure!(
            fs::metadata(path)?.len() == expected.size && digest(path)? == expected.hash,
            "Bundle digest or size mismatch"
        );
    }
    let reader = zstd::stream::read::Decoder::new(fs::File::open(path)?)?;
    let mut archive = tar::Archive::new(reader);
    let mut entries = archive.entries()?;
    let mut first = entries.next().context("Empty metadata bundle")??;
    ensure!(
        first.path()?.as_ref() == Path::new("manifest.bin")
            && first.header().entry_type().is_file(),
        "Bundle must start with its manifest"
    );
    ensure!(
        first.size() <= 64 * 1024 * 1024,
        "Manifest exceeds size bound"
    );
    let mut bytes = Vec::new();
    first.read_to_end(&mut bytes)?;
    let manifest: Manifest = binary::decode(payload(&bytes)?)?;
    if let Some(expected) = expected {
        let mut summary = manifest.clone();
        summary.entries.clear();
        ensure!(summary == expected.manifest, "Catalog and bundle disagree");
    }
    let mut pending: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    for entry in &manifest.entries {
        safe(&entry.path)?;
        ensure!(
            entry.size <= MAX_RECORD && paths.insert(&entry.path),
            "Invalid bundle inventory"
        );
        pending
            .entry(format!("objects/{}", hex(&entry.hash)))
            .or_default()
            .push(entry);
    }
    fs::create_dir_all(destination)?;
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
        let mut bytes = Vec::new();
        item.read_to_end(&mut bytes)?;
        ensure!(
            blake3::hash(&bytes).as_bytes() == &targets[0].hash,
            "Object digest mismatch"
        );
        for target in targets {
            if target.path.ends_with(".sigla") {
                validate_analysis(binary::view::<Analysis>(payload(&bytes)?)?)?;
            } else if target.path.ends_with(".analysis") {
                for analysis in binary::view::<Vec<Analysis>>(payload(&bytes)?)?.iter() {
                    validate_analysis(analysis)?;
                }
            }
            let path = destination.join(&target.path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?
                .write_all(&bytes)?;
        }
    }
    ensure!(pending.is_empty(), "Bundle is missing objects");
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
    save(
        &destination.join("manifest.bin"),
        &binary::encode(&manifest)?,
    )?;
    Ok(manifest)
}
pub(super) fn verify(path: &Path, expected: Option<&Artifact>) -> Result<Manifest> {
    let temp = tempfile::tempdir()?;
    unpack(path, temp.path(), expected)
}
fn validate_analysis(analysis: &ArchivedAnalysis) -> Result<()> {
    let records = &analysis.encoded.records;
    let data = binary::view::<FileData>(
        records
            .iter()
            .find(|(key, _)| key.as_slice() == records::DATA)
            .map(|(_, bytes)| bytes.as_slice())
            .context("Missing source facts")?,
    )?;
    records::validate_archived(data)?;
    binary::view::<crate::store::shared::Names>(&analysis.encoded.names)?;
    for (key, bytes) in records.iter() {
        ensure!(!key.is_empty() && key.len() <= 479, "Invalid analysis key");
        match key[0] {
            1 => {}
            2 => {
                binary::view::<Vec<crate::model::Declaration>>(bytes)?;
            }
            3 | 12 => {
                binary::view::<crate::csharp::syntax::DeclarationFile>(bytes)?;
            }
            4 => {
                binary::view::<Vec<(std::ops::Range<usize>, u32)>>(bytes)?;
            }
            5 | 13 => {
                binary::view::<Vec<crate::csharp::syntax::Import>>(bytes)?;
            }
            6 => {
                binary::view::<Vec<(String, String)>>(bytes)?;
            }
            7 => {
                std::str::from_utf8(bytes)?;
            }
            8 => ensure!(bytes.len() == 32, "Invalid declaration fingerprint"),
            9 => {
                binary::view::<Vec<crate::model::ModuleFile>>(bytes)?;
            }
            10 => {
                binary::view::<crate::csharp::syntax::BodyFile>(bytes)?;
            }
            14..=17 => {
                binary::view::<Vec<u32>>(bytes)?;
            }
            _ => anyhow::bail!("Unknown analysis record"),
        }
    }
    Ok(())
}
