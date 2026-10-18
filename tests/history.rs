//! Public-service regressions. Fixtures contain committed objects, not checked-out
//! source trees; reaching any workspace preparation path is therefore a failure.
mod real;

use sigla::{discovery::Policy, service::App};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

struct Fixture {
    root: tempfile::TempDir,
    cache: tempfile::TempDir,
    app: Arc<App>,
}
fn git(root: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "History Test")
        .env("GIT_AUTHOR_EMAIL", "history@example.invalid")
        .env("GIT_COMMITTER_NAME", "History Test")
        .env("GIT_COMMITTER_EMAIL", "history@example.invalid")
        // Equal timestamps intentionally make date ordering useless.
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        git(
            root.path(),
            &["init", "--quiet", "--template=", "--initial-branch=main"],
            b"",
        );
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                2,
            )
            .unwrap(),
        );
        Self { root, cache, app }
    }
    fn commit(&self, files: &[(&str, &str)], parents: &[&str], subject: &str) -> String {
        git(self.root.path(), &["read-tree", "--empty"], b"");
        for (path, text) in files {
            let blob = git(
                self.root.path(),
                &["hash-object", "-w", "--stdin"],
                text.as_bytes(),
            );
            git(
                self.root.path(),
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                b"",
            );
        }
        let tree = git(self.root.path(), &["write-tree"], b"");
        let mut args = vec!["commit-tree", &tree, "-m", subject];
        for parent in parents {
            args.extend(["-p", *parent]);
        }
        let commit = git(self.root.path(), &args, b"");
        git(
            self.root.path(),
            &["update-ref", "refs/heads/main", &commit],
            b"",
        );
        commit
    }
    fn remove_commit(&self, id: &str) {
        std::fs::remove_file(
            self.root
                .path()
                .join(".git/objects")
                .join(&id[..2])
                .join(&id[2..]),
        )
        .unwrap();
    }
    async fn history(&self, at: &str, query: &str) -> String {
        self.app
            .history(&format!("{}#{at}", self.root.path().display()), query)
            .await
            .unwrap()
    }
    async fn error(&self, at: &str, query: &str) -> String {
        self.app
            .history(&format!("{}#{at}", self.root.path().display()), query)
            .await
            .unwrap_err()
            .to_string()
    }
}
fn ids(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| matches!(word.len(), 40 | 64) && word.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect()
}
fn body(page: &str) -> &str {
    page.split_once("\n\n").unwrap().1
}
fn next(page: &str) -> Option<usize> {
    page.split_once("\n\n").unwrap().0.lines().find_map(|line| {
        line.strip_prefix("more offset:")
            .map(|n| n.parse().unwrap())
    })
}

#[tokio::test]
async fn committed_objects_ignore_worktree_index_and_invalid_build_graph() {
    let fixture = Fixture::new();
    let commit = fixture.commit(
        &[
            ("src/lib.rs", "pub fn committed() -> i32 { 42 }\n"),
            ("Cargo.toml", "this is deliberately invalid TOML"),
            ("Broken.csproj", "<not-a-build-graph"),
        ],
        &[],
        "committed only",
    );
    std::fs::create_dir(fixture.root.path().join("src")).unwrap();
    let dirty = "pub fn dirty() -> i32 { 99 }\n";
    std::fs::write(fixture.root.path().join("src/lib.rs"), dirty).unwrap();
    let index = std::fs::read(fixture.root.path().join(".git/index")).unwrap();
    let text = fixture.history(&commit, "log function:committed").await;
    assert_eq!(ids(&text), [commit.as_str()], "{text}");
    assert!(text.contains("end\n"), "{text}");
    assert!(!fixture.cache.path().join("analysis").exists());
    assert!(!fixture.cache.path().join("local-jobs").exists());
    assert_eq!(
        std::fs::read(fixture.root.path().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.path().join("src/lib.rs")).unwrap(),
        dirty
    );
    assert!(
        fixture
            .error(&commit, "log function:dirty")
            .await
            .contains("No declaration")
    );
}

#[tokio::test]
async fn anchored_declaration_follows_rename_move_and_omits_sibling_edits() {
    let f = Fixture::new();
    let a = "class Worker { int Read(int value) { var shifted = value + 10; return shifted * value; } int Other() { return 1; } }\n";
    let b = a.replace("+ 10", "+ 11");
    let c = b.replace("Read(", "Parse(");
    let d = c.replace("return 1;", "return 2;");
    let root = f.commit(&[("old/Worker.cs", a)], &[], "introduce");
    let edit = f.commit(&[("old/Worker.cs", &b)], &[&root], "edit selected");
    let rename = f.commit(&[("old/Worker.cs", &c)], &[&edit], "rename selected");
    let moved = f.commit(&[("new/Worker.cs", &c)], &[&rename], "move file");
    let tip = f.commit(&[("new/Worker.cs", &d)], &[&moved], "edit sibling");
    let text = f
        .history(&tip, "log m:Worker.Parse(int) path:new/** limit:100")
        .await;
    assert_eq!(
        ids(&text),
        [&moved, &rename, &edit, &root].map(String::as_str),
        "{text}"
    );
    assert!(text.contains("rename") && text.contains("move"), "{text}");
    let skipped = f
        .history(&tip, "log m:Worker.Parse(int) skip:2 limit:1")
        .await;
    assert_eq!(ids(&skipped), [edit.as_str()], "{skipped}");
    let diff = f
        .history(&tip, &format!("show {edit} m:Worker.Parse(int)"))
        .await;
    assert!(
        body(&diff).contains("shifted") && !body(&diff).contains("Other"),
        "{diff}"
    );
    let forward = f
        .history(&root, &format!("diff {root}..{tip} m:Worker.Read(int)"))
        .await;
    assert!(
        body(&forward).contains("Parse") && body(&forward).contains("Read"),
        "{forward}"
    );
    assert!(!body(&forward).contains("Other"));
}

#[tokio::test]
async fn copying_from_an_unchanged_file_retains_uncertain_origin() {
    let f = Fixture::new();
    let origin = "pub fn origin(input: &str) -> String { let trimmed = input.trim(); let canonical = trimmed.to_lowercase(); canonical.replace('-', \"_\") }\n";
    let copy = origin.replace("origin(", "copied(");
    let root = f.commit(&[("origin.rs", origin)], &[], "origin");
    let tip = f.commit(
        &[("origin.rs", origin), ("copied.rs", &copy)],
        &[&root],
        "copy with origin retained",
    );
    let text = f.history(&tip, "log function:copied").await;
    assert_eq!(ids(&text), [tip.as_str(), root.as_str()], "{text}");
    assert!(text.contains("~copy"), "{text}");
}

#[tokio::test]
async fn overload_extraction_identifies_the_retained_wrapper() {
    let f = Fixture::new();
    let before = "class Reader { string Read(string input) { var trimmed = input.Trim(); var canonical = trimmed.ToLowerInvariant(); return canonical.Replace('-', '_'); } }\n";
    let after = "class Reader { string Read(string input) { return Read(input, true); } string Read(string input, bool normalize) { var trimmed = input.Trim(); var canonical = trimmed.ToLowerInvariant(); return canonical.Replace('-', '_'); } }\n";
    let root = f.commit(&[("Reader.cs", before)], &[], "original overload");
    let tip = f.commit(
        &[("Reader.cs", after)],
        &[&root],
        "retain wrapper and extract overload",
    );
    let text = f.history(&tip, "log method:Reader.Read(string,bool)").await;
    assert_eq!(ids(&text), [tip.as_str(), root.as_str()], "{text}");
    assert!(text.contains("wrapper"), "{text}");
    let ambiguous = f.error(&tip, "log method:Reader.Read").await;
    assert!(
        ambiguous.contains("Ambiguous anchor") && ambiguous.contains("string,bool"),
        "{ambiguous}"
    );
}

#[tokio::test]
async fn extraction_tracks_only_the_predecessor_fragment() {
    let f = Fixture::new();
    let fragment = "var trimmed = input.Trim(); var canonical = trimmed.ToLowerInvariant(); return canonical.Replace('-', '_');";
    let before = format!(
        "class Reader {{ string Read(string input) {{ Audit(\"unrelated one\"); Audit(\"unrelated two\"); Audit(\"unrelated three\"); Audit(\"unrelated four\"); Audit(\"unrelated five\"); {fragment} }} }}\n"
    );
    let middle = before.replace("unrelated three", "changed unrelated audit");
    let after = format!(
        "class Reader {{ string Read(string input) {{ Audit(\"changed unrelated audit\"); return Normalize(input); }} string Normalize(string input) {{ {fragment} }} }}\n"
    );
    let root = f.commit(&[("Reader.cs", &before)], &[], "old container");
    let audit = f.commit(&[("Reader.cs", &middle)], &[&root], "only unrelated audit");
    let tip = f.commit(&[("Reader.cs", &after)], &[&audit], "extract helper");
    let text = f.history(&tip, "log method:Reader.Normalize(string)").await;
    assert_eq!(ids(&text), [tip.as_str(), root.as_str()], "{text}");
    let diff = f
        .history(&tip, &format!("show {tip} method:Reader.Normalize(string)"))
        .await;
    assert!(
        body(&diff).contains("trimmed") && !body(&diff).contains("Audit("),
        "{diff}"
    );
}

#[tokio::test]
async fn text_passage_follows_reflow_and_move_without_adjacent_sections() {
    let f = Fixture::new();
    let old = "# Design\n\nSelected passage preserves distinctive ordered words across reflow and file movement.\n\nUnrelated section remains separate.\n";
    let new = "# Design\n\nSelected passage preserves distinctive ordered words\nacross reflow and file movement.\n\nUnrelated section changed independently.\n";
    let root = f.commit(&[("docs/old.md", old)], &[], "passage introduced");
    let tip = f.commit(&[("notes/new.md", new)], &[&root], "move and reflow");
    let text = f.history(&tip, "log @notes/new.md:3-4").await;
    assert_eq!(ids(&text), [tip.as_str(), root.as_str()], "{text}");
    let diff = f
        .history(&tip, &format!("show {tip} @notes/new.md:3-4"))
        .await;
    assert!(
        body(&diff).contains("Selected passage") && !body(&diff).contains("Unrelated section"),
        "{diff}"
    );
}

#[tokio::test]
async fn renamed_directory_includes_members_deleted_before_anchor() {
    let f = Fixture::new();
    let root = f.commit(
        &[
            ("old/a.rs", "const A: i32 = 1;\n"),
            ("old/deleted.rs", "const D: i32 = 2;\n"),
        ],
        &[],
        "two members",
    );
    let deletion = f.commit(
        &[("old/a.rs", "const A: i32 = 1;\n")],
        &[&root],
        "delete member",
    );
    let tip = f.commit(
        &[("new/a.rs", "const A: i32 = 1;\n")],
        &[&deletion],
        "move directory",
    );
    let text = f.history(&tip, "log path:new/**").await;
    assert_eq!(
        ids(&text),
        [tip.as_str(), deletion.as_str(), root.as_str()],
        "{text}"
    );
}

#[tokio::test]
async fn same_name_after_deletion_does_not_claim_a_trivial_neighbour() {
    let f = Fixture::new();
    let root = f.commit(
        &[("lib.rs", "fn target() -> i32 { 1 }\n")],
        &[],
        "first identity",
    );
    let absent = f.commit(
        &[("lib.rs", "fn sibling() -> i32 { 7 }\n")],
        &[&root],
        "replace with unrelated declaration",
    );
    let tip = f.commit(
        &[(
            "lib.rs",
            "fn sibling() -> i32 { 7 }\nfn target() -> i32 { 1 }\n",
        )],
        &[&absent],
        "reuse old name",
    );
    let text = f.history(&tip, "log function:target").await;
    assert_eq!(ids(&text), [tip.as_str()], "{text}");
    assert!(text.ends_with("end\n"), "{text}");
}

#[tokio::test]
async fn merge_rejection_preserves_completed_edit_and_revert_cycles() {
    let f = Fixture::new();
    let base_text = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let root = f.commit(&[("lib.rs", base_text)], &[], "base");
    let edit = f.commit(
        &[("lib.rs", &base_text.replace("X: i32 = 1", "X: i32 = 2"))],
        &[&root],
        "ordinary edit",
    );
    let revert = f.commit(&[("lib.rs", base_text)], &[&edit], "ordinary revert");
    let rejected = f.commit(
        &[("lib.rs", &base_text.replace("X: i32 = 1", "X: i32 = 3"))],
        &[&revert],
        "outstanding rejected change",
    );
    let main = f.commit(&[("lib.rs", base_text)], &[&root], "empty first parent");
    let merge = f.commit(
        &[("lib.rs", base_text)],
        &[&main, &rejected],
        "merge rejects outstanding change",
    );
    let source = f.history(&merge, "log file:lib.rs limit:100").await;
    assert_eq!(
        ids(&source),
        [revert.as_str(), edit.as_str(), root.as_str()],
        "{source}"
    );
    let all = f.history(&merge, "log limit:100").await;
    assert_eq!(
        ids(&all),
        [
            merge.as_str(),
            main.as_str(),
            rejected.as_str(),
            revert.as_str(),
            edit.as_str(),
            root.as_str()
        ],
        "{all}"
    );
    let second = f
        .history(&merge, &format!("show {merge} parent:2 file:lib.rs"))
        .await;
    assert!(second.contains(&format!("from {rejected}\n")), "{second}");
    assert!(
        body(&second).contains("-const X: i32 = 3;")
            && body(&second).contains("+const X: i32 = 1;"),
        "{second}"
    );
}

#[tokio::test]
async fn rejected_deletion_preserves_earlier_source_history() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let root = f.commit(&[("lib.rs", original)], &[], "base");
    let edit = f.commit(
        &[("lib.rs", "pub fn target() -> i32 { 2 }\n")],
        &[&root],
        "ordinary edit",
    );
    let revert = f.commit(&[("lib.rs", original)], &[&edit], "ordinary revert");
    let deleted = f.commit(&[], &[&revert], "delete selected source");
    let main = f.commit(&[("lib.rs", original)], &[&root], "unchanged first parent");
    for parents in [[&main, &deleted], [&deleted, &main]] {
        let merge = f.commit(
            &[("lib.rs", original)],
            &parents.map(String::as_str),
            "reject deletion",
        );
        let deleted_parent = parents.iter().position(|id| *id == &deleted).unwrap() + 1;
        for selector in ["file:lib.rs", "function:target"] {
            let source = f.history(&merge, &format!("log {selector}")).await;
            assert_eq!(
                ids(&source),
                [revert.as_str(), edit.as_str(), root.as_str()],
                "{selector}: {source}"
            );
            assert!(source.ends_with("end\n"), "{source}");
            let page = f
                .history(&merge, &format!("log {selector} skip:1 limit:1"))
                .await;
            assert_eq!(ids(&page), [edit.as_str()], "{page}");
            assert!(page.ends_with("more skip:2\n"), "{page}");
            let deletion = f
                .history(&merge, &format!("show {deleted} {selector}"))
                .await;
            assert!(body(&deletion).contains("+++ /dev/null"), "{deletion}");
            assert!(body(&deletion).contains("-pub fn target()"), "{deletion}");
            let restored = f
                .history(
                    &merge,
                    &format!("show {merge} parent:{deleted_parent} {selector}"),
                )
                .await;
            assert!(body(&restored).contains("--- /dev/null"), "{restored}");
            assert!(body(&restored).contains("+pub fn target()"), "{restored}");
            let diff = f
                .history(&merge, &format!("diff {revert}..{deleted} {selector}"))
                .await;
            assert_eq!(body(&diff), body(&deletion));
        }
    }
}

#[tokio::test]
async fn adjacent_accepted_and_rejected_edits_keep_a_partial_commit() {
    let f = Fixture::new();
    let base = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let mixed = "const X: i32 = 3;\nconst Y: i32 = 2;\n";
    let accepted = "const X: i32 = 1;\nconst Y: i32 = 2;\n";
    let root = f.commit(&[("lib.rs", base)], &[], "base");
    let branch = f.commit(&[("lib.rs", mixed)], &[&root], "mixed change");
    let main = f.commit(&[("lib.rs", base)], &[&root], "empty main");
    let merge = f.commit(&[("lib.rs", accepted)], &[&main, &branch], "accept Y only");
    let text = f.history(&merge, "log file:lib.rs").await;
    assert_eq!(ids(&text), [branch.as_str(), root.as_str()], "{text}");
    let record = text.lines().find(|line| line.starts_with(&branch)).unwrap();
    assert!(
        record.contains("partial") && record.contains("+1 -1"),
        "{record}"
    );
}

