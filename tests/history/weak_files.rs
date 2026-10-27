//! Whole-file history retains absence alongside weak identity candidates.
use super::{Fixture, body, git, ids};

const ANCESTRY: &str = "ancestry: unresolved file identity (absence or candidate ancestry)";

#[tokio::test]
async fn weak_file_alternative_expansion_is_bounded_after_exact_moves() {
    let (before, after) = weak_file_boilerplate();
    let f = Fixture::new();
    // One weak edge is cheap. Following each alternative through a cross-file
    // exact search would perform more than 65,536 inventory comparisons.
    let files: Vec<_> = (0..260)
        .map(|i| {
            (
                format!("Candidate{i}.cs"),
                before.replace("Previous", &format!("Previous{i}")),
            )
        })
        .collect();
    let original: Vec<_> = files
        .iter()
        .map(|(p, s)| (format!("original/{p}"), s))
        .collect();
    let previous: Vec<_> = files
        .iter()
        .map(|(p, s)| (format!("previous/{p}"), s))
        .collect();
    let root = f.commit(
        &original
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect::<Vec<_>>(),
        &[],
        "candidate origins",
    );
    let moved = f.commit(
        &previous
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect::<Vec<_>>(),
        &[&root],
        "move candidates exactly",
    );
    let tip = f.commit(&[("selected.cs", after)], &[&moved], "current path added");
    let first = f.history(&tip, "log file:selected.cs limit:1").await;
    assert_eq!(ids(&first), [tip.as_str()], "{first}");
    assert!(first.contains("more skip:1"), "{first}");
    assert!(first.contains(ANCESTRY), "{first}");
    let history = f.history(&tip, "log file:selected.cs limit:10").await;
    assert_eq!(ids(&history), [tip.as_str()], "{history}");
    assert!(
        history.contains("add selected.cs (path only; predecessor unresolved)"),
        "{history}"
    );
    assert!(
        history.contains("incomplete:") && history.contains("work limit reached"),
        "{history}"
    );
    assert!(history.contains(ANCESTRY), "{history}");
    assert!(!history.ends_with("end\n"), "{history}");
}

fn weak_file_boilerplate() -> (&'static str, &'static str) {
    // The shared comment gives these unrelated files a trigram score of 55.
    (
        "// This is a file in the demo.\nclass Previous { int Read() => 1; }\n",
        "// This is a file in the demo.\nclass SpaceName { string Label() => \"new\"; }\n",
    )
}

fn assert_uncertain_file_record(history: &str, commit: &str) {
    let record = history
        .lines()
        .find(|line| line.starts_with(commit))
        .unwrap();
    assert!(record.contains('~'), "{history}");
    // Alternatives do not have an aggregate line count.
    assert!(
        !record.contains(" +") && !record.contains(" -"),
        "{history}"
    );
}

