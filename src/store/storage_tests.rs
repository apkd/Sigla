use super::tests::{clean, object, put, scope};
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn failed_input_fence_preserves_neighbors_in_one_commit() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let builds = AtomicUsize::new(0);
    let pending: Vec<_> = ["left", "changed", "right"]
        .into_iter()
        .enumerate()
        .map(|(i, file)| {
            a.prepare_install(file, vec![1], [i as u8 + 1; 32], 100, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok(object("Name", b"payload"))
            })
            .unwrap()
        })
        .collect();
    let before = { a.read().unwrap().id() };
    let results = a.install_batch_resilient(&pending, |index| {
        ensure!(index != 1, "Input changed");
        Ok(())
    });
    assert!(results[0].as_ref().unwrap().unwrap().built);
    assert!(results[1].is_err());
    assert!(results[2].as_ref().unwrap().unwrap().built);
    assert_eq!(builds.load(Ordering::Relaxed), 3);
    let tx = a.read().unwrap();
    assert_eq!(tx.id(), before + 1);
    assert!(a.binding(&tx, "changed").unwrap().is_none());
    assert_eq!(db.object_count(&tx).unwrap(), 2);
    assert_eq!(
        a.candidates(&tx, "Name", false, true, |_| false).unwrap(),
        BTreeSet::from(["left".into(), "right".into()])
    );
}

#[test]
fn transactional_retry_reuses_prepared_encodings_and_keeps_valid_files() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let builds = AtomicUsize::new(0);
    let pending: Vec<_> = ["left", "conflict", "right"]
        .into_iter()
        .enumerate()
        .map(|(i, file)| {
            a.prepare_install(file, vec![1], [i as u8 + 1; 32], 100, || {
                builds.fetch_add(1, Ordering::Relaxed);
                Ok(object("Name", b"payload"))
            })
            .unwrap()
        })
        .collect();
    put(&a, "conflict", 2, 9, "Existing");
    let results = a.install_batch_resilient(&pending, |_| Ok(()));
    assert!(
        results
            .iter()
            .all(|result| result.as_ref().unwrap().unwrap().built)
    );
    assert_eq!(builds.load(Ordering::Relaxed), 3);
    let tx = a.read().unwrap();
    assert_eq!(a.binding(&tx, "conflict").unwrap().unwrap().object, [2; 32]);
    assert_eq!(
        a.candidates(&tx, "Name", false, true, |_| false).unwrap(),
        BTreeSet::from(["left".into(), "conflict".into(), "right".into()])
    );
}

#[test]
fn reuse_probe_attaches_aliases_without_encoding_and_detects_collection() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    let input = [42; 32];
    let pending = a
        .prepare_install("original", vec![1], [7; 32], 100, || {
            Ok(object("Name", b"payload"))
        })
        .unwrap()
        .input(Some(input));
    a.install_batch(&[pending], |_| Ok(())).unwrap();
    let Reuse::Pending(pending) = b.prepare_reuse("copy", vec![1], Some(input), 100).unwrap()
    else {
        panic!("Alias was not reused")
    };
    assert_eq!(pending.bytes(), 0);
    assert!(
        b.install_batch(&[*pending], |_| Ok(())).unwrap()[0]
            .unwrap()
            .reused
    );
    assert!(matches!(
        b.prepare_reuse("copy", vec![1], None, 100).unwrap(),
        Reuse::Current
    ));
    // A mutable input with a different revision must not use its old binding.
    assert!(matches!(
        b.prepare_reuse("copy", vec![2], None, 100).unwrap(),
        Reuse::Missing
    ));
    a.detach("original", 100).unwrap();
    b.detach("copy", 100).unwrap();
    assert_eq!(db.collect(100 + RETENTION_MS, 32).unwrap().objects, 1);
    assert!(matches!(
        b.prepare_reuse("new", vec![1], Some(input), 100).unwrap(),
        Reuse::Missing
    ));
}

