//! Reuse the ordinary parse, but add closures only to transient history facts.
use super::{extract_parsed, range};
use crate::extract::{HistoricalDeclaration, HistoricalFacts, callable_region};
use ra_ap_syntax::{AstNode, Edition, SourceFile, ast};
use std::collections::HashMap;

pub(super) fn extract(source: &str) -> anyhow::Result<HistoricalFacts> {
    let parsed = SourceFile::parse(source, Edition::Edition2024);
    let mut facts: HistoricalFacts = extract_parsed(source, &parsed).into();
    let mut owners: HashMap<_, _> = facts
        .declarations
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.local())
        .map(|(index, d)| ((d.span.start, d.span.end), index))
        .collect();
    let root = parsed.tree();
    for node in root.syntax().descendants() {
        let Some(closure) = ast::ClosureExpr::cast(node.clone()) else {
            continue;
        };
        let Some(body) = node.children().find_map(ast::Expr::cast) else {
            continue;
        };
        let owner = node.ancestors().skip(1).find_map(|n| {
            let span = range(&n);
            owners.get(&(span.start, span.end)).copied()
        });
        let span = range(&node);
        let body_start = range(body.syntax()).start;
        let mut declaration = callable_region(
            "<closure>",
            "closure",
            span.clone(),
            body_start,
            owner.map(|i| &facts.declarations[i].declaration),
            "::",
        );
        if let Some(parameters) = closure.param_list() {
            declaration.parameters = parameters
                .params()
                .map(|p| {
                    p.ty()
                        .map_or_else(String::new, |t| t.syntax().text().to_string())
                })
                .collect();
        }
        let mut written = String::new();
        for token in node
            .descendants_with_tokens()
            .filter_map(|n| n.into_token())
            .take_while(|t| usize::from(t.text_range().start()) < body_start)
            .filter(|t| !t.kind().is_trivia())
        {
            if !written.is_empty() {
                written.push(' ');
            }
            written.push_str(token.text());
        }
        if let Some(owner) = owner.filter(|&i| !facts.declarations[i].written.is_empty()) {
            written = format!("{} / {written}", facts.declarations[owner].written);
        }
        owners.insert((span.start, span.end), facts.declarations.len());
        facts.declarations.push(HistoricalDeclaration {
            declaration,
            written,
            guard: 1,
            uncertain: facts.incomplete.is_some(),
        });
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_forms_keep_original_headers_bodies_and_snapshot_facts() {
        for (expression, body) in [
            ("|| 1", "1"),
            ("|value| value + 1", "value + 1"),
            ("move |value: i32| -> i32 { value + 1 }", "{ value + 1 }"),
            ("async move |value: i32| { value + 1 }", "{ value + 1 }"),
            ("|(x, y): (i32, i32)| x + y", "x + y"),
            ("|| /* café */ \"żółw\".len()", "\"żółw\".len()"),
        ] {
            let source = format!("// 🦀\r\nfn run() {{ let target = {expression}; }}\r\n");
            let facts = extract(&source).unwrap();
            assert!(
                facts.incomplete.is_none(),
                "{:?}: {source}",
                facts.incomplete
            );
            let closures: Vec<_> = facts
                .declarations
                .iter()
                .filter(|d| d.kind == "closure")
                .collect();
            assert_eq!(closures.len(), 1, "{source}");
            let closure = closures[0];
            assert_eq!(&source[closure.span.clone()], expression);
            assert_eq!(&source[closure.header.end..closure.span.end], body);
            assert_eq!(closure.owner, "run");
            assert!(!closure.written.contains("café"));
            assert!(!closure.uncertain);

            let ordinary = super::super::extract(&source, "2024").unwrap();
            assert!(!ordinary.errors);
            let named: Vec<_> = facts
                .declarations
                .iter()
                .filter(|d| d.kind != "closure")
                .map(|d| &d.declaration)
                .collect();
            assert_eq!(
                serde_json::to_value(named).unwrap(),
                serde_json::to_value(ordinary.declarations).unwrap()
            );
            assert_eq!(
                serde_json::to_value(facts.occurrences).unwrap(),
                serde_json::to_value(ordinary.occurrences).unwrap()
            );
        }
    }

    #[test]
    fn closure_owners_are_lexical_not_sibling_ordinals() {
        let source = "fn run() { let first = || || 1; let second = || || 2; }";
        let facts = extract(source).unwrap();
        assert!(facts.incomplete.is_none());
        let closures: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind == "closure")
            .collect();
        assert_eq!(closures.len(), 4);
        assert_eq!(closures[0].qualified, closures[2].qualified);
        assert_eq!(closures[1].qualified, closures[3].qualified);
        assert_eq!(closures[1].owner, closures[0].qualified);
        assert_eq!(&source[closures[1].span.clone()], "|| 1");
        assert_eq!(
            closures[1].written,
            format!("{} / {}", closures[0].written, closures[0].written)
        );
    }

    #[test]
    fn macro_tokens_are_not_closures_and_recovery_stays_incomplete() {
        let macros = extract("fn run() { make!(|value| value + 1); }").unwrap();
        assert!(macros.incomplete.is_none());
        assert!(!macros.declarations.iter().any(|d| d.kind == "closure"));
        let broken = extract("fn run() { let f = |value| { value + 1 };").unwrap();
        assert!(broken.incomplete.is_some());
        assert!(broken.declarations.iter().all(|d| d.uncertain));
    }
}
