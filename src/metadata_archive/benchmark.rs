//! Manual format migration measurements using data exported by the previous reader.
//! Exported analysis IDs belong to the previous format: never publish these bundles.
use super::*;
use crate::store::{
    format,
    shared::{Encoded, Names},
};
use anyhow::bail;
use std::time::Instant;

type Export = (ObjectId, Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>);

/// Build each revision's own records from the same raw Unity files. Production
/// and compressed-file seeding are excluded from consumer timings.
#[test]
#[ignore = "requires SIGLA_SETUP_FIXTURE, SIGLA_SETUP_ARTIFACTS and SIGLA_SETUP_CACHE"]
fn dependency_setup_benchmark() -> Result<()> {
    use crate::metadata_archive::{client, producer, registry};
    use crate::model::{MetadataReference, Project, ProjectReference, SourceInput, SourceRoot};
    #[derive(serde::Deserialize)]
    struct Input {
        name: String,
        package: String,
        defines: Vec<String>,
        sources: Vec<String>,
    }
    let fixture =
        PathBuf::from(std::env::var_os("SIGLA_SETUP_FIXTURE").context("Missing raw fixture")?);
    let artifacts = PathBuf::from(
        std::env::var_os("SIGLA_SETUP_ARTIFACTS").context("Missing artifacts directory")?,
    );
    let inputs: Vec<Input> = serde_json::from_slice(&fs::read(fixture.join("projects.json"))?)?;
    let assembly = "Editor/Data/Managed/UnityEngine/UnityEngine.CoreModule";
    let package_prefix =
        |package: &str| format!("Editor/Data/Resources/PackageManager/BuiltInPackages/{package}");
    if std::env::var_os("SIGLA_SETUP_PRODUCE").is_some() {
        let mut builder = Builder::new(
            Origin {
                kind: "editor".into(),
                name: "unity".into(),
                version: "6000.3.16f1".into(),
                revision: "a56f230f6470".into(),
                url: String::new(),
                integrity: None,
            },
            registry::profiles(&["6000.3.16f1".into()])?,
        )?;
        builder.add(
            format!("{assembly}.dll"),
            fs::read(fixture.join("UnityEngine.CoreModule.dll"))?,
            "editor",
        )?;
        for input in &inputs {
            let root = fixture.join(&input.package);
            builder
                .manifest
                .packages
                .push(registry::package(&serde_json::from_slice(&fs::read(
                    root.join("package.json"),
                )?)?)?);
            builder.manifest.package_names.push(input.package.clone());
            for file in producer::files(&root)?.into_iter().filter(|p| {
                p.extension().is_some_and(|e| e == "cs")
                    || p.file_name().is_some_and(|n| n == "package.json")
            }) {
                builder.add(
                    format!(
                        "{}/{}",
                        package_prefix(&input.package),
                        file.strip_prefix(&root)?.display()
                    ),
                    fs::read(&file)?,
                    "package",
                )?;
            }
        }
        let artifact = builder.finish(&artifacts)?;
        fs::write(artifacts.join("artifact.bin"), binary::encode(&artifact)?)?;
        eprintln!("produced_bytes={}", artifact.size);
        return Ok(());
    }
    let cache =
        PathBuf::from(std::env::var_os("SIGLA_SETUP_CACHE").context("Missing fresh cache")?);
    fs::create_dir_all(&cache)?;
    let artifact: Artifact = binary::decode(&fs::read(artifacts.join("artifact.bin"))?)?;
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    fs::create_dir_all(&root)?;
    fs::copy(artifacts.join(&artifact.name), root.join("source.sigla"))?;
    let leases = crate::cache::views::LeaseSet::new(&cache)?;
    let start = Instant::now();
    let roots: BTreeMap<_, _> = client::materialize_sections(
        &cache,
        &artifact,
        &Catalog::default(),
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |_, _| bail!("Unexpected benchmark network request"),
    )
    .into_iter()
    .map(|(role, root)| Ok((role, root?)))
    .collect::<Result<_>>()?;
    let materialize = start.elapsed();
    let local = cache.join("workspace");
    fs::create_dir_all(&local)?;
    let control = local.join("BenchControl.cs");
    fs::write(&control, "public class BenchControl {}")?;
    let assembly = roots["editor"].join(format!("{assembly}.sigla"));
    let mut projects = Vec::new();
    let mut sources = Vec::new();
    let mut dependencies = BTreeSet::from([assembly.clone()]);
    for input in &inputs {
        let root =
            roots[&format!("package/{}", input.package)].join(package_prefix(&input.package));
        dependencies.insert(root.clone());
        let project = projects.len();
        projects.push(Project {
            identity: input.name.clone(),
            name: input.name.clone(),
            origin: None,
            defines: input.defines.clone(),
            edition: String::new(),
            compiler_options: Default::default(),
            references: inputs[..project]
                .iter()
                .map(|p| ProjectReference {
                    target: p.name.clone(),
                    aliases: vec![],
                })
                .collect(),
            assemblies: vec![MetadataReference {
                path: assembly.clone(),
                aliases: vec![],
                provenance: "benchmark".into(),
            }],
            source_roots: vec![SourceRoot {
                physical: root.clone(),
                logical: format!("Packages/{}", input.package),
            }],
        });
        for source in &input.sources {
            let relative = source
                .strip_prefix(&format!("Packages/{}/", input.package))
                .context("Unexpected selected source")?;
            let path = root.join(relative);
            ensure!(
                path.is_file(),
                "Missing benchmark source {}",
                path.display()
            );
            sources.push(SourceInput {
                path,
                project,
                module: String::new(),
                language: Language::CSharp,
                metadata: false,
            });
        }
    }
    sources.push(SourceInput {
        path: control,
        project: 0,
        module: String::new(),
        language: Language::CSharp,
        metadata: false,
    });
    let expected_files = sources.len() + 1;
    let discovery = crate::discovery::Discovery {
        leases,
        root: local.clone(),
        projects,
        sources,
        metadata: Default::default(),
        dependencies,
        diagnostics: vec![],
        dependency_retry: None,
    };
    let mut workspace = crate::workspace::Workspace::open(
        local.clone(),
        &cache,
        crate::discovery::Policy::new(vec![local, cache.clone()])?,
        &cache.join("analysis"),
        None,
        Default::default(),
    )?;
    let start = Instant::now();
    workspace.benchmark_discovery(discovery.clone())?;
    let cold_index = start.elapsed();
    ensure!(
        workspace.manifest.files.len() == expected_files,
        "Benchmark omitted sources: {:?}",
        workspace.manifest.diagnostics
    );
    let mounted: usize = workspace
        .manifest
        .mounts
        .iter()
        .map(|m| m.files.len())
        .sum();
    let cancel = tokio_util::sync::CancellationToken::new();
    let start = Instant::now();
    let mut output = BTreeMap::new();
    {
        let mut search =
            crate::search::Search::new(&workspace.store, &workspace.manifest, &cancel)?;
        for symbol in ["float3", "NativeArray", "Vector3", "BenchControl"] {
            let result = search.run(&crate::query::Query::parse(&format!(
                "type:{symbol} limit:100"
            ))?)?;
            ensure!(
                result.contains(symbol),
                "Missing benchmark result {symbol}: {result}"
            );
            output.insert(symbol, result);
        }
    }
    let query = start.elapsed();
    fs::write(
        cache.join("results.json"),
        serde_json::to_vec_pretty(&output)?,
    )?;
    let start = Instant::now();
    workspace.benchmark_discovery(discovery)?;
    let reuse = start.elapsed();
    eprintln!(
        "materialize_ms,cold_index_ms,reuse_index_ms,query_ms,files,mounted,compressed_bytes\n{},{},{},{},{},{},{}",
        materialize.as_millis(),
        cold_index.as_millis(),
        reuse.as_millis(),
        query.as_millis(),
        expected_files,
        mounted,
        artifact.size
    );
    Ok(())
}