#[tokio::test]
async fn source_history_omits_clean_combination_but_keeps_resolution_edits() {
    let f = Fixture::new();
    let root = f.commit(
        &[("lib.rs", "const X: i32 = 1;\nconst Y: i32 = 1;\n")],
        &[],
        "base",
    );
    let left = f.commit(
        &[("lib.rs", "const X: i32 = 2;\nconst Y: i32 = 1;\n")],
        &[&root],
        "left",
    );
    let right = f.commit(
        &[("lib.rs", "const X: i32 = 1;\nconst Y: i32 = 2;\n")],
        &[&root],
        "right",
    );
    let clean = f.commit(
        &[("lib.rs", "const X: i32 = 2;\nconst Y: i32 = 2;\n")],
        &[&left, &right],
        "clean merge",
    );
    let text = f.history(&clean, "log file:lib.rs").await;
    assert_eq!(
        ids(&text),
        [left.as_str(), right.as_str(), root.as_str()],
        "{text}"
    );
    let resolved = f.commit(
        &[("lib.rs", "const X: i32 = 9;\nconst Y: i32 = 2;\n")],
        &[&left, &right],
        "resolution authored here",
    );
    let text = f.history(&resolved, "log file:lib.rs").await;
    assert_eq!(ids(&text).first(), Some(&resolved.as_str()), "{text}");
    assert!(
        text.contains("merge-overlap attribution unresolved"),
        "{text}"
    );
}

#[tokio::test]
async fn root_show_names_the_real_empty_tree_and_absent_endpoint_is_a_deletion() {
    let f = Fixture::new();
    let root = f.commit(
        &[(
            "lib.rs",
            "pub fn present(value: i32) -> i32 { value * 27 }\n",
        )],
        &[],
        "root",
    );
    let absent = f.commit(&[("other.txt", "unrelated\n")], &[&root], "delete file");
    let root_diff = f
        .history(&root, &format!("show {root} function:present"))
        .await;
    let empty = git(
        f.root.path(),
        &["hash-object", "-t", "tree", "--stdin"],
        b"",
    );
    assert!(
        root_diff.contains(&format!("from {empty} (empty tree)")),
        "{root_diff}"
    );
    assert!(body(&root_diff).contains("--- /dev/null"), "{root_diff}");
    let deletion = f
        .history(&root, &format!("diff {root}..{absent} function:present"))
        .await;
    assert!(body(&deletion).contains("+++ /dev/null"), "{deletion}");
}

#[tokio::test]
async fn canonical_diff_pages_reconstruct_across_fresh_service_and_cache() {
    let f = Fixture::new();
    let old = format!("{}\n", "α".repeat(30_000));
    let new = format!("{}\n", "λ".repeat(30_000));
    let root = f.commit(&[("large.txt", &old)], &[], "old");
    let tip = f.commit(&[("large.txt", &new)], &[&root], "new");
    let query = format!("diff {root}..{tip} file:large.txt");
    let mut canonical = String::new();
    let mut offset = 0;
    loop {
        let page = f.history(&tip, &format!("{query} offset:{offset}")).await;
        canonical.push_str(body(&page));
        let Some(following) = next(&page) else {
            break;
        };
        assert!(following > offset);
        offset = following;
    }
    assert!(offset > 0, "the test must actually paginate");
    assert!(canonical.contains(&format!("-{old}")) && canonical.contains(&format!("+{new}")));
    let fresh_cache = tempfile::tempdir().unwrap();
    let fresh = Arc::new(
        App::new(
            Policy::new(vec![f.root.path().into()]).unwrap(),
            fresh_cache.path().into(),
            2,
        )
        .unwrap(),
    );
    let resumed = fresh
        .history(
            &format!("{}#{tip}", f.root.path().display()),
            &format!("{query} offset:{offset}"),
        )
        .await
        .unwrap();
    assert_eq!(body(&resumed), &canonical[offset..]);
    let inside_character = canonical.find('λ').unwrap() + 1;
    assert!(
        f.error(&tip, &format!("{query} offset:{inside_character}"))
            .await
            .contains("UTF-8")
    );
}

#[tokio::test]
async fn local_missing_parent_is_incomplete_not_an_introduction() {
    let f = Fixture::new();
    let root = f.commit(&[("lib.rs", "const VALUE: i32 = 1;\n")], &[], "root");
    let tip = f.commit(&[("lib.rs", "const VALUE: i32 = 2;\n")], &[&root], "tip");
    f.remove_commit(&root);
    let text = f.history(&tip, "log file:lib.rs").await;
    assert!(
        text.contains("incomplete:") && text.contains(&root),
        "{text}"
    );
    assert!(
        !text.ends_with("end\n") && !text.contains(" introduction "),
        "{text}"
    );
    let repository = f.history(&tip, "log").await;
    assert_eq!(ids(&repository), [tip.as_str()], "{repository}");
    assert!(
        repository.contains("incomplete:") && repository.contains(&root),
        "{repository}"
    );
    let first = f.history(&tip, "log limit:1").await;
    assert_eq!(ids(&first), [tip.as_str()], "{first}");
    assert!(!first.contains("incomplete:"), "{first}");
}

#[tokio::test]
async fn committed_attributes_exclude_lfs_and_binary_even_with_source_extensions() {
    let f = Fixture::new();
    let pointer = "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 1000000000\n";
    let tip = f.commit(
        &[
            (".gitattributes", "large.rs filter=lfs\nopaque.cs binary\n"),
            ("large.rs", pointer),
            ("opaque.cs", "class ShouldNotBeParsed {}\n"),
            ("normal.rs", "pub fn normal() {}\n"),
        ],
        &[],
        "attributes",
    );
    assert!(
        f.error(&tip, "log file:large.rs")
            .await
            .contains("excluded")
    );
    assert!(
        f.error(&tip, "log class:ShouldNotBeParsed")
            .await
            .contains("No declaration")
    );
    let text = f.history(&tip, &format!("show {tip}")).await;
    assert!(
        body(&text).contains("normal") && !body(&text).contains("git-lfs.github.com"),
        "{text}"
    );
    assert!(!body(&text).contains("ShouldNotBeParsed"), "{text}");
}

#[tokio::test]
async fn mcp_history_executes_without_preparing_a_workspace() {
    use rmcp::{
        ServiceExt,
        model::CallToolRequestParams,
        transport::{
            StreamableHttpClientTransport,
            streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            },
        },
    };
    let f = Fixture::new();
    let commit = f.commit(
        &[("lib.rs", "pub fn from_objects() -> i32 { 42 }\n")],
        &[],
        "MCP history",
    );
    let app = f.app.clone();
    let service = StreamableHttpService::new(
        move || Ok(sigla::service::Mcp::new(app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_json_response(true),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    let client = ().serve(StreamableHttpClientTransport::from_uri(url)).await.unwrap();
    let response = client.call_tool(CallToolRequestParams::new("history").with_arguments(
        serde_json::json!({"codebase":format!("{}#{commit}", f.root.path().display()),"query":"log function:from_objects"}).as_object().unwrap().clone()
    )).await.unwrap();
    let value = serde_json::to_value(response).unwrap();
    assert_ne!(value["isError"], true, "{value}");
    let text = value["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(&commit) && text.contains("from_objects"),
        "{text}"
    );
    assert!(!f.cache.path().join("analysis").exists());
    client.cancel().await.unwrap();
    task.abort();
}

#[tokio::test]
async fn unchanged_text_passage_preserves_punctuation_and_whitespace() {
    for passage in [
        "Stable selected passage with distinctive words.",
        "    Stable selected passage with distinctive words.",
        "Stable selected passage with λ and 🦀.",
    ] {
        let f = Fixture::new();
        let old = format!("# Design\n\n{passage}\n\nUnrelated paragraph.\n");
        let new = old
            .replace(
                "Unrelated paragraph.",
                "Unrelated paragraph edited independently.",
            )
            .replace("# Design", "# Updated design");
        let root = f.commit(&[("design.md", &old)], &[], "selected passage");
        let tip = f.commit(&[("design.md", &new)], &[&root], "unrelated paragraphs");
        let text = f.history(&tip, "log @design.md:3-3").await;
        let comparison = f.history(&tip, &format!("show {tip} @design.md:3-3")).await;
        assert_eq!(ids(&text), [root.as_str()], "{text}\n{comparison}");
        assert_eq!(body(&comparison), "No changes.\n");
    }
}

#[tokio::test]
async fn an_excluded_predecessor_is_unresolved_rather_than_an_introduction() {
    let f = Fixture::new();
    let source = "pub fn selected(value: i32) -> i32 { value * 31 }\n";
    let root = f.commit(
        &[(".gitattributes", "lib.rs binary\n"), ("lib.rs", source)],
        &[],
        "binary policy",
    );
    let tip = f.commit(&[("lib.rs", source)], &[&root], "source policy");
    let text = f.history(&tip, "log function:selected").await;
    assert!(
        text.contains("incomplete:") && text.contains("excluded by committed source attributes"),
        "{text}"
    );
    assert!(ids(&text).is_empty(), "{text}");
}

#[tokio::test]
async fn rejected_declaration_deletion_preserves_history_in_a_surviving_file() {
    for (path, selector, original, removed) in [
        (
            "lib.rs",
            "function:target",
            "fn sibling() -> i32 { 7 }\npub fn target() -> i32 { 1 }\n",
            "fn sibling() -> i32 { 7 }\n",
        ),
        (
            "Worker.cs",
            "method:Worker.Target()",
            "class Worker { int Sibling() { return 7; } int Target() { return 1; } }\n",
            "class Worker { int Sibling() { return 7; } }\n",
        ),
    ] {
        let f = Fixture::new();
        let changed = original.replace("1", "2");
        let root = f.commit(&[(path, original)], &[], "base");
        let edit = f.commit(&[(path, &changed)], &[&root], "ordinary edit");
        let revert = f.commit(&[(path, original)], &[&edit], "ordinary revert");
        let deleted = f.commit(&[(path, removed)], &[&revert], "delete declaration only");
        let side = f.commit(
            &[(path, &removed.replace("7", "8"))],
            &[&deleted],
            "unrelated sibling edit while absent",
        );
        let main = f.commit(&[(path, original)], &[&root], "unchanged first parent");
        let merge = f.commit(
            &[(path, &original.replace("7", "8"))],
            &[&main, &side],
            "accept sibling edit and reject target deletion",
        );
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(
            ids(&text),
            [revert.as_str(), edit.as_str(), root.as_str()],
            "{selector}: {text}"
        );
        assert!(text.ends_with("end\n"), "{text}");
        let deletion = f
            .history(&merge, &format!("show {deleted} {selector}"))
            .await;
        assert!(body(&deletion).contains("+++ /dev/null"), "{deletion}");
        assert!(!body(&deletion).contains("ibling"), "{deletion}");
    }
}

#[tokio::test]
async fn rejected_deletion_recovers_history_across_a_file_move() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let root = f.commit(&[("old.rs", original)], &[], "base");
    let moved = f.commit(&[("moved.rs", original)], &[&root], "move selected source");
    let edit = f.commit(
        &[("moved.rs", "pub fn target() -> i32 { 2 }\n")],
        &[&moved],
        "ordinary edit after move",
    );
    let revert = f.commit(&[("moved.rs", original)], &[&edit], "ordinary revert");
    let deleted = f.commit(&[], &[&revert], "delete moved source");
    let side = f.commit(
        &[("note.txt", "unrelated\n")],
        &[&deleted],
        "absent interval",
    );
    let main = f.commit(&[("old.rs", original)], &[&root], "unchanged first parent");
    let merge = f.commit(
        &[("old.rs", original)],
        &[&main, &side],
        "keep original source",
    );
    for selector in ["file:old.rs", "function:target"] {
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(
            ids(&text),
            [
                revert.as_str(),
                edit.as_str(),
                moved.as_str(),
                root.as_str()
            ],
            "{selector}: {text}"
        );
        assert!(text.contains("move old.rs→moved.rs"), "{text}");
        let deletion = f
            .history(&merge, &format!("show {deleted} {selector}"))
            .await;
        assert!(body(&deletion).contains("--- a/moved.rs"), "{deletion}");
        assert!(body(&deletion).contains("+++ /dev/null"), "{deletion}");
    }
}

#[tokio::test]
async fn rejected_deletion_does_not_follow_reused_paths_or_declarations() {
    for (selector, keep_file) in [("file:lib.rs", false), ("function:target", true)] {
        let f = Fixture::new();
        let original = "fn sibling() -> i32 { 7 }\npub fn target() -> i32 { 1 }\n";
        let absent = if keep_file {
            vec![("lib.rs", "fn sibling() -> i32 { 7 }\n")]
        } else {
            Vec::new()
        };
        let root = f.commit(&[("lib.rs", original)], &[], "base");
        let edit = f.commit(
            &[("lib.rs", &original.replace("{ 1 }", "{ 2 }"))],
            &[&root],
            "ordinary edit",
        );
        let revert = f.commit(&[("lib.rs", original)], &[&edit], "ordinary revert");
        let deleted = f.commit(&absent, &[&revert], "delete original lifetime");
        // Identical bytes deliberately defeat a last-known-blob/name/path search.
        let reused = f.commit(&[("lib.rs", original)], &[&deleted], "unrelated reuse");
        let unrelated = f.commit(
            &[("lib.rs", &original.replace("{ 1 }", "{ 9 }"))],
            &[&reused],
            "edit unrelated incarnation",
        );
        let side = f.commit(&absent, &[&unrelated], "delete unrelated incarnation");
        let main = f.commit(&[("lib.rs", original)], &[&root], "unchanged first parent");
        let merge = f.commit(
            &[("lib.rs", original)],
            &[&main, &side],
            "retain original lifetime",
        );
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(
            ids(&text),
            [revert.as_str(), edit.as_str(), root.as_str()],
            "{selector}: {text}"
        );
        let show = f
            .history(&merge, &format!("show {unrelated} {selector}"))
            .await;
        assert_eq!(body(&show), "No changes.\n", "{show}");
        let show = f
            .history(&merge, &format!("show {deleted} {selector}"))
            .await;
        assert!(body(&show).contains("+++ /dev/null"), "{show}");
    }
}

#[tokio::test]
async fn a_reused_path_has_a_new_lifetime_in_logs_and_comparisons() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let root = f.commit(&[("lib.rs", original)], &[], "old lifetime");
    let absent = f.commit(&[], &[&root], "end old lifetime");
    let reused = f.commit(
        &[("lib.rs", original)],
        &[&absent],
        "new lifetime, identical bytes",
    );
    for selector in ["file:lib.rs", "function:target"] {
        let text = f.history(&reused, &format!("log {selector}")).await;
        assert_eq!(ids(&text), [reused.as_str()], "{text}");
        assert!(text.ends_with("end\n"), "{text}");
        let diff = f
            .history(&root, &format!("diff {root}..{reused} {selector}"))
            .await;
        assert!(body(&diff).contains("+++ /dev/null"), "{diff}");
        let show = f.history(&root, &format!("show {reused} {selector}")).await;
        assert_eq!(body(&show), "No changes.\n", "{show}");
    }
}

#[tokio::test]
async fn a_merge_does_not_bridge_before_the_selected_lifetime() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let old = f.commit(
        &[("lib.rs", original)],
        &[],
        "unrelated earlier incarnation",
    );
    let base = f.commit(&[], &[&old], "old lifetime ended");
    let introduced = f.commit(
        &[("lib.rs", original)],
        &[&base],
        "introduce selected lifetime",
    );
    let side = f.commit(&[], &[&base], "selected lifetime never existed here");
    let merge = f.commit(
        &[("lib.rs", original)],
        &[&introduced, &side],
        "import new lifetime",
    );
    for selector in ["file:lib.rs", "function:target"] {
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(ids(&text), [introduced.as_str()], "{text}");
        assert!(text.ends_with("end\n"), "{text}");
    }
}

