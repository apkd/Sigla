use super::*;
use crate::{
    model::Language,
    store::{FileData, Store},
};

fn analysis(source: &str) -> (ObjectId, Encoded) {
    (
        payload::source_id(source, Language::CSharp, &[], "").unwrap(),
        payload::encode(FileData {
            source: source.into(),
            facts: crate::extract::extract(source, Language::CSharp, &[], "").unwrap(),
            assembly: None,
        })
        .unwrap(),
    )
}

fn fixture(root: &Path) -> (PathBuf, [u32; 2]) {
    let path = root.join("unity-metadata/fixture/sections/source/indexes/main.sigla-index");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut builder = Builder::new(root).unwrap();
    let mut ordinals = [0; 2];
    for (i, source) in [
        "global using System; class Alpha { int Value() => 42; }",
        "class Beta { Alpha field; }",
    ]
    .into_iter()
    .enumerate()
    {
        let (id, encoded) = analysis(source);
        ordinals[i] = builder.object(id, &encoded).unwrap();
        builder
            .file(&format!("{i}.cs"), &[ordinals[i]], true)
            .unwrap();
    }
    builder.finish(&mut File::create(&path).unwrap()).unwrap();
    Segment::open(&path, None).unwrap().validate(true).unwrap();
    (path, ordinals)
}

#[test]
fn mounted_records_filter_memberships_and_preserve_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let (path, [alpha, beta]) = fixture(root.path());
    let store = Store::open_workspace(
        &root.path().join("analysis"),
        [1; 32],
        Path::new("project"),
        None,
    )
    .unwrap();
    let mounts = vec![Mount {
        path: path.clone(),
        files: BTreeMap::from([("first".into(), alpha), ("alias".into(), alpha)]),
    }];
    let store = store.with_mounts(&mounts, root.path()).unwrap();
    let (id, encoded) = analysis("class Local {}");
    store
        .install_encoded("local", &1, id, || Ok(()), || Ok(encoded))
        .unwrap();
    store
        .scope
        .finish_refresh(b"snapshot", &BTreeSet::from(["local".into()]), 1)
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    {
        let tx = store.query_read().unwrap();
        assert_eq!(
            store.candidates(&tx, "Alpha", false, false).unwrap(),
            BTreeSet::from(["first".into(), "alias".into()])
        );
        assert!(
            store
                .candidates(&tx, "Beta", false, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.candidates(&tx, "Local", false, false).unwrap(),
            BTreeSet::from(["local".into()])
        );
        assert_eq!(store.csharp_global_imports(&tx).unwrap().len(), 2);
        assert!(store.csharp_headers(&tx, "first").unwrap().is_some());
        let source = store.source(&tx, "first").unwrap().unwrap();
        assert!(
            store
                .csharp_body(&tx, "alias", source.find("42").unwrap())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.declaration_revision_in(&tx, "first").unwrap(),
            store.declaration_revision_in(&tx, "alias").unwrap()
        );
        assert!(store.scope.binding(&tx, "first").unwrap().is_none());
    }
    let replacements = vec![Mount {
        path,
        files: BTreeMap::from([("second".into(), beta)]),
    }];
    let replacement = store.with_mounts(&replacements, root.path()).unwrap();
    let tx = replacement.read().unwrap();
    assert_eq!(
        replacement.candidates(&tx, "Alpha", false, true).unwrap(),
        BTreeSet::from(["second".into()])
    );
    drop(tx);
    let tx = snapshot.query_read().unwrap();
    assert_eq!(
        snapshot
            .candidates(&tx, "Alpha", false, false)
            .unwrap()
            .len(),
        2
    );
    drop(tx);
    let encoded = postcard::to_allocvec(&mounts).unwrap();
    let restored = store
        .with_mounts(
            &postcard::from_bytes::<Vec<Mount>>(&encoded).unwrap(),
            root.path(),
        )
        .unwrap();
    assert_eq!(
        crate::binary::encode(&restored.declarations("first").unwrap()).unwrap(),
        crate::binary::encode(&store.declarations("first").unwrap()).unwrap()
    );
}