#[tokio::test]
async fn weak_file_boilerplate_reports_the_path_add_and_preserves_ancestry_choices() {
    let (before, after) = weak_file_boilerplate();
    let f = Fixture::new();
    let root = f.commit(&[("src/Previous.cs", before)], &[], "previous file");
    let tip = f.commit(&[("src/Space Name.cs", after)], &[&root], "new file");
    let selector = r#"file:"src/Space Name.cs""#;
    assert_eq!(
        git(
            f.root.path(),
            &["diff", "--name-status", "-M", &root, &tip],
            b""
        ),
        "D\tsrc/Previous.cs\nA\tsrc/Space Name.cs"
    );
    assert_eq!(
        ids(&git(
            f.root.path(),
            &[
                "log",
                "--format=%H",
                "--follow",
                &tip,
                "--",
                "src/Space Name.cs"
            ],
            b""
        )),
        [tip.as_str()]
    );
    let history = f.history(&tip, &format!("log {selector} limit:10")).await;
    assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
    assert!(
        history.contains("add src/Space Name.cs (path only; predecessor unresolved)"),
        "{history}"
    );
    assert!(
        history.contains("~move src/Previous.cs→src/Space Name.cs"),
        "{history}"
    );
    assert!(history.contains("absence also possible"), "{history}");
    assert!(!history.contains("incomplete:"), "{history}");
    for commit in [&tip, &root] {
        assert_uncertain_file_record(&history, commit);
    }
    for (skip, expected) in [
        (0, vec![tip.as_str()]),
        (1, vec![root.as_str()]),
        (2, vec![]),
    ] {
        let page = f
            .history(&tip, &format!("log {selector} skip:{skip} limit:1"))
            .await;
        assert_eq!(ids(&page), expected, "{page}");
        assert!(
            page.contains("ancestry: unresolved file identity (absence or candidate ancestry)"),
            "{page}"
        );
        if skip == 0 {
            assert!(page.contains("more skip:1"), "{page}");
        }
    }
    for (query, old_path, new_path) in [
        (
            format!("show {tip}"),
            "src/Previous.cs",
            "src/Space Name.cs",
        ),
        (
            format!("diff {root}..{tip}"),
            "src/Previous.cs",
            "src/Space Name.cs",
        ),
        (
            format!("diff {tip}..{root}"),
            "src/Space Name.cs",
            "src/Previous.cs",
        ),
    ] {
        let shown = f.history(&tip, &format!("{query} {selector}")).await;
        let patch = body(&shown);
        assert!(
            shown.contains("ancestry: unresolved file identity"),
            "{shown}"
        );
        assert!(
            patch.contains("file may be absent; candidate regions are alternative ancestry"),
            "{shown}"
        );
        assert!(
            patch.contains(&format!("--- a/{old_path}\n+++ /dev/null")),
            "{shown}"
        );
        assert!(
            patch.contains(&format!("--- /dev/null\n+++ b/{new_path}")),
            "{shown}"
        );
        assert!(
            !patch.contains("# move ") && !patch.contains("# ~move "),
            "{shown}"
        );
        // A continuation can start after the canonical ancestry note. The
        // page header must still warn without changing canonical byte offsets.
        let offset = patch.find("diff --git ").unwrap();
        let continued = f
            .history(&tip, &format!("{query} {selector} offset:{offset}"))
            .await;
        assert_eq!(body(&continued), &patch[offset..], "{continued}");
        assert!(
            continued.contains("ancestry: unresolved file identity"),
            "{continued}"
        );
    }
    // Reusing completed evidence must preserve the same alternatives.
    assert_eq!(
        f.history(&tip, &format!("log {selector} limit:10")).await,
        history
    );
}

#[tokio::test]
async fn weak_edited_file_moves_keep_earlier_history_uncertain_in_both_directions() {
    let before = "class Worker {\n    int Calculate(int value) {\n        var shifted = value + 10;\n        var scaled = shifted * 3;\n        return scaled ^ 127;\n    }\n}\n";
    // A real relocation with an added guard and changed arithmetic, score 56.
    let after = "class Calculator {\n    int Calculate(int value) {\n        if (value < 0) return 0;\n        var shifted = value + 11;\n        var scaled = shifted * 5;\n        return scaled ^ 127;\n    }\n}\n";
    let f = Fixture::new();
    let initial = before.replace("+ 10", "+ 9");
    let root = f.commit(&[("old/Worker.cs", &initial)], &[], "initial calculation");
    let edited = f.commit(&[("old/Worker.cs", before)], &[&root], "adjust calculation");
    let moved = f.commit(
        &[("new/Calculator.cs", after)],
        &[&edited],
        "relocate and revise",
    );
    let tip = f.commit(
        &[("new/Calculator.cs", after), ("other.txt", "unrelated\n")],
        &[&moved],
        "other change",
    );
    let history = f.history(&tip, "log file:new/Calculator.cs").await;
    assert_eq!(
        ids(&history),
        [moved.as_str(), edited.as_str(), root.as_str()],
        "{history}"
    );
    assert!(
        history.contains("~move old/Worker.cs→new/Calculator.cs"),
        "{history}"
    );
    assert!(
        history.contains("add new/Calculator.cs (path only; predecessor unresolved)"),
        "{history}"
    );
    assert!(!history.contains("incomplete:"), "{history}");
    for commit in [&moved, &edited, &root] {
        assert_uncertain_file_record(&history, commit);
    }
    // Certainty cannot be recovered merely by crossing a later strong same-path edge.
    for (anchor, selector) in [
        (&tip, "file:new/Calculator.cs"),
        (&root, "file:old/Worker.cs"),
    ] {
        for (from, to, old_path, new_path) in [
            (&root, &moved, "old/Worker.cs", "new/Calculator.cs"),
            (&moved, &root, "new/Calculator.cs", "old/Worker.cs"),
        ] {
            let shown = f
                .history(anchor, &format!("diff {from}..{to} {selector}"))
                .await;
            let patch = body(&shown);
            assert!(
                shown.contains("ancestry: unresolved file identity"),
                "{shown}"
            );
            assert!(
                patch.contains("file may be absent; candidate regions are alternative ancestry"),
                "{shown}"
            );
            assert!(
                patch.contains(&format!("--- a/{old_path}\n+++ /dev/null")),
                "{shown}"
            );
            assert!(
                patch.contains(&format!("--- /dev/null\n+++ b/{new_path}")),
                "{shown}"
            );
            assert!(
                patch.contains("Calculate") && patch.contains("if (value < 0)"),
                "{shown}"
            );
            assert!(
                !patch.contains("# move ") && !patch.contains("# ~move "),
                "{shown}"
            );
        }
    }
    let older = f
        .history(&tip, &format!("show {edited} file:new/Calculator.cs"))
        .await;
    assert!(
        older.contains("ancestry: unresolved file identity"),
        "{older}"
    );
    assert!(
        body(&older).contains("-        var shifted = value + 9;"),
        "{older}"
    );
    assert!(
        body(&older).contains("+        var shifted = value + 10;"),
        "{older}"
    );
    assert!(body(&older).contains("file may be absent"), "{older}");
}

