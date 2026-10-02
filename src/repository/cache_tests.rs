//! Local ownership/recovery tests. No network or repository discovery is performed.
use super::*;

fn identity() -> Identity {
    Repository::parse("https://github.com/example/sharing-fixture")
        .unwrap()
        .unwrap()
        .identity
}
fn pin(pool: &Arc<Pool>) -> Pin {
    let root = Arc::new(Root {
        revision: "a".repeat(40),
        selected: BTreeSet::from(["b".repeat(40)]),
    });
    pool.pending.lock().unwrap().push(Arc::downgrade(&root));
    Pin {
        _root: root,
        _pool: pool.clone(),
    }
}
fn candidate(path: &Path, complete: bool, sentinel: &str) {
    private_dir(path).unwrap();
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(path.join("sentinel"), sentinel).unwrap();
    if complete {
        fs::write(path.join("sigla-complete"), b"sigla-selected-git-pool-1\n").unwrap();
    }
}

#[test]
fn pin_keeps_the_registry_operation_owner_alive() {
    let cache = tempfile::tempdir().unwrap();
    let id = identity();
    let pool = Pool::open(cache.path(), &id).unwrap();
    let weak = Arc::downgrade(&pool);
    let pin = pin(&pool);
    drop(pool);
    let reopened = Pool::open(cache.path(), &id).unwrap();
    assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &reopened));
    drop(reopened);
    drop(pin);
    assert!(weak.upgrade().is_none());
}

#[test]
fn captured_pending_roots_survive_the_pin_being_released() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    let pin = pin(&pool);
    let captured = pool.roots(cache.path()).unwrap();
    drop(pin);
    assert!(pool.roots(cache.path()).unwrap().is_empty());
    assert_eq!(captured.revisions, BTreeSet::from(["a".repeat(40)]));
    assert_eq!(captured.selected, BTreeSet::from(["b".repeat(40)]));
}

#[test]
fn idle_orphan_pools_expire_only_after_pending_publication_finishes() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    let mut state = pool.state().unwrap();
    state.used = 0;
    pool.persist(&state).unwrap();
    let pending = pin(&pool);
    maintain_all(cache.path(), Duration::from_secs(1)).unwrap();
    assert!(pool.root.exists());
    drop(pending);
    maintain_all(cache.path(), Duration::from_secs(1)).unwrap();
    assert!(!pool.root.exists());
}

#[test]
fn initial_pool_clock_is_persisted_even_before_successful_acquisition() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    assert!(!pool.root.join("pool.json").exists());
    let state = pool.state().unwrap();
    assert!(pool.root.join("pool.json").is_file());
    assert_eq!(pool.state().unwrap().replaced, state.replaced);
    assert_eq!(pool.state().unwrap().used, state.used);
}

#[test]
fn replacement_recovery_preserves_a_valid_side_of_each_handoff() {
    for (present, complete, winner) in [
        (false, false, "old"),
        (true, false, "old"),
        (true, true, "new"),
    ] {
        let cache = tempfile::tempdir().unwrap();
        let pool = Pool::open(cache.path(), &identity()).unwrap();
        candidate(&pool.root.join("previous"), false, "old");
        if present {
            candidate(&pool.root.join("current"), complete, "new");
        }
        pool.recover().unwrap();
        assert_eq!(
            fs::read_to_string(pool.root.join("current/sentinel")).unwrap(),
            winner
        );
        assert!(!pool.root.join("previous").exists());
    }
}

#[test]
fn retention_deduplicates_identical_objects_across_selectors() {
    let mut roots = Retention::default();
    roots.add(&"a".repeat(40), ["c".repeat(40), "d".repeat(40)]);
    roots.add(&"a".repeat(40), ["c".repeat(40)]);
    roots.add(&"b".repeat(40), ["c".repeat(40)]);
    assert_eq!(roots.revisions.len(), 2);
    assert_eq!(roots.selected.len(), 2);
}