#[tokio::test]
async fn rejected_empty_file_deletion_is_not_an_unchanged_empty_source() {
    let f = Fixture::new();
    let root = f.commit(&[("empty.txt", "")], &[], "empty source exists");
    let edit = f.commit(&[("empty.txt", "transient\n")], &[&root], "ordinary edit");
    let revert = f.commit(&[("empty.txt", "")], &[&edit], "ordinary revert to empty");
    let deleted = f.commit(&[], &[&revert], "delete empty source");
    let main = f.commit(&[("empty.txt", "")], &[&root], "unchanged first parent");
    let merge = f.commit(
        &[("empty.txt", "")],
        &[&main, &deleted],
        "keep empty source",
    );
    let text = f.history(&merge, "log file:empty.txt").await;
    assert_eq!(
        ids(&text),
        [revert.as_str(), edit.as_str(), root.as_str()],
        "{text}"
    );
    let deletion = f
        .history(&merge, &format!("show {deleted} file:empty.txt"))
        .await;
    assert!(
        body(&deletion).contains("--- a/empty.txt\n+++ /dev/null"),
        "{deletion}"
    );
    assert!(!body(&deletion).contains("@@"), "{deletion}");
    let addition = f
        .history(&merge, &format!("show {root} file:empty.txt"))
        .await;
    assert!(
        body(&addition).contains("--- /dev/null\n+++ b/empty.txt"),
        "{addition}"
    );
}

#[tokio::test]
async fn rejected_deletion_filters_only_outstanding_content() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let root = f.commit(&[("lib.rs", original)], &[], "base");
    let edit = f.commit(
        &[("lib.rs", &original.replace("1", "2"))],
        &[&root],
        "ordinary edit",
    );
    let revert = f.commit(&[("lib.rs", original)], &[&edit], "ordinary revert");
    let rejected = f.commit(
        &[("lib.rs", &original.replace("1", "3"))],
        &[&revert],
        "outstanding rejected content",
    );
    let deleted = f.commit(&[], &[&rejected], "outstanding rejected deletion");
    let main = f.commit(&[("lib.rs", original)], &[&root], "unchanged first parent");
    let merge = f.commit(
        &[("lib.rs", original)],
        &[&main, &deleted],
        "reject deletion and content",
    );
    for selector in ["file:lib.rs", "function:target"] {
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(
            ids(&text),
            [revert.as_str(), edit.as_str(), root.as_str()],
            "{selector}: {text}"
        );
        let deletion = f
            .history(&merge, &format!("show {deleted} {selector}"))
            .await;
        assert!(body(&deletion).contains("{ 3 }"), "{deletion}");
    }
}

#[tokio::test]
async fn rejecting_deletion_can_accept_the_preceding_content_edit() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let changed = "pub fn target() -> i32 { 2 }\n";
    let root = f.commit(&[("lib.rs", original)], &[], "base");
    let edit = f.commit(&[("lib.rs", changed)], &[&root], "accepted edit");
    let deleted = f.commit(&[], &[&edit], "rejected deletion");
    let main = f.commit(&[("lib.rs", original)], &[&root], "unchanged first parent");
    let merge = f.commit(
        &[("lib.rs", changed)],
        &[&main, &deleted],
        "accept edit, reject deletion",
    );
    for selector in ["file:lib.rs", "function:target"] {
        let text = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(
            ids(&text),
            [edit.as_str(), root.as_str()],
            "{selector}: {text}"
        );
        assert!(text.ends_with("end\n"), "{text}");
    }
}