#[tokio::test]
async fn weak_file_candidates_do_not_break_ties_or_override_stronger_origins() {
    let (before, after) = weak_file_boilerplate();
    for evidence in ["weak_tie", "weak_further", "strong", "exact"] {
        let f = Fixture::new();
        let stronger = after.replace("\"new\"", "\"older\"");
        // Score 45 remains a possible origin even though it is outside the
        // ordinary score margin of the 55-point candidate.
        let further = before.replace("Read() => 1", "Read(int input) => input + 1");
        let rival = match evidence {
            "weak_further" => further.as_str(),
            "strong" => stronger.as_str(),
            "exact" => after,
            _ => before,
        };
        let root = f.commit(
            &[("a.cs", before), ("z.cs", rival)],
            &[],
            "possible origins",
        );
        let mut files = vec![("selected.cs", after)];
        // Force the search past changed weak candidates to an unchanged origin.
        if !evidence.starts_with("weak") {
            files.push(("z.cs", rival));
        }
        let tip = f.commit(&files, &[&root], "selected occurrence");
        let history = f.history(&tip, "log file:selected.cs").await;
        assert_eq!(ids(&history), [tip.as_str(), root.as_str()], "{history}");
        assert!(!history.contains("incomplete:"), "{history}");
        if evidence.starts_with("weak") {
            for path in ["a.cs", "z.cs"] {
                assert!(
                    history.contains(&format!("~move {path}→selected.cs")),
                    "{history}"
                );
            }
            assert!(
                history.contains("add selected.cs (path only; predecessor unresolved)"),
                "{history}"
            );
            let shown = f
                .history(&tip, &format!("show {root} file:selected.cs"))
                .await;
            for path in ["a.cs", "z.cs"] {
                assert!(body(&shown).contains(&format!("+++ b/{path}")), "{shown}");
            }
            assert_eq!(body(&shown).matches("diff --git ").count(), 2, "{shown}");
            assert!(
                shown.contains("ancestry: unresolved file identity"),
                "{shown}"
            );
        } else {
            assert!(history.contains("~copy z.cs→selected.cs"), "{history}");
            assert!(!history.contains("a.cs→selected.cs"), "{history}");
            assert!(!history.contains("predecessor unresolved"), "{history}");
            assert!(
                !history.contains("ancestry: unresolved file identity"),
                "{history}"
            );
            let shown = f
                .history(&tip, &format!("show {tip} file:selected.cs"))
                .await;
            assert!(body(&shown).contains("copy z.cs → selected.cs"), "{shown}");
            assert!(!body(&shown).contains("/dev/null"), "{shown}");
        }
    }
}

