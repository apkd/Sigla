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

fn publish_selector(cache: &Path, pool: &Pool, root: &Root) {
    let selected: std::collections::BTreeMap<_, _> = root
        .selected
        .iter()
        .enumerate()
        .map(|(i, id)| (format!("file-{i}"), id))
        .collect();
    super::super::manager::write_json(
        &cache
            .join("repositories")
            .join(pool.identity.storage_key())
            .join("state.json"),
        &serde_json::json!({
            "schema": 4, "repository": pool.identity,
            "transport": "https://github.com/example/sharing-fixture", "branch": "main",
            "last_use": 0, "refreshed": 0, "policy": "", "indexed_revision": null, "repair": false,
            "prepared": { "branch": "main", "revision": root.revision, "selected": selected,
                "tracked": {}, "directories": [] }
        }),
    )
    .unwrap();
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
fn maintenance_snapshot_preserves_a_pin_released_before_collection() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    pool.state().unwrap();
    let pending = pin(&pool);
    let snapshot = Snapshot::capture(pool.clone()).unwrap().unwrap();
    drop(pending);
    maintain_pool(
        cache.path(),
        &pool.identity,
        Duration::ZERO,
        true,
        Some(&snapshot),
    )
    .unwrap();
    assert!(pool.root.exists());
    reclaim(cache.path()).unwrap();
    assert!(!pool.root.exists());
}

#[test]
fn acquisition_after_snapshot_cannot_lose_its_pending_objects() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    pool.state().unwrap();
    let snapshot = Snapshot::capture(pool.clone()).unwrap().unwrap();
    let pending = {
        let mut operation = pool.operation.lock().unwrap();
        *operation += 1;
        pin(&pool)
    };
    maintain_pool(
        cache.path(),
        &pool.identity,
        Duration::ZERO,
        true,
        Some(&snapshot),
    )
    .unwrap();
    assert!(pool.root.exists());
    drop(pending);
    reclaim(cache.path()).unwrap();
    assert!(!pool.root.exists());
}

#[test]
fn acquisition_published_after_snapshot_survives_without_its_pin() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    pool.state().unwrap();
    let snapshot = Snapshot::capture(pool.clone()).unwrap().unwrap();
    let pending = {
        let mut operation = pool.operation.lock().unwrap();
        *operation += 1;
        pin(&pool)
    };
    publish_selector(cache.path(), &pool, &pending._root);
    drop(pending);
    maintain_pool(
        cache.path(),
        &pool.identity,
        Duration::ZERO,
        true,
        Some(&snapshot),
    )
    .unwrap();
    assert!(pool.root.exists());
}

#[test]
fn shared_selector_scan_assigns_roots_to_their_pool() {
    let cache = tempfile::tempdir().unwrap();
    let kept = Pool::open(cache.path(), &identity()).unwrap();
    kept.state().unwrap();
    let orphan_id = Repository::parse("https://github.com/example/orphan")
        .unwrap()
        .unwrap()
        .identity;
    let orphan = Pool::open(cache.path(), &orphan_id).unwrap();
    orphan.state().unwrap();
    let pending = pin(&kept);
    publish_selector(cache.path(), &kept, &pending._root);
    drop(pending);
    reclaim(cache.path()).unwrap();
    assert!(kept.root.exists());
    assert!(!orphan.root.exists());
}

