use super::*;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn scope(db: &Arc<Database>, n: u8) -> Scope {
    db.workspace([n; 32], Path::new(&format!("/workspace/{n}")), None)
        .unwrap()
}
fn object(name: &str, body: &[u8]) -> Encoded {
    let mut records = BTreeMap::from([(vec![1], body.to_vec())]);
    let names = crate::store::format::write(
        &[],
        None,
        true,
        &Names {
            declarations: BTreeSet::from([name.to_owned()]),
            occurrences: BTreeSet::from(["Use".into()]),
            global_imports: true,
        },
        &mut records,
    )
    .unwrap();
    Encoded { names, records }
}
fn put(scope: &Scope, file: &str, revision: u8, id: u8, name: &str) -> Installed {
    scope
        .install(
            file,
            vec![revision],
            [id; 32],
            100,
            || Ok(()),
            || Ok(object(name, &[id])),
        )
        .unwrap()
}
fn clean(scope: &Scope, files: &[&str]) {
    scope
        .finish_refresh(
            b"test manifest",
            &files.iter().map(|s| s.to_string()).collect(),
            100,
        )
        .unwrap();
}

#[test]
fn dirty_workspace_read_is_retryable_until_refresh_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let workspace = scope(&db, 1);
    clean(&workspace, &[]);
    assert!(workspace.query_read().is_ok());

    workspace.begin_refresh().unwrap();
    let error = workspace.query_read().err().expect("dirty read must fail");
    let details = crate::diagnostics::details(&error, "refresh-read");
    assert_eq!(details["error_code"], "UNAVAILABLE");
    assert_eq!(details["retryable"], true);
    clean(&workspace, &[]);
    assert!(workspace.query_read().is_ok());

    workspace.mark_deleting().unwrap();
    let error = workspace
        .query_read()
        .err()
        .expect("retired read must fail");
    let details = crate::diagnostics::details(&error, "retired-read");
    assert_eq!(details["error_code"], "QUERY_FAILED");
    assert!(details["retryable"].is_null());
}

#[test]
fn batch_publication_rolls_back_together_and_shares_objects() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    let pending: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|file| {
            a.prepare_install(file, vec![1], [7; 32], 100, || Ok(object("Name", b"body")))
                .unwrap()
        })
        .collect();
    assert!(
        a.install_batch(&pending, |index| {
            ensure!(index == 0, "Source changed");
            Ok(())
        })
        .is_err()
    );
    {
        let tx = a.read().unwrap();
        assert_eq!(db.object_count(&tx).unwrap(), 0);
        assert!(a.binding(&tx, "one").unwrap().is_none());
        assert!(
            a.candidates(&tx, "Name", false, true, |_| false)
                .unwrap()
                .is_empty()
        );
    }
    let results = a.install_batch(&pending, |_| Ok(())).unwrap();
    assert!(results[0].unwrap().built);
    assert!(results[1].unwrap().reused);
    clean(&a, &["one", "two"]);
    let tx = a.read().unwrap();
    assert_eq!(db.object_count(&tx).unwrap(), 1);
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(2));
    assert_eq!(
        a.candidates(&tx, "Name", false, true, |_| false).unwrap(),
        BTreeSet::from(["one".into(), "two".into()])
    );
}

#[test]
fn batch_hits_collected_before_publication_request_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "old", 1, 7, "Name");
    let pending = a
        .prepare_install("new", vec![1], [7; 32], 100, || panic!("cache hit rebuilt"))
        .unwrap();
    a.detach("old", 100).unwrap();
    db.collect(100 + RETENTION_MS, 32).unwrap();
    assert!(a.install_batch(&[pending], |_| Ok(())).unwrap()[0].is_none());
    let tx = a.read().unwrap();
    assert!(a.binding(&tx, "new").unwrap().is_none());
}

#[test]
fn shares_payload_not_file_identity_or_workspace_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    assert!(put(&a, "one", 1, 7, "Name").built);
    assert!(put(&a, "two", 1, 7, "Name").reused);
    assert!(put(&b, "other", 1, 7, "Name").reused);
    clean(&a, &["one", "two"]);
    clean(&b, &["other"]);
    let tx = a.read().unwrap();
    assert_eq!(db.object_count(&tx).unwrap(), 1);
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(3));
    assert_eq!(
        a.candidates(&tx, "Name", false, true, |_| false).unwrap(),
        BTreeSet::from(["one".into(), "two".into()])
    );
    assert_eq!(
        b.candidates(&tx, "*", false, false, |_| true).unwrap(),
        BTreeSet::from(["other".into()])
    );
    assert_eq!(b.global_files(&tx).unwrap(), vec!["other"]);
}