#[test]
fn broad_vocabulary_merges_terms_once_and_filters_foreign_members() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let owner = scope(&db, 1);
    let view = scope(&db, 2);
    // Cross a real posting boundary; no assertion depends on its tuning value.
    let count = PARTITION as usize + 2;
    let id = |index: usize| {
        let mut id = [0; 32];
        id[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        id
    };
    let pending: Vec<_> = (0..count)
        .map(|index| {
            owner
                .prepare_install(&format!("file-{index}"), vec![1], id(index), 100, || {
                    Ok(object(
                        if index == 1 { "Foreign" } else { "Common" },
                        b"payload",
                    ))
                })
                .unwrap()
        })
        .collect();
    owner.install_batch(&pending, |_| Ok(())).unwrap();
    let pending: Vec<_> = [("first", 0), ("last", count - 1)]
        .into_iter()
        .map(|(file, index)| {
            view.prepare_install(file, vec![1], id(index), 100, || {
                panic!("Rebuilt a shared object")
            })
            .unwrap()
        })
        .collect();
    view.install_batch(&pending, |_| Ok(())).unwrap();
    clean(&view, &["first", "last"]);
    let tx = view.query_read().unwrap();
    assert!(
        db.control
            .get(&tx, &scope_key(view.id, b"vocabulary"))
            .unwrap()
            .is_none()
    );
    let mut visits = BTreeMap::<String, usize>::new();
    let found = view
        .candidates(&tx, "*", false, false, |term| {
            *visits.entry(term.to_owned()).or_default() += 1;
            term == "Common"
        })
        .unwrap();
    assert_eq!(found, BTreeSet::from(["first".into(), "last".into()]));
    assert_eq!(visits.get("Common"), Some(&1));
    assert!(visits.values().all(|&count| count == 1));
    assert!(
        view.candidates(&tx, "Fore*", false, false, |term| term.starts_with("Fore"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        view.candidates(&tx, "u*", true, false, |term| term
            .eq_ignore_ascii_case("use"))
            .unwrap(),
        found
    );
    assert_eq!(view.global_files(&tx).unwrap(), vec!["first", "last"]);
}

#[test]
fn shared_dictionary_survives_membership_writes_but_not_posting_changes() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    put(&a, "left", 1, 7, "Before");
    clean(&a, &["left"]);
    let before = {
        let tx = a.query_read().unwrap();
        names::dictionary(&db, &tx, &a.partitions(&tx).unwrap(), false)
            .unwrap()
            .unwrap()
    };
    put(&b, "right", 1, 7, "Before");
    clean(&b, &["right"]);
    let tx = b.query_read().unwrap();
    let reused = names::dictionary(&db, &tx, &b.partitions(&tx).unwrap(), false)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&before, &reused));
    assert_eq!(
        b.candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
            .unwrap(),
        BTreeSet::from(["right".into()])
    );
    drop(tx);
    put(&b, "right", 2, 8, "After");
    clean(&b, &["right"]);
    let tx = b.query_read().unwrap();
    let changed = names::dictionary(&db, &tx, &b.partitions(&tx).unwrap(), false)
        .unwrap()
        .unwrap();
    assert!(!Arc::ptr_eq(&before, &changed));
    assert!(
        b.candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn broad_search_obeys_old_transactions_snapshots_and_collection() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Before");
    clean(&a, &["file"]);
    let snapshot = a.snapshot().unwrap();
    let old = a.query_read().unwrap();
    assert_eq!(
        a.candidates(&old, "B*", false, false, |term| term.starts_with('B'))
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
    let writer = a.clone();
    std::thread::spawn(move || {
        put(&writer, "file", 2, 8, "After");
        clean(&writer, &["file"]);
        assert_eq!(
            writer
                .database
                .collect(100 + RETENTION_MS, 32)
                .unwrap()
                .objects,
            0
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        a.candidates(&old, "*", false, false, |_| true).unwrap(),
        BTreeSet::from(["file".into()])
    );
    assert!(
        a.candidates(&old, "A*", false, false, |term| term.starts_with('A'))
            .unwrap()
            .is_empty()
    );
    drop(old);
    let tx = a.query_read().unwrap();
    assert!(
        a.candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        snapshot
            .candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
    assert!(
        snapshot
            .candidates(&tx, "A*", false, false, |term| term.starts_with('A'))
            .unwrap()
            .is_empty()
    );
    drop(tx);
    drop(snapshot);
    assert_eq!(db.collect(100 + RETENTION_MS, 32).unwrap().objects, 1);
    let tx = a.query_read().unwrap();
    assert_eq!(
        a.candidates(&tx, "A*", false, false, |term| term.starts_with('A'))
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
    let dictionary = names::dictionary(&db, &tx, &a.partitions(&tx).unwrap(), false)
        .unwrap()
        .unwrap();
    assert!(!dictionary.terms().any(|(term, _)| term == "Before"));
}

#[test]
fn clean_workspace_reopens_without_a_persisted_vocabulary() {
    let directory = tempfile::tempdir().unwrap();
    {
        let db = Database::open(directory.path()).unwrap();
        let a = scope(&db, 1);
        put(&a, "file", 1, 7, "Name");
        clean(&a, &["file"]);
        db.checkpoint().unwrap();
        let tx = a.query_read().unwrap();
        assert_eq!(
            a.candidates(&tx, "N*", false, false, |term| term.starts_with('N'))
                .unwrap(),
            BTreeSet::from(["file".into()])
        );
    }
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let tx = a.query_read().unwrap();
    assert_eq!(a.record(&tx, "file", &[1]).unwrap(), Some([7].as_slice()));
    assert_eq!(
        a.candidates(&tx, "N*", false, false, |term| term.starts_with('N'))
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
}

#[test]
fn invalid_dictionary_generation_falls_back_to_committed_postings() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Before");
    clean(&a, &["file"]);
    {
        let tx = a.query_read().unwrap();
        assert_eq!(
            a.candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
                .unwrap(),
            BTreeSet::from(["file".into()])
        );
    }
    put(&a, "file", 2, 8, "After");
    clean(&a, &["file"]);
    let partition = {
        let tx = a.query_read().unwrap();
        *a.partitions(&tx).unwrap().first().unwrap()
    };
    {
        let mut tx = db.write().unwrap();
        db.control
            .put(
                &mut tx,
                &scope_key(partition, b"posting-generation"),
                b"invalid",
            )
            .unwrap();
        tx.commit().unwrap();
    }
    let tx = a.query_read().unwrap();
    assert!(
        names::dictionary(&db, &tx, &a.partitions(&tx).unwrap(), false)
            .unwrap()
            .is_none()
    );
    assert!(
        a.candidates(&tx, "B*", false, false, |term| term.starts_with('B'))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        a.candidates(&tx, "A*", false, false, |term| term.starts_with('A'))
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
}

#[test]
fn conflict_recovery_rechecks_the_input_before_rebinding() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let pending = a
        .prepare_install("file", vec![1], [7; 32], 100, || {
            Ok(object("Stale", b"payload"))
        })
        .unwrap();
    put(&a, "file", 2, 8, "Current");
    let mut checks = 0;
    let results = a.install_batch_resilient(&[pending], |_| {
        checks += 1;
        ensure!(checks == 1, "Input changed before retry");
        Ok(())
    });
    assert!(results[0].is_err());
    let tx = a.read().unwrap();
    assert_eq!(a.binding(&tx, "file").unwrap().unwrap().object, [8; 32]);
    assert_eq!(db.object_count(&tx).unwrap(), 1);
    assert_eq!(
        a.candidates(&tx, "Current", false, true, |_| false)
            .unwrap(),
        BTreeSet::from(["file".into()])
    );
}