#[tokio::test]
async fn absence_recovery_does_not_claim_excluded_or_unparseable_source() {
    for (files, reason) in [
        (
            vec![("lib.rs", "/*")],
            "historical parse recovery left candidate scope incomplete",
        ),
        (
            vec![
                (".gitattributes", "lib.rs binary\n"),
                ("lib.rs", "pub fn target() -> i32 { 1 }\n"),
            ],
            "excluded by committed source attributes",
        ),
    ] {
        let f = Fixture::new();
        let original = "pub fn target() -> i32 { 1 }\n";
        let root = f.commit(&[("lib.rs", original)], &[], "base");
        let unresolved = f.commit(&files, &[&root], "unresolved source before deletion");
        let deleted = f.commit(&[], &[&unresolved], "absent merge parent");
        let main = f.commit(&[("lib.rs", original)], &[&root], "unchanged first parent");
        let merge = f.commit(&[("lib.rs", original)], &[&main, &deleted], "keep source");
        let text = f.history(&merge, "log function:target").await;
        assert!(
            ids(&text).is_empty(),
            "unfinished interval leaked records: {text}"
        );
        assert!(
            text.contains("incomplete:") && text.contains(reason),
            "{text}"
        );
        assert!(!text.ends_with("end\n"), "{text}");
    }
}
#[tokio::test]
async fn merge_pages_do_not_require_ancestry_before_the_common_base() {
    let f = Fixture::new();
    let original = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let old = f.commit(&[("lib.rs", original)], &[], "older ancestry");
    let base = f.commit(&[("lib.rs", original)], &[&old], "common base");
    let left = f.commit(
        &[("lib.rs", &original.replace("X: i32 = 1", "X: i32 = 2"))],
        &[&base],
        "left edit",
    );
    let right = f.commit(
        &[("lib.rs", &original.replace("Y: i32 = 1", "Y: i32 = 2"))],
        &[&base],
        "right edit",
    );
    let merged = original.replace("i32 = 1", "i32 = 2");
    let merge = f.commit(
        &[("lib.rs", &merged)],
        &[&left, &right],
        "clean combination",
    );
    f.remove_commit(&old);

    let page = f.history(&merge, "log file:lib.rs limit:1").await;
    assert_eq!(ids(&page), [left.as_str()], "{page}");
    assert!(page.ends_with("more skip:1\n"), "{page}");
    assert!(!page.contains("incomplete:"), "{page}");
    let repository = f.history(&merge, "log limit:2").await;
    assert_eq!(
        ids(&repository),
        [merge.as_str(), left.as_str()],
        "{repository}"
    );
    assert!(repository.ends_with("more skip:2\n"), "{repository}");
    let second = f.history(&merge, "log file:lib.rs skip:1 limit:1").await;
    assert_eq!(ids(&second), [right.as_str()], "{second}");
    assert!(second.ends_with("more skip:2\n"), "{second}");
    let unfinished = f.history(&merge, "log file:lib.rs limit:3").await;
    assert_eq!(
        ids(&unfinished),
        [left.as_str(), right.as_str()],
        "{unfinished}"
    );
    assert!(
        unfinished.contains("incomplete:") && unfinished.contains(&old),
        "{unfinished}"
    );
    let boundary = f.history(&merge, "log skip:3 limit:1").await;
    assert_eq!(ids(&boundary), [base.as_str()], "{boundary}");
    assert!(boundary.ends_with("more skip:4\n"), "{boundary}");
    let unfinished = f.history(&merge, "log limit:5").await;
    assert_eq!(
        ids(&unfinished),
        [merge.as_str(), left.as_str(), right.as_str(), base.as_str()],
        "{unfinished}",
    );
    assert!(
        unfinished.contains("incomplete:") && unfinished.contains(&old),
        "{unfinished}"
    );

    let cache = tempfile::tempdir().unwrap();
    let fresh = Arc::new(
        App::new(
            Policy::new(vec![f.root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    for (query, expected) in [
        ("log file:lib.rs skip:1 limit:1", second),
        ("log skip:3 limit:1", boundary),
    ] {
        let replay = fresh
            .history(&format!("{}#{merge}", f.root.path().display()), query)
            .await
            .unwrap();
        assert_eq!(replay, expected);
    }
}

#[tokio::test]
async fn source_comparisons_do_not_require_older_ancestry() {
    let f = Fixture::new();
    let old = f.commit(
        &[("lib.rs", "const VALUE: i32 = 0;\n")],
        &[],
        "older ancestry",
    );
    let parent = f.commit(&[("lib.rs", "const VALUE: i32 = 1;\n")], &[&old], "from");
    let tip = f.commit(&[("lib.rs", "const VALUE: i32 = 2;\n")], &[&parent], "to");
    f.remove_commit(&old);

    for selector in ["file:lib.rs", "const:VALUE"] {
        for anchor in [&tip, &parent] {
            let shown = f.history(anchor, &format!("show {tip} {selector}")).await;
            assert!(body(&shown).contains("-const VALUE: i32 = 1;"), "{shown}");
            assert!(body(&shown).contains("+const VALUE: i32 = 2;"), "{shown}");
            let compared = f
                .history(anchor, &format!("diff {parent}..{tip} {selector}"))
                .await;
            assert_eq!(body(&compared), body(&shown));
            let reverse = f
                .history(anchor, &format!("diff {tip}..{parent} {selector}"))
                .await;
            assert!(
                body(&reverse).contains("-const VALUE: i32 = 2;"),
                "{reverse}"
            );
            assert!(
                body(&reverse).contains("+const VALUE: i32 = 1;"),
                "{reverse}"
            );
        }
        let error = f.error(&tip, &format!("show {parent} {selector}")).await;
        assert!(error.contains(&old), "{error}");
    }
    f.remove_commit(&parent);
    for selector in ["file:lib.rs", "const:VALUE"] {
        let same = f
            .history(&tip, &format!("diff {tip}..{tip} {selector}"))
            .await;
        assert_eq!(body(&same), "No changes.\n", "{same}");
        let error = f.error(&tip, &format!("show {tip} {selector}")).await;
        assert!(error.contains(&parent), "{error}");
    }
}

#[tokio::test]
async fn metadata_can_stop_before_a_missing_base_but_source_cannot() {
    let f = Fixture::new();
    let original = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let base = f.commit(&[("lib.rs", original)], &[], "base");
    let left = f.commit(
        &[("lib.rs", &original.replace("X: i32 = 1", "X: i32 = 2"))],
        &[&base],
        "left",
    );
    let right = f.commit(
        &[("lib.rs", &original.replace("Y: i32 = 1", "Y: i32 = 2"))],
        &[&base],
        "right",
    );
    let merge = f.commit(
        &[("lib.rs", &original.replace("i32 = 1", "i32 = 2"))],
        &[&left, &right],
        "merge",
    );
    f.remove_commit(&base);
    let source = f.history(&merge, "log file:lib.rs limit:1").await;
    assert!(
        ids(&source).is_empty(),
        "An unfinished interval leaked records: {source}"
    );
    assert!(
        source.contains("incomplete:") && source.contains(&base),
        "{source}"
    );
    let page = f.history(&merge, "log limit:3").await;
    assert_eq!(
        ids(&page),
        [merge.as_str(), left.as_str(), right.as_str()],
        "{page}"
    );
    assert!(page.ends_with("more skip:3\n"), "{page}");
    let unfinished = f.history(&merge, "log limit:4").await;
    assert_eq!(ids(&unfinished), ids(&page), "{unfinished}");
    assert!(
        unfinished.contains("incomplete:") && unfinished.contains(&base),
        "{unfinished}"
    );
}

#[tokio::test]
async fn an_unfinished_nested_interval_does_not_invalidate_the_closed_prefix() {
    let f = Fixture::new();
    let original = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let root = f.commit(&[("lib.rs", original)], &[], "root");
    let old_left = f.commit(&[("lib.rs", original)], &[&root], "old left");
    let old_right = f.commit(&[("lib.rs", original)], &[&root], "old right");
    let base = f.commit(
        &[("lib.rs", original)],
        &[&old_left, &old_right],
        "base is a merge",
    );
    let left = f.commit(
        &[("lib.rs", &original.replace("X: i32 = 1", "X: i32 = 2"))],
        &[&base],
        "left",
    );
    let right = f.commit(
        &[("lib.rs", &original.replace("Y: i32 = 1", "Y: i32 = 2"))],
        &[&base],
        "right",
    );
    let merge = f.commit(
        &[("lib.rs", &original.replace("i32 = 1", "i32 = 2"))],
        &[&left, &right],
        "clean merge",
    );
    f.remove_commit(&old_left);
    f.remove_commit(&old_right);
    let source = f.history(&merge, "log file:lib.rs limit:2").await;
    assert_eq!(ids(&source), [left.as_str(), right.as_str()], "{source}");
    assert!(source.ends_with("more skip:2\n"), "{source}");
    let unfinished = f.history(&merge, "log file:lib.rs limit:3").await;
    assert_eq!(ids(&unfinished), ids(&source), "{unfinished}");
    assert!(unfinished.contains("incomplete:"), "{unfinished}");
    let repository = f.history(&merge, "log limit:4").await;
    assert_eq!(
        ids(&repository),
        [merge.as_str(), left.as_str(), right.as_str(), base.as_str()],
        "{repository}",
    );
    assert!(repository.ends_with("more skip:4\n"), "{repository}");
    let unfinished = f.history(&merge, "log limit:5").await;
    assert_eq!(ids(&unfinished), ids(&repository), "{unfinished}");
    assert!(unfinished.contains("incomplete:"), "{unfinished}");
}

#[tokio::test]
async fn bounded_deletion_recovery_preserves_cycles_in_both_parent_orders() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let old = f.commit(&[("lib.rs", original)], &[], "older ancestry");
    let base = f.commit(&[("lib.rs", original)], &[&old], "shared base");
    let edit = f.commit(
        &[("lib.rs", &original.replace("1", "2"))],
        &[&base],
        "ordinary edit",
    );
    let revert = f.commit(&[("lib.rs", original)], &[&edit], "ordinary revert");
    let rejected = f.commit(
        &[("lib.rs", &original.replace("1", "3"))],
        &[&revert],
        "rejected edit",
    );
    let deleted = f.commit(&[], &[&rejected], "rejected deletion");
    let main = f.commit(&[("lib.rs", original)], &[&base], "unchanged main");
    // Use the same closed interval with each recorded parent order.
    let merges: Vec<_> = [
        [main.as_str(), deleted.as_str()],
        [deleted.as_str(), main.as_str()],
    ]
    .iter()
    .map(|parents| {
        f.commit(
            &[("lib.rs", original)],
            parents,
            "reject deletion and content",
        )
    })
    .collect();
    f.remove_commit(&old);
    for merge in &merges {
        for selector in ["file:lib.rs", "function:target"] {
            let page = f.history(merge, &format!("log {selector} limit:2")).await;
            assert_eq!(
                ids(&page),
                [revert.as_str(), edit.as_str()],
                "{selector}: {page}"
            );
            assert!(page.ends_with("more skip:2\n"), "{page}");
            let second = f
                .history(merge, &format!("log {selector} skip:1 limit:1"))
                .await;
            assert_eq!(ids(&second), [edit.as_str()], "{second}");
            assert!(second.ends_with("more skip:2\n"), "{second}");
            let unfinished = f.history(merge, &format!("log {selector} limit:3")).await;
            assert_eq!(ids(&unfinished), ids(&page), "{unfinished}");
            assert!(
                unfinished.contains("incomplete:") && unfinished.contains(&old),
                "{unfinished}",
            );
            let deletion = f
                .history(merge, &format!("show {deleted} {selector}"))
                .await;
            assert!(body(&deletion).contains("+++ /dev/null"), "{deletion}");
            assert!(body(&deletion).contains("{ 3 }"), "{deletion}");
        }
    }
}

#[tokio::test]
async fn multiple_bases_remain_conservative_without_older_ancestry() {
    let f = Fixture::new();
    let original = "const X: i32 = 1;\nconst Y: i32 = 1;\n";
    let old = f.commit(&[("lib.rs", original)], &[], "older ancestry");
    let base = f.commit(&[("lib.rs", original)], &[&old], "base");
    let a = f.commit(
        &[("lib.rs", &original.replace("X: i32 = 1", "X: i32 = 2"))],
        &[&base],
        "first base",
    );
    let b = f.commit(
        &[("lib.rs", &original.replace("Y: i32 = 1", "Y: i32 = 2"))],
        &[&base],
        "second base",
    );
    let left = f.commit(
        &[("lib.rs", &original.replace("i32 = 1", "i32 = 2"))],
        &[&a, &b],
        "left merge",
    );
    let right = f.commit(
        &[("lib.rs", &original.replace("i32 = 1", "i32 = 3"))],
        &[&b, &a],
        "right merge",
    );
    let merge = f.commit(
        &[("lib.rs", &original.replace("i32 = 1", "i32 = 4"))],
        &[&left, &right],
        "ambiguous merge",
    );
    f.remove_commit(&old);
    let source = f.history(&merge, "log file:lib.rs limit:1").await;
    assert_eq!(ids(&source), [merge.as_str()], "{source}");
    assert!(
        source.contains("merge attribution unresolved (alternative regions or bases)"),
        "{source}",
    );
    assert!(source.ends_with("more skip:1\n"), "{source}");
    let repository = f.history(&merge, "log limit:5").await;
    assert_eq!(
        ids(&repository),
        [
            merge.as_str(),
            left.as_str(),
            right.as_str(),
            a.as_str(),
            b.as_str()
        ],
        "{repository}",
    );
    assert!(repository.ends_with("more skip:5\n"), "{repository}");
}

#[tokio::test]
async fn sibling_comparisons_require_the_bridge_but_not_its_older_ancestry() {
    let f = Fixture::new();
    let old = f.commit(
        &[("lib.rs", "const VALUE: i32 = 0;\n")],
        &[],
        "older ancestry",
    );
    let base = f.commit(&[("lib.rs", "const VALUE: i32 = 1;\n")], &[&old], "bridge");
    let left = f.commit(&[("lib.rs", "const VALUE: i32 = 2;\n")], &[&base], "left");
    let right = f.commit(&[("lib.rs", "const VALUE: i32 = 3;\n")], &[&base], "right");
    f.remove_commit(&old);
    for selector in ["file:lib.rs", "const:VALUE"] {
        let compared = f
            .history(&left, &format!("diff {left}..{right} {selector}"))
            .await;
        assert!(
            body(&compared).contains("-const VALUE: i32 = 2;"),
            "{compared}"
        );
        assert!(
            body(&compared).contains("+const VALUE: i32 = 3;"),
            "{compared}"
        );
        let shown = f.history(&left, &format!("show {right} {selector}")).await;
        assert!(body(&shown).contains("-const VALUE: i32 = 1;"), "{shown}");
        assert!(body(&shown).contains("+const VALUE: i32 = 3;"), "{shown}");
    }
    f.remove_commit(&base);
    for selector in ["file:lib.rs", "const:VALUE"] {
        let error = f
            .error(&left, &format!("diff {left}..{right} {selector}"))
            .await;
        assert!(
            error.contains(&base),
            "A missing bridge must not fall back to name lookup: {error}",
        );
    }
}

#[tokio::test]
async fn a_redundant_parent_must_follow_its_descendant_on_every_page() {
    let f = Fixture::new();
    let base = f.commit(
        &[("lib.rs", "const VALUE: i32 = 0;\n")],
        &[],
        "unneeded older base",
    );
    let parent = f.commit(
        &[("lib.rs", "const VALUE: i32 = 1;\n")],
        &[&base],
        "first parent",
    );
    let child = f.commit(
        &[("lib.rs", "const VALUE: i32 = 2;\n")],
        &[&parent],
        "second parent is newer",
    );
    let merge = f.commit(
        &[("lib.rs", "const VALUE: i32 = 2;\n")],
        &[&parent, &child],
        "redundant first parent",
    );
    f.remove_commit(&base);
    let source = f.history(&merge, "log const:VALUE limit:1").await;
    assert_eq!(ids(&source), [child.as_str()], "{source}");
    assert!(source.ends_with("more skip:1\n"), "{source}");
    let repository = f.history(&merge, "log limit:2").await;
    assert_eq!(
        ids(&repository),
        [merge.as_str(), child.as_str()],
        "{repository}"
    );
    assert!(repository.ends_with("more skip:2\n"), "{repository}");
    let next = f.history(&merge, "log skip:2 limit:1").await;
    assert_eq!(ids(&next), [parent.as_str()], "{next}");
    assert!(next.ends_with("more skip:3\n"), "{next}");
    let unfinished = f.history(&merge, "log const:VALUE limit:2").await;
    assert_eq!(ids(&unfinished), ids(&source), "{unfinished}");
    assert!(
        unfinished.contains("incomplete:") && unfinished.contains(&base),
        "{unfinished}"
    );
}

#[tokio::test]
async fn proven_absence_at_the_cut_ends_the_lifetime_without_claiming_a_git_root() {
    let f = Fixture::new();
    let original = "pub fn target() -> i32 { 1 }\n";
    let old = f.commit(&[("lib.rs", original)], &[], "unrelated old lifetime");
    let base = f.commit(&[], &[&old], "absence boundary");
    let introduced = f.commit(&[("lib.rs", original)], &[&base], "selected lifetime");
    let side = f.commit(&[], &[&base], "no selected lifetime here");
    let merge = f.commit(
        &[("lib.rs", original)],
        &[&introduced, &side],
        "import selected lifetime",
    );
    f.remove_commit(&old);
    for selector in ["file:lib.rs", "function:target"] {
        let log = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(ids(&log), [introduced.as_str()], "{log}");
        assert!(log.ends_with("end\n"), "{log}");
        let comparison = f
            .history(&merge, &format!("diff {base}..{merge} {selector}"))
            .await;
        assert!(body(&comparison).contains("--- /dev/null"), "{comparison}");
        let error = f.error(&merge, &format!("show {base} {selector}")).await;
        assert!(error.contains(&old), "{error}");
    }
}

#[tokio::test]
async fn nested_rejection_stays_uncertain_inside_a_bounded_interval() {
    let f = Fixture::new();
    let original = "const VALUE: i32 = 1;\n";
    let old = f.commit(&[("lib.rs", original)], &[], "older ancestry");
    let base = f.commit(&[("lib.rs", original)], &[&old], "shared base");
    let a = f.commit(
        &[("lib.rs", "const VALUE: i32 = 2;\n")],
        &[&base],
        "first edit",
    );
    let b = f.commit(
        &[("lib.rs", "const VALUE: i32 = 3;\n")],
        &[&base],
        "second edit",
    );
    let nested = f.commit(
        &[("lib.rs", "const VALUE: i32 = 3;\n")],
        &[&a, &b],
        "nested merge",
    );
    let main = f.commit(&[("lib.rs", original)], &[&base], "unchanged main");
    let merge = f.commit(
        &[("lib.rs", original)],
        &[&main, &nested],
        "reject nested contribution",
    );
    f.remove_commit(&old);
    let page = f.history(&merge, "log file:lib.rs limit:2").await;
    assert_eq!(ids(&page), [a.as_str(), b.as_str()], "{page}");
    for id in [&a, &b] {
        let record = page
            .lines()
            .find(|line| line.starts_with(id.as_str()))
            .unwrap();
        assert!(
            record.contains("nested or alternative merge attribution unresolved"),
            "{record}"
        );
    }
    assert!(page.ends_with("more skip:2\n"), "{page}");
}
#[tokio::test]
async fn directive_text_inside_a_csharp_string_does_not_hide_an_introduction() {
    let f = Fixture::new();
    let original = r#"class Worker {
    const string Banner = """
        #if FEATURE
        """;
}
"#;
    let root = f.commit(&[("Worker.cs", original)], &[], "string content");
    let added = original.replace("\n}\n", "\n    int Target() { return 1; }\n}\n");
    let tip = f.commit(&[("Worker.cs", &added)], &[&root], "introduce target");
    let history = f.history(&tip, "log method:Worker.Target()").await;
    assert_eq!(ids(&history), [tip.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
}

#[tokio::test]
async fn conditional_csharp_parameters_remain_selectable_as_alternatives() {
    let f = Fixture::new();
    let original = "class Worker {\n    int Target(\n#if FEATURE\n        int value\n#else\n        string value\n#endif\n    ) { return 1; }\n}\n";
    let root = f.commit(&[("Worker.cs", original)], &[], "conditional signature");
    let changed = original.replace("return 1;", "return 2;");
    let tip = f.commit(&[("Worker.cs", &changed)], &[&root], "edit body");
    for selector in ["method:Worker.Target(int)", "method:Worker.Target(string)"] {
        let history = f.history(&tip, &format!("log {selector}")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        let addition = f.history(&tip, &format!("show {root} {selector}")).await;
        assert!(body(&addition).contains("+#if FEATURE"), "{addition}");
        assert!(
            body(&addition).contains("+        string value"),
            "{addition}"
        );
    }
}

#[tokio::test]
async fn shared_range_signatures_are_ambiguous_by_name_and_by_location() {
    let f = Fixture::new();
    let source = "class Worker {\n    int Target(\n#if FEATURE\n        int value\n#else\n        string value\n#endif\n    ) { return 1; }\n}\n";
    let root = f.commit(&[("Worker.cs", source)], &[], "written alternatives");
    for selector in ["method:Worker.Target", "@Worker.cs:2:10"] {
        let error = f.error(&root, &format!("log {selector}")).await;
        assert!(error.contains("Ambiguous anchor"), "{error}");
        assert!(error.contains("Worker.Target(int)"), "{error}");
        assert!(error.contains("Worker.Target(string)"), "{error}");
    }
}

#[tokio::test]
async fn same_parameter_lists_do_not_erase_other_written_header_alternatives() {
    let f = Fixture::new();
    let source = "class Worker {\n    [Marker]\n#if FEATURE\n    int\n#else\n    string\n#endif\n    Target() { return default; }\n}\n";
    let root = f.commit(&[("Worker.cs", source)], &[], "return alternatives");
    let error = f.error(&root, "log method:Worker.Target()").await;
    assert!(error.contains("Ambiguous anchor"), "{error}");
    assert!(error.contains("int Target"), "{error}");
    assert!(error.contains("string Target"), "{error}");
}

#[tokio::test]
async fn real_conditions_and_literal_directive_text_do_not_poison_an_unrelated_introduction() {
    let f = Fixture::new();
    let source = r#"class Worker {
    const string Banner = """
        #if NOT_A_DIRECTIVE
        """;
#if FEATURE
    int Other;
#else
    string Other;
#endif
}
"#;
    let root = f.commit(
        &[("Worker.cs", source)],
        &[],
        "conditional fields and literal",
    );
    let added = source.replace("\n}\n", "\n    int Target() { return 1; }\n}\n");
    let tip = f.commit(&[("Worker.cs", &added)], &[&root], "introduce target");
    let history = f.history(&tip, "log method:Worker.Target()").await;
    assert_eq!(ids(&history), [tip.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
}

#[tokio::test]
async fn conditional_owner_names_remain_separate_source_alternatives() {
    let f = Fixture::new();
    let source = "#if FEATURE\nclass First\n#else\nclass Second\n#endif\n{\n    int Target(int value) { return value + 1; }\n}\n";
    let root = f.commit(&[("Worker.cs", source)], &[], "conditional owners");
    let changed = source.replace("value + 1", "value + 2");
    let tip = f.commit(&[("Worker.cs", &changed)], &[&root], "edit shared method");
    for owner in ["First", "Second"] {
        let history = f
            .history(&tip, &format!("log method:{owner}.Target(int)"))
            .await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(history.ends_with("end\n"), "{history}");
    }
    let error = f.error(&tip, "log method:Target(int)").await;
    assert!(error.contains("Ambiguous anchor"), "{error}");
}

#[tokio::test]
async fn conditional_show_and_diff_keep_original_crlf_and_utf8_source() {
    let f = Fixture::new();
    let source = "class Worker {\r\n    int Target(\r\n#if FEATURE\r\n        int π\r\n#else\r\n        string π\r\n#endif\r\n    ) { return 1; }\r\n}\r\n";
    let root = f.commit(&[("Worker.cs", source)], &[], "conditional UTF-8 source");
    let changed = source.replace("return 1;", "return 2;");
    let tip = f.commit(&[("Worker.cs", &changed)], &[&root], "body edit");
    for parameter in ["int", "string"] {
        let selector = format!("method:Worker.Target({parameter})");
        let addition = f.history(&tip, &format!("show {root} {selector}")).await;
        assert!(body(&addition).contains("+#if FEATURE\r\n"), "{addition}");
        assert!(body(&addition).contains("+        int π\r\n"), "{addition}");
        assert!(
            body(&addition).contains("+        string π\r\n"),
            "{addition}"
        );
        let diff = f
            .history(&tip, &format!("diff {root}..{tip} {selector}"))
            .await;
        assert!(body(&diff).contains("-    ) { return 1; }"), "{diff}");
        assert!(body(&diff).contains("+    ) { return 2; }"), "{diff}");
        assert!(!diff.contains("incomplete:"), "{diff}");
    }
}

#[tokio::test]
async fn unfinished_csharp_predecessors_are_not_proof_of_an_introduction() {
    let f = Fixture::new();
    let root = f.commit(
        &[("Worker.cs", "class Worker {\n#if FEATURE\n}\n")],
        &[],
        "unfinished source",
    );
    let tip = f.commit(
        &[("Worker.cs", "class Worker { int Target() { return 1; } }\n")],
        &[&root],
        "recovered source",
    );
    let history = f.history(&tip, "log method:Worker.Target()").await;
    assert!(ids(&history).is_empty(), "{history}");
    assert!(
        history.contains("incomplete:") && history.contains("historical C#"),
        "{history}"
    );
    assert!(!history.ends_with("end\n"), "{history}");
    let error = f.error(&root, "log method:Worker.Missing()").await;
    assert!(error.contains("Unresolved anchor"), "{error}");
    assert!(!error.contains("No declaration"), "{error}");
}

#[tokio::test]
async fn an_exhausted_csharp_alternative_budget_is_not_a_negative_selector_result() {
    let f = Fixture::new();
    let mut source = "class Worker { void Target(int seed\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!("#if A{index}\n, int p{index}\n#endif\n"));
    }
    source.push_str(") {} }\n");
    let root = f.commit(
        &[("Worker.cs", &source)],
        &[],
        "many independent conditions",
    );
    for selector in ["method:Worker.Target(int)", "method:Worker.Missing()"] {
        let error = f.error(&root, &format!("log {selector}")).await;
        assert!(error.contains("Unresolved anchor"), "{error}");
        assert!(error.contains("limit reached"), "{error}");
        assert!(!error.contains("No declaration"), "{error}");
    }
}

#[tokio::test]
async fn a_positional_anchor_does_not_discard_a_longer_conditional_header() {
    let f = Fixture::new();
    let source = "class Worker {\n#if FEATURE\n[Marker]\n#endif\nint Target() { return 1; }\n}\n";
    let root = f.commit(&[("Worker.cs", source)], &[], "conditional attribute");
    for selector in ["@Worker.cs:5:6", "method:@Worker.cs:5:6", "@Worker.cs:3:2"] {
        let error = f.error(&root, &format!("log {selector}")).await;
        assert!(error.contains("Ambiguous anchor"), "{error}");
        assert!(error.contains("2 declarations"), "{error}");
    }
    let history = f.history(&root, "log method:@Worker.cs:3:2").await;
    assert_eq!(ids(&history), [root.as_str()], "{history}");
}

#[tokio::test]
async fn a_named_anchor_ignores_unrelated_incomplete_sources() {
    let f = Fixture::new();
    let root = f.commit(
        &[
            (
                "Worker.cs",
                "class @Worker { int @Target() { return 1; } }\n",
            ),
            ("Other.cs", "#if FEATURE\nclass Other { int Broken(\n"),
            ("Other.cpp", "class Other { int Broken(\n"),
            ("Other.h", "struct Header { void Broken(\n"),
            ("other.rs", "pub fn broken(\n"),
        ],
        &[],
        "unrelated incomplete source",
    );
    let history = f.history(&root, "log method:Worker.Target()").await;
    assert_eq!(ids(&history), [root.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
    // A broad selector cannot prove the incomplete file irrelevant.
    let error = f.error(&root, "log method:*").await;
    assert!(error.contains("Unresolved anchor"), "{error}");
}

#[tokio::test]
async fn escaped_names_in_incomplete_csharp_sources_remain_candidates() {
    let f = Fixture::new();
    let root = f.commit(
        &[
            ("Worker.cs", "class Worker { int Target() { return 1; } }\n"),
            (
                "Other.cs",
                "#if FEATURE\nclass Worker { int \\u0054arget(\n",
            ),
        ],
        &[],
        "escaped competing source",
    );
    let error = f.error(&root, "log method:Worker.Target()").await;
    assert!(error.contains("Unresolved anchor"), "{error}");
}

#[tokio::test]
async fn unchanged_files_still_obey_changed_ancestor_attributes() {
    let f = Fixture::new();
    let source = "pub fn selected() {}\n";
    let root = f.commit(
        &[
            ("src/.gitattributes", "*.rs binary\n"),
            ("src/nested/lib.rs", source),
        ],
        &[],
        "excluded ancestor",
    );
    let tip = f.commit(&[("src/nested/lib.rs", source)], &[&root], "allowed source");
    let history = f.history(&tip, "log file:src/nested/lib.rs").await;
    assert!(
        history.contains("incomplete:")
            && history.contains("excluded by committed source attributes"),
        "{history}"
    );
}

#[tokio::test]
async fn an_unfinished_candidate_file_cannot_prove_anchor_uniqueness() {
    let f = Fixture::new();
    let root = f.commit(
        &[
            ("Worker.cs", "class Worker { int Target() { return 1; } }\n"),
            ("Other.cs", "#if FEATURE\nclass Worker { int Target(\n"),
        ],
        &[],
        "incomplete competing source",
    );
    let error = f.error(&root, "log method:Worker.Target()").await;
    assert!(error.contains("Unresolved anchor"), "{error}");
    let history = f
        .history(&root, "log method:Worker.Target() path:Worker.cs")
        .await;
    assert_eq!(ids(&history), [root.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
}

#[tokio::test]
async fn independent_csharp_declarations_do_not_require_cartesian_file_profiles() {
    let f = Fixture::new();
    let mut source = "class Worker {\n    int Target() { return 1; }\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!(
            "    int Other{index}(\n#if FEATURE_{index}\n        int value\n#else\n        string value\n#endif\n    ) {{ return 0; }}\n"
        ));
    }
    source.push_str("}\n");
    let root = f.commit(
        &[("Worker.cs", &source)],
        &[],
        "independent method signatures",
    );
    let changed = source.replace("int Target() { return 1; }", "int Target() { return 2; }");
    let tip = f.commit(
        &[("Worker.cs", &changed)],
        &[&root],
        "edit unconditional method",
    );

    let history = f.history(&tip, "log method:Worker.Target()").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
    for index in 0..16 {
        for parameter in ["int", "string"] {
            let history = f
                .history(
                    &tip,
                    &format!("log method:Worker.Other{index}({parameter})"),
                )
                .await;
            assert_eq!(ids(&history), [root.as_str()], "{history}");
            assert!(history.ends_with("end\n"), "{history}");
        }
    }
    let positional = f.history(&tip, "log @Worker.cs:2:10").await;
    assert_eq!(
        ids(&positional),
        [tip.as_str(), root.as_str()],
        "{positional}"
    );
    let line = source
        .lines()
        .position(|s| s.contains("int Other15("))
        .unwrap()
        + 1;
    let error = f.error(&tip, &format!("log @Worker.cs:{line}:10")).await;
    assert!(
        error.contains("Ambiguous anchor") && error.contains("2 declarations"),
        "{error}"
    );
}

#[tokio::test]
async fn factored_positions_keep_long_headers_and_conditional_absence() {
    let f = Fixture::new();
    let mut source =
        "class Worker {\n#if ATTRIBUTE\n[Marker]\n#endif\nint Target() { return 1; }\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!(
            "void Other{index}(\n#if FEATURE_{index}\nint value\n#else\nstring value\n#endif\n) {{}}\n"
        ));
    }
    source.push_str("}\n");
    let root = f.commit(
        &[("Worker.cs", &source)],
        &[],
        "independent positional alternatives",
    );
    // At the name both headers win in disjoint environments. At the optional
    // attribute, the shorter method is absent from the position, so Worker wins
    // in that environment instead. Neither ambiguity may be collapsed.
    for selector in ["method:Worker.Target()", "@Worker.cs:5:6", "@Worker.cs:3:2"] {
        let error = f.error(&root, &format!("log {selector}")).await;
        assert!(
            error.contains("Ambiguous anchor") && error.contains("2 declarations"),
            "{error}"
        );
    }
    let history = f.history(&root, "log method:Worker.Other15(string)").await;
    assert_eq!(ids(&history), [root.as_str()], "{history}");
    assert!(history.ends_with("end\n"), "{history}");
}

#[tokio::test]
async fn independently_optional_methods_are_discovered_without_file_products() {
    let f = Fixture::new();
    let mut source = "class Worker {\nint Target() { return 1; }\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!(
            "#if FEATURE_{index}\nvoid Other{index}() {{}}\n#endif\n"
        ));
    }
    source.push_str("}\n");
    let root = f.commit(
        &[("Worker.cs", &source)],
        &[],
        "optional independent declarations",
    );
    for index in 0..16 {
        let history = f
            .history(&root, &format!("log method:Worker.Other{index}()"))
            .await;
        assert_eq!(ids(&history), [root.as_str()], "{history}");
        assert!(history.ends_with("end\n"), "{history}");
    }
    let line = source
        .lines()
        .position(|s| s.contains("void Other15()"))
        .unwrap()
        + 1;
    let error = f.error(&root, &format!("log @Worker.cs:{line}:6")).await;
    assert!(
        error.contains("Ambiguous anchor") && error.contains("2 declarations"),
        "{error}"
    );
}
async fn assert_nested_callable_history(path: &str, source: &str) {
    assert_callable_history(
        path,
        source,
        ("value - 7", "value - 9"),
        ("adjusted * 3", "adjusted * 4"),
    )
    .await;
}

