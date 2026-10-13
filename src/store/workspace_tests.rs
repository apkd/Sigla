use super::*;

fn project() -> Project {
    Project {
        identity: "storage-test".into(),
        origin: None,
        name: "storage-test".into(),
        defines: Vec::new(),
        references: Vec::new(),
        assemblies: Vec::new(),
        edition: "2021".into(),
        compiler_options: BTreeMap::new(),
        source_roots: Vec::new(),
    }
}

#[test]
fn current_batch_progresses_while_parser_budget_is_fully_reserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Cached.cs");
    std::fs::write(&path, "public class Cached {}\n").unwrap();
    let store = Store::open_workspace(
        &directory.path().join("analysis"),
        [1; 32],
        directory.path(),
        None,
    )
    .unwrap();
    let input = SourceInput {
        path,
        project: 0,
        module: String::new(),
        language: Language::CSharp,
        metadata: false,
    };
    let project = project();
    let stamp = Stamp::read(&input.path).unwrap();
    let key = input_key(&input, &project, &stamp);
    assert!(
        index_input(&store, &key, &input, &project, &stamp)
            .unwrap()
            .changed
    );
    let jobs = [(key.clone(), (stamp, &input, &project))];
    std::thread::scope(|threads| {
        let reservation = crate::memory::admit_file(u64::MAX, false);
        let (sender, receiver) = std::sync::mpsc::channel();
        threads.spawn(move || {
            sender.send(index_source_batch(&store, &jobs)).unwrap();
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(10));
        // Release before asserting, so a regressed implementation can unwind
        // and join rather than deadlocking the test runner.
        drop(reservation);
        let result = result.expect("Cache hit waited for parser admission");
        let indexed = result[&key].1.as_ref().unwrap();
        assert!(!indexed.changed);
        assert_eq!(indexed.extraction, std::time::Duration::ZERO);
    });
}

#[test]
fn planning_stamp_reuse_does_not_bypass_mutable_input_fences() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Changed.cs");
    std::fs::write(&path, "public class Before {}\n").unwrap();
    let store = Store::open_workspace(
        &directory.path().join("analysis"),
        [1; 32],
        directory.path(),
        None,
    )
    .unwrap();
    let input = SourceInput {
        path,
        project: 0,
        module: String::new(),
        language: Language::CSharp,
        metadata: false,
    };
    let project = project();
    let mut stamps = BTreeMap::new();
    let stamp = planning_stamp(&mut stamps, &input.path).unwrap();
    let key = input_key(&input, &project, &stamp);
    index_input(&store, &key, &input, &project, &stamp).unwrap();
    std::fs::write(&input.path, "public class ChangedAfterPlanning {}\n").unwrap();
    assert_eq!(planning_stamp(&mut stamps, &input.path).unwrap(), stamp);
    assert_ne!(Stamp::read(&input.path).unwrap(), stamp);
    let stale = [(key.clone(), (stamp, &input, &project))];
    assert!(index_source_batch(&store, &stale)[&key].1.is_err());
    let current = [(
        key.clone(),
        (Stamp::read(&input.path).unwrap(), &input, &project),
    )];
    assert!(
        index_source_batch(&store, &current)[&key]
            .1
            .as_ref()
            .unwrap()
            .changed
    );
    let tx = store.read().unwrap();
    assert!(
        store
            .candidates(&tx, "Before", false, false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .candidates(&tx, "ChangedAfterPlanning", false, false)
            .unwrap(),
        BTreeSet::from([key])
    );
}

#[test]
fn declaration_revision_batch_keeps_the_same_environment_hash() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_workspace(
        &directory.path().join("analysis"),
        [1; 32],
        directory.path(),
        None,
    )
    .unwrap();
    let project = project();
    let mut keys = Vec::new();
    for (index, source) in ["public class A {}", "public class B {}"]
        .into_iter()
        .enumerate()
    {
        let path = directory.path().join(format!("{index}.cs"));
        std::fs::write(&path, source).unwrap();
        let input = SourceInput {
            path,
            project: 0,
            module: String::new(),
            language: Language::CSharp,
            metadata: false,
        };
        let stamp = Stamp::read(&input.path).unwrap();
        let key = input_key(&input, &project, &stamp);
        index_input(&store, &key, &input, &project, &stamp).unwrap();
        keys.push(key);
    }
    keys.sort();
    let mut individual = blake3::Hasher::new();
    for key in &keys {
        individual.update(key.as_bytes());
        individual.update(&store.declaration_revision(key).unwrap());
    }
    let tx = store.read().unwrap();
    let mut batched = blake3::Hasher::new();
    for key in &keys {
        batched.update(key.as_bytes());
        batched.update(&store.declaration_revision_in(&tx, key).unwrap());
    }
    assert_eq!(individual.finalize(), batched.finalize());
}
