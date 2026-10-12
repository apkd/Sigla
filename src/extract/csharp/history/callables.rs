//! Additional positional regions for history, isolated from snapshot extraction.
use super::super::{child, field, text};
use crate::{extract::callable_region, model::Facts};
use std::ops::Range;
use tree_sitter::Node;

pub(super) fn declaration(node: Node<'_>, source: &str, facts: &mut Facts) {
    let (kind, name, name_node) = match node.kind() {
        "lambda_expression" => ("closure", "<lambda>", None),
        "anonymous_method_expression" => ("closure", "<anonymous method>", None),
        "accessor_declaration" => {
            let Some(name) = node.child_by_field_name("name") else {
                return;
            };
            ("accessor", text(name, source), Some(name))
        }
        _ => return,
    };
    let body = node
        .child_by_field_name("body")
        .or_else(|| child(node, "block"))
        .or_else(|| child(node, "arrow_expression_clause"));
    if kind == "closure" && body.is_none() {
        return; // Recovery cannot invent an anonymous body.
    }
    let span = node.byte_range();
    let owner = facts
        .declarations
        .iter()
        .rev()
        .find(|d| !d.local() && d.span.start <= span.start && span.end <= d.span.end);
    let mut region = callable_region(
        name,
        kind,
        span.clone(),
        body.map_or(span.end, |n| n.start_byte()),
        owner,
        ".",
    );
    if let Some(name) = name_node {
        region.name_span = name.byte_range();
        // Accessors of overloaded indexers retain their owner's signature.
        region.parameters = owner.map_or_else(Vec::new, |d| d.parameters.clone());
    }
    if let Some(parameters) = node.child_by_field_name("parameters") {
        region.parameters = if parameters.kind() == "implicit_parameter" {
            vec![String::new()]
        } else {
            parameters
                .named_children(&mut parameters.walk())
                .filter(|n| matches!(n.kind(), "parameter" | "implicit_parameter"))
                .map(|n| field(n, "type", source))
                .collect()
        };
    }
    facts.declarations.push(region);
}

/// A field's lexical owner header is its shared prefix and this declarator's
/// name, not any initializer (including initializers of earlier siblings).
pub(super) fn field_header(node: Node<'_>) -> Option<[Range<usize>; 2]> {
    if node.kind() != "variable_declarator" {
        return None;
    }
    let variables = node.parent()?;
    let member = variables.parent()?;
    if !matches!(
        member.kind(),
        "field_declaration" | "event_field_declaration"
    ) {
        return None;
    }
    let first = variables
        .named_children(&mut variables.walk())
        .find(|n| n.kind() == "variable_declarator")?;
    let name = node.child_by_field_name("name")?;
    Some([member.start_byte()..first.start_byte(), name.byte_range()])
}