#[test]
fn validation_rejects_in_range_posting_substitution_and_overlapping_values() {
    let root = tempfile::tempdir().unwrap();
    let (path, [_, beta]) = fixture(root.path());
    let original = std::fs::read(&path).unwrap();
    let segment = Segment::open(&path, None).unwrap();
    let row = segment.lower_bound(b"DAlpha").unwrap();
    assert_eq!(segment.row(row).unwrap().0, b"DAlpha");
    let at = segment.index + row * ROW;
    let offset = u64::from_le_bytes(number(&original, at + 12).unwrap()) as usize;
    let mut corrupt = original.clone();
    corrupt[offset..offset + 4].copy_from_slice(&beta.to_le_bytes());
    std::fs::write(&path, corrupt).unwrap();
    let bad = Segment::open(&path, None).unwrap();
    bad.validate(false).unwrap();
    assert!(bad.validate(true).is_err());
    let mut corrupt = original;
    corrupt[at + 20..at + 28].copy_from_slice(
        &((segment.keys - HEADER - segment.objects * OBJECT) as u64).to_le_bytes(),
    );
    std::fs::write(&path, corrupt).unwrap();
    assert!(Segment::open(&path, None).unwrap().validate(false).is_err());
}
/// Compare actual File and BufWriter<File> paths; wrappers would disable the
/// standard library's kernel-assisted body copy and distort this measurement.
#[test]
#[ignore = "requires SIGLA_PROTOTYPE_SEGMENTS and SIGLA_PROTOTYPE_OUTPUT"]
fn buffered_segment_output_benchmark() -> anyhow::Result<()> {
    use std::time::Instant;
    let inputs = std::env::var_os("SIGLA_PROTOTYPE_SEGMENTS").unwrap();
    let directory = PathBuf::from(std::env::var_os("SIGLA_PROTOTYPE_OUTPUT").unwrap());
    std::fs::create_dir_all(&directory)?;
    println!("segment,variant,run,finish_us,bytes,objects,rows");
    for (index, path) in std::env::split_paths(&inputs).enumerate() {
        let segment = Segment::open(&path, None)?;
        segment.validate(true)?;
        let mut source = Builder::new(&directory)?;
        for ordinal in 0..segment.objects as u32 {
            source.object(segment.object(ordinal)?, &segment.encoded(ordinal)?)?;
        }
        let mut files = BTreeMap::<String, (bool, Vec<u32>)>::new();
        segment.files(|path, invariant, ordinal| {
            files
                .entry(path.into())
                .or_insert_with(|| (invariant, Vec::new()))
                .1
                .push(ordinal);
            Ok(())
        })?;
        for (path, (invariant, ordinals)) in files {
            source.file(&path, &ordinals, invariant)?;
        }
        let mut expected = None;
        for (run, buffered) in [false, true, true, false, false, true, true, false]
            .into_iter()
            .enumerate()
        {
            if let Ok(only) = std::env::var("SIGLA_PROTOTYPE_VARIANT")
                && only != if buffered { "buffered" } else { "plain" }
            {
                continue;
            }
            let mut body = tempfile::NamedTempFile::new_in(&directory)?;
            std::fs::copy(source.body.path(), body.path())?;
            body.seek(std::io::SeekFrom::End(0))?;
            let builder = Builder {
                body,
                objects: source.objects.clone(),
                ids: source.ids.clone(),
                rows: source.rows.clone(),
                postings: source.postings.clone(),
            };
            let output = tempfile::NamedTempFile::new_in(&directory)?;
            let mut file = File::create(output.path())?;
            let started = Instant::now();
            if buffered {
                builder.finish(&mut file)?;
            } else {
                builder.write_to(&mut file)?;
            }
            let elapsed = started.elapsed();
            file.sync_all()?;
            let mut digest = blake3::Hasher::new();
            digest.update_reader(File::open(output.path())?)?;
            let digest = digest.finalize();
            if let Some(expected) = expected {
                assert_eq!(digest, expected);
            } else {
                Segment::open(output.path(), None)?.validate(true)?;
                expected = Some(digest);
            }
            println!(
                "{index},{},{run},{},{},{},{}",
                if buffered { "buffered" } else { "plain" },
                elapsed.as_micros(),
                file.metadata()?.len(),
                source.objects.len(),
                source.rows.len() + source.postings.len()
            );
        }
    }
    Ok(())
}
