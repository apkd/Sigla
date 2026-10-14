//! Transient Java implementation/call facts for history source ranges only.
//! This adapter does not participate in indexing, binding or named selectors.
use crate::{
    extract::{HistoricalDeclaration, HistoricalFacts},
    model::{Declaration, Occurrence, OccurrenceRole, WriteKind},
    repository::history::check,
};
use anyhow::{Context, Result, ensure};
use std::{
    ops::Range,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use tree_sitter::{Node, ParseOptions, Parser};

const SOURCE_BYTES: usize = 16 * 1024 * 1024;
const FACT_BYTES: usize = 16 * 1024 * 1024;
const WORK: usize = 262_144;
const DEPTH: usize = 256;

pub(super) fn path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("java"))
}

/// Include only line trivia, never adjacent code or comments.
pub(super) fn lines(source: &str, mut span: Range<usize>) -> Range<usize> {
    let start = source[..span.start].rfind('\n').map_or(0, |i| i + 1);
    if source[start..span.start].trim().is_empty() {
        span.start = start;
    }
    let end = source[span.end..]
        .find('\n')
        .map_or(source.len(), |i| span.end + i + 1);
    if source[span.end..end].trim().is_empty() {
        span.end = end;
    }
    span
}

#[derive(Default)]
struct Work {
    nodes: usize,
    bytes: usize,
}
impl Work {
    fn step(&mut self, cancel: &AtomicBool) -> Result<()> {
        check(cancel)?;
        ensure!(
            self.nodes < WORK,
            "historical Java traversal work limit reached"
        );
        self.nodes += 1;
        Ok(())
    }
    fn fact(&mut self, bytes: usize) -> Result<()> {
        ensure!(
            bytes <= FACT_BYTES.saturating_sub(self.bytes),
            "historical Java fact memory limit reached"
        );
        self.bytes += bytes;
        Ok(())
    }
}

/// Iterative traversal also bounds helper scans and deeply nested valid syntax.
fn walk(root: Node<'_>, mut visit: impl FnMut(Node<'_>) -> Result<bool>) -> Result<()> {
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        if visit(cursor.node())? && cursor.goto_first_child() {
            depth += 1;
            ensure!(depth <= DEPTH, "historical Java syntax depth limit reached");
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

fn annotation(node: Node<'_>) -> bool {
    matches!(node.kind(), "annotation" | "marker_annotation")
}

fn spelling(node: Node<'_>, source: &str, work: &mut Work, cancel: &AtomicBool) -> Result<String> {
    let mut text = String::new();
    walk(node, |node| {
        work.step(cancel)?;
        if node.is_extra() || annotation(node) {
            return Ok(false);
        }
        if node.child_count() == 0 {
            text.push_str(&source[node.byte_range()]);
        }
        Ok(true)
    })?;
    Ok(text)
}

fn parameters(
    node: Node<'_>,
    source: &str,
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Vec<String>> {
    let mut result = Vec::new();
    if let Some(parameters) = node.child_by_field_name("parameters") {
        for parameter in parameters.named_children(&mut parameters.walk()) {
            work.step(cancel)?;
            if !matches!(parameter.kind(), "formal_parameter" | "spread_parameter") {
                continue; // Comments and an explicit receiver are not arguments.
            }
            let ty = parameter
                .child_by_field_name("type")
                .or_else(|| {
                    parameter.named_children(&mut parameter.walk()).find(|n| {
                        !n.is_extra()
                            && !annotation(*n)
                            && !matches!(n.kind(), "modifiers" | "variable_declarator")
                    })
                })
                .context("incomplete historical Java parameter type")?;
            let mut ty = spelling(ty, source, work, cancel)?;
            if let Some(dimensions) = parameter.child_by_field_name("dimensions") {
                ty.push_str(&spelling(dimensions, source, work, cancel)?);
            }
            if parameter.kind() == "spread_parameter" {
                ty.push_str("[]");
            }
            result.push(ty);
        }
    }
    Ok(result)
}

fn declaration(
    node: Node<'_>,
    source: &str,
    package: &str,
    work: &mut Work,
    cancel: &AtomicBool,
) -> Result<Option<HistoricalDeclaration>> {
    let (Some(name), Some(body)) = (
        node.child_by_field_name("name"),
        node.child_by_field_name("body"),
    ) else {
        return Ok(None); // Abstract/native declarations have no implementation.
    };
    if node.has_error() {
        return Ok(None);
    }
    let mut owners = Vec::new();
    let mut parent = node.parent();
    while let Some(node) = parent {
        work.step(cancel)?;
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "enum_constant" => {
                if let Some(name) = node.child_by_field_name("name") {
                    owners.push(source[name.byte_range()].to_owned());
                }
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    owners.push(format!(
                        "{}({})",
                        &source[name.byte_range()],
                        parameters(node, source, work, cancel)?.join(",")
                    ));
                }
            }
            "object_creation_expression" => owners.push("<anonymous>".into()),
            "lambda_expression" => owners.push("<lambda>".into()),
            _ => {}
        }
        parent = node.parent();
    }
    if !package.is_empty() {
        owners.push(package.into());
    }
    owners.reverse();
    let owner = owners.join(".");
    let name_text = &source[name.byte_range()];

    // A signature-through-brace selection need not include preceding annotation
    // lines. Inline annotations remain on the signature line. Do not include docs.
    let mut signature = node.start_byte();
    'header: for child in node.children(&mut node.walk()) {
        work.step(cancel)?;
        if child.is_extra() || annotation(child) {
            continue;
        }
        if child.kind() == "modifiers" {
            for modifier in child.children(&mut child.walk()) {
                work.step(cancel)?;
                if !modifier.is_extra() && !annotation(modifier) {
                    signature = modifier.start_byte();
                    break 'header;
                }
            }
        } else {
            signature = child.start_byte();
            break;
        }
    }
    let line = source[..signature].rfind('\n').map_or(0, |i| i + 1);
    let span = node.start_byte().max(line)..node.end_byte();
    Ok(Some(HistoricalDeclaration {
        declaration: Declaration {
            name: name_text.into(),
            qualified: if owner.is_empty() {
                name_text.into()
            } else {
                format!("{owner}.{name_text}")
            },
            kind: if node.kind() == "constructor_declaration" {
                "constructor"
            } else {
                "method"
            }
            .into(),
            namespace: package.into(),
            owner,
            name_span: name.byte_range(),
            header: span.start..body.start_byte(),
            scope: span.clone(),
            span,
            parameters: parameters(node, source, work, cancel)?,
            ty: String::new(),
            access: String::new(),
            attributes: Vec::new(),
            modifiers: Vec::new(),
            bases: Vec::new(),
        },
        written: String::new(),
        guard: 1,
        uncertain: false,
    }))
}