async fn assert_callable_history(
    path: &str,
    source: &str,
    sibling_edit: (&str, &str),
    selected_edit: (&str, &str),
) {
    let f = Fixture::new();
    assert_eq!(source.matches(sibling_edit.0).count(), 1);
    assert_eq!(source.matches(selected_edit.0).count(), 1);
    let root = f.commit(&[(path, source)], &[], "nested callables");
    let unrelated = source.replace(sibling_edit.0, sibling_edit.1);
    let sibling = f.commit(&[(path, &unrelated)], &[&root], "edit sibling callable");
    let changed = unrelated.replace(selected_edit.0, selected_edit.1);
    let tip = f.commit(&[(path, &changed)], &[&sibling], "edit selected callable");
    let location = changed
        .lines()
        .enumerate()
        .find_map(|(line, text)| {
            text.find(selected_edit.1)
                .map(|byte| format!("@{path}:{}:{}", line + 1, text[..byte].chars().count() + 1))
        })
        .unwrap();
    let history = f.history(&tip, &format!("log {location}")).await;
    assert_eq!(
        ids(&history),
        [tip.as_str(), root.as_str()],
        "{path}: {history}"
    );
    assert!(history.ends_with("end\n"), "{history}");
    let unchanged = f.history(&tip, &format!("show {sibling} {location}")).await;
    assert_eq!(body(&unchanged), "No changes.\n", "{unchanged}");
    let selected = f.history(&tip, &format!("show {tip} {location}")).await;
    assert!(body(&selected).contains(selected_edit.0), "{selected}");
    assert!(body(&selected).contains(selected_edit.1), "{selected}");
    assert!(!body(&selected).contains(sibling_edit.1), "{selected}");
}

#[tokio::test]
async fn csharp_lambda_history_omits_sibling_lambda_edits() {
    assert_nested_callable_history(
        "Worker.cs",
        r#"class Worker {
    int Run() {
        System.Func<int, int> target = value => {
            var adjusted = value + 10;
            return adjusted * 3;
        };
        System.Func<int, int> sibling = value => value - 7;
        return target(1) + sibling(1);
    }
}
"#,
    )
    .await;
}

#[tokio::test]
async fn rust_closure_history_omits_sibling_closure_edits() {
    assert_nested_callable_history(
        "lib.rs",
        r#"fn run() -> i32 {
    let target = |value: i32| {
        let adjusted = value + 10;
        adjusted * 3
    };
    let sibling = |value: i32| value - 7;
    target(1) + sibling(1)
}
"#,
    )
    .await;
}

#[tokio::test]
async fn csharp_getter_history_omits_setter_edits() {
    assert_nested_callable_history(
        "Worker.cs",
        r#"class Worker {
    int backing;
    int Value {
        get {
            var adjusted = backing + 10;
            return adjusted * 3;
        }
        set { backing = value - 7; }
    }
}
"#,
    )
    .await;
}

#[tokio::test]
async fn csharp_local_function_history_omits_sibling_edits() {
    assert_nested_callable_history(
        "Worker.cs",
        r#"class Worker {
    int Run() {
        int Target(int value) {
            var adjusted = value + 10;
            return adjusted * 3;
        }
        int Sibling(int value) => value - 7;
        return Target(1) + Sibling(1);
    }
}
"#,
    )
    .await;
}

#[tokio::test]
async fn csharp_anonymous_method_history_omits_sibling_edits() {
    for expression in [
        "delegate(int value) { var adjusted = value + 10; return adjusted * 3; }",
        "delegate { var adjusted = 10; return adjusted * 3; }",
        "async (int value) => { await Load(); var adjusted = value + 10; return adjusted * 3; }",
    ] {
        let source = format!(
            "class Worker {{ void Run() {{ var target = {expression}; System.Func<int, int> sibling = value => value - 7; }} }}\n"
        );
        assert_nested_callable_history("Worker.cs", &source).await;
    }
}

#[tokio::test]
async fn csharp_expression_and_nested_lambda_history_omits_sibling_edits() {
    for expression in [
        "adjusted => adjusted * 3",
        "(int adjusted) => adjusted * 3",
        "static (int adjusted) => adjusted * 3",
    ] {
        let source = format!(
            "class Worker {{ void Run() {{ System.Func<int, int> target = {expression}; System.Func<int, int> sibling = value => value - 7; }} }}\n"
        );
        assert_nested_callable_history("Worker.cs", &source).await;
    }
    for source in [
        "class Worker { void Run() { System.Func<int, System.Func<int, int>> factory = seed => { System.Func<int, int> sibling = value => value - 7; return adjusted => adjusted * 3; }; } }\n",
        "class Worker { System.Func<int, int> sibling = value => value - 7, target = adjusted => adjusted * 3; }\n",
        "// 🦀\r\nSystem.Func<int, int> sibling = value => value - 7;\r\nSystem.Func<int, int> target = adjusted => adjusted * 3;\r\n",
    ] {
        assert_nested_callable_history("Worker.cs", source).await;
    }
}

#[tokio::test]
async fn csharp_accessor_roles_history_omits_other_accessors() {
    for member in [
        "int Value { get { int value = backing; return value - 7; } set { int adjusted = value + 10; backing = adjusted * 3; } }",
        "int Value { get { int value = backing; return value - 7; } init { int adjusted = value + 10; backing = adjusted * 3; } }",
        "int this[int adjusted] { get => adjusted * 3; set => Save(value - 7); }",
        "int Value { get { int adjusted = backing; return adjusted * 3; } set => Save(value - 7); }",
    ] {
        let source = format!("class Worker {{ int backing; {member} }}\n");
        assert_nested_callable_history("Worker.cs", &source).await;
    }
    for member in [
        "event System.Action Changed { add { int adjusted = value.GetHashCode(); Save(adjusted * 3); } remove => Detach(value, 7); }",
        "event System.Action Changed { add => Detach(value, 7); remove { int adjusted = value.GetHashCode(); Save(adjusted * 3); } }",
    ] {
        let source = format!("class Worker {{ {member} }}\n");
        assert_callable_history(
            "Worker.cs",
            &source,
            ("Detach(value, 7)", "Detach(value, 9)"),
            ("adjusted * 3", "adjusted * 4"),
        )
        .await;
    }
    assert_callable_history(
        "Worker.cs",
        "class Worker { int Value { [Marker(7)] get; [Marker(3)] private set; } }\n",
        ("Marker(7)", "Marker(9)"),
        ("Marker(3)", "Marker(4)"),
    )
    .await;
}

#[tokio::test]
async fn rust_expression_move_async_and_nested_closure_history_omits_sibling_edits() {
    for expression in [
        "|adjusted: i32| adjusted * 3",
        "move |adjusted: i32| adjusted * 3",
        "async move |adjusted: i32| adjusted * 3",
    ] {
        let source = format!(
            "fn run() {{ let target = {expression}; let sibling = |value: i32| value - 7; }}\n"
        );
        assert_nested_callable_history("lib.rs", &source).await;
    }
    assert_nested_callable_history(
        "lib.rs",
        "fn run() { let factory = |seed: i32| { let sibling = |value: i32| value - 7; |adjusted: i32| adjusted * 3 }; }\n",
    )
    .await;
}

#[tokio::test]
async fn inserting_a_sibling_does_not_renumber_anonymous_history() {
    for (path, source, inserted) in [
        (
            "Worker.cs",
            "class Worker { void Run() { /* insert */ System.Func<int, int> target = adjusted => adjusted * 3; } }\n",
            "System.Func<int, int> sibling = value => value - 7;",
        ),
        (
            "lib.rs",
            "fn run() { /* insert */ let target = |adjusted: i32| adjusted * 3; }\n",
            "let sibling = |value: i32| value - 7;",
        ),
    ] {
        assert_callable_history(
            path,
            source,
            ("/* insert */", inserted),
            ("adjusted * 3", "adjusted * 4"),
        )
        .await;
    }
}

#[tokio::test]
async fn conditional_callable_positions_keep_owner_headers_and_absence() {
    for (source, needle) in [
        (
            "class Worker {\n#if TYPE\nint\n#else\nstring\n#endif\nValue { get { return default; } } }\n",
            "return default",
        ),
        (
            "class Worker {\n#if ATTRIBUTE\n[Marker]\n#endif\nSystem.Func<int> Field = () => 1; }\n",
            "=> 1",
        ),
        (
            "class Worker {\n#if OWNER\nvoid Run(int value)\n#else\nvoid Run(string value)\n#endif\n{ var f = () => value; } }\n",
            "=> value",
        ),
        (
            "class Worker { void Run() {\n#if OPTIONAL\nvar f = () => 1;\n#endif\n} }\n",
            "=> 1",
        ),
    ] {
        let f = Fixture::new();
        let root = f.commit(&[("Worker.cs", source)], &[], "conditional callable");
        let byte = source.find(needle).unwrap();
        let (line, column) = sigla::model::position(source, byte);
        let error = f
            .error(&root, &format!("log @Worker.cs:{line}:{column}"))
            .await;
        assert!(
            error.contains("Ambiguous anchor") && error.contains("2 declarations"),
            "{error}"
        );
    }
}
#[tokio::test]
async fn a_stronger_renamed_continuation_separates_an_edited_copy() {
    for (path, before, after, selected, retained) in [
        (
            "Worker.cs",
            "class Worker {\n    int Old(int value) {\n        var adjusted = value + 10;\n        var scaled = adjusted * 3;\n        return scaled ^ 127;\n    }\n}\n",
            "class Worker {\n    int Retained(int value) {\n        var adjusted = value + 10;\n        var scaled = adjusted * 3;\n        return scaled ^ 127;\n    }\n    int Old(int value) {\n        var adjusted = value - 20;\n        var scaled = adjusted * 5;\n        return scaled ^ 255;\n    }\n}\n",
            "method:Worker.Old",
            "method:Worker.Retained",
        ),
        (
            "lib.rs",
            "fn old(value: i32) -> i32 {\n    let adjusted = value + 10;\n    let scaled = adjusted * 3;\n    scaled ^ 127\n}\n",
            "fn retained(value: i32) -> i32 {\n    let adjusted = value + 10;\n    let scaled = adjusted * 3;\n    scaled ^ 127\n}\nfn old(value: i32) -> i32 {\n    let adjusted = value - 20;\n    let scaled = adjusted * 5;\n    scaled ^ 255\n}\n",
            "function:old",
            "function:retained",
        ),
    ] {
        // Also cover a mildly edited namesake: the old name plus a high body
        // similarity must not relabel the stronger retained implementation as a copy.
        for after in [
            after.to_owned(),
            after.replace(" - 20", " + 10").replace("* 5", "* 3"),
        ] {
            let f = Fixture::new();
            let root = f.commit(&[(path, before)], &[], "original implementation");
            let tip = f.commit(&[(path, &after)], &[&root], "split implementations");
            let history = f.history(&tip, &format!("log {selected}")).await;
            assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
            assert!(history.contains("copy"), "{path}: {history}");
            let continuation = f.history(&tip, &format!("log {retained}")).await;
            assert_eq!(
                ids(&continuation),
                [tip.as_str(), root.as_str()],
                "{continuation}"
            );
            assert!(continuation.contains("rename"), "{continuation}");
            assert!(!continuation.contains("copy"), "{continuation}");
            let forward = f
                .history(&root, &format!("diff {root}..{tip} {selected}"))
                .await;
            let retained_name = retained.rsplit([':', '.']).next().unwrap();
            assert!(body(&forward).contains(retained_name), "{forward}");
            assert!(!body(&forward).contains("^ 255"), "{forward}");
        }
    }
}

// Distinctive implementations; subjects deliberately contain no event labels.
fn continuation_member(path: &str, name: &str) -> String {
    if path.ends_with(".cs") {
        format!(
            "    int {name}(int value) {{\n        var adjusted = value + 10;\n        var scaled = adjusted * 3;\n        return scaled ^ 127;\n    }}\n"
        )
    } else {
        format!(
            "fn {name}(value: i32) -> i32 {{\n    let adjusted = value + 10;\n    let scaled = adjusted * 3;\n    scaled ^ 127\n}}\n"
        )
    }
}
fn continuation_source(path: &str, members: &str) -> String {
    if path.ends_with(".cs") {
        format!("class Worker {{\n{members}}}\n")
    } else {
        members.into()
    }
}
fn history_position(path: &str, source: &str, needle: &str) -> String {
    let (line, column) = sigla::model::position(source, source.find(needle).unwrap());
    format!("@{path}:{line}:{column}")
}

