//! Committed-source C# syntax, without binding or a chosen symbol environment.
//!
//! Preprocessing yields guarded original byte ranges. Grammar-list fragments
//! explore their own shapes together with lexical-owner shells, never products
//! of sibling declarations. Concrete parses validate every proposed syntax seam.
mod callables;
pub(super) mod conditions;
mod contexts;
mod document;
mod lexical;

use super::{DeclarationScopes, declaration, occurrence};
use crate::{
    extract::{HistoricalDeclaration, HistoricalFacts},
    model::Facts,
};
use anyhow::{Result, ensure};
use conditions::{Conditions, FALSE, Guard, TRUE};
use contexts::Context;
use document::Document;
use lexical::newline;
use std::{collections::HashMap, ops::Range};

/// Per declaration context, including its lexical owners, not per source file.
const PROJECTIONS: usize = 64;
const PARSE_BYTES: usize = 64 * 1024 * 1024;
const FACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Work {
    bytes: usize,
    parses: usize,
    contexts: usize,
    max_alternatives: usize,
    fallbacks: usize,
    fact_heap: usize,
}
impl Work {
    fn parse(&mut self, parser: &mut tree_sitter::Parser, text: &str) -> Result<tree_sitter::Tree> {
        ensure!(
            text.len() <= PARSE_BYTES.saturating_sub(self.bytes),
            "historical C# parse byte limit reached"
        );
        self.bytes += text.len();
        self.parses += 1;
        parser
            .parse(text, None)
            .ok_or_else(|| anyhow::anyhow!("historical C# parsing cancelled"))
    }
}

pub(super) fn extract(source: &str) -> Result<HistoricalFacts> {
    run(source, &mut Work::default())
}

fn run(source: &str, work: &mut Work) -> Result<HistoricalFacts> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into())?;
    let mut conditions = Conditions::default();
    let mut result = HistoricalFacts::default();
    if let Err(error) = explore(source, &mut parser, &mut conditions, &mut result, work) {
        result.incomplete.get_or_insert_with(|| error.to_string());
    }
    if let Err(error) = normalize(&mut result.declarations, &mut conditions) {
        result.incomplete.get_or_insert_with(|| error.to_string());
    }
    result.presence = conditions.finish();
    if fact_bytes(&result) > FACT_BYTES {
        result.declarations = Vec::new();
        result.occurrences = Vec::new();
        result.incomplete = Some("historical C# fact memory limit reached".into());
    }
    if result.incomplete.is_some() {
        for declaration in &mut result.declarations {
            declaration.uncertain = true;
        }
        result.occurrences = Vec::new();
    }
    Ok(result)
}

fn explore(
    source: &str,
    parser: &mut tree_sitter::Parser,
    conditions: &mut Conditions,
    result: &mut HistoricalFacts,
    work: &mut Work,
) -> Result<()> {
    let document = Document::new(source, conditions)?;
    let whole = Context::whole(source.len());
    let values = conditions.witness(TRUE, false);
    let seed = document.sample(conditions, &values)?;
    let tree = work.parse(parser, &seed)?;
    let mut guards = document.guards();
    if guards.iter().all(|&guard| guard == TRUE) {
        *result = parse(&tree, &seed, TRUE);
        return Ok(());
    }
    let mut inventory = contexts::Inventory::default();
    let mut recovered = tree.root_node().has_error();
    inventory.record(&tree)?;
    guards.retain(|&guard| !conditions.value(guard, &values));
    while !recovered && let Some(&guard) = guards.first() {
        let values = conditions.witness(guard, true);
        let text = document.sample(conditions, &values)?;
        let tree = work.parse(parser, &text)?;
        recovered = tree.root_node().has_error();
        inventory.record(&tree)?;
        guards.retain(|&guard| !conditions.value(guard, &values));
    }
    if !recovered {
        let mut factored = true;
        for context in inventory.plan(source.len())? {
            if !explore_context(&document, &context, true, parser, conditions, result, work)? {
                // A speculative seam is not a source error. Discard partial
                // compositional facts and check the original source instead.
                *result = HistoricalFacts::default();
                work.fact_heap = 0;
                work.fallbacks += 1;
                factored = false;
                break;
            }
        }
        if factored {
            return Ok(());
        }
    }
    explore_context(&document, &whole, false, parser, conditions, result, work)?;
    Ok(())
}

fn explore_context(
    document: &Document<'_>,
    context: &Context,
    validate: bool,
    parser: &mut tree_sitter::Parser,
    conditions: &mut Conditions,
    result: &mut HistoricalFacts,
    work: &mut Work,
) -> Result<bool> {
    work.contexts += 1;
    let mut remaining = TRUE;
    let mut count = 0;
    while remaining != FALSE && count < PROJECTIONS {
        let values = conditions.witness(remaining, !count.is_multiple_of(2));
        let projection = document.project(conditions, &values, &context.ranges)?;
        let tree = work.parse(parser, &projection.text)?;
        if validate && (tree.root_node().has_error() || !context.valid(&tree)) {
            return Ok(false);
        }
        let parsed = parse(&tree, &projection.text, projection.guard);
        if parsed.incomplete.is_some() {
            result.incomplete = parsed.incomplete;
        }
        work.fact_heap += parsed
            .declarations
            .iter()
            .map(HistoricalDeclaration::heap_bytes)
            .sum::<usize>();
        result.declarations.extend(parsed.declarations);
        // Even one local shape says nothing about calls in sibling choices.
        // Calls survive only the single-whole-source-shape fast path above.
        count += 1;
        work.max_alternatives = work.max_alternatives.max(count);
        if work.fact_heap
            + result.declarations.capacity() * std::mem::size_of::<HistoricalDeclaration>()
            > FACT_BYTES
        {
            result.declarations = Vec::new();
            anyhow::bail!("historical C# fact memory limit reached");
        }
        let rest = conditions.not(projection.guard)?;
        remaining = conditions.and(remaining, rest)?;
    }
    ensure!(
        remaining == FALSE,
        "historical C# alternative exploration limit reached in a declaration context"
    );
    Ok(true)
}

