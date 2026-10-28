//! Controls for the October 2026 mixed-language anchor failure.
use super::*;
use crate::query::Query;

#[test]
fn smoke_rust_separator_preserves_owner_spelling_checks() {
    for selector in ["method:Request::parse", "method:Request.parse"] {
        let query = Query::parse(selector).unwrap();
        assert!(may_declare(&query, "impl Request { fn parse() {} }"));
        // A malformed owner must not be declared irrelevant just because the
        // member token could have been lost to recovery.
        assert!(may_declare(&query, "impl Request { fn broken("));
        assert!(!may_declare(&query, "class Other { void Broken("));
    }
}

#[test]
fn smoke_anchor_language_filter_is_not_a_rust_language_guess() {
    let query = Query::parse("method:Request::parse").unwrap();
    assert!(!anchor_language(&query, Language::CSharp));
    assert!(anchor_language(&query, Language::Rust));
    assert!(anchor_language(&query, Language::Cpp));
    assert!(anchor_language(&query, Language::C));
    for selector in ["method:parse", "method:Request.parse", "type:Request"] {
        let query = Query::parse(selector).unwrap();
        assert!(anchor_language(&query, Language::CSharp));
    }
}

#[test]
fn smoke_prefilter_retains_unsupported_spellings_and_search_modes() {
    for selector in [
        "method:Request::parse match:loose",
        "method:*::parse",
        "method:Réponse::parse",
        "@src/lib.rs:1:1",
    ] {
        assert!(
            may_declare(&Query::parse(selector).unwrap(), ""),
            "{selector}"
        );
    }
    let query = Query::parse("method:Request::parse").unwrap();
    assert!(may_declare(
        &query,
        r"class \u0052equest { void parse() {} }"
    ));
    assert!(may_declare(
        &query,
        r"class \u{52}equest { void parse() {} }"
    ));
    assert!(may_declare(&query, r"class \u005"));
}

#[test]
fn smoke_prefilter_does_not_invent_external_rust_owners() {
    let query = Query::parse("method:sigla::history::query::Request::parse").unwrap();
    // History sees committed lexical facts, not the index's crate/module prefix.
    // Do not turn this into a suffix-only lookup that could pick another crate.
    assert!(!may_declare(&query, "impl Request { fn parse() {} }"));
    let lexical = Query::parse("method:api::Request::parse").unwrap();
    assert!(may_declare(
        &lexical,
        "mod api { struct Request; impl Request { fn parse() {} } }"
    ));
}