#[tokio::test]
async fn weak_file_alternatives_require_a_complete_wider_search() {
    let (before, after) = weak_file_boilerplate();
    for failure in ["missing blob", "malformed source"] {
        let f = Fixture::new();
        let unchanged = if failure == "malformed source" {
            "#if FEATURE\nclass Worker { int Target(\n"
        } else {
            "class Library { static int Value = 7; }\n"
        };
        let root = f.commit(
            &[("src/Previous.cs", before), ("src/Unchanged.cs", unchanged)],
            &[],
            "possible origins",
        );
        let tip = f.commit(
            &[("src/Selected.cs", after), ("src/Unchanged.cs", unchanged)],
            &[&root],
            "selected occurrence",
        );
        if failure == "missing blob" {
            let blob = git(
                f.root.path(),
                &["rev-parse", &format!("{root}:src/Unchanged.cs")],
                b"",
            );
            std::fs::remove_file(
                f.root
                    .path()
                    .join(".git/objects")
                    .join(&blob[..2])
                    .join(&blob[2..]),
            )
            .unwrap();
        }
        // The changed file is weak evidence, but the unchanged origin can
        // still outrank it. A failed wider search cannot commit either choice.
        let history = f.history(&tip, "log file:src/Selected.cs").await;
        assert!(ids(&history).is_empty(), "{failure}: {history}");
        assert!(history.contains("incomplete:"), "{failure}: {history}");
        assert!(!history.ends_with("end\n"), "{history}");
        assert!(!history.contains("add src/Selected.cs"), "{history}");
        assert!(
            !history.contains("ancestry: unresolved file identity"),
            "{history}"
        );
        assert!(
            f.app
                .history(
                    &format!("{}#{tip}", f.root.path().display()),
                    &format!("show {tip} file:src/Selected.cs"),
                )
                .await
                .is_err(),
            "{failure} must not produce a partial comparison"
        );
    }
}

#[tokio::test]
async fn weak_file_identity_is_not_settled_by_later_exact_endpoint_contents() {
    let (before, after) = weak_file_boilerplate();
    let f = Fixture::new();
    let root = f.commit(&[("old.cs", after)], &[], "initial content");
    let middle = f.commit(&[("old.cs", before)], &[&root], "rewrite old path");
    let tip = f.commit(&[("new.cs", after)], &[&middle], "possible relocation");
    // The weak tip-to-middle edge makes ancestry optional. Identical endpoint
    // blobs cannot turn that unresolved edge into a snapshot-inferred move.
    for (from, to, old_path, new_path) in [
        (&root, &tip, "old.cs", "new.cs"),
        (&tip, &root, "new.cs", "old.cs"),
    ] {
        let shown = f
            .history(&tip, &format!("diff {from}..{to} file:new.cs"))
            .await;
        let patch = body(&shown);
        assert!(
            shown.contains("ancestry: unresolved file identity"),
            "{shown}"
        );
        assert!(
            patch.contains(&format!("--- a/{old_path}\n+++ /dev/null")),
            "{shown}"
        );
        assert!(
            patch.contains(&format!("--- /dev/null\n+++ b/{new_path}")),
            "{shown}"
        );
        assert_eq!(patch.matches("diff --git ").count(), 2, "{shown}");
        assert!(
            !patch.contains("move ") && !patch.contains("copy "),
            "{shown}"
        );
    }
}

#[tokio::test]
async fn weak_file_introduction_at_merge_is_not_suppressed_or_counted() {
    for reverse_parents in [false, true] {
        let (before, after) = weak_file_boilerplate();
        let f = Fixture::new();
        let root = f.commit(&[("src/Previous.cs", before)], &[], "previous file");
        let left = f.commit(
            &[("src/Previous.cs", before), ("left.txt", "left branch\n")],
            &[&root],
            "left branch",
        );
        let right = f.commit(
            &[("src/Previous.cs", before), ("right.txt", "right branch\n")],
            &[&root],
            "right branch",
        );
        let parents = if reverse_parents {
            [&right, &left]
        } else {
            [&left, &right]
        };
        let merge = f.commit(
            &[("src/Space Name.cs", after)],
            &[parents[0], parents[1]],
            "introduce current path at merge",
        );
        let selector = r#"file:"src/Space Name.cs""#;
        let history = f.history(&merge, &format!("log {selector}")).await;
        assert_eq!(ids(&history), [merge.as_str(), root.as_str()], "{history}");
        assert!(history.contains(ANCESTRY), "{history}");
        assert!(
            history.contains("add src/Space Name.cs (path only; predecessor unresolved)"),
            "{history}"
        );
        assert!(
            history.contains("merge attribution unresolved (alternative regions or bases)"),
            "{history}"
        );
        assert!(!history.contains("incomplete:"), "{history}");
        for commit in [&merge, &root] {
            assert_uncertain_file_record(&history, commit);
        }

        // A page boundary inside the closed merge interval does not promote
        // the remaining predecessor's root to a definite source introduction.
        for (skip, expected) in [(0, &merge), (1, &root)] {
            let page = f
                .history(&merge, &format!("log {selector} skip:{skip} limit:1"))
                .await;
            assert_eq!(ids(&page), [expected.as_str()], "{page}");
            assert!(page.contains(ANCESTRY), "{page}");
            assert_uncertain_file_record(&page, expected);
        }
        for parent in [1, 2] {
            let shown = f
                .history(&merge, &format!("show {merge} parent:{parent} {selector}"))
                .await;
            assert!(shown.contains(ANCESTRY), "{shown}");
            assert!(
                body(&shown).contains("--- /dev/null\n+++ b/src/Space Name.cs"),
                "{shown}"
            );
            assert!(!body(&shown).contains("# ~move "), "{shown}");
        }
    }
}