#[tokio::test]
async fn equal_renamed_continuations_remain_uncertain_in_either_file_order() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        let path = format!("original.{extension}");
        let first = format!("a.{extension}");
        let second = format!("z.{extension}");
        let before = continuation_source(&path, &continuation_member(&path, "original"));
        let after = continuation_source(&path, &continuation_member(&path, "retained"));
        let f = Fixture::new();
        let root = f.commit(&[(&path, &before)], &[], "initial implementation");
        let tip = f.commit(
            &[(&first, &after), (&second, &after)],
            &[&root],
            "two destinations",
        );
        for selected in [&first, &second] {
            let history = f
                .history(&tip, &format!("log {kind}retained path:{selected}"))
                .await;
            assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
            assert!(history.contains("~move"), "{history}");
            assert!(
                history.contains("competing direct continuations"),
                "{history}"
            );
            assert!(!history.contains("copy"), "{history}");
        }
    }
}

#[tokio::test]
async fn an_edited_local_namesake_does_not_hide_a_changed_file_tie() {
    for (path, destination, kind) in [
        ("Worker.cs", "Retained.cs", "method:Worker."),
        ("lib.rs", "retained.rs", "function:"),
    ] {
        let before = continuation_source(path, &continuation_member(path, "Old"));
        let edited = before.replace("+ 10", "+ 20").replace("^ 127", "^ 255");
        let retained = continuation_source(path, &continuation_member(path, "Retained"));
        let f = Fixture::new();
        let root = f.commit(&[(path, &before)], &[], "initial implementation");
        let tip = f.commit(
            &[(path, &edited), (destination, &retained)],
            &[&root],
            "two destinations",
        );
        for name in ["Old", "Retained"] {
            let history = f.history(&tip, &format!("log {kind}{name}")).await;
            assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
            assert!(
                history.contains("competing direct continuations"),
                "{history}"
            );
            assert!(!history.contains("copy"), "{history}");
        }
        let forward = f
            .history(&root, &format!("diff {root}..{tip} {kind}Old"))
            .await;
        assert!(body(&forward).contains("Old("), "{forward}");
        assert!(body(&forward).contains("Retained("), "{forward}");
    }
}

#[tokio::test]
async fn a_renamed_survivor_in_another_file_reserves_its_continuation() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        let path = format!("original.{extension}");
        let retained_path = format!("retained.{extension}");
        let selected_path = format!("selected.{extension}");
        let before = continuation_source(&path, &continuation_member(&path, "original"));
        let retained = continuation_source(&path, &continuation_member(&path, "retained"));
        let selected = before
            .replace("+ 10", "- 20")
            .replace("* 3", "* 5")
            .replace("^ 127", "^ 255");
        let f = Fixture::new();
        let root = f.commit(&[(&path, &before)], &[], "initial implementation");
        let tip = f.commit(
            &[(&retained_path, &retained), (&selected_path, &selected)],
            &[&root],
            "two destinations",
        );
        let history = f.history(&tip, &format!("log {kind}original")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(history.contains("~copy"), "{history}");
        let history = f.history(&tip, &format!("log {kind}retained")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(
            history.contains("move") && !history.contains("copy"),
            "{history}"
        );
    }
}

#[tokio::test]
async fn conditional_survivors_do_not_make_unconditional_reservations() {
    let path = "Worker.cs";
    let original = continuation_member(path, "Old");
    let retained = continuation_member(path, "Retained");
    let edited = original
        .replace("+ 10", "- 20")
        .replace("* 3", "* 5")
        .replace("^ 127", "^ 255");
    let before = continuation_source(path, &original);
    for members in [
        format!("#if FEATURE\n{retained}#endif\n{edited}"),
        format!("#if FEATURE\n{retained}#else\n{edited}#endif\n"),
        format!(
            "{}{edited}",
            retained.replace(
                "int Retained",
                "\n#if FEATURE\nint\n#else\nlong\n#endif\nRetained"
            )
        ),
    ] {
        let after = continuation_source(path, &members);
        let f = Fixture::new();
        let root = f.commit(&[(path, &before)], &[], "initial implementation");
        let tip = f.commit(&[(path, &after)], &[&root], "source alternatives");
        let history = f.history(&tip, "log method:Worker.Old").await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(
            history.contains("competing direct continuations"),
            "{history}"
        );
        assert!(!history.contains("copy"), "{history}");
    }
}

#[tokio::test]
async fn anonymous_survivors_compete_without_using_their_display_labels() {
    for (path, before, after) in [
        (
            "Worker.cs",
            "class Worker { void Run() { System.Func<int, int> original = value => { var adjusted = value + 10; var scaled = adjusted * 3; return scaled ^ 127; }; } }\n",
            "class Worker { void Run() { System.Func<int, int> retained = value => { var adjusted = value + 10; var scaled = adjusted * 3; return scaled ^ 127; }; System.Func<int, int> selected = value => { var adjusted = value - 20; var scaled = adjusted * 5; return scaled ^ 255; }; } }\n",
        ),
        (
            "lib.rs",
            "fn run() { let original = |value: i32| { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }; }\n",
            "fn run() { let retained = |value: i32| { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }; let selected = |value: i32| { let adjusted = value - 20; let scaled = adjusted * 5; scaled ^ 255 }; }\n",
        ),
    ] {
        let f = Fixture::new();
        let root = f.commit(&[(path, before)], &[], "initial callables");
        let tip = f.commit(&[(path, after)], &[&root], "two callables");
        let selected = history_position(path, after, "scaled ^ 255");
        let history = f.history(&tip, &format!("log {selected}")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(
            history.contains("~copy") && !history.contains("rename"),
            "{history}"
        );
        let retained = history_position(path, after, "scaled ^ 127");
        let history = f.history(&tip, &format!("log {retained}")).await;
        assert_eq!(ids(&history), [root.as_str()], "{history}");
    }
}

#[tokio::test]
async fn a_reused_name_without_shared_implementation_has_no_copy_origin() {
    for (path, kind, unrelated) in [
        (
            "Worker.cs",
            "method:Worker.",
            "int Old(int value) { return \"unrelated\".Length; }\n",
        ),
        (
            "lib.rs",
            "function:",
            "fn Old(value: i32) -> i32 { panic!(\"unrelated\"); }\n",
        ),
    ] {
        let before = continuation_source(path, &continuation_member(path, "Old"));
        let after = continuation_source(
            path,
            &format!("{}{unrelated}", continuation_member(path, "Retained")),
        );
        let f = Fixture::new();
        let root = f.commit(&[(path, &before)], &[], "initial implementation");
        let tip = f.commit(&[(path, &after)], &[&root], "new implementation");
        let history = f.history(&tip, &format!("log {kind}Old")).await;
        assert_eq!(ids(&history), [tip.as_str()], "{history}");
        assert!(
            !history.contains("copy") && !history.contains("rename"),
            "{history}"
        );
    }
}

#[tokio::test]
async fn forward_deletion_does_not_claim_another_occurrences_survivor() {
    for (path, kind) in [("Worker.cs", "method:Worker."), ("lib.rs", "function:")] {
        let original = continuation_member(path, "original");
        let sibling = continuation_member(path, "sibling");
        let before = continuation_source(path, &format!("{original}{sibling}"));
        let after = continuation_source(path, &sibling);
        let f = Fixture::new();
        let root = f.commit(&[(path, &before)], &[], "two implementations");
        let tip = f.commit(&[(path, &after)], &[&root], "one implementation");
        let diff = f
            .history(&root, &format!("diff {root}..{tip} {kind}original"))
            .await;
        assert!(body(&diff).contains("original("), "{diff}");
        assert!(!body(&diff).contains("sibling("), "{diff}");
        assert!(
            !body(&diff)
                .lines()
                .any(|line| line.starts_with('+') && !line.starts_with("+++")),
            "{diff}"
        );
        let history = f.history(&tip, &format!("log {kind}sibling")).await;
        assert_eq!(ids(&history), [root.as_str()], "{history}");
    }
}

#[tokio::test]
async fn local_continuation_competition_does_not_hydrate_unrelated_source() {
    for (path, poison_path, kind) in [
        ("Worker.cs", "unrelated.cs", "method:Worker."),
        ("lib.rs", "unrelated.rs", "function:"),
    ] {
        let original = continuation_member(path, "original");
        let before = continuation_source(path, &original);
        let edited = original.replace("^ 127", "^ 255");
        let after = continuation_source(
            path,
            &format!("{}{edited}", continuation_member(path, "retained")),
        );
        let f = Fixture::new();
        // NUL source is rejected if fetched, but has no committed exclusion attribute.
        let root = f.commit(
            &[(path, &before), (poison_path, "\0")],
            &[],
            "initial implementation",
        );
        let tip = f.commit(
            &[(path, &after), (poison_path, "\0")],
            &[&root],
            "two implementations",
        );
        for selected in ["original", "retained"] {
            let history = f
                .history(&tip, &format!("log {kind}{selected} path:{path}"))
                .await;
            assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
            let event = if selected == "original" {
                "copy"
            } else {
                "rename"
            };
            assert!(history.contains(event), "{history}");
        }
    }
}

#[tokio::test]
async fn changed_file_continuations_do_not_hydrate_unchanged_source() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        for (source, destination) in [("a", "z"), ("z", "a")] {
            for edited in [false, true] {
                let path = format!("{source}.{extension}");
                let moved_path = format!("{destination}.{extension}");
                let poison_path = format!("unrelated.{extension}");
                let before = continuation_source(&path, &continuation_member(&path, "original"));
                let mut after =
                    continuation_source(&path, &continuation_member(&path, "continued"));
                if edited {
                    after = after.replace("^ 127", "^ 255");
                }
                let f = Fixture::new();
                let root = f.commit(
                    &[(&path, &before), (&poison_path, "\0")],
                    &[],
                    "initial implementation",
                );
                let tip = f.commit(
                    &[
                        (&path, "// moved implementation\n"),
                        (&moved_path, &after),
                        (&poison_path, "\0"),
                    ],
                    &[&root],
                    "new destination",
                );
                for (at, query) in [
                    (
                        &tip,
                        format!("show {tip} {kind}continued path:{moved_path}"),
                    ),
                    (
                        &root,
                        format!("diff {root}..{tip} {kind}original path:{path}"),
                    ),
                ] {
                    let shown = f.history(at, &query).await;
                    assert!(shown.contains(&format!("a/{path}")), "{shown}");
                    assert!(shown.contains(&format!("b/{moved_path}")), "{shown}");
                    assert!(shown.contains("# move "), "{shown}");
                    assert!(!shown.contains("/dev/null"), "{shown}");
                }
            }
        }
    }
}

#[tokio::test]
async fn changed_file_predecessor_ties_do_not_require_unchanged_origins() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        let first = format!("a.{extension}");
        let second = format!("z.{extension}");
        let path = format!("selected.{extension}");
        let poison_path = format!("unrelated.{extension}");
        let left = continuation_source(&first, &continuation_member(&first, "left"));
        let right = continuation_source(&second, &continuation_member(&second, "right"));
        let after = continuation_source(&path, &continuation_member(&path, "continued"));
        let f = Fixture::new();
        let root = f.commit(
            &[(&first, &left), (&second, &right), (&poison_path, "\0")],
            &[],
            "two implementations",
        );
        let tip = f.commit(
            &[(&path, &after), (&poison_path, "\0")],
            &[&root],
            "one destination",
        );
        let history = f
            .history(&tip, &format!("log {kind}continued path:{path}"))
            .await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        for predecessor in [&first, &second] {
            assert!(
                history.contains(&format!("~move {predecessor}→{path}")),
                "{history}"
            );
        }
        assert!(history.contains("competing predecessors"), "{history}");
        assert!(!history.contains("copy"), "{history}");
    }
}

#[tokio::test]
async fn weak_changed_candidates_widen_before_reporting_unresolved_evidence() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        for retained_origin in [false, true] {
            let changed = format!("changed.{extension}");
            let origin_path = format!("origin.{extension}");
            let path = format!("selected.{extension}");
            let before = continuation_source(&changed, &continuation_member(&changed, "unrelated"))
                .replace("+ 10", "- 20")
                .replace("* 3", "* 5")
                .replace("^ 127", "^ 255");
            let origin = continuation_source(&path, &continuation_member(&path, "origin"));
            let after = continuation_source(&path, &continuation_member(&path, "copied"));
            let mut old_files = vec![(changed.as_str(), before.as_str())];
            let mut new_files = vec![
                (changed.as_str(), "// removed implementation\n"),
                (path.as_str(), after.as_str()),
            ];
            if retained_origin {
                old_files.push((&origin_path, &origin));
                new_files.push((&origin_path, &origin));
            }
            let f = Fixture::new();
            let root = f.commit(&old_files, &[], "initial implementations");
            let tip = f.commit(&new_files, &[&root], "new occurrence");
            let history = f
                .history(&tip, &format!("log {kind}copied path:{path}"))
                .await;
            if retained_origin {
                assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
                assert!(
                    history.contains(&format!("~copy {origin_path}→{path}")),
                    "{history}"
                );
                assert!(!history.contains("incomplete:"), "{history}");
            } else {
                assert!(ids(&history).is_empty(), "{history}");
                assert!(history.contains("incomplete:"), "{history}");
                assert!(history.contains("weak predecessor evidence"), "{history}");
                assert!(!history.ends_with("end\n"), "{history}");
            }
        }
    }
}

#[tokio::test]
async fn a_changed_file_copy_origin_does_not_hide_an_unchanged_origin() {
    for (extension, kind) in [("cs", "method:Worker."), ("rs", "function:")] {
        let changed = format!("changed.{extension}");
        let origin_path = format!("origin.{extension}");
        let path = format!("selected.{extension}");
        let before = continuation_source(&changed, &continuation_member(&changed, "original"));
        let retained = continuation_source(&changed, &continuation_member(&changed, "retained"));
        let origin = continuation_source(&path, &continuation_member(&path, "origin"));
        let after = continuation_source(&path, &continuation_member(&path, "copied"));
        let f = Fixture::new();
        let root = f.commit(
            &[(&changed, &before), (&origin_path, &origin)],
            &[],
            "initial implementations",
        );
        let tip = f.commit(
            &[
                (&changed, &retained),
                (&origin_path, &origin),
                (&path, &after),
            ],
            &[&root],
            "retained implementations",
        );
        let history = f
            .history(&tip, &format!("log {kind}copied path:{path}"))
            .await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        for predecessor in [&changed, &origin_path] {
            assert!(
                history.contains(&format!("~copy {predecessor}→{path}")),
                "{history}"
            );
        }
        assert!(history.contains("competing predecessors"), "{history}");
    }
}

fn fragment_competition_members() -> (String, String, String) {
    let original = continuation_member("Worker.cs", "Read");
    let copied = original
        .replace("int value", "short input")
        .replace("= value +", "= input +");
    let container = continuation_member("Worker.cs", "Container").replace(
        "        var adjusted",
        "        Audit(\"one\"); Audit(\"two\"); Audit(\"three\"); Audit(\"four\"); Audit(\"five\");\n        var adjusted",
    );
    (original, copied, container)
}

#[tokio::test]
async fn whole_copy_origin_outranks_boilerplate_fragments() {
    let path = "Worker.cs";
    let (original, copied, container) = fragment_competition_members();
    let before = continuation_source(path, &format!("{original}{container}"));
    let after = continuation_source(path, &format!("{original}{copied}{container}"));
    let f = Fixture::new();
    let root = f.commit(&[(path, &before)], &[], "initial implementations");
    let tip = f.commit(&[(path, &after)], &[&root], "additional member");
    let selected = "method:Worker.Read(short)";
    let history = f.history(&tip, &format!("log {selected}")).await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(history.contains("copy"), "{history}");
    let retained = f.history(&tip, "log method:Worker.Read(int)").await;
    assert_eq!(ids(&retained), [root.as_str()], "{retained}");
    let shown = f.history(&tip, &format!("show {tip} {selected}")).await;
    assert!(!body(&shown).contains("# fragment "), "{shown}");
    assert!(!body(&shown).contains("/dev/null"), "{shown}");
    let predecessor = f.history(&tip, &format!("show {root} {selected}")).await;
    let expected = format!("+{}", original.trim().replace('\n', "\n+"));
    assert!(body(&predecessor).contains(&expected), "{predecessor}");
    assert_eq!(body(&predecessor).matches("diff --git ").count(), 1);
}