#[test]
#[ignore = "requires SIGLA_METADATA_ARCHIVE pointing to a downloaded editor bundle"]
fn materialization_benchmark() -> Result<()> {
    let archive =
        PathBuf::from(std::env::var_os("SIGLA_METADATA_ARCHIVE").context("Missing archive")?);
    let parent = std::env::var_os("SIGLA_METADATA_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::tempdir_in(parent)?;
    let output = directory.path().join("contents");
    // Prepare trusted catalog facts outside the timed consumer path.
    super::verify(&archive, None)?;
    let artifact = sections::artifact(&archive)?;
    let blobs = crate::cache::blobs::Store::open(directory.path())?;
    let mut tree = blobs.owned_tree(&output)?;
    let start = Instant::now();
    let manifest = super::unpack_stream(fs::File::open(&archive)?, &output, &artifact, &mut tree)?;
    let unpack = start.elapsed();
    let start = Instant::now();
    tree.publish()?;
    let import = start.elapsed();
    eprintln!(
        "entries={} unpack_ms={} import_ms={}",
        manifest.entries.len(),
        unpack.as_millis(),
        import.as_millis()
    );
    Ok(())
}

fn convert((id, names, mut records): Export) -> Result<Analysis> {
    let names: Names = binary::decode(&names)?;
    let mut data: FileData = binary::decode(&records[&vec![1]])?;
    let mut headers = Vec::new();
    for (key, bytes) in &records {
        if key[0] == 12 {
            let file: crate::csharp::syntax::DeclarationFile = binary::decode(bytes)?;
            ensure!(
                file.headers.len() == 1 && file.declarations.len() == 1,
                "Invalid benchmark input"
            );
            ensure!(
                binary::encode(&data.facts.declarations[headers.len()])?
                    == binary::encode(&file.declarations[0])?,
                "Input declaration mismatch"
            );
            headers.extend(file.headers);
        }
    }
    let csharp = records.contains_key(&vec![13]);
    let names = format::write(
        &data.facts.declarations,
        csharp.then_some(headers.as_slice()),
        data.assembly.is_none(),
        &names,
        &mut records,
    )?;
    let mut reader = format::Reader::new(|key| Ok(records.get(key).map(Vec::as_slice)))?;
    let restored = reader.all()?;
    ensure!(
        binary::encode(&restored.declarations)? == binary::encode(&data.facts.declarations)?,
        "Declaration round trip failed"
    );
    ensure!(
        binary::encode(&restored.headers)? == binary::encode(&headers)?,
        "Header round trip failed"
    );
    drop(reader);
    records.retain(|key, _| !matches!(key[0], 2 | 3 | 5 | 12));
    data.facts.declarations.clear();
    data.facts.modules.clear();
    data.assembly = None;
    records.insert(vec![1], binary::encode(&data)?);
    Ok(Analysis {
        id,
        encoded: Encoded { names, records },
    })
}

#[test]
#[ignore = "requires SIGLA_FORMAT_BUNDLE exported by the previous reader"]
fn migrated_bundle_benchmark() -> Result<()> {
    let input = std::env::var("SIGLA_FORMAT_BUNDLE")?;
    let output = PathBuf::from(std::env::var("SIGLA_FORMAT_OUTPUT")?);
    let started = Instant::now();
    let mut archive = tar::Archive::new(zstd::Decoder::new(fs::File::open(input)?)?);
    let mut entries = archive.entries()?;
    let mut bytes = Vec::new();
    entries
        .next()
        .context("Missing benchmark manifest")??
        .read_to_end(&mut bytes)?;
    let manifest: Manifest = binary::decode(&bytes)?;
    let mut paths = BTreeMap::<String, Vec<Entry>>::new();
    for entry in &manifest.entries {
        paths
            .entry(format!("objects/{}", hex(&entry.hash)))
            .or_default()
            .push(entry.clone());
    }
    let mut builder = Builder::new(manifest.origin.clone(), vec![Vec::new()])?;
    builder.manifest = manifest;
    builder.manifest.entries.clear();
    let mut profiles = 0;
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let (kind, name) = path.split_once('/').context("Invalid benchmark entry")?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if kind != "raw" {
            let exports: Vec<Export> = binary::decode(&bytes)?;
            let mut analyses = Vec::new();
            for export in exports {
                analyses.push(convert(export).with_context(|| format!("Cannot convert {name}"))?);
                profiles += 1;
                if profiles % 1000 == 0 {
                    eprintln!(
                        "converted_profiles={profiles} elapsed_seconds={:.1}",
                        started.elapsed().as_secs_f64()
                    );
                }
            }
            bytes = if kind == "assembly" {
                envelope(&binary::encode(&analyses[0])?)
            } else {
                let profiles = analyses
                    .into_iter()
                    .map(|analysis| Ok((analysis.id, binary::encode(&analysis.encoded)?)))
                    .collect::<Result<Vec<_>>>()?;
                envelope(&binary::encode(&profiles)?)
            };
        }
        for entry in paths.remove(name).context("Unknown benchmark object")? {
            builder.object(entry.path, &bytes, &entry.group)?;
        }
    }
    ensure!(paths.is_empty(), "Missing benchmark objects");
    let raw_bytes: u64 = builder
        .manifest
        .entries
        .iter()
        .map(|e| (e.hash, e.size))
        .collect::<BTreeMap<_, _>>()
        .values()
        .sum();
    let conversion = started.elapsed();
    #[cfg(target_os = "linux")]
    eprintln!(
        "before_compression {}",
        fs::read_to_string("/proc/self/status")?
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .context("Missing peak memory")?
    );
    eprintln!("compression_started profiles={profiles} object_bytes={raw_bytes}");
    let compressed = Instant::now();
    let artifact = builder.finish(&output)?;
    #[cfg(target_os = "linux")]
    eprintln!(
        "after_compression_and_validation {}",
        fs::read_to_string("/proc/self/status")?
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .context("Missing peak memory")?
    );
    eprintln!(
        "profiles={profiles} object_bytes={raw_bytes} bundle_bytes={} conversion_seconds={:.3} compression_and_validation_seconds={:.3} path={}",
        artifact.size,
        conversion.as_secs_f64(),
        compressed.elapsed().as_secs_f64(),
        output.join(artifact.name).display()
    );
    Ok(())
}

