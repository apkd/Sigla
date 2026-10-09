//! Manual format migration measurements using data exported by the previous reader.
//! Exported analysis IDs belong to the previous format: never publish these bundles.
use super::*;
use crate::store::{
    format,
    shared::{Encoded, Names},
};
use std::time::Instant;

type Export = (ObjectId, Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>);

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
