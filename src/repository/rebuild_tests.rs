//! Actual Rust/Git integration tests; not executed in the draft environment.
use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    work: std::path::PathBuf,
    store: std::path::PathBuf,
    first: String,
    middle: String,
    last: String,
    before: String,
    after: String,
    excluded: String,
    gitlink: String,
}
fn command(work: &Path, args: &[&str]) -> Vec<u8> {
    run(Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(work)
        .args(args))
    .unwrap()
}
fn text(work: &Path, args: &[&str]) -> String {
    String::from_utf8(command(work, args))
        .unwrap()
        .trim()
        .to_owned()
}
impl Fixture {
    fn new(format: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let work = directory.path().join("work");
        run(Command::new("git")
            .args(["init", "--quiet", "--template="])
            .arg(format!("--object-format={format}"))
            .arg(&work))
        .unwrap();
        command(&work, &["config", "user.name", "Fixture"]);
        command(&work, &["config", "user.email", "fixture@example.invalid"]);
        command(&work, &["config", "commit.gpgsign", "false"]);
        fs::create_dir(work.join("src")).unwrap();
        fs::write(work.join("src/lib.rs"), "pub fn before() {}\n").unwrap();
        fs::write(work.join("excluded.bin"), b"excluded bytes").unwrap();
        command(&work, &["add", "."]);
        command(&work, &["commit", "-qm", "first"]);
        let first = text(&work, &["rev-parse", "HEAD"]);
        let before = text(&work, &["rev-parse", "HEAD:src/lib.rs"]);
        fs::write(work.join("src/lib.rs"), "pub fn middle() {}\n").unwrap();
        command(&work, &["add", "."]);
        command(&work, &["commit", "-qm", "middle"]);
        let middle = text(&work, &["rev-parse", "HEAD"]);
        fs::write(work.join("src/lib.rs"), "pub fn after() {}\n").unwrap();
        command(&work, &["add", "."]);
        let gitlink = "d".repeat(first.len());
        command(
            &work,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{gitlink},submodule"),
            ],
        );
        command(&work, &["commit", "-qm", "last"]);
        let last = text(&work, &["rev-parse", "HEAD"]);
        let after = text(&work, &["rev-parse", "HEAD:src/lib.rs"]);
        let excluded = text(&work, &["rev-parse", "HEAD:excluded.bin"]);
        let store = work.join(".git");
        Self {
            _directory: directory,
            work,
            store,
            first,
            middle,
            last,
            before,
            after,
            excluded,
            gitlink,
        }
    }
    fn target(&self, name: &str) -> std::path::PathBuf {
        self._directory.path().join(name)
    }
    fn roots(&self) -> Retention {
        Retention {
            revisions: BTreeSet::from([self.first.clone(), self.last.clone()]),
            selected: BTreeSet::from([self.before.clone(), self.after.clone()]),
        }
    }
}

#[test]
fn offline_rebuild_keeps_only_selected_snapshots_for_both_hash_formats() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format);
        let target = f.target("retained");
        rebuild(&f.store, &target, &f.roots()).unwrap();
        assert!(completed(&target).unwrap());
        for commit in [&f.first, &f.last] {
            assert_eq!(
                tree_ids(&f.store, commit).unwrap(),
                tree_ids(&target, commit).unwrap()
            );
        }
        let absent = BTreeSet::from([f.middle.clone(), f.excluded.clone(), f.gitlink.clone()]);
        assert!(kinds(&target, &absent).unwrap().is_empty());
        assert_eq!(
            run(git(&target).args(["show", &format!("{}:src/lib.rs", f.last)])).unwrap(),
            b"pub fn after() {}\n"
        );
    }
}

#[test]
fn absent_root_commit_does_not_discard_its_available_blob() {
    let f = Fixture::new("sha1");
    let target = f.target("missing-commit");
    let roots = Retention {
        revisions: BTreeSet::from(["e".repeat(40)]),
        selected: BTreeSet::from([f.before.clone()]),
    };
    rebuild(&f.store, &target, &roots).unwrap();
    assert_eq!(
        kinds(&target, &BTreeSet::from([f.before.clone()]))
            .unwrap()
            .len(),
        1
    );
    assert!(
        kinds(&target, &BTreeSet::from([f.first.clone()]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn excluded_missing_blobs_do_not_get_imported_during_rebuild() {
    let f = Fixture::new("sha1");
    let first = f.target("partial");
    let second = f.target("again");
    rebuild(&f.store, &first, &f.roots()).unwrap();
    let mut roots = f.roots();
    roots.selected.insert(f.excluded.clone());
    rebuild(&first, &second, &roots).unwrap();
    assert!(completed(&second).unwrap());
    assert!(
        kinds(&second, &BTreeSet::from([f.excluded.clone()]))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn incomplete_retained_tree_fails_without_publishing_a_candidate() {
    let f = Fixture::new("sha1");
    let broken = f.target("broken");
    let target = f.target("candidate");
    run(Command::new("git")
        .args(["init", "--bare", "--quiet", "--template="])
        .arg(&broken))
    .unwrap();
    let bytes = command(&f.work, &["cat-file", "commit", &f.first]);
    let inserted = crate::process::capture(
        git(&broken).args(["hash-object", "-w", "-t", "commit", "--stdin"]),
        Duration::from_secs(120),
        Some(bytes),
        None,
    )
    .unwrap();
    assert!(inserted.status.success());
    assert!(rebuild(&broken, &target, &f.roots()).is_err());
    assert!(!target.exists());
    assert_eq!(
        kinds(&broken, &BTreeSet::from([f.first.clone()]))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn empty_retention_creates_a_valid_empty_pool() {
    let f = Fixture::new("sha256");
    let target = f.target("empty");
    rebuild(&f.store, &target, &Retention::default()).unwrap();
    assert!(completed(&target).unwrap());
    assert_eq!(
        run(git(&target).args(["rev-parse", "--show-object-format"])).unwrap(),
        b"sha256\n"
    );
}