#[test]
#[ignore = "requires SIGLA_FORMAT_FIXTURES exported by the previous reader"]
fn migrated_fixtures_benchmark() -> Result<()> {
    use heed::{Database, EnvOpenOptions, types::Bytes};
    let input = PathBuf::from(std::env::var("SIGLA_FORMAT_FIXTURES")?);
    for entry in fs::read_dir(input)? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "bin") {
            continue;
        }
        let analysis = convert(binary::decode(&fs::read(&path)?)?)?;
        let temp = tempfile::tempdir()?;
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(1024 * 1024 * 1024)
                .max_dbs(1)
                .open(temp.path())?
        };
        let mut tx = env.write_txn()?;
        let db: Database<Bytes, Bytes> = env.create_database(&mut tx, Some("records"))?;
        for (key, bytes) in &analysis.encoded.records {
            let mut full = analysis.id.to_vec();
            full.extend(key);
            db.put(&mut tx, &full, bytes)?;
        }
        db.put(&mut tx, &analysis.id, &analysis.encoded.names)?;
        tx.commit()?;
        env.force_sync()?;
        let disk = fs::metadata(temp.path().join("data.mdb"))?.len();
        let tx = env.read_txn()?;
        let read = |key: &[u8]| {
            let mut full = analysis.id.to_vec();
            full.extend(key);
            Ok(db.get(&tx, &full)?)
        };
        let count = format::Reader::new(read)?.directory.count;
        let iterations = 1000;
        let start = Instant::now();
        for i in 0..iterations {
            let mut reader = format::Reader::new(read)?;
            std::hint::black_box(reader.declaration(i % count)?);
        }
        let cold = start.elapsed().as_secs_f64() * 1e6 / iterations as f64;
        let start = Instant::now();
        for i in 0..iterations {
            let mut reader = format::Reader::for_analysis(read, analysis.id)?;
            std::hint::black_box(reader.declaration(i % count)?);
        }
        let shared = start.elapsed().as_secs_f64() * 1e6 / iterations as f64;
        eprintln!(
            "fixture={} declarations={count} lmdb_bytes={disk} cold_declaration_us={cold:.3} shared_cache_us={shared:.3}",
            path.display()
        );
    }
    Ok(())
}
/// Replay verified local sections without network or client scheduling changes.
/// Compare staging order separately from the number of data barriers.
#[test]
#[ignore = "requires SIGLA_SETUP_ARTIFACTS, SIGLA_SETUP_CACHE and SIGLA_DURABILITY_MODE"]
fn section_durability_benchmark() -> Result<()> {
    use crate::cache::blobs::{OwnedTree, Store};
    use std::{
        io::{Read, Seek, SeekFrom},
        os::unix::fs::MetadataExt,
    };
    struct Stage {
        _temporary: tempfile::TempDir,
        output: PathBuf,
        destination: PathBuf,
        frame: sections::Frame,
    }
    fn prepare<'a>(
        store: &'a Store,
        parent: &Path,
        source: &Path,
        artifact: &Artifact,
        section: &Section,
    ) -> Result<(OwnedTree<'a>, Stage)> {
        let temporary = tempfile::tempdir_in(parent)?;
        let output = temporary.path().join("contents");
        let mut tree = store.owned_tree(&output)?;
        let mut file = fs::File::open(source)?;
        file.seek(SeekFrom::Start(artifact.header_size + section.offset))?;
        let frame = sections::authenticated(
            file.take(section.size),
            section.size,
            &section.hash,
            |reader| super::inspect_section(reader, Some(&output), section, Some(&mut tree)),
        )?;
        tree.write(
            &output.join("section.bin"),
            0o600,
            envelope(&binary::encode(&frame)?).as_slice(),
        )?;
        Ok((
            tree,
            Stage {
                _temporary: temporary,
                output,
                destination: parent.join(hex(&section.hash)),
                frame,
            },
        ))
    }
    fn activate(stage: Stage) -> Result<(PathBuf, sections::Frame)> {
        fs::rename(&stage.output, &stage.destination)?;
        fs::File::open(stage.destination.parent().unwrap())?.sync_all()?;
        Ok((stage.destination, stage.frame))
    }
    let artifacts =
        PathBuf::from(std::env::var_os("SIGLA_SETUP_ARTIFACTS").context("Missing artifacts")?);
    let cache = PathBuf::from(std::env::var_os("SIGLA_SETUP_CACHE").context("Missing cache")?);
    let mode = std::env::var("SIGLA_DURABILITY_MODE")?;
    ensure!(!cache.exists(), "Benchmark requires a fresh cache root");
    ensure!(
        ["serial", "staged", "shared"].contains(&mode.as_str()),
        "Unknown durability prototype"
    );
    let artifact: Artifact = binary::decode(&fs::read(artifacts.join("artifact.bin"))?)?;
    ensure!(
        !artifact.sections.is_empty() && artifact.sections.len() <= 4,
        "Prototype admits at most four sections"
    );
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    let parent = root.join("sections");
    fs::create_dir_all(&parent)?;
    ensure!(
        fs::read_dir(&parent)?.next().is_none(),
        "Benchmark requires an empty section cache"
    );
    let source = root.join("source.sigla");
    fs::copy(artifacts.join(&artifact.name), &source)?;
    // Seeding is excluded, including its writeback, in every variant.
    fs::File::open(&source)?.sync_all()?;
    fs::File::open(&root)?.sync_all()?;
    let leases = crate::cache::views::LeaseSet::new(&cache)?;
    leases.retain(&root)?;
    let store = Store::open(&cache)?;
    let mut completed = Vec::new();
    let mut prepare_time = std::time::Duration::ZERO;
    let mut publication_time = std::time::Duration::ZERO;
    let mut sync_time = std::time::Duration::ZERO;
    let started = Instant::now();
    crate::acquisition::shared(&format!("durability-prototype:{}", cache.display()), || {
        let mut pending = Vec::new();
        for section in &artifact.sections {
            let start = Instant::now();
            let (tree, stage) = prepare(&store, &parent, &source, &artifact, section)?;
            prepare_time += start.elapsed();
            if mode == "serial" {
                let start = Instant::now();
                sync_time += tree.publish()?.sync;
                completed.push(activate(stage)?);
                publication_time += start.elapsed();
            } else {
                pending.push((tree, stage));
            }
        }
        if mode != "serial" {
            let (trees, stages): (Vec<_>, Vec<_>) = pending.into_iter().unzip();
            let start = Instant::now();
            let results = if mode == "shared" {
                OwnedTree::publish_together(trees)?
            } else {
                trees.into_iter().map(OwnedTree::publish).collect()
            };
            for (result, stage) in results.into_iter().zip(stages) {
                sync_time += result?.sync;
                completed.push(activate(stage)?);
            }
            publication_time += start.elapsed();
        }
        Ok(())
    })?;
    let total = started.elapsed();
    let mut inventory = BTreeMap::new();
    let mut inodes = BTreeMap::new();
    let mut bytes = 0;
    for (root, frame) in completed {
        let expected = artifact
            .sections
            .iter()
            .find(|section| section.role == frame.role)
            .unwrap();
        let marker = fs::read(root.join("section.bin"))?;
        ensure!(
            blake3::hash(&marker).as_bytes() == &expected.manifest_hash,
            "Published section inventory changed"
        );
        let frame: sections::Frame = binary::decode(payload(&marker)?)?;
        ensure!(
            producer::files(&root)?.len() == frame.entries.len() + 1,
            "Unexpected published files"
        );
        for entry in frame.entries {
            let path = root.join(&entry.path);
            let metadata = fs::metadata(&path)?;
            ensure!(metadata.len() == entry.size, "Published file size changed");
            let mode = metadata.mode() & 0o777;
            let mut hash = crate::cache::file_id::hasher(sections::OBJECT_MODE);
            hash.update_reader(fs::File::open(&path)?)?;
            ensure!(
                hash.finalize().as_bytes() == &entry.hash,
                "Published file identity changed"
            );
            let identity = if mode == sections::OBJECT_MODE {
                entry.hash
            } else {
                let mut hash = crate::cache::file_id::hasher(mode);
                hash.update_reader(fs::File::open(&path)?)?;
                *hash.finalize().as_bytes()
            };
            ensure!(
                store.file_identity(&path)? == Some((identity, mode)),
                "Missing immutable identity"
            );
            let inode = (metadata.dev(), metadata.ino());
            if let Some(previous) = inodes.insert(identity, inode) {
                ensure!(
                    previous == inode,
                    "Shared content did not adopt its canonical inode"
                );
            }
            if entry.group == "index" {
                crate::store::segment::Segment::open(&path, Some(&cache))?.validate(true)?;
            }
            bytes += metadata.len();
            inventory.insert(
                format!("{}/{}", frame.role, entry.path),
                (hex(&identity), metadata.len(), mode),
            );
        }
    }
    fs::write(
        cache.join("inventory.json"),
        serde_json::to_vec(&inventory)?,
    )?;
    println!(
        "mode,prepare_us,publication_us,sync_us,total_us,files,bytes\n{mode},{},{},{},{},{},{}",
        prepare_time.as_micros(),
        publication_time.as_micros(),
        sync_time.as_micros(),
        total.as_micros(),
        inventory.len(),
        bytes
    );
    Ok(())
}
