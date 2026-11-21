use super::*;
use crate::{
    model::Language,
    store::{FileData, Store, payload as records},
};

fn indexed_source(root: &Path, path: &Path, id: &ObjectId) -> Result<Option<Encoded>> {
    if let Some(encoded) = source_analysis(path, id)? {
        return Ok(Some(encoded));
    }
    let manifest = manifest(&root.join("manifest.bin"))?;
    for entry in manifest
        .entries
        .iter()
        .filter(|entry| entry.group == "index")
    {
        let segment = crate::store::segment::Segment::open(&root.join(&entry.path), None)?;
        if let Some(ordinal) =
            segment.file(path.strip_prefix(root)?.to_str().unwrap(), || Ok(*id))?
            && segment.object(ordinal)? == *id
        {
            return Ok(Some(segment.encoded(ordinal)?));
        }
    }
    Ok(None)
}

fn indexed_assembly(root: &Path, path: &Path) -> Result<Analysis> {
    let reference = IndexReference::decode(&read(path)?)?;
    for entry in fs::read_dir(root.join("indexes"))? {
        let segment = crate::store::segment::Segment::open(&entry?.path(), None)?;
        segment.validate(true)?;
        if let Some(ordinal) = segment.find(&reference.id)? {
            return Ok(Analysis {
                id: reference.id,
                encoded: segment.encoded(ordinal)?,
            });
        }
    }
    anyhow::bail!("Missing fixture assembly")
}

fn origin() -> Origin {
    Origin {
        kind: "package".into(),
        name: "com.example.fixture".into(),
        version: "1.0.0".into(),
        revision: String::new(),
        url: "https://example.org/fixture.tgz".into(),
        integrity: None,
    }
}

fn response(bytes: Vec<u8>, range: Option<String>) -> reqwest::blocking::Response {
    let length = bytes.len();
    response_with_length(bytes, range, length)
}

fn response_with_length(
    bytes: Vec<u8>,
    range: Option<String>,
    length: usize,
) -> reqwest::blocking::Response {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        for line in std::io::BufReader::new(&mut stream).lines() {
            if line.unwrap().is_empty() {
                break;
            }
        }
        let (status, header) = range.map_or(("200 OK", String::new()), |range| {
            ("206 Partial Content", format!("Content-Range: {range}\r\n"))
        });
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n",
            length
        )
        .and_then(|_| stream.write_all(&bytes));
    });
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
        .get(format!("http://{address}"))
        .send()
        .unwrap()
}

pub(crate) fn cached_sources(
    cache: &Path,
    files: &[(&str, &str)],
    profiles: Vec<Vec<String>>,
) -> PathBuf {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), profiles).unwrap();
    for (path, source) in files {
        builder
            .add((*path).into(), source.as_bytes().to_vec(), "package")
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache).unwrap();
    client::materialize_section_with(
        cache,
        &artifact,
        &Catalog::default(),
        &artifact.sections[0],
        &leases,
        |_, _| Ok(response(bytes.clone(), None)),
    )
    .unwrap()
}

pub(crate) fn cached_package_artifact(
    cache: &Path,
    name: &str,
    version: &str,
    dependencies: &[(&str, &str)],
) -> Artifact {
    let mut origin = origin();
    origin.name = name.into();
    origin.version = version.into();
    cached_package_with_origin(cache, name, version, dependencies, origin)
}

pub(crate) fn cached_builtin_artifact(cache: &Path, name: &str, editor: &str) -> Artifact {
    let origin = Origin {
        kind: "editor".into(),
        name: "unity-editor".into(),
        version: editor.into(),
        ..origin()
    };
    cached_package_with_origin(cache, name, "1.0.0", &[], origin)
}

fn cached_package_with_origin(
    cache: &Path,
    name: &str,
    version: &str,
    dependencies: &[(&str, &str)],
    origin: Origin,
) -> Artifact {
    let info = Package {
        name: name.into(),
        version: version.into(),
        minimum_editor: None,
        dependencies: dependencies
            .iter()
            .map(|(name, version)| ((*name).into(), (*version).into()))
            .collect(),
    };
    let base = if origin.kind == "editor" {
        format!("Editor/Data/Resources/PackageManager/BuiltInPackages/{name}")
    } else {
        format!("packages/{name}")
    };
    let mut builder = bundle::Builder::new(origin, vec![vec![]]).unwrap();
    builder.manifest.packages.push(info.clone());
    let json = serde_json::to_vec(
        &serde_json::json!({ "name": name, "version": version, "dependencies": info.dependencies }),
    )
    .unwrap();
    builder
        .add(format!("{base}/package.json"), json, "package")
        .unwrap();
    builder
        .add(
            format!("{base}/Code.cs"),
            b"class PackageContents {}".to_vec(),
            "package",
        )
        .unwrap();
    let output = tempfile::tempdir().unwrap();
    let artifact = builder.finish(output.path()).unwrap();
    let root = cache.join("unity-metadata").join(hex(&artifact.hash));
    fs::create_dir_all(&root).unwrap();
    fs::copy(
        output.path().join(&artifact.name),
        root.join("source.sigla"),
    )
    .unwrap();
    artifact
}

