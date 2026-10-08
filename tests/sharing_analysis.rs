//! Run in Sigla after the integration changes; exercises real C# split records.
use sigla::{
    model::Language,
    store::{FileData, Store, canonical_defines, metadata_id, source_id},
};
use std::{collections::BTreeSet, path::Path, sync::Arc};

fn open(path: &Path, n: u8) -> Arc<Store> {
    Store::open_workspace(path, [n; 32], Path::new(&format!("/work/{n}")), None).unwrap()
}
fn install(store: &Store, file: &str, revision: u8, source: &str, defines: &[String]) {
    let defines = canonical_defines(defines);
    let id = source_id(source, Language::CSharp, &defines, "").unwrap();
    store
        .install(
            file,
            &revision,
            id,
            || Ok(()),
            || {
                Ok(FileData {
                    source: source.to_owned(),
                    facts: sigla::extract::extract(source, Language::CSharp, &defines, "").unwrap(),
                    assembly: None,
                })
            },
        )
        .unwrap();
}
#[test]
fn split_csharp_records_share_and_remain_readable() {
    let cache = tempfile::tempdir().unwrap();
    let a = open(cache.path(), 1);
    let b = open(cache.path(), 2);
    let source = "global using System; class C { public int Read() { return 17; } }";
    install(&a, "A.cs", 1, source, &[]);
    install(&b, "B.cs", 1, source, &[]);
    let tx = a.read().unwrap();
    assert_eq!(a.scope.database.object_count(&tx).unwrap(), 1);
    let da = a.csharp_headers(&tx, "A.cs").unwrap().unwrap();
    let db = b.csharp_headers(&tx, "B.cs").unwrap().unwrap();
    assert!(Arc::ptr_eq(&da, &db));
    assert!(
        a.csharp_body(&tx, "A.cs", source.find("17").unwrap())
            .unwrap()
            .is_some()
    );
    assert_eq!(b.csharp_global_imports(&tx).unwrap()[0].0, "B.cs");
    assert_eq!(b.load(&tx, "B.cs").unwrap().unwrap().source, source);
}
#[test]
fn body_and_location_changes_do_not_alias_analysis_objects() {
    let cache = tempfile::tempdir().unwrap();
    let a = open(cache.path(), 1);
    let before = "class C { public int Read() { return 17; } }";
    install(&a, "file", 1, before, &[]);
    let declaration = a.declaration_revision("file").unwrap();
    let after = "\n\nclass C { public int Read() { return 23; } }";
    install(&a, "file", 2, after, &[]);
    assert_eq!(a.declaration_revision("file").unwrap(), declaration);
    let tx = a.read().unwrap();
    assert_eq!(a.scope.database.object_count(&tx).unwrap(), 2);
    assert_eq!(a.load(&tx, "file").unwrap().unwrap().source, after);
}
#[test]
fn profiles_are_canonical_and_metadata_fallback_is_part_of_identity() {
    let defs = vec!["B".into(), "A".into(), "A".into()];
    assert_eq!(
        source_id("x", Language::CSharp, &defs, "unused").unwrap(),
        source_id("x", Language::CSharp, &["A".into(), "B".into()], "").unwrap()
    );
    assert_eq!(
        source_id("x", Language::CSharp, &defs, "").unwrap(),
        source_id("x", Language::CSharp, &[], "").unwrap()
    );
    let conditional = "#if A\nclass Enabled {}\n#else\nclass Disabled {}\n#endif\n";
    assert_ne!(
        source_id(conditional, Language::CSharp, &defs, "").unwrap(),
        source_id(conditional, Language::CSharp, &[], "").unwrap()
    );
    assert_ne!(
        source_id("x", Language::Rust, &[], "2021").unwrap(),
        source_id("x", Language::Rust, &[], "2024").unwrap()
    );
    assert_ne!(
        metadata_id(b"same bytes", "One").unwrap(),
        metadata_id(b"same bytes", "Two").unwrap()
    );
}
#[test]
fn same_content_in_two_file_instances_returns_two_results() {
    let cache = tempfile::tempdir().unwrap();
    let a = open(cache.path(), 1);
    let source = "class C {}";
    install(&a, "one", 1, source, &[]);
    install(&a, "two", 1, source, &[]);
    let tx = a.read().unwrap();
    assert_eq!(
        a.candidates(&tx, "C", false, false).unwrap(),
        BTreeSet::from(["one".into(), "two".into()])
    );
}