#[tokio::test]
async fn whole_copy_origin_ties_survive_higher_scoring_local_fragments() {
    let path = "Worker.cs";
    let (original, copied, container) = fragment_competition_members();
    let second = original.replace("Read(int", "Read(byte");
    let left = continuation_source(path, &original);
    let right = continuation_source(path, &second);
    let before = continuation_source(path, &container);
    let after = continuation_source(path, &format!("{copied}{container}"));
    let f = Fixture::new();
    let root = f.commit(
        &[("a.cs", &left), ("z.cs", &right), (path, &before)],
        &[],
        "initial implementations",
    );
    let tip = f.commit(
        &[("a.cs", &left), ("z.cs", &right), (path, &after)],
        &[&root],
        "additional member",
    );
    let selected = "method:Worker.Read(short) path:Worker.cs";
    let history = f.history(&tip, &format!("log {selected}")).await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    for origin in ["a.cs", "z.cs"] {
        assert!(
            history.contains(&format!("~copy {origin}→{path}")),
            "{history}"
        );
    }
    assert!(history.contains("competing predecessors"), "{history}");
    let predecessor = f.history(&tip, &format!("show {root} {selected}")).await;
    assert!(!body(&predecessor).contains("# fragment "), "{predecessor}");
    assert_eq!(body(&predecessor).matches("diff --git ").count(), 2);
    for member in [&original, &second] {
        let expected = format!("+{}", member.trim().replace('\n', "\n+"));
        assert!(body(&predecessor).contains(&expected), "{predecessor}");
    }
}

#[tokio::test]
async fn rejected_whole_candidates_leave_fragment_ancestry_available() {
    let path = "Worker.cs";
    let (_, copied, container) = fragment_competition_members();
    let selected = copied.replace("Read(short", "Read(int");
    let original = selected.replace(
        "        var scaled = adjusted * 3;\n        return scaled ^ 127;",
        "        return ChooseFallback(adjusted);",
    );
    let retained = original.replace("Read(", "Retained(");
    let before = continuation_source(path, &format!("{original}{container}"));
    let after = continuation_source(path, &format!("{retained}{selected}{container}"));
    let f = Fixture::new();
    let root = f.commit(&[(path, &before)], &[], "initial implementations");
    let tip = f.commit(&[(path, &after)], &[&root], "new occurrence");
    let history = f.history(&tip, "log method:Worker.Read(int)").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(history.contains("extract"), "{history}");
    let shown = f
        .history(&tip, &format!("show {tip} method:Worker.Read(int)"))
        .await;
    assert!(body(&shown).contains("# fragment "), "{shown}");
    assert!(body(&shown).contains("scaled ^ 127"), "{shown}");
    assert!(!body(&shown).contains("ChooseFallback"), "{shown}");
    assert!(!body(&shown).contains("Audit("), "{shown}");
}

fn rust_owner_source(owner: &str, sibling: &str) -> String {
    format!(
        "impl<T, F> {owner}<T, F> {{\n    fn new(init: F) -> Self {{\n        {owner} {{ cell: OnceCell::new(), init: Cell::new(Some(init)) }}\n    }}\n}}\nimpl<T, F> {owner}<T, F> {{\n{sibling}}}\n"
    )
}

#[tokio::test]
async fn edited_members_follow_renamed_rust_owners_without_widening() {
    let sibling = continuation_member("lib.rs", "anchor");
    let before = rust_owner_source("Before", &sibling);
    let after = rust_owner_source("After", &sibling);
    for (path, destination) in [("a.rs", "z.rs"), ("z.rs", "a.rs")] {
        let f = Fixture::new();
        let root = f.commit(
            &[(path, &before), ("unrelated.rs", "\0")],
            &[],
            "initial owner",
        );
        let tip = f.commit(
            &[
                (path, "// split\n"),
                (destination, &after),
                ("unrelated.rs", "\0"),
            ],
            &[&root],
            "new owner header",
        );
        for (at, query) in [
            (&tip, format!("show {tip} method:new path:{destination}")),
            (&root, format!("diff {root}..{tip} method:new path:{path}")),
        ] {
            let shown = f.history(at, &query).await;
            assert!(shown.contains(&format!("a/{path}")), "{shown}");
            assert!(shown.contains(&format!("b/{destination}")), "{shown}");
            assert!(
                shown.contains("# move ") && shown.contains("# rename "),
                "{shown}"
            );
            assert!(
                !shown.contains("/dev/null") && !body(&shown).contains("fn anchor"),
                "{shown}"
            );
        }
    }
}

#[tokio::test]
async fn renamed_rust_owner_ties_keep_both_predecessors_without_widening() {
    let f = Fixture::new();
    let sibling = continuation_member("lib.rs", "anchor");
    let first = rust_owner_source("First", &sibling);
    let second = rust_owner_source("Second", &sibling);
    let after = rust_owner_source("After", &sibling);
    let root = f.commit(
        &[("a.rs", &first), ("z.rs", &second), ("unrelated.rs", "\0")],
        &[],
        "initial owners",
    );
    let tip = f.commit(
        &[("selected.rs", &after), ("unrelated.rs", "\0")],
        &[&root],
        "new owner header",
    );
    let history = f.history(&tip, "log method:new path:selected.rs").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    for path in ["a.rs", "z.rs"] {
        assert!(
            history.contains(&format!("~move {path}→selected.rs")),
            "{history}"
        );
    }
    assert!(history.contains("competing predecessors"), "{history}");
    assert!(!history.contains("copy"), "{history}");
}

#[tokio::test]
async fn renamed_rust_owner_evidence_does_not_override_a_retained_continuation() {
    let f = Fixture::new();
    let sibling = continuation_member("lib.rs", "anchor");
    let before = rust_owner_source("Before", &sibling);
    let after = rust_owner_source("After", &sibling);
    let root = f.commit(&[("before.rs", &before)], &[], "initial owner");
    let tip = f.commit(
        &[("before.rs", &before), ("after.rs", &after)],
        &[&root],
        "additional owner",
    );
    let history = f.history(&tip, "log method:new path:after.rs").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(history.contains("~copy before.rs→after.rs"), "{history}");
    assert!(!history.contains("move"), "{history}");
}

#[tokio::test]
async fn constructors_need_distinctive_owner_evidence_not_just_similar_bodies() {
    for (before, after, weak) in [
        (
            rust_owner_source("Before", ""),
            rust_owner_source("After", ""),
            true,
        ),
        (
            rust_owner_source("Before", "fn anchor() -> i32 { 1 }\n"),
            rust_owner_source("After", "fn anchor() -> i32 { 1 }\n"),
            true,
        ),
        (
            "impl Before { fn new() -> Self { Self::default() } fn anchor() -> i32 { 1 } }".into(),
            "impl After { fn new() -> Self { Self::default() } fn anchor() -> i32 { 1 } }".into(),
            false,
        ),
    ] {
        let f = Fixture::new();
        let root = f.commit(&[("before.rs", &before)], &[], "initial owner");
        let tip = f.commit(&[("after.rs", &after)], &[&root], "new occurrence");
        let history = f.history(&tip, "log method:new path:after.rs").await;
        assert!(
            !history.contains("move") && !history.contains("copy"),
            "{history}"
        );
        if weak {
            assert!(ids(&history).is_empty(), "{history}");
            assert!(
                history.contains("incomplete:") && history.contains("weak predecessor evidence"),
                "{history}"
            );
        } else {
            assert_eq!(ids(&history), [tip.as_str()], "{history}");
            assert!(history.ends_with("end\n"), "{history}");
        }
    }
}

#[tokio::test]
async fn an_owner_witness_cannot_be_the_candidate_itself() {
    let f = Fixture::new();
    let original = rust_owner_source("Before", "");
    let before = format!("{original}{}", original.replace("fn new", "fn make"));
    let after = rust_owner_source("After", "");
    let root = f.commit(&[("before.rs", &before)], &[], "initial members");
    let tip = f.commit(&[("after.rs", &after)], &[&root], "new occurrence");
    for (at, query) in [
        (
            &root,
            format!("diff {root}..{tip} method:make path:before.rs"),
        ),
        (&tip, format!("show {tip} method:new path:after.rs")),
    ] {
        let error = f
            .app
            .history(&format!("{}#{at}", f.root.path().display()), &query)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("weak predecessor evidence"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn conditional_siblings_do_not_establish_a_renamed_owner() {
    let target = continuation_member("Worker.cs", "Target");
    let sibling = continuation_member("Worker.cs", "Anchor");
    let before = format!("class Before {{\n{target}#if FEATURE\n{sibling}#endif\n}}\n");
    let after = before
        .replace("class Before", "class After")
        .replace("^ 127", "^ 255");
    let f = Fixture::new();
    let root = f.commit(&[("Before.cs", &before)], &[], "initial owner");
    let tip = f.commit(&[("After.cs", &after)], &[&root], "new owner header");
    let history = f.history(&tip, "log method:After.Target").await;
    assert!(ids(&history).is_empty(), "{history}");
    assert!(history.contains("weak predecessor evidence"), "{history}");
}

#[tokio::test]
async fn distinctive_owner_groups_still_carry_unchanged_small_members() {
    let f = Fixture::new();
    let before = format!(
        "class Before {{ int Tiny() {{ return 1; }} {} }}\nclass Neighbour {{ int Tiny() {{ return 1; }} }}\n",
        continuation_member("Worker.cs", "Anchor"),
    );
    let middle = before.replace("+ 10", "+ 11");
    let after = middle.replace("class Before", "class After");
    let root = f.commit(&[("Worker.cs", &before)], &[], "initial groups");
    let sibling = f.commit(&[("Worker.cs", &middle)], &[&root], "sibling adjustment");
    let tip = f.commit(&[("Worker.cs", &after)], &[&sibling], "new group header");
    let history = f.history(&tip, "log method:After.Tiny").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(
        history.contains("rename Before.Tiny()→After.Tiny()"),
        "{history}"
    );
    assert!(!history.contains("copy"), "{history}");
}

#[tokio::test]
async fn a_copy_origin_does_not_hide_an_available_direct_predecessor() {
    for (path, kind, signature) in [
        (
            "Worker.cs",
            "method:Worker.",
            "Worker.Other(int)→Worker.Old(int)",
        ),
        ("lib.rs", "function:", "Other(i32)→Old(i32)"),
    ] {
        let original = continuation_member(path, "Old");
        let other = continuation_member(path, "Other")
            .replace("* 3", "* 5")
            .replace("^ 127", "^ 255");
        let before = continuation_source(path, &format!("{original}{other}"));
        let after = continuation_source(
            path,
            &format!(
                "{}{}",
                continuation_member(path, "Retained"),
                original.replace("+ 10", "+ 20"),
            ),
        );
        let f = Fixture::new();
        let root = f.commit(&[(path, &before)], &[], "initial implementations");
        let tip = f.commit(&[(path, &after)], &[&root], "revised implementations");
        let history = f.history(&tip, &format!("log {kind}Old")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(history.contains("copy"), "{history}");
        assert!(
            history.contains(&format!("~rename {signature}")),
            "{history}"
        );
        assert!(history.contains("competing predecessors"), "{history}");

        let other_path = if path.ends_with(".cs") {
            "Other.cs"
        } else {
            "other.rs"
        };
        let other = continuation_source(
            path,
            &continuation_member(path, "Other").replace("+ 10", "+ 20"),
        );
        let f = Fixture::new();
        let before = continuation_source(path, &original);
        let root = f.commit(
            &[(path, &before), (other_path, &other)],
            &[],
            "initial implementations",
        );
        let tip = f.commit(&[(path, &after)], &[&root], "revised implementations");
        let history = f.history(&tip, &format!("log {kind}Old")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(history.contains("copy"), "{history}");
        assert!(
            history.contains(&format!("~move {other_path}→{path}")),
            "{history}"
        );
        assert!(history.contains("competing predecessors"), "{history}");
    }
}

#[tokio::test]
async fn rejecting_a_local_claim_widens_before_proving_forward_absence() {
    for (path, destination, kind) in [
        ("Worker.cs", "Moved.cs", "method:Worker."),
        ("lib.rs", "moved.rs", "function:"),
    ] {
        let original = continuation_member(path, "original");
        let sibling = continuation_member(path, "sibling");
        let before = continuation_source(path, &format!("{original}{sibling}"));
        let after = continuation_source(path, &sibling);
        let moved = continuation_source(path, &continuation_member(path, "continued"));
        let poison_path = if path.ends_with(".cs") {
            "unrelated.cs"
        } else {
            "unrelated.rs"
        };
        let f = Fixture::new();
        let root = f.commit(
            &[(path, &before), (poison_path, "\0")],
            &[],
            "initial implementations",
        );
        let tip = f.commit(
            &[(path, &after), (destination, &moved), (poison_path, "\0")],
            &[&root],
            "two destinations",
        );
        let diff = f
            .history(
                &root,
                &format!("diff {root}..{tip} {kind}original path:{path}"),
            )
            .await;
        assert!(body(&diff).contains("original("), "{diff}");
        assert!(body(&diff).contains("continued("), "{diff}");
        assert!(!body(&diff).contains("sibling("), "{diff}");
    }
}
#[tokio::test]
async fn an_edited_directory_move_preserves_deleted_members_and_exclusions() {
    let f = Fixture::new();
    let alpha = "fn alpha(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let beta = "struct Beta { value: i32 }\n";
    let gamma = "enum Gamma { First, Second }\n";
    let root = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/c.rs", gamma),
            ("old/retired.rs", "fn retired() -> i32 { 41 }\n"),
            ("old/private/secret.rs", "const SECRET: i32 = 9;\n"),
        ],
        &[],
        "initial directory",
    );
    let private_edit = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/c.rs", gamma),
            ("old/retired.rs", "fn retired() -> i32 { 41 }\n"),
            ("old/private/secret.rs", "const SECRET: i32 = 10;\n"),
        ],
        &[&root],
        "private adjustment",
    );
    let deletion = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/c.rs", gamma),
            ("old/private/secret.rs", "const SECRET: i32 = 10;\n"),
        ],
        &[&private_edit],
        "retire member",
    );
    let edited = alpha.replace("+ 10", "+ 11");
    let tip = f.commit(
        &[
            ("new/a.rs", &edited),
            ("new/b.rs", beta),
            ("new/c.rs", gamma),
            ("new/private/secret.rs", "const SECRET: i32 = 10;\n"),
        ],
        &[&deletion],
        "relocate directory and adjust member",
    );
    let included = f.history(&tip, "log path:new/**").await;
    assert!(included.ends_with("end\n"), "{included}");
    assert!(
        !included.contains("unresolved directory scope"),
        "{included}"
    );
    assert_eq!(
        ids(&included),
        [
            tip.as_str(),
            deletion.as_str(),
            private_edit.as_str(),
            root.as_str()
        ],
        "{included}"
    );
    let excluded = f
        .history(&tip, "log path:new/** -path:new/private/**")
        .await;
    assert_eq!(
        ids(&excluded),
        [tip.as_str(), deletion.as_str(), root.as_str()],
        "{excluded}"
    );
    assert!(excluded.ends_with("end\n"), "{excluded}");
    // Literal directory selections retain their spelling and exclusions too.
    let literal = f.history(&tip, "log path:new -path:new/private").await;
    assert_eq!(ids(&literal), ids(&excluded), "{literal}");
    assert!(literal.ends_with("end\n"), "{literal}");
    for exclusion in ["new*/private/**", "n?w/private/**", "**/private/**"] {
        let history = f
            .history(&tip, &format!("log path:new/** -path:{exclusion}"))
            .await;
        assert_eq!(ids(&history), ids(&excluded), "{history}");
        assert!(history.ends_with("end\n"), "{history}");
    }

    for (anchor, scope) in [
        (&tip, "path:new/** -path:new/private/**"),
        (&root, "path:old/** -path:old/private/**"),
    ] {
        // Exercise backward anchoring and forward propagation, with both
        // endpoint orders. The retired member must be found from path scope.
        for (from, to, sign) in [
            (&private_edit, &deletion, '-'),
            (&deletion, &private_edit, '+'),
        ] {
            let diff = f
                .history(anchor, &format!("diff {from}..{to} {scope}"))
                .await;
            assert!(body(&diff).contains("old/retired.rs"), "{diff}");
            assert!(
                body(&diff).contains(&format!("{sign}fn retired()")),
                "{diff}"
            );
            assert!(!body(&diff).contains("SECRET"), "{diff}");
            assert!(
                !body(&diff).contains("unresolved directory scope"),
                "{diff}"
            );
        }
        for (from, to, sign) in [(&root, &tip, '-'), (&tip, &root, '+')] {
            let diff = f
                .history(anchor, &format!("diff {from}..{to} {scope}"))
                .await;
            assert!(body(&diff).contains("new/a.rs"), "{diff}");
            assert!(
                body(&diff).contains(&format!("{sign}fn retired()")),
                "{diff}"
            );
            assert!(!body(&diff).contains("SECRET"), "{diff}");
            assert!(
                !body(&diff).contains("unresolved directory scope"),
                "{diff}"
            );
        }
        let show = f.history(anchor, &format!("show {deletion} {scope}")).await;
        assert!(body(&show).contains("-fn retired()"), "{show}");
        assert!(!body(&show).contains("SECRET"), "{show}");
    }
}