#[test]
fn adjacent_ranges_share_transport_without_publishing_unselected_sections() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for name in ["alpha", "beta", "gamma"] {
        builder
            .add(
                format!("packages/{name}/license.txt"),
                name.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let requests = std::cell::Cell::new(0);
    let roots = client::materialize_sections(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &[&artifact.sections[0], &artifact.sections[2]],
        &leases,
        |start, end| {
            requests.set(requests.get() + 1);
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    );
    assert_eq!(requests.get(), 1);
    assert_eq!(roots.len(), 2);
    for root in roots.into_values() {
        assert!(root.unwrap().join("section.bin").is_file());
    }
    let omitted = cache
        .path()
        .join("unity-metadata")
        .join(hex(&artifact.hash))
        .join("sections")
        .join(hex(&artifact.sections[1].hash));
    assert!(!omitted.exists());

    let selected = artifact.sections.iter().collect::<Vec<_>>();
    let missing = &artifact.sections[1];
    let roots = client::materialize_sections(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &selected,
        &leases,
        |start, end| {
            requests.set(requests.get() + 1);
            assert_eq!(start, artifact.header_size + missing.offset);
            assert_eq!(end + 1 - start, missing.size);
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    );
    assert_eq!(requests.get(), 2);
    assert!(roots.into_values().all(|root| root.is_ok()));
    let roots = client::materialize_sections(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &selected,
        &leases,
        |_, _| panic!("Complete cached sections must not acquire network input"),
    );
    assert!(roots.into_values().all(|root| root.is_ok()));
}

fn publication_fixture(output: &Path) -> Artifact {
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for name in ["alpha", "beta", "gamma"] {
        builder
            .add(
                format!("packages/{name}/license.txt"),
                name.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    builder.finish(output).unwrap()
}

#[test]
fn publication_batches_do_not_split_small_http_ranges() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        builder
            .add(
                format!("packages/{name}/file.txt"),
                name.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let requests = std::cell::Cell::new(0);
    let results = client::materialize_sections(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |start, end| {
            requests.set(requests.get() + 1);
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    );
    assert_eq!(requests.get(), 1);
    assert_eq!(results.len(), artifact.sections.len());
    assert!(
        results
            .into_values()
            .all(|result| result.unwrap().join("section.bin").is_file())
    );
}

#[test]
fn section_setup_failures_keep_their_retry_cooldown() {
    let output = tempfile::tempdir().unwrap();
    let mut artifact = publication_fixture(output.path());
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let root = cache
        .path()
        .join("unity-metadata")
        .join(hex(&artifact.hash));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("sections"), b"blocked").unwrap();
    for attempt in 0..2 {
        assert!(
            client::materialize_section_with(
                cache.path(),
                &artifact,
                &Catalog::default(),
                &artifact.sections[0],
                &leases,
                |_, _| panic!("Setup failure or cooldown acquired transport")
            )
            .is_err()
        );
        if attempt == 0 {
            fs::remove_file(root.join("sections")).unwrap();
        }
    }
    artifact.release = "retry".into();
    assert!(
        client::materialize_section_with(
            cache.path(),
            &artifact,
            &Catalog::default(),
            &artifact.sections[0],
            &leases,
            |start, end| {
                Ok(response(
                    bytes[start as usize..=end as usize].to_vec(),
                    Some(format!("bytes {start}-{end}/{}", artifact.size)),
                ))
            }
        )
        .unwrap()
        .join("section.bin")
        .is_file()
    );
}

#[test]
fn section_publication_waits_for_the_transfer_to_finish() {
    let output = tempfile::tempdir().unwrap();
    let mut artifact = publication_fixture(output.path());
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let section = &artifact.sections[0];
    let failed = client::materialize_section_with(
        cache.path(),
        &artifact,
        &Catalog::default(),
        section,
        &leases,
        |start, end| {
            let body = bytes[start as usize..=end as usize].to_vec();
            let length = body.len() + 1;
            Ok(response_with_length(
                body,
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
                length,
            ))
        },
    );
    assert!(failed.is_err());
    let destination = cache
        .path()
        .join("unity-metadata")
        .join(hex(&artifact.hash))
        .join("sections")
        .join(hex(&section.hash));
    assert!(!destination.exists());
    artifact.release = "retry".into();
    let repaired = client::materialize_section_with(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &artifact.sections[0],
        &leases,
        |start, end| {
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    )
    .unwrap();
    assert_eq!(repaired, destination);
}

#[test]
fn cooldown_members_do_not_turn_into_large_unselected_range_gaps() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    let noise: Vec<_> = (0..4096u32)
        .flat_map(|n| *blake3::hash(&n.to_le_bytes()).as_bytes())
        .collect();
    for (name, bytes) in [
        ("a", b"first".to_vec()),
        ("b", noise),
        ("c", b"last".to_vec()),
    ] {
        builder
            .add(format!("packages/{name}/file.txt"), bytes, "package")
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let catalog = Catalog::default();
    assert!(
        client::materialize_section_with(
            cache.path(),
            &artifact,
            &catalog,
            &artifact.sections[1],
            &leases,
            |_, _| anyhow::bail!("unavailable member")
        )
        .is_err()
    );
    let requests = std::cell::RefCell::new(Vec::new());
    let acquired = client::materialize_sections(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |start, end| {
            requests.borrow_mut().push((start, end));
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    );
    assert!(acquired[&artifact.sections[0].role].is_ok());
    assert!(acquired[&artifact.sections[1].role].is_err());
    assert!(acquired[&artifact.sections[2].role].is_ok());
    let skipped = &artifact.sections[1];
    let skipped_start = artifact.header_size + skipped.offset;
    let skipped_end = skipped_start + skipped.size;
    assert!(
        requests
            .borrow()
            .iter()
            .all(|&(start, end)| end < skipped_start || start >= skipped_end)
    );
}

#[test]
fn grouped_sections_keep_good_neighbors_and_retry_failed_members() {
    let output = tempfile::tempdir().unwrap();
    let mut artifact = publication_fixture(output.path());
    let original = fs::read(output.path().join(&artifact.name)).unwrap();
    let mut corrupt = original.clone();
    let middle = &artifact.sections[1];
    corrupt[(artifact.header_size + middle.offset + middle.size / 2) as usize] ^= 1;
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let catalog = Catalog::default();
    let roots = client::materialize_sections(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |start, end| {
            Ok(response(
                corrupt[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    );
    for index in [0, 2] {
        assert!(
            roots[&artifact.sections[index].role]
                .as_ref()
                .unwrap()
                .join("section.bin")
                .is_file()
        );
    }
    assert!(roots[&middle.role].is_err());
    assert!(
        !cache
            .path()
            .join("unity-metadata")
            .join(hex(&artifact.hash))
            .join("sections")
            .join(hex(&middle.hash))
            .exists()
    );
    let reused = client::materialize_sections(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |_, _| panic!("Cached successes and failure acquired transport"),
    );
    assert_eq!(reused.values().filter(|result| result.is_ok()).count(), 2);
    artifact.release = "replacement".into();
    let repaired = client::materialize_section_with(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections[1],
        &leases,
        |start, end| {
            Ok(response(
                original[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    )
    .unwrap();
    assert!(repaired.join("section.bin").is_file());
}

#[test]
fn failed_coalesced_transport_allows_a_single_section_retry() {
    let output = tempfile::tempdir().unwrap();
    let artifact = publication_fixture(output.path());
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let catalog = Catalog::default();
    let failed = client::materialize_sections(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections.iter().collect::<Vec<_>>(),
        &leases,
        |start, end| {
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {}-{end}/{}", start + 1, artifact.size)),
            ))
        },
    );
    assert!(failed.values().all(Result::is_err));
    let repaired = client::materialize_section_with(
        cache.path(),
        &artifact,
        &catalog,
        &artifact.sections[1],
        &leases,
        |start, end| {
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    )
    .unwrap();
    assert!(repaired.join("section.bin").is_file());
}

#[test]
fn overlapping_section_requests_share_the_full_response_fallback() {
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let output = tempfile::tempdir().unwrap();
    let artifact = publication_fixture(output.path());
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let ready = Barrier::new(4);
    let requests = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let jobs: Vec<_> = [vec![0, 1], vec![1, 2], vec![1], vec![2]]
            .into_iter()
            .map(|indices| {
                let (artifact, bytes, cache, ready, requests) =
                    (&artifact, &bytes, &cache, &ready, &requests);
                scope.spawn(move || {
                    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
                    ready.wait();
                    let selected: Vec<_> = indices
                        .iter()
                        .map(|&index| &artifact.sections[index])
                        .collect();
                    let results = client::materialize_sections(
                        cache.path(),
                        artifact,
                        &Catalog::default(),
                        &selected,
                        &leases,
                        |_, _| {
                            requests.fetch_add(1, Ordering::Relaxed);
                            Ok(response(bytes.clone(), None))
                        },
                    );
                    assert_eq!(results.len(), selected.len());
                    assert!(
                        results
                            .into_values()
                            .all(|result| result.unwrap().join("section.bin").is_file())
                    );
                })
            })
            .collect();
        for job in jobs {
            job.join().unwrap();
        }
    });
    assert_eq!(requests.load(Ordering::Relaxed), 1);
}

#[test]
fn selected_sections_publish_independently_and_repair_missing_indexes() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for (name, source) in [("alpha", "class Alpha {}"), ("beta", "class Beta {}")] {
        builder
            .add(
                format!("packages/{name}/Code.cs"),
                source.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    assert_eq!(
        producer::artifacts(output.path()).unwrap()[0].name,
        artifact.name
    );
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let catalog = Catalog {
        artifacts: vec![artifact.clone()],
        ..Default::default()
    };
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let first = &artifact.sections[0];
    let fetch = |start: u64, end: u64| {
        Ok(response(
            bytes[start as usize..=end as usize].to_vec(),
            Some(format!("bytes {start}-{end}/{}", artifact.size)),
        ))
    };
    let path =
        client::materialize_section_with(cache.path(), &artifact, &catalog, first, &leases, fetch)
            .unwrap();
    assert!(path.join("packages/alpha/Code.cs").is_file());
    assert!(!path.join("packages/beta/Code.cs").exists());
    assert_eq!(
        client::materialize_section_with(
            cache.path(),
            &artifact,
            &catalog,
            first,
            &leases,
            |_, _| panic!("cached section downloaded again")
        )
        .unwrap(),
        path
    );
    let mut indexes = Indexes::new(cache.path());
    let (index, ordinal) = indexes
        .file(&path.join("packages/alpha/Code.cs"), || {
            panic!("invariant source hashed again")
        })
        .unwrap()
        .unwrap();
    let store = Store::open_workspace(
        &cache.path().join("analysis"),
        [4; 32],
        Path::new("selected"),
        None,
    )
    .unwrap();
    let store = store
        .with_mounts(
            &[crate::store::segment::Mount {
                path: index.clone(),
                files: BTreeMap::from([("alpha".into(), ordinal)]),
            }],
            cache.path(),
        )
        .unwrap();
    assert!(
        store
            .declarations("alpha")
            .unwrap()
            .iter()
            .any(|declaration| declaration.name == "Alpha")
    );
    fs::remove_file(index).unwrap();
    client::materialize_section_with(cache.path(), &artifact, &catalog, first, &leases, fetch)
        .unwrap();
    assert!(
        store
            .declarations("alpha")
            .unwrap()
            .iter()
            .any(|declaration| declaration.name == "Alpha")
    );
    let mut wrong = artifact.sections[1].clone();
    wrong.hash[0] ^= 1;
    assert!(
        client::materialize_section_with(cache.path(), &artifact, &catalog, &wrong, &leases, fetch)
            .is_err()
    );
    assert!(path.join("section.bin").is_file());
    assert!(!path.parent().unwrap().join(hex(&wrong.hash)).exists());
    assert!(
        !crate::cache::views::Registry::open(cache.path())
            .unwrap()
            .roots()
            .is_empty()
    );
}

#[test]
fn section_import_handles_identical_assembly_references_before_alias_publication() {
    let source = "class Library {}";
    let encoded = records::encode(FileData {
        source: source.into(),
        facts: crate::extract::extract(source, Language::CSharp, &[], "").unwrap(),
        assembly: Some("Library".into()),
    })
    .unwrap();
    let analysis = envelope(
        &binary::encode(&Analysis {
            id: [5; 32],
            encoded,
        })
        .unwrap(),
    );
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for name in ["first", "second"] {
        builder
            .object(format!("{name}/Library.sigla"), &analysis, "framework")
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let root = client::materialize_section_with(
        cache.path(),
        &artifact,
        &Catalog::default(),
        &artifact.sections[0],
        &leases,
        |start, end| {
            Ok(response(
                bytes[start as usize..=end as usize].to_vec(),
                Some(format!("bytes {start}-{end}/{}", artifact.size)),
            ))
        },
    )
    .unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::metadata(root.join("first/Library.sigla"))
            .unwrap()
            .ino(),
        fs::metadata(root.join("second/Library.sigla"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        indexed_assembly(&root, &root.join("first/Library.sigla"))
            .unwrap()
            .id,
        [5; 32]
    );
    assert!(load_analysis(&root.join("first/Library.sigla")).is_err());
}

#[test]
fn range_fallback_downloads_once_and_rejects_malformed_responses() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for name in ["alpha", "beta"] {
        builder
            .add(
                format!("packages/{name}/license.txt"),
                name.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    let bytes = fs::read(output.path().join(&artifact.name)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
    let count = std::cell::Cell::new(0);
    for section in &artifact.sections {
        let path = client::materialize_section_with(
            cache.path(),
            &artifact,
            &Catalog::default(),
            section,
            &leases,
            |_, _| {
                count.set(count.get() + 1);
                Ok(response(bytes.clone(), None))
            },
        )
        .unwrap();
        assert!(path.join("section.bin").is_file());
    }
    assert_eq!(count.get(), 1);
    for truncate in [false, true] {
        let cache = tempfile::tempdir().unwrap();
        let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
        let section = &artifact.sections[0];
        assert!(
            client::materialize_section_with(
                cache.path(),
                &artifact,
                &Catalog::default(),
                section,
                &leases,
                |start, end| {
                    let body_end = if truncate { end - 1 } else { end };
                    Ok(response(
                        bytes[start as usize..=body_end as usize].to_vec(),
                        Some(format!(
                            "bytes {}-{end}/{}",
                            start + u64::from(!truncate),
                            artifact.size
                        )),
                    ))
                }
            )
            .is_err()
        );
        let parent = cache
            .path()
            .join("unity-metadata")
            .join(hex(&artifact.hash))
            .join("sections");
        assert!(fs::read_dir(parent).unwrap().next().is_none());
    }
}

fn package_artifact(
    name: &str,
    version: &str,
    size: u64,
    dependencies: &[(&str, &str)],
) -> Artifact {
    let mut origin = origin();
    origin.name = name.into();
    origin.version = version.into();
    let package = Package {
        name: name.into(),
        version: version.into(),
        minimum_editor: None,
        dependencies: dependencies
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
    };
    Artifact {
        name: format!("{name}-{version}.tar.zst"),
        sections: Vec::new(),
        header_size: 0,
        hash: [0; 32],
        size,
        release: TAG.into(),
        manifest: Manifest {
            origin,
            entries: vec![],
            packages: vec![package],
            package_names: vec![],
            recommended: BTreeMap::new(),
        },
    }
}

#[test]
fn packing_follows_dependencies_and_limits_unwanted_downloads() {
    let artifacts = vec![
        package_artifact("runtime", "1.0.0", 10_000, &[]),
        package_artifact("provider", "1.0.0", 20_000, &[("runtime", "1.0.0")]),
        package_artifact("unrelated", "1.0.0", 10_000, &[]),
        package_artifact("runtime", "2.0.0", 10_000, &[]),
        package_artifact(
            "large-provider",
            "1.0.0",
            2_000_000,
            &[("runtime", "2.0.0")],
        ),
    ];
    let groups = packing::groups(&artifacts, &[]);
    assert!(
        groups
            .iter()
            .any(|g| g.len() == 2 && g.contains(&0) && g.contains(&1))
    );
    for i in 2..artifacts.len() {
        assert!(groups.contains(&vec![i]));
    }
    let pack = Pack {
        name: "existing.tar".into(),
        hash: [0; 32],
        size: 0,
        release: TAG.into(),
        members: vec![artifacts[0].name.clone(), artifacts[1].name.clone()],
    };
    let mut expanded = artifacts.clone();
    expanded.push(package_artifact(
        "another-provider",
        "1.0.0",
        100,
        &[("runtime", "1.0.0")],
    ));
    let groups = packing::groups(&expanded, std::slice::from_ref(&pack));
    assert!(
        groups
            .iter()
            .any(|g| g.len() == 2 && g.contains(&0) && g.contains(&1))
    );
    expanded.remove(0);
    let groups = packing::groups(&expanded, &[pack]);
    let mut members: Vec<_> = groups.into_iter().flatten().collect();
    members.sort();
    assert_eq!(members, (0..expanded.len()).collect::<Vec<_>>());
}

fn saved_packs(artifacts: &[Artifact], groups: &[Vec<usize>]) -> Vec<Pack> {
    groups
        .iter()
        .enumerate()
        .filter(|(_, g)| g.len() > 1)
        .map(|(i, g)| Pack {
            name: format!("pack-{i}.tar"),
            hash: [0; 32],
            size: 0,
            release: TAG.into(),
            members: g.iter().map(|&i| artifacts[i].name.clone()).collect(),
        })
        .collect()
}

#[test]
fn unchanged_dependency_graph_never_creates_additional_packs() {
    let mut artifacts = vec![package_artifact("shared-runtime", "1.0.0", 100, &[])];
    for i in 0..50 {
        let name = format!("com.unity.family.plugin{i}");
        let companion = format!("companion-{i}");
        let mut dependencies = vec![("shared-runtime", "1.0.0")];
        if i > 1 {
            artifacts.push(package_artifact(&companion, "1.0.0", 100, &[]));
            dependencies.push((&companion, "1.0.0"));
        }
        artifacts.push(package_artifact(&name, "1.0.0", 100, &dependencies));
    }
    let first = packing::groups(&artifacts, &[]);
    let repeated = packing::groups(&artifacts, &saved_packs(&artifacts, &first));
    assert_eq!(first, repeated);
}

#[test]
fn packs_preserve_compressed_bytes_and_individual_analysis() {
    let output = tempfile::tempdir().unwrap();
    let mut artifacts = Vec::new();
    for name in ["runtime", "provider"] {
        let mut origin = origin();
        origin.name = name.into();
        let mut builder = bundle::Builder::new(origin, vec![vec![]]).unwrap();
        builder
            .add(
                format!("packages/{name}/Code.cs"),
                b"class C {}".to_vec(),
                "package",
            )
            .unwrap();
        builder
            .add(
                format!("packages/{name}-docs/license.txt"),
                b"license".to_vec(),
                "package",
            )
            .unwrap();
        artifacts.push(builder.finish(output.path()).unwrap());
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    let pack = packing::build(&artifacts.iter().collect::<Vec<_>>(), output.path(), |a| {
        Ok(output.path().join(&a.name))
    })
    .unwrap();
    let catalog = Catalog {
        artifacts,
        packs: vec![pack.clone()],
        unavailable: vec![],
    };
    let decoded = Catalog::decode(&catalog.encode().unwrap()).unwrap();
    assert_eq!(decoded.packs[0].members, pack.members);
    let extracted = tempfile::tempdir().unwrap();
    packing::unpack(
        &output.path().join(&pack.name),
        extracted.path(),
        &pack,
        &catalog,
    )
    .unwrap();
    for artifact in &catalog.artifacts {
        assert_eq!(
            fs::read(extracted.path().join(&artifact.name)).unwrap(),
            fs::read(output.path().join(&artifact.name)).unwrap()
        );
        let contents = tempfile::tempdir().unwrap();
        bundle::unpack(
            &extracted.path().join(&artifact.name),
            contents.path(),
            Some(artifact),
        )
        .unwrap();
        let path = contents.path().join(format!(
            "packages/{}/Code.cs",
            artifact.manifest.origin.name
        ));
        let id = crate::store::source_id("class C {}", Language::CSharp, &[], "").unwrap();
        assert!(
            indexed_source(contents.path(), &path, &id)
                .unwrap()
                .is_some()
        );
    }
    // Concurrent members share the pack download but acquire their extraction
    // permits separately. Seed compressed transport data to keep this offline.
    let cache = tempfile::tempdir().unwrap();
    packing::unpack(
        &output.path().join(&pack.name),
        &cache
            .path()
            .join("unity-metadata-packs")
            .join(hex(&pack.hash))
            .join("contents"),
        &pack,
        &catalog,
    )
    .unwrap();
    let start = std::sync::Barrier::new(catalog.artifacts.len());
    for artifact in &catalog.artifacts {
        let root = cache
            .path()
            .join("unity-metadata")
            .join(hex(&artifact.hash));
        // The same release can replace a standalone download with a pack.
        assert!(
            crate::acquisition::shared_versioned::<()>(
                &root.to_string_lossy(),
                &artifact.release,
                || anyhow::bail!("Standalone asset was retired")
            )
            .is_err()
        );
        for section in &artifact.sections {
            assert!(
                crate::acquisition::shared_versioned::<()>(
                    &root
                        .join("sections")
                        .join(hex(&section.hash))
                        .to_string_lossy(),
                    &artifact.release,
                    || anyhow::bail!("Standalone section was retired"),
                )
                .is_err()
            );
        }
    }
    std::thread::scope(|scope| {
        for artifact in &catalog.artifacts {
            let (catalog, cache, start) = (&catalog, cache.path(), &start);
            scope.spawn(move || {
                start.wait();
                let leases = crate::cache::views::LeaseSet::new(cache).unwrap();
                let path = client::materialize(cache, artifact, catalog, &leases).unwrap();
                assert!(path.join("manifest.bin").is_file());
                let root = path.parent().unwrap();
                crate::cache::records::write(
                    &root.join("last-use"),
                    &crate::cache::policy::Usage::new(0),
                )
                .unwrap();
                assert_eq!(
                    client::materialize(cache, artifact, catalog, &leases).unwrap(),
                    path
                );
                let sections = client::materialize_sections(
                    cache,
                    artifact,
                    catalog,
                    &artifact.sections.iter().collect::<Vec<_>>(),
                    &leases,
                    |_, _| panic!("A cached pack must supply its sections"),
                );
                assert_eq!(sections.len(), artifact.sections.len());
                assert!(
                    sections
                        .into_values()
                        .all(|root| root.unwrap().join("section.bin").is_file())
                );
                assert!(crate::cache::views::metadata_last_use(root).unwrap() > 0);
                // Usage bookkeeping failures must not make cached inputs unavailable.
                fs::remove_file(root.join("last-use")).unwrap();
                fs::create_dir(root.join("last-use")).unwrap();
                assert_eq!(
                    client::materialize(cache, artifact, catalog, &leases).unwrap(),
                    path
                );
            });
        }
    });
    let mut broken = pack.clone();
    broken.members.pop();
    let invalid = tempfile::tempdir().unwrap();
    assert!(
        packing::unpack(
            &output.path().join(&pack.name),
            invalid.path(),
            &broken,
            &catalog
        )
        .is_err()
    );
    broken = pack.clone();
    broken.hash[0] ^= 1;
    assert!(
        packing::unpack(
            &output.path().join(&pack.name),
            invalid.path(),
            &broken,
            &catalog
        )
        .is_err()
    );
}

#[test]
fn relocated_archives_retry_without_waiting_for_old_failure_cache() {
    let cache = tempfile::tempdir().unwrap();
    let identity = cache.path().to_string_lossy();
    assert!(
        crate::acquisition::shared_versioned::<()>(&identity, "old", || anyhow::bail!(
            "Asset was removed"
        ))
        .is_err()
    );
    assert!(
        crate::acquisition::shared_versioned::<()>(&identity, "old", || panic!(
            "Unchanged sources retain their failure cache"
        ))
        .is_err()
    );
    crate::acquisition::shared_versioned(&identity, "new", || Ok(())).unwrap();
}

#[test]
fn catalog_rejects_obsolete_layout() {
    #[derive(Serialize)]
    struct Legacy {
        artifacts: Vec<Artifact>,
        unavailable: Vec<String>,
    }
    let legacy = Legacy {
        artifacts: vec![package_artifact("runtime", "1.0.0", 10, &[])],
        unavailable: vec![],
    };
    assert!(Catalog::decode(&binary::encode(&legacy).unwrap()).is_err());
}

#[test]
#[ignore = "requires SIGLA_TEST_CATALOG pointing to a downloaded release catalog"]
fn live_catalog_packing_retains_all_packages_with_headroom() {
    let bytes = fs::read(std::env::var("SIGLA_TEST_CATALOG").unwrap()).unwrap();
    let catalog = Catalog::decode(payload(&bytes).unwrap()).unwrap();
    let groups = packing::groups(&catalog.artifacts, &catalog.packs);
    println!(
        "{} logical bundles -> {} assets including catalog",
        catalog.artifacts.len(),
        groups.len() + 1
    );
    assert!(groups.len() + 1 < 1000);
    assert_eq!(
        groups,
        packing::groups(
            &catalog.artifacts,
            &saved_packs(&catalog.artifacts, &groups)
        )
    );
    let mut members: Vec<_> = groups.into_iter().flatten().collect();
    members.sort();
    assert_eq!(members, (0..catalog.artifacts.len()).collect::<Vec<_>>());
}

#[test]
fn precomputed_sources_preserve_navigation_and_profile_selection() {
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![], vec!["ENABLED".into()]]).unwrap();
    let csharp = "#if ENABLED\nclass Enabled { public int Value() { return 42; } }\n#else\nclass Disabled {}\n#endif\n";
    let shader =
        "float Scale(float value) { return value * 2.0; }\nfloat Run() { return Scale(1.0); }\n";
    for (path, source) in [("Code.cs", csharp), ("Lighting.hlsl", shader)] {
        builder
            .add(
                format!("packages/com.example.fixture/{path}"),
                source.as_bytes().to_vec(),
                "package",
            )
            .unwrap();
    }
    let artifact = builder.finish(output.path()).unwrap();
    assert!(artifact.manifest.entries.is_empty());
    let cache = tempfile::tempdir().unwrap();
    let manifest = bundle::unpack(
        &output.path().join(&artifact.name),
        cache.path(),
        Some(&artifact),
    )
    .unwrap();
    assert_eq!(
        bundle::verify(&output.path().join(&artifact.name), Some(&artifact)).unwrap(),
        manifest
    );
    assert_eq!(manifest.entries.len(), 4);
    let db = tempfile::tempdir().unwrap();
    let store = Store::open_workspace(db.path(), [1; 32], Path::new("fixture"), None).unwrap();
    for (file, source, language, defines) in [
        ("Code.cs", csharp, Language::CSharp, vec![]),
        (
            "Code.cs",
            csharp,
            Language::CSharp,
            vec!["ENABLED".into(), "IRRELEVANT".into()],
        ),
        ("Lighting.hlsl", shader, Language::Hlsl, vec![]),
    ] {
        let path = cache.path().join("packages/com.example.fixture").join(file);
        let id = crate::store::source_id(source, language, &defines, "").unwrap();
        let encoded = indexed_source(cache.path(), &path, &id).unwrap().unwrap();
        let facts = crate::extract::extract(source, language, &defines, "").unwrap();
        let expected = records::encode(FileData {
            source: source.into(),
            facts,
            assembly: None,
        })
        .unwrap();
        assert_eq!(encoded.names, expected.names);
        assert_eq!(encoded.records, expected.records);
        store
            .install_encoded(file, &id, id, || Ok(()), || Ok(encoded))
            .unwrap();
        let tx = store.read().unwrap();
        for (key, bytes) in &expected.records {
            assert_eq!(store.scope.record(&tx, file, key).unwrap().unwrap(), bytes);
        }
        assert_eq!(store.load(&tx, file).unwrap().unwrap().source, source);
        if language == Language::CSharp && !defines.is_empty() {
            assert!(
                store
                    .csharp_body(&tx, file, source.find("42").unwrap())
                    .unwrap()
                    .is_some()
            );
        }
    }
    let path = cache.path().join("packages/com.example.fixture/Code.cs");
    let changed = crate::store::source_id("class Changed {}", Language::CSharp, &[], "").unwrap();
    assert!(
        indexed_source(cache.path(), &path, &changed)
            .unwrap()
            .is_none()
    );
}

#[test]
fn streamed_bundles_validate_before_publishing_and_share_duplicate_files() {
    use std::os::unix::fs::MetadataExt;
    let archive = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    let source = b"class Example { int Value => 1; }\n";
    for path in ["first/Code.cs", "second/Code.cs"] {
        builder
            .add(path.into(), source.to_vec(), "package")
            .unwrap();
    }
    let artifact = builder.finish(archive.path()).unwrap();
    let bytes = fs::read(archive.path().join(&artifact.name)).unwrap();
    let reference = tempfile::tempdir().unwrap();
    let manifest = bundle::unpack(
        &archive.path().join(&artifact.name),
        reference.path(),
        Some(&artifact),
    )
    .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let rejected = cache.path().join("rejected");
    let store = crate::cache::blobs::Store::open(cache.path()).unwrap();
    let mut import = store.owned_tree(&rejected).unwrap();
    let mut wrong_manifest = artifact.clone();
    wrong_manifest.name = format!(
        "{}-{}.tar.zst",
        artifact.manifest.origin.kind,
        "0".repeat(64)
    );
    assert!(
        bundle::unpack_stream(bytes.as_slice(), &rejected, &wrong_manifest, &mut import).is_err()
    );
    assert!(fs::read_dir(&rejected).unwrap().next().is_none());
    drop(import);
    let output = cache.path().join("stage");
    let store = crate::cache::blobs::Store::open(cache.path()).unwrap();
    let mut import = store.owned_tree(&output).unwrap();
    bundle::unpack_stream(bytes.as_slice(), &output, &artifact, &mut import).unwrap();
    let first = output.join("first/Code.cs");
    assert!(
        crate::cache::blobs::identity(&first, &fs::metadata(&first).unwrap())
            .unwrap()
            .is_none()
    );
    import.publish().unwrap();
    for path in manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .chain(["manifest.bin"])
    {
        assert_eq!(
            fs::read(output.join(path)).unwrap(),
            fs::read(reference.path().join(path)).unwrap()
        );
        assert_eq!(
            fs::metadata(output.join(path)).unwrap().mode() & 0o777,
            fs::metadata(reference.path().join(path)).unwrap().mode() & 0o777
        );
    }
    assert_eq!(
        fs::metadata(&first).unwrap().ino(),
        fs::metadata(output.join("second/Code.cs")).unwrap().ino()
    );
    assert!(
        crate::cache::blobs::identity(&first, &fs::metadata(&first).unwrap())
            .unwrap()
            .is_some()
    );

    for truncated in [false, true] {
        let cache = tempfile::tempdir().unwrap();
        let output = cache.path().join("stage");
        let store = crate::cache::blobs::Store::open(cache.path()).unwrap();
        let mut import = store.owned_tree(&output).unwrap();
        let mut expected = artifact.clone();
        expected.hash[0] ^= 1;
        let input = if truncated {
            &bytes[..bytes.len() / 2]
        } else {
            bytes.as_slice()
        };
        assert!(bundle::unpack_stream(input, &output, &expected, &mut import).is_err());
        drop(import);
        assert!(
            !fs::read_dir(cache.path().join("blobs"))
                .unwrap()
                .any(|entry| entry.unwrap().file_type().unwrap().is_dir())
        );
    }
}

#[test]
fn streamed_bundles_reject_tar_extensions() {
    let archive = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add("license.txt".into(), b"license".to_vec(), "package")
        .unwrap();
    let artifact = builder.finish(archive.path()).unwrap();
    let bytes = fs::read(archive.path().join(&artifact.name)).unwrap();
    let raw = zstd::decode_all(&bytes[artifact.header_size as usize..]).unwrap();
    for kind in [
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
        tar::EntryType::XHeader,
    ] {
        for before in [0, 1] {
            let mut source = tar::Archive::new(raw.as_slice());
            let mut output = tar::Builder::new(Vec::new());
            for (index, item) in source.entries().unwrap().enumerate() {
                let mut item = item.unwrap();
                if index == before {
                    let body = match kind {
                        tar::EntryType::GNULongName => {
                            let mut path = item.path_bytes().into_owned();
                            path.push(0);
                            path
                        }
                        tar::EntryType::GNULongLink => b"unused\0".to_vec(),
                        _ => b"19 comment=ignored\n".to_vec(),
                    };
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(kind);
                    header.set_size(body.len() as u64);
                    header.set_mode(0o644);
                    output
                        .append_data(&mut header, "extension", body.as_slice())
                        .unwrap();
                }
                output.append(&item.header().clone(), &mut item).unwrap();
            }
            let bytes = zstd::encode_all(output.into_inner().unwrap().as_slice(), 1).unwrap();
            let expected = Section {
                hash: *blake3::hash(&bytes).as_bytes(),
                size: bytes.len() as u64,
                ..artifact.sections[0].clone()
            };
            let cache = tempfile::tempdir().unwrap();
            let output = cache.path().join("stage");
            let store = crate::cache::blobs::Store::open(cache.path()).unwrap();
            let mut import = store.owned_tree(&output).unwrap();
            assert!(
                bundle::inspect_section(
                    bytes.as_slice(),
                    Some(&output),
                    &expected,
                    Some(&mut import)
                )
                .is_err()
            );
            assert!(fs::read_dir(output).unwrap().next().is_none());
        }
    }
}

#[test]
fn streamed_bundles_reject_missing_or_excessive_archive_padding() {
    let archive = tempfile::tempdir().unwrap();
    let artifact = bundle::Builder::new(origin(), vec![vec![]])
        .unwrap()
        .finish(archive.path())
        .unwrap();
    let bytes = fs::read(archive.path().join(&artifact.name)).unwrap();
    let raw = zstd::decode_all(&bytes[artifact.header_size as usize..]).unwrap();
    for change in 0..3 {
        let mut changed = raw.clone();
        match change {
            0 => {
                *changed.last_mut().unwrap() = 1;
            }
            1 => {
                changed.extend_from_slice(&[0; 512]);
            }
            _ => {
                changed.truncate(changed.len() - 512);
            }
        }
        let bytes = zstd::encode_all(changed.as_slice(), 1).unwrap();
        let expected = Section {
            hash: *blake3::hash(&bytes).as_bytes(),
            size: bytes.len() as u64,
            ..artifact.sections[0].clone()
        };
        let cache = tempfile::tempdir().unwrap();
        let output = cache.path().join("stage");
        let store = crate::cache::blobs::Store::open(cache.path()).unwrap();
        let mut import = store.owned_tree(&output).unwrap();
        assert!(
            bundle::inspect_section(
                bytes.as_slice(),
                Some(&output),
                &expected,
                Some(&mut import)
            )
            .is_err()
        );
        assert!(fs::read_dir(output).unwrap().next().is_none());
    }
}

#[test]
fn publication_rejects_changed_bundle_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add("LICENSE".into(), b"License text".to_vec(), "package")
        .unwrap();
    let artifact = builder.finish(dir.path()).unwrap();
    let path = dir.path().join(&artifact.name);
    bundle::verify_digest(&path, &artifact).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(bundle::verify_digest(&path, &artifact).is_err());
    bytes.pop();
    fs::write(&path, &bytes).unwrap();
    assert!(bundle::verify_digest(&path, &artifact).is_err());
}

#[test]
fn producer_rejects_invalid_analysis_records() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add("Broken.cs.analysis".into(), vec![0], "analysis")
        .unwrap();
    assert!(builder.finish(dir.path()).is_err());
}

#[test]
fn producer_checks_source_profile_ownership_and_package_inventory() {
    let source = "class Profile {}";
    let id = records::source_id(source, Language::CSharp, &[], "").unwrap();
    let encoded = records::encode(FileData {
        source: source.into(),
        facts: crate::extract::extract(source, Language::CSharp, &[], "").unwrap(),
        assembly: None,
    })
    .unwrap();
    let profiles =
        envelope(&binary::encode(&vec![(id, binary::encode(&encoded).unwrap())]).unwrap());
    for matches in [true, false] {
        let output = tempfile::tempdir().unwrap();
        let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
        builder
            .object(
                "Code.cs".into(),
                if matches {
                    source.as_bytes()
                } else {
                    b"class Different {}"
                },
                "package",
            )
            .unwrap();
        builder
            .object("Code.cs.analysis".into(), &profiles, "analysis")
            .unwrap();
        assert_eq!(builder.finish(output.path()).is_ok(), matches);
    }
    for matches in [true, false] {
        let output = tempfile::tempdir().unwrap();
        let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
        builder.manifest.packages.push(Package {
            name: "fixture".into(),
            version: if matches { "1.0.0" } else { "2.0.0" }.into(),
            minimum_editor: None,
            dependencies: BTreeMap::new(),
        });
        builder
            .add(
                "packages/fixture/package.json".into(),
                br#"{"name":"fixture","version":"1.0.0"}"#.to_vec(),
                "package",
            )
            .unwrap();
        assert_eq!(builder.finish(output.path()).is_ok(), matches);
    }
}

#[test]
fn consumer_copies_analysis_while_producer_verification_checks_its_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bundle.tar.zst");
    let mut encoded = records::encode(FileData {
        source: "class C {}".into(),
        facts: Default::default(),
        assembly: None,
    })
    .unwrap();
    encoded.records.insert(vec![1], vec![255]);
    let analysis =
        envelope(&binary::encode(&vec![([1u8; 32], binary::encode(&encoded).unwrap())]).unwrap());
    let hash = sections::object_hash(&analysis);
    let source = b"class C {}";
    let source_hash = sections::object_hash(source);
    let manifest = sections::Frame {
        package: None,
        role: "package".into(),
        entries: ["Code.cs.analysis", "nested/Copy.cs.analysis"]
            .into_iter()
            .map(|path| Entry {
                path: path.into(),
                hash,
                size: analysis.len() as u64,
                group: "analysis".into(),
            })
            .chain(["Code.cs", "nested/Copy.cs"].into_iter().map(|path| Entry {
                path: path.into(),
                hash: source_hash,
                size: source.len() as u64,
                group: "package".into(),
            }))
            .collect(),
    };
    let manifest_bytes = envelope(&binary::encode(&manifest).unwrap());
    let mut tar =
        tar::Builder::new(zstd::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap());
    for (name, bytes) in [
        ("section.bin".to_owned(), manifest_bytes.clone()),
        (format!("objects/{}", hex(&hash)), analysis.clone()),
        (format!("objects/{}", hex(&source_hash)), source.to_vec()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    let output = tempfile::tempdir().unwrap();
    let expected = Section {
        role: manifest.role,
        offset: 0,
        size: fs::metadata(&path).unwrap().len(),
        hash: digest(&path).unwrap(),
        manifest_hash: *blake3::hash(&manifest_bytes).as_bytes(),
    };
    bundle::inspect_section(
        fs::File::open(&path).unwrap(),
        Some(output.path()),
        &expected,
        None,
    )
    .unwrap();
    assert_eq!(
        fs::read(output.path().join("Code.cs.analysis")).unwrap(),
        analysis
    );
    assert_eq!(
        fs::read(output.path().join("nested/Copy.cs.analysis")).unwrap(),
        analysis
    );
    assert!(
        bundle::inspect_section(fs::File::open(&path).unwrap(), None, &expected, None).is_err()
    );
}

#[test]
fn verification_rejects_conflicting_file_and_directory_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for path in ["directory", "directory/file"] {
        builder.add(path.into(), vec![1], "package").unwrap();
    }
    assert!(builder.finish(dir.path()).is_err());
}

#[test]
fn source_profiles_keep_preprocessor_selection() {
    let source = "#if ENABLED\nclass Enabled {}\n#else\nclass Disabled {}\n#endif\n";
    let defines = vec!["ENABLED".into()];
    let id = records::source_id(source, Language::CSharp, &defines, "").unwrap();
    let encoded = records::encode(FileData {
        source: source.into(),
        facts: crate::extract::extract(source, Language::CSharp, &defines, "").unwrap(),
        assembly: None,
    })
    .unwrap();
    let bytes = envelope(&binary::encode(&vec![(id, binary::encode(&encoded).unwrap())]).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Code.cs");
    fs::write(dir.path().join("Code.cs.analysis"), bytes).unwrap();
    for (text, defines, matches) in [
        (source, vec!["ENABLED".into(), "IRRELEVANT".into()], true),
        (source, vec![], false),
        ("class Changed {}", vec!["ENABLED".into()], false),
    ] {
        let id = records::source_id(text, Language::CSharp, &defines, "").unwrap();
        let restored = source_analysis(&path, &id).unwrap();
        assert_eq!(restored.is_some(), matches);
        if let Some(restored) = restored {
            assert_eq!(restored.names, encoded.names);
            assert_eq!(restored.records, encoded.records);
        }
    }
}

#[test]
fn records_reject_corruption_incompatibility_and_excessive_recursion() {
    let bytes = envelope(&binary::encode(&vec!["catalog".to_owned()]).unwrap());
    for position in [0, 8, HEADER - 1, bytes.len() - 1] {
        let mut corrupt = bytes.clone();
        corrupt[position] ^= 1;
        assert!(payload(&corrupt).is_err());
    }
    assert!(payload(&bytes[..HEADER - 1]).is_err());
    use crate::csharp::types::WrittenType;
    let leaf = binary::encode(&WrittenType::Dynamic).unwrap();
    let pointer = binary::encode(&WrittenType::Pointer(Box::new(WrittenType::Dynamic))).unwrap();
    let mut bytes = pointer[..pointer.len() - leaf.len()].repeat(1024);
    bytes.extend_from_slice(&leaf);
    assert!(binary::decode::<crate::csharp::types::WrittenType>(&bytes).is_err());
    let bytes = binary::encode(&vec!["unaligned".to_owned()]).unwrap();
    let mut shifted = vec![0];
    shifted.extend_from_slice(&bytes);
    assert_eq!(
        binary::decode::<Vec<String>>(&shifted[1..]).unwrap(),
        vec!["unaligned"]
    );
}

#[test]
fn oversized_compression_windows_are_rejected() {
    use std::io::{Read, Write};
    let manifest = sections::Frame {
        package: None,
        role: "empty".into(),
        entries: Vec::new(),
    };
    let mut tar = tar::Builder::new(Vec::new());
    let bytes = envelope(&binary::encode(&manifest).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "section.bin", bytes.as_slice())
        .unwrap();
    let raw = tar.into_inner().unwrap();
    let expected = Section {
        role: manifest.role,
        offset: 0,
        size: 0,
        hash: [0; 32],
        manifest_hash: *blake3::hash(&bytes).as_bytes(),
    };
    let valid = zstd::encode_all(raw.as_slice(), 1).unwrap();
    bundle::inspect_section(valid.as_slice(), None, &expected, None).unwrap();
    let mut compressor = zstd::Encoder::new(Vec::new(), 1).unwrap();
    compressor.window_log(30).unwrap();
    compressor.write_all(&raw).unwrap();
    compressor.flush().unwrap();
    let compressed = compressor.finish().unwrap();
    let mut unrestricted = zstd::Decoder::new(compressed.as_slice()).unwrap();
    unrestricted.window_log_max(30).unwrap();
    let mut restored = Vec::new();
    unrestricted.read_to_end(&mut restored).unwrap();
    assert_eq!(restored, raw);
    assert!(bundle::inspect_section(compressed.as_slice(), None, &expected, None).is_err());
}

#[test]
fn selecting_a_profile_does_not_decode_other_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Code.cs");
    let encoded = records::encode(FileData {
        source: String::new(),
        facts: Default::default(),
        assembly: None,
    })
    .unwrap();
    let selected = [1; 32];
    let other = [2; 32];
    save(
        &dir.path().join("Code.cs.analysis"),
        &binary::encode(&vec![
            (selected, binary::encode(&encoded).unwrap()),
            (other, vec![255]),
        ])
        .unwrap(),
    )
    .unwrap();
    let restored = source_analysis(&path, &selected).unwrap().unwrap();
    assert_eq!(restored.records, encoded.records);
    assert!(source_analysis(&path, &other).is_err());
}

#[test]
fn editor_only_generated_directives_do_not_block_other_profiles() {
    let output = tempfile::tempdir().unwrap();
    let defines = vec!["UNITY_EDITOR".into()];
    let mut builder = bundle::Builder::new(origin(), vec![vec![], defines.clone()]).unwrap();
    let source = "#if UNITY_EDITOR\nclass Generator { string text = @\"\n#if GENERATED\nclass Generated {}\n\"; }\n#endif\n";
    builder
        .add("Generator.cs".into(), source.as_bytes().to_vec(), "package")
        .unwrap();
    let artifact = builder.finish(output.path()).unwrap();
    let cache = tempfile::tempdir().unwrap();
    bundle::unpack(&output.path().join(artifact.name), cache.path(), None).unwrap();
    let path = cache.path().join("Generator.cs");
    assert_eq!(fs::read_to_string(&path).unwrap(), source);
    let id = crate::store::source_id(source, Language::CSharp, &defines, "").unwrap();
    assert!(indexed_source(cache.path(), &path, &id).unwrap().is_some());
}

#[test]
fn bundles_reject_missing_duplicate_and_escaping_objects() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add(
            "packages/com.example.fixture/Code.cs".into(),
            b"class C {}".to_vec(),
            "package",
        )
        .unwrap();
    let artifact = builder.finish(dir.path()).unwrap();
    let mut wrong = artifact.clone();
    wrong.hash[0] ^= 1;
    assert!(bundle::verify(&dir.path().join(&artifact.name), Some(&wrong)).is_err());
    let manifest = bundle::verify(&dir.path().join(&artifact.name), Some(&artifact)).unwrap();
    for escaped in [false, true] {
        let mut manifest = sections::Frame {
            package: None,
            role: artifact.sections[0].role.clone(),
            entries: manifest.entries.clone(),
        };
        if escaped {
            manifest.entries[0].path = "../escape".into();
        }
        let path = dir.path().join("invalid.tar.zst");
        let mut tar = tar::Builder::new(
            zstd::stream::write::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap(),
        );
        let bytes = envelope(&binary::encode(&manifest).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "section.bin", bytes.as_slice())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
        let expected = Section {
            manifest_hash: *blake3::hash(&bytes).as_bytes(),
            ..artifact.sections[0].clone()
        };
        assert!(
            bundle::inspect_section(fs::File::open(&path).unwrap(), None, &expected, None).is_err()
        );
        let extracted = tempfile::tempdir().unwrap();
        assert!(
            bundle::inspect_section(
                fs::File::open(&path).unwrap(),
                Some(extracted.path()),
                &expected,
                None
            )
            .is_err()
        );
    }
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add("NOTICE".into(), b"first".to_vec(), "license")
        .unwrap();
    assert!(
        builder
            .add("NOTICE".into(), b"second".to_vec(), "license")
            .is_err()
    );
}

#[test]
#[ignore = "requires tests/metadata-fixture to be built"]
fn precomputed_assembly_matches_local_pe_analysis() {
    let path = Path::new("tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll");
    let bytes = fs::read(path).unwrap();
    let expected = records::encode(
        crate::metadata::file_data_bytes(bytes.clone(), "MetadataFixture").unwrap(),
    )
    .unwrap();
    let output = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    builder
        .add(
            "packages/com.example.fixture/Code.cs".into(),
            b"class User { public int Run() { return 42; } }".to_vec(),
            "package",
        )
        .unwrap();
    builder
        .add(
            "packages/com.example.fixture/Lighting.hlsl".into(),
            b"float Run(float x) { return x * 2; }".to_vec(),
            "package",
        )
        .unwrap();
    builder
        .add(
            "packages/com.example.fixture/MetadataFixture.dll".into(),
            bytes,
            "package",
        )
        .unwrap();
    let artifact = builder.finish(output.path()).unwrap();
    let dest = tempfile::tempdir().unwrap();
    bundle::unpack(
        &output.path().join(&artifact.name),
        dest.path(),
        Some(&artifact),
    )
    .unwrap();
    let analysis = indexed_assembly(
        dest.path(),
        &dest
            .path()
            .join("packages/com.example.fixture/MetadataFixture.sigla"),
    )
    .unwrap();
    assert_eq!(analysis.encoded.records, expected.records);
    assert_eq!(analysis.encoded.names, expected.names);
    if let Some(path) = std::env::var_os("SIGLA_TEST_BUNDLE") {
        fs::copy(output.path().join(&artifact.name), path).unwrap();
    }
}
