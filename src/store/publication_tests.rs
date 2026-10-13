use super::super::tests::{object, put, scope};
use super::*;

/// Hold the normal writer until both synchronous publications are queued.
/// The timeout is only a deadlock guard, not a throughput assertion.
fn publish_together(
    database: &Database,
    a: &Scope,
    left: &[PendingInstall],
    b: &Scope,
    right: &[PendingInstall],
) -> (Outcome, Outcome) {
    let writer = database.write().unwrap();
    std::thread::scope(|threads| {
        let left = threads.spawn(|| a.install_batch(left, |_| Ok(())));
        let right = threads.spawn(|| b.install_batch(right, |_| Ok(())));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut queue = database.publications.lock().unwrap();
        while queue.waiting.len() < 2 && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            queue = database
                .publication_changed
                .wait_timeout(queue, remaining)
                .unwrap()
                .0;
        }
        let queued = queue.waiting.len() == 2;
        drop(queue);
        drop(writer);
        assert!(queued, "Publication did not reach the queue");
        (left.join().unwrap(), right.join().unwrap())
    })
}

#[test]
fn queued_workspaces_share_one_durable_commit() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    let prepare = |space: &Scope, file| {
        space
            .prepare_install(file, vec![1], [7; 32], 100, || {
                Ok(object("Shared", b"payload"))
            })
            .unwrap()
    };
    let left = [prepare(&a, "left")];
    let right = [prepare(&b, "right")];
    let before = { a.read().unwrap().id() };
    let (left, right) = publish_together(&db, &a, &left, &b, &right);
    let left = left.unwrap()[0].unwrap();
    let right = right.unwrap()[0].unwrap();
    assert_eq!(usize::from(left.built) + usize::from(right.built), 1);
    assert_eq!(usize::from(left.reused) + usize::from(right.reused), 1);
    let tx = a.read().unwrap();
    assert_eq!(tx.id(), before + 1);
    assert_eq!(db.binding_count(&tx, &[7; 32]).unwrap(), Some(2));
    assert!(a.binding(&tx, "left").unwrap().is_some());
    assert!(b.binding(&tx, "right").unwrap().is_some());
}

#[test]
fn rejected_request_rolls_back_its_prefix_not_its_neighbors() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::open(directory.path()).unwrap();
    let a = scope(&db, 1);
    let b = scope(&db, 2);
    let left: Vec<_> = ["prefix", "conflict"]
        .into_iter()
        .enumerate()
        .map(|(i, file)| {
            a.prepare_install(file, vec![1], [i as u8 + 1; 32], 100, || {
                Ok(object("Rejected", b"payload"))
            })
            .unwrap()
        })
        .collect();
    let right = [b
        .prepare_install("neighbor", vec![1], [8; 32], 100, || {
            Ok(object("Kept", b"payload"))
        })
        .unwrap()];
    put(&a, "conflict", 2, 9, "Existing");
    let before = { a.read().unwrap().id() };
    let (left, right) = publish_together(&db, &a, &left, &b, &right);
    assert!(left.is_err());
    assert!(right.unwrap()[0].unwrap().built);
    let tx = a.read().unwrap();
    assert_eq!(tx.id(), before + 1);
    assert!(a.binding(&tx, "prefix").unwrap().is_none());
    assert_eq!(a.binding(&tx, "conflict").unwrap().unwrap().object, [9; 32]);
    assert!(b.binding(&tx, "neighbor").unwrap().is_some());
    assert_eq!(db.object_count(&tx).unwrap(), 2);
}
