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
        let encoded = source_analysis(&path, &id).unwrap().unwrap();
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
    assert!(source_analysis(&path, &changed).unwrap().is_none());
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
    let mut ty = crate::csharp::types::WrittenType::Dynamic;
    for _ in 0..256 {
        ty = crate::csharp::types::WrittenType::Pointer(Box::new(ty));
    }
    let bytes = binary::encode(&ty).unwrap();
    assert!(binary::view::<crate::csharp::types::WrittenType>(&bytes).is_err());
    let bytes = binary::encode(&vec!["unaligned".to_owned()]).unwrap();
    let mut shifted = vec![0];
    shifted.extend_from_slice(&bytes);
    assert_eq!(
        binary::decode::<Vec<String>>(&shifted[1..]).unwrap(),
        vec!["unaligned"]
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

#[test]
#[ignore = "manual record-format benchmark"]
fn record_format_benchmark() {
    use std::{hint::black_box, time::Instant};
    let path = Path::new("tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll");
    let data = crate::metadata::file_data(path).unwrap();
    let archived = binary::encode(&data).unwrap();
    let postcard = postcard::to_allocvec(&data).unwrap();
    let compressed = zstd::encode_all(postcard.as_slice(), 1).unwrap();
    let wire = zstd::encode_all(archived.as_slice(), 12).unwrap();
    let start = Instant::now();
    for _ in 0..1000 {
        black_box(binary::decode::<FileData>(black_box(&archived)).unwrap());
    }
    let archived_read = start.elapsed();
    let start = Instant::now();
    for _ in 0..1000 {
        black_box(binary::view::<FileData>(black_box(&archived)).unwrap());
    }
    let borrowed_read = start.elapsed();
    let start = Instant::now();
    for _ in 0..1000 {
        let bytes = zstd::decode_all(black_box(compressed.as_slice())).unwrap();
        black_box(postcard::from_bytes::<FileData>(&bytes).unwrap());
    }
    let old_read = start.elapsed();
    eprintln!(
        "rkyv={} bytes, transport={} bytes, old_store={} bytes, 1000 reads: rkyv_owned={archived_read:?}, rkyv_borrowed={borrowed_read:?}, postcard+zstd={old_read:?}",
        archived.len(),
        wire.len(),
        compressed.len()
    );
}