struct Projection {
    text: String,
    /// All environments with this context's text; unrelated choices are free.
    guard: Guard,
}
fn blank(source: &str, bytes: &mut [u8], range: Range<usize>) {
    for (offset, c) in source[range.clone()].char_indices() {
        if !newline(c) {
            bytes[range.start + offset..range.start + offset + c.len_utf8()].fill(b' ');
        }
    }
}

fn parse(tree: &tree_sitter::Tree, analysis: &str, guard: Guard) -> HistoricalFacts {
    let mut facts = Facts::default();
    let recovered = tree.root_node().has_error();
    let mut tokens = Vec::new();
    let mut field_headers = HashMap::new();
    let mut cursor = tree.walk();
    let mut parents = Vec::new();
    let mut scopes = DeclarationScopes::default();
    'walk: loop {
        let node = cursor.node();
        let field_header = callables::field_header(node);
        // A local inside a field-initializer lambda is not itself a field.
        if node.kind() != "variable_declarator" || field_header.is_some() {
            declaration(node, &parents, analysis, &mut facts, &mut scopes);
        }
        if let Some(header) = field_header {
            field_headers.insert(node.start_byte(), header);
        }
        let before = facts.declarations.len();
        callables::declaration(node, analysis, &mut facts);
        if facts.declarations.len() != before {
            scopes.push(before, &facts.declarations[before], analysis);
        }
        if node.kind() == "identifier" || node.kind() == "predefined_type" {
            occurrence(node, analysis, &mut facts);
            if facts
                .occurrences
                .last()
                .is_some_and(|o| !o.call || o.opaque)
            {
                facts.occurrences.pop();
            }
        }
        if node.child_count() == 0 && node.kind() != "comment" && !node.is_missing() {
            tokens.push(node.byte_range());
        }
        if node.kind() != "comment" && cursor.goto_first_child() {
            parents.push(node);
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
            parents.pop();
        }
    }
    let mut owners = HashMap::<String, String>::new();
    let mut declarations = Vec::new();
    for declaration in facts.declarations {
        if declaration.local() || matches!(declaration.kind.as_str(), "scope" | "impl" | "lambda") {
            continue;
        }
        // Written owner headers matter too: Worker<T> and Worker<T,U> must not
        // lend an invented common identity to a shared body. These are lexical
        // contexts, not bindings, and initializer bodies are not owner headers.
        let owner = owners.get(&declaration.owner).map(String::as_str);
        let written = written_header(
            analysis,
            &tokens,
            std::slice::from_ref(&declaration.header),
            owner,
        );
        let owned = field_headers.get(&declaration.span.start).map_or_else(
            || written.clone(),
            |ranges| written_header(analysis, &tokens, ranges, owner),
        );
        owners.insert(declaration.qualified.clone(), owned);
        declarations.push(HistoricalDeclaration {
            declaration,
            written,
            guard,
            uncertain: recovered,
        });
    }
    HistoricalFacts {
        declarations,
        occurrences: facts.occurrences,
        incomplete: recovered
            .then(|| "historical parse recovery left candidate scope incomplete".into()),
        ..HistoricalFacts::default()
    }
}

fn written_header(
    analysis: &str,
    tokens: &[Range<usize>],
    ranges: &[Range<usize>],
    owner: Option<&str>,
) -> String {
    let mut written = owner.map_or_else(String::new, |owner| format!("{owner} /"));
    for range in ranges {
        let start = tokens.partition_point(|r| r.end <= range.start);
        for token in tokens[start..].iter().take_while(|r| r.start < range.end) {
            if !written.is_empty() {
                written.push(' ');
            }
            written.push_str(&analysis[token.clone()]);
        }
    }
    written
}

fn normalize(
    declarations: &mut Vec<HistoricalDeclaration>,
    conditions: &mut Conditions,
) -> Result<()> {
    fn compare(a: &HistoricalDeclaration, b: &HistoricalDeclaration) -> std::cmp::Ordering {
        a.span
            .start
            .cmp(&b.span.start)
            .then_with(|| a.span.end.cmp(&b.span.end))
            .then_with(|| a.header.start.cmp(&b.header.start))
            .then_with(|| a.header.end.cmp(&b.header.end))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.qualified.cmp(&b.qualified))
            .then_with(|| a.parameters.cmp(&b.parameters))
            .then_with(|| a.written.cmp(&b.written))
    }
    declarations.sort_by(compare);
    let mut status = Ok(());
    declarations.dedup_by(|a, b| {
        if compare(a, b).is_eq() {
            b.uncertain |= a.uncertain;
            match conditions.or(b.guard, a.guard) {
                Ok(guard) => b.guard = guard,
                Err(error) => status = Err(error),
            }
            true
        } else {
            false
        }
    });
    status
}
fn fact_bytes(facts: &HistoricalFacts) -> usize {
    facts.presence.heap_bytes()
        + facts.declarations.capacity() * std::mem::size_of::<HistoricalDeclaration>()
        + facts
            .declarations
            .iter()
            .map(HistoricalDeclaration::heap_bytes)
            .sum::<usize>()
        + facts.occurrences.capacity() * std::mem::size_of::<crate::model::Occurrence>()
        + facts
            .occurrences
            .iter()
            .map(|o| o.name.capacity() + o.receiver.capacity())
            .sum::<usize>()
}

#[cfg(test)]
mod tests;
