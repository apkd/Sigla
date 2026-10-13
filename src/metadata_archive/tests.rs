use super::*;
use crate::{
    model::Language,
    store::{FileData, Store, payload as records},
};

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
        let encoded = source_analysis(&path, &id, |_| unreachable!())
            .unwrap()
            .unwrap();
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
        source_analysis(&path, &changed, |_| unreachable!())
            .unwrap()
            .is_none()
    );
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
fn verification_rejects_conflicting_file_and_directory_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = bundle::Builder::new(origin(), vec![vec![]]).unwrap();
    for path in ["directory", "directory/file"] {
        builder.add(path.into(), vec![1], "package").unwrap();
    }
    assert!(builder.finish(dir.path()).is_err());
}

#[test]
fn compatible_source_profiles_keep_preprocessor_selection() {
    let version = "d70ac251b161d05505fdc5790766a45bdd917c9e97093b7df3751e7479131b09";
    if !crate::store::compatibility::accepts(version.as_bytes()) {
        return;
    }
    let source = "#if ENABLED\nclass Enabled {}\n#else\nclass Disabled {}\n#endif\n";
    let defines = vec!["ENABLED".into()];
    let id =
        records::source_id_at_version(source, Language::CSharp, &defines, "", version).unwrap();
    let encoded = records::encode(FileData {
        source: source.into(),
        facts: crate::extract::extract(source, Language::CSharp, &defines, "").unwrap(),
        assembly: None,
    })
    .unwrap();
    let mut bytes =
        envelope(&binary::encode(&vec![(id, binary::encode(&encoded).unwrap())]).unwrap());
    bytes[8..72].copy_from_slice(version.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Code.cs");
    fs::write(dir.path().join("Code.cs.analysis"), bytes).unwrap();
    for (text, defines, matches) in [
        (source, vec!["ENABLED".into(), "IRRELEVANT".into()], true),
        (source, vec![], false),
        ("class Changed {}", vec!["ENABLED".into()], false),
    ] {
        let id = records::source_id(text, Language::CSharp, &defines, "").unwrap();
        let restored = source_analysis(&path, &id, |version| {
            records::source_id_at_version(text, Language::CSharp, &defines, "", version)
        })
        .unwrap();
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
    let manifest = Manifest {
        origin: origin(),
        entries: Vec::new(),
        packages: Vec::new(),
        package_names: Vec::new(),
        recommended: BTreeMap::new(),
    };
    let mut tar = tar::Builder::new(Vec::new());
    let bytes = envelope(&binary::encode(&manifest).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.bin", bytes.as_slice())
        .unwrap();
    let raw = tar.into_inner().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized-window.tar.zst");
    fs::write(&path, compressed).unwrap();
    assert!(bundle::verify(&path, None).is_err());
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
    let restored = source_analysis(&path, &selected, |_| unreachable!())
        .unwrap()
        .unwrap();
    assert_eq!(restored.records, encoded.records);
    assert!(source_analysis(&path, &other, |_| unreachable!()).is_err());
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
    assert!(
        source_analysis(&path, &id, |_| unreachable!())
            .unwrap()
            .is_some()
    );
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
        let mut manifest = manifest.clone();
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
        tar.append_data(&mut header, "manifest.bin", bytes.as_slice())
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
        assert!(bundle::verify(&path, None).is_err());
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
    let analysis = load_analysis(
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