#[tokio::test]
async fn weak_file_forward_bridge_does_not_erase_possible_branch_history() {
    for reverse_parents in [false, true] {
        let (before, after) = weak_file_boilerplate();
        let f = Fixture::new();
        let root = f.commit(&[("src/Previous.cs", before)], &[], "original lifetime");
        let moved = f.commit(
            &[("src/Space Name.cs", after)],
            &[&root],
            "possible edited move on side branch",
        );
        let deleted = f.commit(&[], &[&moved], "remove possible successor");
        let main = f.commit(
            &[("src/Previous.cs", before), ("main.txt", "unrelated\n")],
            &[&root],
            "retain original on main",
        );
        let parents = if reverse_parents {
            [&deleted, &main]
        } else {
            [&main, &deleted]
        };
        let merge = f.commit(
            &[("src/Previous.cs", before)],
            &[parents[0], parents[1]],
            "retain original at merge",
        );
        let selector = "file:src/Previous.cs";
        let history = f.history(&merge, &format!("log {selector}")).await;
        // Backward traversal reaches an absent side tip. Recovering its branch
        // from the shared root must preserve the possible move and deletion,
        // without pretending the merge definitely rejected that deletion.
        assert_eq!(
            ids(&history),
            [deleted.as_str(), moved.as_str(), root.as_str()],
            "{history}"
        );
        assert!(history.contains(ANCESTRY), "{history}");
        assert!(!history.contains("incomplete:"), "{history}");
        for commit in [&deleted, &moved] {
            assert_uncertain_file_record(&history, commit);
        }
        // Reconciling the optional child must not overwrite the independently
        // established parent occurrence with an absence alternative.
        let root_record = history
            .lines()
            .find(|line| line.starts_with(&root))
            .unwrap();
        assert!(root_record.contains(" add +2 -0 "), "{history}");
        assert!(!root_record.contains('~'), "{history}");
        for (skip, expected) in [(0, &deleted), (1, &moved)] {
            let page = f
                .history(&merge, &format!("log {selector} skip:{skip} limit:1"))
                .await;
            assert_eq!(ids(&page), [expected.as_str()], "{page}");
            assert!(page.contains(ANCESTRY), "{page}");
            assert_uncertain_file_record(&page, expected);
        }
        for (from, to) in [(&root, &moved), (&moved, &root)] {
            let shown = f
                .history(&merge, &format!("diff {from}..{to} {selector}"))
                .await;
            assert!(shown.contains(ANCESTRY), "{shown}");
            assert!(
                body(&shown)
                    .contains("file may be absent; candidate regions are alternative ancestry"),
                "{shown}"
            );
            assert!(!body(&shown).contains("# ~move "), "{shown}");
        }
    }
}

#[tokio::test]
async fn weak_file_forward_bridge_does_not_accept_additional_predecessors() {
    for reverse_parents in [false, true] {
        let (before, after) = weak_file_boilerplate();
        let f = Fixture::new();
        let root = f.commit(
            &[("src/Original.cs", before), ("src/Rival.cs", before)],
            &[],
            "two possible predecessors",
        );
        let moved = f.commit(
            &[("src/Space Name.cs", after)],
            &[&root],
            "possible edited move",
        );
        let deleted = f.commit(&[], &[&moved], "remove possible successor");
        let main = f.commit(
            &[("src/Original.cs", before)],
            &[&root],
            "retain original path",
        );
        let parents = if reverse_parents {
            [&deleted, &main]
        } else {
            [&main, &deleted]
        };
        let merge = f.commit(
            &[("src/Original.cs", before)],
            &[parents[0], parents[1]],
            "retain original at merge",
        );
        let history = f.history(&merge, "log file:src/Original.cs").await;
        // Root-to-main path continuity established Original specifically. The
        // reverse side-branch mapping also finds Rival; allowing an absence
        // alternative must not weaken that distinct identity contradiction.
        assert!(ids(&history).is_empty(), "unfinished interval: {history}");
        assert!(history.contains("incomplete:"), "{history}");
        assert!(
            history.contains("branch bridge has competing predecessors"),
            "{history}"
        );
        assert!(!history.ends_with("end\n"), "{history}");
    }
}