#[test]
fn ordinary_maintenance_skips_a_pool_with_unchanged_roots() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    let mut state = pool.state().unwrap();
    let pending = pin(&pool);
    let roots = pool.roots(cache.path()).unwrap();
    state.replaced = 0;
    state.retained = Some(
        blake3::hash(&serde_json::to_vec(&roots).unwrap())
            .to_hex()
            .to_string(),
    );
    // A rebuild of this deliberately incomplete pool would fail and set a retry deadline.
    candidate(&pool.root.join("current"), true, "retained");
    pool.maybe_rebuild(cache.path(), &mut state, false, Some(&roots))
        .unwrap();
    assert_eq!(state.retry_after, 0);
    drop(pending);
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
fn pressure_keeps_inflight_git_objects_and_reclaims_an_idle_pool() {
    let cache = tempfile::tempdir().unwrap();
    let pool = Pool::open(cache.path(), &identity()).unwrap();
    pool.state().unwrap();
    let pending = pin(&pool);
    reclaim(cache.path()).unwrap();
    assert!(pool.root.exists());
    drop(pending);
    let operation = pool.operation.lock().unwrap();
    reclaim(cache.path()).unwrap();
    assert!(pool.root.exists());
    drop(operation);
    reclaim(cache.path()).unwrap();
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

#[test]
#[ignore = "manual comparison of per-pool and shared selector scans"]
fn retention_scan_benchmark() {
    let cache = tempfile::tempdir().unwrap();
    let mut pools = Vec::new();
    for n in 0..32 {
        let identity = Repository::parse(&format!("https://github.com/example/repo-{n}"))
            .unwrap()
            .unwrap()
            .identity;
        let pool = Pool::open(cache.path(), &identity).unwrap();
        pool.state().unwrap();
        let root = Root {
            revision: "a".repeat(40),
            selected: (0..2048)
                .map(|i| {
                    blake3::hash(format!("{n}/{i}").as_bytes())
                        .to_hex()
                        .to_string()
                })
                .collect(),
        };
        publish_selector(cache.path(), &pool, &root);
        pools.push(pool);
    }
    let start = std::time::Instant::now();
    let expected: usize = pools
        .iter()
        .map(|pool| pool.roots(cache.path()).unwrap().selected.len())
        .sum();
    eprintln!("per-pool selector scans: {:?}", start.elapsed());
    let start = std::time::Instant::now();
    let mut snapshots: HashMap<_, _> = pools
        .iter()
        .map(|pool| {
            (
                pool.identity.storage_key(),
                Snapshot::capture(pool.clone()).unwrap().unwrap(),
            )
        })
        .collect();
    visit_selectors(cache.path(), |state| {
        snapshots
            .get_mut(&state.repository.storage_key())
            .unwrap()
            .roots
            .add(
                &state.prepared.revision,
                state.prepared.selected.into_values(),
            );
    })
    .unwrap();
    eprintln!("shared selector scan: {:?}", start.elapsed());
    assert_eq!(
        snapshots
            .values()
            .map(|snapshot| snapshot.roots.selected.len())
            .sum::<usize>(),
        expected
    );
}

fn history_blob(store: &Path, value: &[u8]) -> String {
    let output = crate::process::capture(
        super::super::history::git(store).args(["hash-object", "-w", "--stdin"]),
        Duration::from_secs(10),
        Some(value.to_vec()),
        None,
    )
    .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn read_history_blob(store: &Path, oid: &str) -> Vec<u8> {
    crate::process::run(
        super::super::history::git(store).args(["cat-file", "blob", oid]),
        Duration::from_secs(10),
    )
    .unwrap()
}
#[test]
fn history_reader_survives_replacement_and_can_republish_its_exact_objects() {
    let cache = tempfile::tempdir().unwrap();
    let id = identity();
    let lease = object_lease(cache.path(), &id, "sha1").unwrap();
    let pool = Pool::open(cache.path(), &id).unwrap();
    let current = pool.root.join("current");
    let oid = history_blob(&current, b"reader-owned bytes\n");
    link_objects(&current, lease.path()).unwrap();
    lease.retain(&oid);
    let stage = tempfile::tempdir_in(&pool.root).unwrap();
    let target = stage.path().join("git");
    super::super::history::init(&target, "sha1").unwrap();
    generation(&target).unwrap();
    // Simulates replacement after a root capture preceding this read. Immutable
    // hardlinks keep the old object accessible even after previous is deleted.
    fs::write(
        target.join("sigla-complete"),
        b"sigla-selected-git-pool-1\n",
    )
    .unwrap();
    {
        let _gate = pool.operation.lock().unwrap();
        pool.replace(&target).unwrap();
    }
    assert!(!pool.root.join("previous").exists());
    assert_eq!(
        read_history_blob(lease.path(), &oid),
        b"reader-owned bytes\n"
    );
    lease.publish(&BTreeSet::new(), &BTreeSet::new()).unwrap();
    assert_eq!(read_history_blob(&current, &oid), b"reader-owned bytes\n");
    assert!(pool.roots(cache.path()).unwrap().history.contains(&oid));
    let owner = lease.owner();
    assert!(!evict_history(&owner).unwrap());
    drop(lease);
    assert!(evict_history(&owner).unwrap());
    reclaim(cache.path()).unwrap();
    assert!(!pool.root.exists());
}
#[test]
fn generation_mismatch_invalidates_coverage_without_invalidating_live_reader() {
    let cache = tempfile::tempdir().unwrap();
    let id = identity();
    let lease = object_lease(cache.path(), &id, "sha1").unwrap();
    let pool = Pool::open(cache.path(), &id).unwrap();
    let current = pool.root.join("current");
    let oid = history_blob(&current, b"verified bytes\n");
    link_objects(&current, lease.path()).unwrap();
    lease.retain(&oid);
    lease
        .publish(&BTreeSet::from(["a".repeat(40)]), &BTreeSet::new())
        .unwrap();
    assert!(pool.history_coverage().unwrap().is_some());
    fs::write(current.join("sigla-generation"), "f".repeat(64)).unwrap();
    assert!(pool.history_coverage().unwrap().is_none());
    assert!(pool.roots(cache.path()).unwrap().history.contains(&oid));
    assert_eq!(read_history_blob(lease.path(), &oid), b"verified bytes\n");
    drop(lease);
    assert!(pool.roots(cache.path()).unwrap().history.is_empty());
}

#[test]
fn abandoned_reader_views_are_collected_without_disturbing_live_readers() {
    let cache = tempfile::tempdir().unwrap();
    let id = identity();
    let first = object_lease(cache.path(), &id, "sha1").unwrap();
    let pool = Pool::open(cache.path(), &id).unwrap();
    let oid = history_blob(&pool.root.join("current"), b"retained source\n");
    link_objects(&pool.root.join("current"), first.path()).unwrap();
    first.retain(&oid);
    first.publish(&BTreeSet::new(), &BTreeSet::new()).unwrap();
    let abandoned = pool.root.join("history-reader-abandoned");
    super::super::history::init(&abandoned, "sha1").unwrap();
    link_objects(first.path(), &abandoned).unwrap();
    let second = object_lease(cache.path(), &id, "sha1").unwrap();
    assert_eq!(read_history_blob(first.path(), &oid), b"retained source\n");
    assert_eq!(read_history_blob(second.path(), &oid), b"retained source\n");
    drop(first);
    drop(second);
    let recovered = object_lease(cache.path(), &id, "sha1").unwrap();
    assert!(!abandoned.exists());
    assert_eq!(
        read_history_blob(recovered.path(), &oid),
        b"retained source\n"
    );
}