#[test]
fn unchanged_object_rebind_only_changes_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Name");
    let result = a
        .install(
            "file",
            vec![2],
            [7; 32],
            110,
            || Ok(()),
            || panic!("hit must not rebuild"),
        )
        .unwrap();
    assert!(result.reused);
    let tx = a.read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
    assert_eq!(a.binding(&tx, "file").unwrap().unwrap().revision, vec![2]);
}

#[test]
fn concurrent_first_use_builds_once_across_workspaces() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let scopes: Vec<_> = (1..=4).map(|i| scope(&db, i)).collect();
    let barrier = Barrier::new(4);
    let builds = AtomicUsize::new(0);
    std::thread::scope(|threads| {
        for scope in &scopes {
            let barrier = &barrier;
            let builds = &builds;
            threads.spawn(move || {
                barrier.wait();
                scope
                    .install(
                        "file",
                        vec![1],
                        [7; 32],
                        100,
                        || Ok(()),
                        || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            Ok(object("Name", b"same"))
                        },
                    )
                    .unwrap();
            });
        }
    });
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    let tx = scopes[0].read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(4));
}

#[test]
fn old_reader_survives_edit_and_collection() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Before");
    clean(&a, &["file"]);
    let old = a.query_read().unwrap();
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
            1
        );
    })
    .join()
    .unwrap();
    assert_eq!(a.record(&old, "file", &[1]).unwrap(), Some([7].as_slice()));
    assert_eq!(
        a.candidates(&old, "Before", false, true, |_| false)
            .unwrap()
            .len(),
        1
    );
    drop(old);
    let current = a.query_read().unwrap();
    assert_eq!(
        a.record(&current, "file", &[1]).unwrap(),
        Some([8].as_slice())
    );
    assert_eq!(db.binding_count(&current, &[7; 32]).unwrap(), None);
}

#[test]
fn failed_verification_publishes_neither_object_nor_binding() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    assert!(
        a.install(
            "file",
            vec![1],
            [7; 32],
            100,
            || anyhow::bail!("changed input"),
            || Ok(object("Name", b"new"))
        )
        .is_err()
    );
    let tx = a.read().unwrap();
    assert_eq!(db.object_count(&tx).unwrap(), 0);
    assert!(a.binding(&tx, "file").unwrap().is_none());
}

#[test]
fn cache_hits_still_verify_input() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    put(&a, "file", 1, 7, "Name");
    assert!(
        b.install(
            "file",
            vec![1],
            [7; 32],
            100,
            || anyhow::bail!("changed input"),
            || panic!("existing object must not rebuild")
        )
        .is_err()
    );
    let tx = a.read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
}

