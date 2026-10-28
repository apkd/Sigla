//! Object-only public-service coverage for mixed-language history anchors.
use super::*;

const RUST: &str =
    "struct Request; impl Request { fn parse(input: &str) -> usize { input.len() } }\n";

#[tokio::test]
async fn smoke_rust_anchor_ignores_impossible_csharp_candidates() {
    let fixture = Fixture::new();
    let commit = fixture.commit(
        &[
            ("src/lib.rs", RUST),
            // The owner deliberately has the same spelling. A mere text
            // prefilter cannot establish that this language is irrelevant.
            ("Broken.cs", "class Request { void parse( }\n"),
        ],
        &[],
        "mixed language anchor",
    );
    for query in [
        "log method:Request::parse limit:3",
        "log method:Request::parse path:src/lib.rs limit:3",
    ] {
        let result = fixture.history(&commit, query).await;
        assert_eq!(ids(&result), [commit.as_str()], "{result}");
        assert!(result.contains("Request::parse"), "{result}");
    }
    assert!(!fixture.cache.path().join("analysis").exists());
}

#[tokio::test]
async fn smoke_relevant_damaged_rust_owner_stays_unresolved() {
    let fixture = Fixture::new();
    let commit = fixture.commit(
        &[
            ("src/lib.rs", RUST),
            ("broken.rs", "impl Request { fn other("),
        ],
        &[],
        "relevant damaged owner",
    );
    let error = fixture.error(&commit, "log method:Request::parse").await;
    assert!(error.contains("Unresolved anchor"), "{error}");
    assert!(error.contains("broken.rs"), "{error}");
}

#[tokio::test]
async fn smoke_unqualified_cross_language_ambiguity_is_retained() {
    let fixture = Fixture::new();
    let commit = fixture.commit(
        &[
            ("src/lib.rs", RUST),
            (
                "Other.cs",
                "class Other { int parse(string input) { return input.Length; } }\n",
            ),
        ],
        &[],
        "both languages declare parse",
    );
    let error = fixture.error(&commit, "log method:parse").await;
    assert!(error.contains("Ambiguous anchor"), "{error}");
}

#[tokio::test]
async fn smoke_external_rust_prefix_is_not_silently_discarded() {
    let fixture = Fixture::new();
    let commit = fixture.commit(&[("src/lib.rs", RUST)], &[], "lexical owner");
    let error = fixture
        .error(
            &commit,
            "log method:sigla::history::query::Request::parse path:src/lib.rs",
        )
        .await;
    assert!(error.contains("No declaration"), "{error}");
}

#[tokio::test]
async fn smoke_inline_rust_qualification_still_resolves() {
    let fixture = Fixture::new();
    let source = format!("mod api {{ {RUST} }}\n");
    let commit = fixture.commit(&[("src/lib.rs", &source)], &[], "inline owner");
    let result = fixture
        .history(&commit, "log method:api::Request::parse")
        .await;
    assert_eq!(ids(&result), [commit.as_str()], "{result}");
}