#[tokio::test]
async fn importing_one_member_does_not_claim_its_old_directory_history() {
    let f = Fixture::new();
    let shared = "fn shared(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let root = f.commit(
        &[
            ("library/shared.rs", shared),
            ("library/unrelated.rs", "const OTHER: i32 = 7;\n"),
        ],
        &[],
        "initial sources",
    );
    let sibling = f.commit(
        &[
            ("library/shared.rs", shared),
            ("library/unrelated.rs", "const OTHER: i32 = 9;\n"),
        ],
        &[&root],
        "unrelated adjustment",
    );
    let tip = f.commit(
        &[
            ("library/shared.rs", shared),
            ("library/unrelated.rs", "const OTHER: i32 = 9;\n"),
            ("selected/shared.rs", shared),
        ],
        &[&sibling],
        "import shared source",
    );
    let history = f.history(&tip, "log path:selected/**").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(
        history.contains("incomplete: unresolved directory scope"),
        "{history}"
    );
    assert!(!history.ends_with("end\n"), "{history}");
    let page = f.history(&tip, "log path:selected/** limit:1").await;
    assert!(
        page.contains("coverage: unresolved directory scope"),
        "{page}"
    );
    assert!(page.contains("more skip:1"), "{page}");
    let last = f.history(&tip, "log path:selected/** skip:1").await;
    assert_eq!(ids(&last), [root.as_str()], "{last}");
    assert!(
        last.contains("incomplete: unresolved directory scope"),
        "{last}"
    );
    for (from, to) in [(&root, &sibling), (&sibling, &root)] {
        let diff = f
            .history(&tip, &format!("diff {from}..{to} path:selected/**"))
            .await;
        assert!(body(&diff).contains("unresolved directory scope"), "{diff}");
        assert!(!body(&diff).contains("diff --git"), "{diff}");
        assert!(!body(&diff).contains("OTHER"), "{diff}");
    }
}

#[tokio::test]
async fn partial_directory_moves_and_splits_keep_only_supported_members() {
    let alpha = "fn alpha(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let edited = alpha.replace("+ 10", "+ 11");
    let beta = "struct Beta { value: i32 }\n";
    for kind in ["copy", "partial", "split"] {
        let f = Fixture::new();
        let root = f.commit(
            &[
                ("library/a.rs", alpha),
                ("library/b.rs", beta),
                ("library/neighbour.rs", "enum Neighbour { First }\n"),
                ("library/retired.rs", "const RETIRED: bool = true;\n"),
            ],
            &[],
            "initial library",
        );
        let neighbour = f.commit(
            &[
                ("library/a.rs", alpha),
                ("library/b.rs", beta),
                ("library/neighbour.rs", "enum Neighbour { First, Second }\n"),
                ("library/retired.rs", "const RETIRED: bool = true;\n"),
            ],
            &[&root],
            "unrelated neighbour edit",
        );
        let deletion = f.commit(
            &[
                ("library/a.rs", alpha),
                ("library/b.rs", beta),
                ("library/neighbour.rs", "enum Neighbour { First, Second }\n"),
            ],
            &[&neighbour],
            "retire unrelated member",
        );
        let mut files = vec![
            ("selected/a.rs", edited.as_str()),
            ("selected/b.rs", beta),
            (
                if kind == "split" {
                    "other/neighbour.rs"
                } else {
                    "library/neighbour.rs"
                },
                "enum Neighbour { First, Second }\n",
            ),
        ];
        if kind == "copy" {
            files.extend([("library/a.rs", alpha), ("library/b.rs", beta)]);
        }
        let tip = f.commit(&files, &[&deletion], kind);
        let history = f.history(&tip, "log path:selected/**").await;
        assert_eq!(
            ids(&history),
            [tip.as_str(), root.as_str()],
            "{kind}: {history}"
        );
        assert!(
            history.contains("incomplete: unresolved directory scope"),
            "{history}"
        );
        for (from, to) in [(&root, &deletion), (&deletion, &root)] {
            let diff = f
                .history(&tip, &format!("diff {from}..{to} path:selected/**"))
                .await;
            assert!(body(&diff).contains("unresolved directory scope"), "{diff}");
            assert!(!body(&diff).contains("diff --git"), "{diff}");
        }
        let root_show = f
            .history(&tip, &format!("show {root} path:selected/**"))
            .await;
        assert!(body(&root_show).contains("+fn alpha"), "{root_show}");
        assert!(body(&root_show).contains("+struct Beta"), "{root_show}");
        assert!(!body(&root_show).contains("Neighbour"), "{root_show}");
        assert!(!body(&root_show).contains("RETIRED"), "{root_show}");
    }
}

#[tokio::test]
async fn a_copied_singleton_subtree_does_not_recover_unrelated_retirements() {
    let f = Fixture::new();
    let shared = "struct Shared { value: i32 }\n";
    let root = f.commit(
        &[
            ("library/shared.rs", shared),
            ("library/retired.rs", "enum Retired { First, Last }\n"),
        ],
        &[],
        "initial library",
    );
    let deletion = f.commit(
        &[("library/shared.rs", shared)],
        &[&root],
        "retire neighbour",
    );
    let tip = f.commit(
        &[
            ("library/shared.rs", shared),
            ("selected/shared.rs", shared),
        ],
        &[&deletion],
        "copy singleton subtree",
    );
    let history = f.history(&tip, "log path:selected/**").await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(
        history.contains("incomplete: unresolved directory scope"),
        "{history}"
    );
    let show = f
        .history(&tip, &format!("show {deletion} path:selected/**"))
        .await;
    assert!(body(&show).contains("unresolved directory scope"), "{show}");
    assert!(!body(&show).contains("Retired"), "{show}");
}

#[tokio::test]
async fn a_resolved_directory_does_not_hide_an_independent_import() {
    let f = Fixture::new();
    let alpha = "fn alpha(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let edited = alpha.replace("+ 10", "+ 11");
    let beta = "struct Beta { value: i32 }\n";
    let shared = "enum Shared { First, Last }\n";
    let root = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/retired.rs", "const RETIRED: i32 = 123;\n"),
            ("library/shared.rs", shared),
            ("library/neighbour.rs", "const NEIGHBOUR: i32 = 1;\n"),
        ],
        &[],
        "initial sources",
    );
    let deletion = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("library/shared.rs", shared),
            ("library/neighbour.rs", "const NEIGHBOUR: i32 = 1;\n"),
        ],
        &[&root],
        "retire directory member",
    );
    let tip = f.commit(
        &[
            ("new/a.rs", &edited),
            ("new/b.rs", beta),
            ("imported/shared.rs", shared),
            ("library/shared.rs", shared),
            ("library/neighbour.rs", "const NEIGHBOUR: i32 = 1;\n"),
        ],
        &[&deletion],
        "move directory and import independent file",
    );
    let scope = "path:new/** path:imported/**";
    let history = f.history(&tip, &format!("log {scope}")).await;
    assert_eq!(
        ids(&history),
        [tip.as_str(), deletion.as_str(), root.as_str()],
        "{history}"
    );
    assert!(
        history.contains("incomplete: unresolved directory scope"),
        "{history}"
    );
    let show = f.history(&tip, &format!("show {deletion} {scope}")).await;
    assert!(body(&show).contains("-const RETIRED"), "{show}");
    assert!(body(&show).contains("unresolved directory scope"), "{show}");
    assert!(!body(&show).contains("NEIGHBOUR"), "{show}");
}

#[tokio::test]
async fn successive_edited_directory_moves_preserve_wildcard_exclusions() {
    let f = Fixture::new();
    let alpha = "fn alpha(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let beta = "struct Beta { value: i32 }\n";
    let first = alpha.replace("+ 10", "+ 11");
    let second = alpha.replace("+ 10", "+ 12");
    let root = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/private/p.rs", "const SECRET: i32 = 1;\n"),
        ],
        &[],
        "initial directory",
    );
    let moved = f.commit(
        &[
            ("new/a.rs", &first),
            ("new/b.rs", beta),
            ("new/private/p.rs", "const SECRET: i32 = 1;\n"),
        ],
        &[&root],
        "move and edit",
    );
    let private = f.commit(
        &[
            ("new/a.rs", &first),
            ("new/b.rs", beta),
            ("new/private/p.rs", "const SECRET: i32 = 2;\n"),
        ],
        &[&moved],
        "private-only edit",
    );
    let tip = f.commit(
        &[
            ("latest/a.rs", &second),
            ("latest/b.rs", beta),
            ("latest/private/p.rs", "const SECRET: i32 = 2;\n"),
        ],
        &[&private],
        "move and edit again",
    );
    for (at, scope) in [
        (&tip, "path:latest/** -path:latest*/private/**"),
        (&root, "path:old/** -path:old*/private/**"),
    ] {
        let show = f.history(at, &format!("show {private} {scope}")).await;
        assert!(!body(&show).contains("SECRET"), "{show}");
        assert!(!body(&show).contains("diff --git"), "{show}");
        assert!(
            !body(&show).contains("unresolved directory scope"),
            "{show}"
        );
        let diff = f.history(at, &format!("diff {root}..{tip} {scope}")).await;
        assert!(body(&diff).contains("latest/a.rs"), "{diff}");
        assert!(!body(&diff).contains("SECRET"), "{diff}");
        assert!(
            !body(&diff).contains("unresolved directory scope"),
            "{diff}"
        );
    }
    let history = f
        .history(&tip, "log path:latest/** -path:latest*/private/**")
        .await;
    assert_eq!(
        ids(&history),
        [tip.as_str(), moved.as_str(), root.as_str()],
        "{history}"
    );
    assert!(history.ends_with("end\n"), "{history}");
}

#[tokio::test]
async fn directory_scopes_cross_all_merge_parents_before_recovering_retirements() {
    let f = Fixture::new();
    let alpha = "fn alpha(value: i32) -> i32 { let adjusted = value + 10; let scaled = adjusted * 3; scaled ^ 127 }\n";
    let beta = "struct Beta { value: i32 }\n";
    let gamma = "fn gamma(input: u64) -> u64 { let mask = input & 255; let rotated = mask.rotate_left(2); rotated.wrapping_sub(input) }\n";
    let alpha_edit = alpha.replace("+ 10", "+ 11");
    let gamma_edit = gamma.replace("& 255", "& 511");
    let root = f.commit(
        &[
            ("old/a.rs", alpha),
            ("old/b.rs", beta),
            ("old/c.rs", gamma),
            ("old/retired.rs", "const RETIRED: bool = true;\n"),
        ],
        &[],
        "initial members",
    );
    let deletion = f.commit(
        &[("old/a.rs", alpha), ("old/b.rs", beta), ("old/c.rs", gamma)],
        &[&root],
        "retire member",
    );
    let left = f.commit(
        &[
            ("left/a.rs", &alpha_edit),
            ("left/b.rs", beta),
            ("left/c.rs", gamma),
        ],
        &[&deletion],
        "left relocation and edit",
    );
    let right = f.commit(
        &[
            ("right/a.rs", alpha),
            ("right/b.rs", beta),
            ("right/c.rs", &gamma_edit),
        ],
        &[&deletion],
        "right relocation and edit",
    );
    for parents in [
        [left.as_str(), right.as_str()],
        [right.as_str(), left.as_str()],
    ] {
        let merge = f.commit(
            &[
                ("new/a.rs", &alpha_edit),
                ("new/b.rs", beta),
                ("new/c.rs", &gamma_edit),
            ],
            &parents,
            "combine both contributions",
        );
        let history = f.history(&merge, "log path:new/**").await;
        for expected in [&left, &right, &deletion, &root] {
            assert!(ids(&history).contains(&expected.as_str()), "{history}");
        }
        assert!(history.ends_with("end\n"), "{history}");
        assert!(!history.contains("unresolved directory scope"), "{history}");
        let show = f
            .history(&merge, &format!("show {deletion} path:new/**"))
            .await;
        assert!(body(&show).contains("-const RETIRED"), "{show}");
        for parent in [1, 2] {
            let show = f
                .history(&merge, &format!("show {merge} parent:{parent} path:new/**"))
                .await;
            assert!(body(&show).contains("new/"), "{show}");
            assert!(
                !body(&show).contains("unresolved directory scope"),
                "{show}"
            );
        }
    }
}
#[tokio::test]
async fn history_selects_native_and_document_files_supported_by_sigla() {
    for (path, source) in [
        (
            "effect.hlsl",
            "float4 shade(float4 color) { return color * 0.5; }\n",
        ),
        (
            "effect.glsl",
            "vec4 shade(vec4 color) { return color * 0.5; }\n",
        ),
        (
            "effect.shader",
            "Shader \"Custom/Example\" { SubShader { Pass {} } }\n",
        ),
        ("header.hxx", "struct Example { int value; };\n"),
        ("settings.nix", "{ version = 1; }\n"),
    ] {
        let f = Fixture::new();
        let root = f.commit(&[(path, source)], &[], "initial source");
        let history = f.history(&root, &format!("log file:{path}")).await;
        assert_eq!(ids(&history), [root.as_str()], "{history}");
        let shown = f.history(&root, &format!("show {root} file:{path}")).await;
        assert!(body(&shown).contains(path), "{shown}");
    }
}

#[tokio::test]
async fn edited_body_anchors_preserve_whole_origins_and_competition() {
    let member = r#"    int Original(int value) {
        var adjusted = value + 10;
        var scaled = adjusted * 3;
        var rotated = (scaled << 5) | (scaled >> 27);
        var combined = rotated + value;
        var spread = combined ^ (combined >> 16);
        var avalanche = spread * 668265263;
        var folded = avalanche ^ (avalanche >> 13);
        return folded & 65535;
    }
"#;
    let before = format!("class Before {{\n{member}}}\n");
    let rival = before.replace("class Before", "class Rival");
    // Both owner and member names change; there are no sibling witnesses.
    let after = before
        .replace("class Before", "class After")
        .replace("Original(", "Relocated(")
        .replace("+ 10", "+ 11");
    for case in ["move", "tie", "copy"] {
        let f = Fixture::new();
        let mut old_files = vec![("before.cs", before.as_str())];
        let mut new_files = vec![("after.cs", after.as_str())];
        if case == "tie" {
            old_files.push(("rival.cs", &rival));
        }
        if case == "copy" {
            new_files.push(("before.cs", &before));
        } else {
            // Direct continuations must settle without fetching unchanged origins.
            old_files.push(("unrelated.cs", "\0"));
            new_files.push(("unrelated.cs", "\0"));
        }
        let root = f.commit(&old_files, &[], "initial implementations");
        let tip = f.commit(&new_files, &[&root], "revised implementation");
        let selected = "method:After.Relocated(int) path:after.cs";
        let history = f.history(&tip, &format!("log {selected}")).await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        if case == "copy" {
            assert!(history.contains("~copy before.cs→after.cs"), "{history}");
            assert!(!history.contains("move"), "{history}");
        } else {
            assert!(history.contains("~move before.cs→after.cs"), "{history}");
            assert!(!history.contains("copy"), "{history}");
        }
        if case == "tie" {
            assert!(history.contains("~move rival.cs→after.cs"), "{history}");
            assert!(history.contains("competing predecessors"), "{history}");
        }
        let predecessor = f.history(&tip, &format!("show {root} {selected}")).await;
        let expected = format!("+{}", member.trim().replace('\n', "\n+"));
        assert!(body(&predecessor).contains(&expected), "{predecessor}");
        assert!(!body(&predecessor).contains("# fragment "), "{predecessor}");
        assert_eq!(
            body(&predecessor).matches("diff --git ").count(),
            if case == "tie" { 2 } else { 1 },
            "{predecessor}"
        );
        if case == "move" {
            let shown = f
                .history(
                    &root,
                    &format!("diff {root}..{tip} method:Before.Original(int) path:before.cs"),
                )
                .await;
            assert!(body(&shown).contains("--- a/before.cs"), "{shown}");
            assert!(body(&shown).contains("+++ b/after.cs"), "{shown}");
            assert!(body(&shown).contains("Relocated("), "{shown}");
            assert!(!body(&shown).contains("/dev/null"), "{shown}");
        }
    }
}