#[test]
fn aborted_writer_rolls_back_postings_binding_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Name");
    {
        let mut tx = db.env.write_txn().unwrap();
        a.detach_in(&mut tx, "file", 100).unwrap();
        assert!(a.binding(&tx, "file").unwrap().is_none());
        // Deliberately abort after all three logical changes.
    }
    let tx = a.read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
    assert!(a.binding(&tx, "file").unwrap().is_some());
    assert_eq!(
        a.candidates(&tx, "Name", false, true, |_| false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn dirty_refresh_reconciles_orphans_not_in_old_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "original", 1, 7, "Keep");
    clean(&a, &["original"]);
    a.begin_refresh().unwrap();
    put(&a, "partial", 1, 8, "Orphan"); // Simulated failure before new manifest publication.
    assert!(a.query_read().is_err());
    assert!(a.manifest().unwrap().is_none());
    clean(&a, &["original"]);
    let tx = a.query_read().unwrap();
    assert!(a.binding(&tx, "partial").unwrap().is_none());
    assert_eq!(db.binding_count(&tx, &[8; 32]).unwrap(), Some(0));
    assert!(
        a.candidates(&tx, "Orphan", false, true, |_| false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn deleting_namespace_resumes_and_never_reuses_numeric_id() {
    let dir = tempfile::tempdir().unwrap();
    let old_id;
    {
        let db = Database::open(dir.path()).unwrap();
        let a = scope(&db, 1);
        old_id = a.id;
        put(&a, "one", 1, 7, "Name");
        put(&a, "two", 1, 7, "Name");
        a.mark_deleting().unwrap();
        assert!(!a.delete_batch(100, 1).unwrap());
    }
    let db = Database::open(dir.path()).unwrap();
    assert!(
        db.workspace([1; 32], Path::new("/workspace/1"), None)
            .is_err()
    );
    let old = db.existing([1; 32]).unwrap().unwrap();
    while !old.delete_batch(100, 1).unwrap() {}
    let new = scope(&db, 1);
    assert_ne!(old_id, new.id);
    assert!(
        old.install(
            "file",
            vec![1],
            [7; 32],
            100,
            || Ok(()),
            || Ok(object("Name", b"x"))
        )
        .is_err()
    );
    let tx = new.read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(0));
}

#[test]
fn retirement_preserves_shared_objects_and_other_workspace_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let retiring_id;
    {
        let db = Database::open(dir.path()).unwrap();
        let retiring = scope(&db, 1);
        let retained = scope(&db, 2);
        retiring_id = retiring.id;
        put(&retiring, "one", 1, 7, "Shared");
        put(&retiring, "two", 1, 7, "Shared");
        put(&retiring, "unique", 1, 8, "Unique");
        put(&retained, "keep", 1, 7, "Shared");
        clean(&retiring, &["one", "two", "unique"]);
        clean(&retained, &["keep"]);
        retiring.mark_deleting().unwrap();
        assert!(!retiring.delete_batch(100, 1).unwrap());
    }
    // Reopen between every batch, including the transition to binding removal.
    loop {
        let db = Database::open(dir.path()).unwrap();
        let retiring = db.existing([1; 32]).unwrap().unwrap();
        if retiring.delete_batch(100, 1).unwrap() {
            break;
        }
    }
    let db = Database::open(dir.path()).unwrap();
    db.collect_unused(100, true).unwrap();
    let retained = scope(&db, 2);
    assert!(db.existing([1; 32]).unwrap().is_none());
    let tx = retained.query_read().unwrap();
    assert!(
        db.control
            .get(&tx, &manifest_key(retiring_id))
            .unwrap()
            .is_none()
    );
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
    assert_eq!(db.binding_count(&tx, &[8; 32]).unwrap(), None);
    for table in [db.declarations, db.occurrences, db.globals] {
        assert!(
            table
                .prefix_iter(&tx, &retiring_id.to_be_bytes())
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            table
                .prefix_iter(&tx, &retained.id.to_be_bytes())
                .unwrap()
                .next()
                .is_some()
        );
    }
    assert_eq!(
        retained
            .candidates(&tx, "Shared", false, true, |_| false)
            .unwrap(),
        BTreeSet::from(["keep".into()])
    );
    assert_eq!(
        retained
            .candidates(&tx, "Use", true, true, |_| false)
            .unwrap(),
        BTreeSet::from(["keep".into()])
    );
    assert!(db.record(&tx, &[7; 32], &[]).unwrap().is_some());
}

#[test]
fn retirement_does_not_decode_analysis_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let retiring = scope(&db, 1);
    put(&retiring, "file", 1, 7, "Name");
    retiring.mark_deleting().unwrap();
    let mut tx = db.env.write_txn().unwrap();
    db.records
        .put(&mut tx, &record_key(&[7; 32], &[]), &[255])
        .unwrap();
    tx.commit().unwrap();
    while !retiring.delete_batch(100, 1).unwrap() {}
    db.collect_unused(100, true).unwrap();
    assert!(db.workspaces().unwrap().is_empty());
    assert_eq!(db.object_count(&db.env.read_txn().unwrap()).unwrap(), 0);
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "manual comparison with the former per-file retirement algorithm"]
fn retirement_benchmark() {
    fn io() -> (u64, u64) {
        let text = std::fs::read_to_string("/proc/self/io").unwrap();
        let value = |key| {
            text.lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
        };
        (value("read_bytes:"), value("write_bytes:"))
    }
    for per_file in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Database::open(dir.path()).unwrap();
            let retiring = scope(&db, 1);
            for i in 0..512 {
                let id = (i % 4) as u8;
                retiring
                    .install(
                        &format!("file-{i}"),
                        vec![1],
                        [id; 32],
                        100,
                        || Ok(()),
                        || {
                            let mut records = BTreeMap::new();
                            let declarations: BTreeSet<_> = (0..512)
                                .map(|n| format!("Shared{n}"))
                                .chain((0..16).map(|n| format!("Object{id}Name{n}")))
                                .collect();
                            let names = crate::store::format::write(
                                &[],
                                None,
                                true,
                                &Names {
                                    occurrences: declarations.clone(),
                                    declarations,
                                    global_imports: true,
                                },
                                &mut records,
                            )?;
                            Ok(Encoded { names, records })
                        },
                    )
                    .unwrap();
            }
            retiring.mark_deleting().unwrap();
        }
        let db = Database::open(dir.path()).unwrap();
        let retiring = db.existing([1; 32]).unwrap().unwrap();
        let before = io();
        let start = std::time::Instant::now();
        if per_file {
            loop {
                let mut tx = db.env.write_txn().unwrap();
                for file in retiring.files_after(&tx, None, 128).unwrap() {
                    retiring.detach_in(&mut tx, &file, 100).unwrap();
                }
                let done = retiring.files_after(&tx, None, 1).unwrap().is_empty();
                if done {
                    db.control
                        .delete(&mut tx, &manifest_key(retiring.id))
                        .unwrap();
                    db.spaces.delete(&mut tx, &retiring.fingerprint).unwrap();
                }
                tx.commit().unwrap();
                if done {
                    break;
                }
            }
        } else {
            while !retiring.delete_batch(100, 128).unwrap() {}
        }
        let elapsed = start.elapsed();
        let after = io();
        eprintln!(
            "per_file={per_file} elapsed={elapsed:?} read_bytes={} write_bytes={}",
            after.0 - before.0,
            after.1 - before.1
        );
        assert!(db.workspaces().unwrap().is_empty());
        for table in [db.declarations, db.occurrences, db.globals] {
            assert!(table.is_empty(&db.env.read_txn().unwrap()).unwrap());
        }
    }
}