pub(super) fn extract(source: &str, cancel: &AtomicBool) -> Result<HistoricalFacts> {
    check(cancel)?;
    ensure!(
        source.len() <= SOURCE_BYTES,
        "historical Java source byte limit reached"
    );
    // Java's pre-lexical Unicode translation is not implemented here. Never
    // certify boundaries from a different interpretation of escaped source.
    ensure!(
        !source.contains("\\u"),
        "historical Java Unicode escapes are unsupported"
    );
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into())?;
    let mut steps = 0;
    let mut progress = |_: &tree_sitter::ParseState| {
        steps += 1;
        cancel.load(Ordering::Relaxed) || steps >= WORK
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| source.as_bytes().get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .context("historical Java parse work limit or cancellation")?;
    check(cancel)?;
    let root = tree.root_node();
    let mut facts = HistoricalFacts {
        incomplete: root
            .has_error()
            .then(|| "historical Java parse recovery left candidate scope incomplete".into()),
        ..HistoricalFacts::default()
    };
    let mut work = Work::default();
    let mut package = String::new();
    walk(root, |node| {
        work.step(cancel)?;
        match node.kind() {
            "package_declaration" => {
                if let Some(name) = node
                    .named_children(&mut node.walk())
                    .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
                {
                    package = spelling(name, source, &mut work, cancel)?;
                }
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(mut declaration) =
                    declaration(node, source, &package, &mut work, cancel)?
                {
                    declaration.uncertain = facts.incomplete.is_some();
                    work.fact(
                        std::mem::size_of::<HistoricalDeclaration>() + declaration.heap_bytes(),
                    )?;
                    facts.declarations.push(declaration);
                }
            }
            "method_invocation" if facts.incomplete.is_none() => {
                if let (Some(name), Some(arguments)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("arguments"),
                ) {
                    let receiver = node
                        .child_by_field_name("object")
                        .map(|n| spelling(n, source, &mut work, cancel))
                        .transpose()?
                        .unwrap_or_default();
                    let name_text = source[name.byte_range()].to_owned();
                    work.fact(
                        std::mem::size_of::<Occurrence>()
                            + name_text.capacity()
                            + receiver.capacity(),
                    )?;
                    facts.occurrences.push(Occurrence {
                        role: OccurrenceRole::Value,
                        name: name_text,
                        span: name.byte_range(),
                        call: true,
                        construction: false,
                        write: WriteKind::None,
                        receiver,
                        arguments: Some(
                            arguments
                                .named_children(&mut arguments.walk())
                                .filter(|n| !n.is_extra())
                                .count(),
                        ),
                        opaque: false,
                    });
                }
            }
            _ => {}
        }
        Ok(true)
    })?;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{Region, Source, Sources},
        *,
    };
    use crate::{model::Language, repository::history::Store};
    use std::{process::Command, sync::Arc};

    #[test]
    fn java_facts_keep_types_calls_and_lexical_owners_private() {
        let text = r#"package demo;
class Outer {
    @Override
    public <T> T[] run(final java.util.List<T> values, int dims[], String... rest) {
        return helper(values, /* , { } */ dims);
    }
    Outer() {}
    abstract void missing();
    class Inner { int run(int x) { return x + 1; } }
    Object task() { return new Runnable() { public void run() { sink(); } }; }
}"#;
        let facts = extract(text, &AtomicBool::new(false)).unwrap();
        assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
        let method = facts
            .declarations
            .iter()
            .find(|d| d.qualified == "demo.Outer.run")
            .unwrap();
        assert_eq!(
            method.parameters,
            ["java.util.List<T>", "int[]", "String[]"]
        );
        assert!(!text[method.span.clone()].contains("@Override"));
        assert!(facts.declarations.iter().any(|d| d.kind == "constructor"));
        assert!(!facts.declarations.iter().any(|d| d.name == "missing"));
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.owner == "demo.Outer.Inner")
        );
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.owner == "demo.Outer.task().<anonymous>")
        );
        let call = facts
            .occurrences
            .iter()
            .find(|c| c.name == "helper")
            .unwrap();
        assert_eq!(call.arguments, Some(2));
        assert!(call.receiver.is_empty());
        assert_eq!(super::super::language("Example.JAVA"), Some(Language::Text));
        let ordinary = crate::extract::historical(text, Language::Text).unwrap();
        assert!(ordinary.declarations.is_empty() && ordinary.occurrences.is_empty());
    }

    #[test]
    fn only_exact_complete_implementation_lines_are_enriched() {
        for newline in ["\n", "\r\n"] {
            let text = "class C {\n    @Override\n    int run() {\n        return work(\"é } {\"); // }\n    }  \n}\n".replace('\n', newline);
            let facts = extract(&text, &AtomicBool::new(false)).unwrap();
            assert!(facts.incomplete.is_none());
            let source = Source {
                text: text.into(),
                calls: Vec::new(),
                declarations: facts.declarations,
                presence: facts.presence,
                incomplete: None,
                cost: 0,
            };
            let span = super::super::line_range(&source.text, 3, 5).unwrap();
            let region = Region::selected("C.java", "blob", span.clone(), &source);
            assert_eq!(region.kind, "method");
            assert_eq!(region.span, span);
            assert_eq!(
                &source.text[region.range(&source)],
                &source.text[span.clone()]
            );
            let body = &source.text[region.body_range(&source)];
            assert!(body.starts_with(newline) && body.ends_with("    "));
            for (first, last) in [(4, 4), (2, 5), (1, 6), (3, 4)] {
                let span = super::super::line_range(&source.text, first, last).unwrap();
                let region = Region::selected("C.java", "blob", span.clone(), &source);
                assert_eq!(region.kind, "text");
                assert_eq!(region.span, span);
            }
            assert_eq!(
                Region::selected("C.txt", "blob", span, &source).kind,
                "text"
            );
        }
        let text = "class C { int run() { return 1; } int next() { return 2; } }";
        let facts = extract(text, &AtomicBool::new(false)).unwrap();
        for method in facts.declarations {
            let span = lines(text, method.span.clone());
            assert_eq!(span, method.span); // Neither adjacent method nor owner is included.
        }
        let text = "class C {\n    int run() { return 1; }";
        let span = text.find("int run").unwrap()..text.len();
        assert_eq!(&text[lines(text, span)], "    int run() { return 1; }");
    }

    #[test]
    fn malformed_escaped_cancelled_and_bounded_java_is_not_complete_evidence() {
        let cancel = AtomicBool::new(false);
        let facts = extract("class C { void good() { work(); } void broken( }", &cancel).unwrap();
        assert!(facts.incomplete.is_some());
        assert!(facts.declarations.iter().all(|d| d.uncertain));
        assert!(facts.occurrences.is_empty());
        assert!(extract(r"class C { int value = \u0031; }", &cancel).is_err());
        assert!(extract("class C {}", &AtomicBool::new(true)).is_err());
        assert!(extract(&" ".repeat(SOURCE_BYTES + 1), &cancel).is_err());
        let deep = format!(
            "class C {{ int run() {{ return {}1{}; }} }}",
            "(".repeat(DEPTH + 1),
            ")".repeat(DEPTH + 1)
        );
        assert!(
            extract(&deep, &cancel)
                .err()
                .unwrap()
                .to_string()
                .contains("depth limit")
        );
    }

    #[test]
    fn identical_text_blobs_do_not_share_java_policy_in_either_order() {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet", "--bare"]);
        let mut store = Store::local(directory.path(), Arc::new(AtomicBool::new(false))).unwrap();
        for text in [
            "class C { int run() { return 1; } }",
            "class C { void broken( }",
        ] {
            std::fs::write(directory.path().join("input"), text).unwrap();
            let blob = git(&["hash-object", "-w", "input"]);
            for paths in [["C.txt", "C.java"], ["C.java", "C.txt"]] {
                let sources = Sources::default();
                for path in paths {
                    sources.get(&mut store, path, &blob).unwrap();
                }
                let plain = sources.get(&mut store, "C.txt", &blob).unwrap();
                assert!(plain.declarations.is_empty() && plain.incomplete.is_none());
                let java = sources.get(&mut store, "C.JAVA", &blob).unwrap();
                assert_eq!(java.incomplete.is_some(), text.contains("broken"));
                if java.incomplete.is_none() {
                    assert_eq!(java.declarations.len(), 1);
                }
            }
        }
    }
}