#[test]
fn long_names_remain_indexable_and_oversize_keys_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    let name = "x".repeat(480);
    put(&a, "file", 1, 7, &name);
    assert!(
        a.install(
            &"x".repeat(512),
            vec![1],
            [8; 32],
            100,
            || Ok(()),
            || Ok(object("X", b"x"))
        )
        .is_err()
    );
    let tx = a.read().unwrap();
    assert_eq!(
        a.candidates(&tx, &name, false, true, |_| false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn gc_grace_and_cursor_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        let a = scope(&db, 1);
        for id in 1..=3 {
            put(&a, &format!("f{id}"), 1, id, "Name");
        }
        a.detach("f1", 100).unwrap();
        a.detach("f2", 100).unwrap();
        assert_eq!(db.collect(100 + RETENTION_MS - 1, 1).unwrap().objects, 0);
    }
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    assert_eq!(db.collect(100 + RETENTION_MS, 1).unwrap().objects, 1); // resumes at object 2
    for _ in 0..4 {
        db.collect(100 + RETENTION_MS, 1).unwrap();
    }
    let tx = a.read().unwrap();
    assert_eq!(db.object_count(&tx).unwrap(), 1);
    assert_eq!(db.binding_count(&tx, &[3; 32]).unwrap(), Some(1));
}

#[test]
fn weak_registry_does_not_pin_environment() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let weak = Arc::downgrade(&db);
    let a = scope(&db, 1);
    drop(db);
    assert!(weak.upgrade().is_some());
    drop(a);
    assert!(weak.upgrade().is_none());
    Database::open(dir.path()).unwrap();
}

#[test]
fn collected_hit_retries_builder_before_attaching() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    put(&a, "original", 1, 7, "Name");
    let mut checks = 0;
    let mut builds = 0;
    let result = b
        .install(
            "new",
            vec![1],
            [7; 32],
            100 + RETENTION_MS,
            || {
                checks += 1;
                if checks == 1 {
                    let writer = a.clone();
                    std::thread::spawn(move || {
                        writer.detach("original", 100).unwrap();
                        assert_eq!(
                            writer
                                .database
                                .collect(100 + RETENTION_MS, 32)
                                .unwrap()
                                .objects,
                            1
                        );
                    })
                    .join()
                    .unwrap();
                }
                Ok(())
            },
            || {
                builds += 1;
                Ok(object("Name", &[7]))
            },
        )
        .unwrap();
    assert!(result.built);
    assert_eq!(builds, 1);
    assert_eq!(checks, 2);
    let tx = b.read().unwrap();
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(1));
    assert!(b.binding(&tx, "new").unwrap().is_some());
}

#[test]
fn intervening_binding_change_aborts_without_overwriting_newer_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let a = scope(&db, 1);
    put(&a, "file", 1, 7, "Old");
    let writer = a.clone();
    let result = a.install(
        "file",
        vec![2],
        [8; 32],
        100,
        || {
            std::thread::scope(|threads| {
                threads
                    .spawn(|| {
                        put(&writer, "file", 3, 9, "Newer");
                    })
                    .join()
                    .unwrap();
            });
            Ok(())
        },
        || Ok(object("Superseded", &[8])),
    );
    assert!(result.is_err());
    let tx = a.read().unwrap();
    assert_eq!(a.binding(&tx, "file").unwrap().unwrap().object, [9; 32]);
    assert_eq!(db.binding_count(&tx, &[8; 32]).unwrap(), None);
    assert_eq!(db.binding_count(&tx, &[9; 32]).unwrap(), Some(1));
}
