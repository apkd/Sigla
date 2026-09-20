use crate::{model::*, query::normalize_name};
use anyhow::Result;
use tree_sitter::Node;

fn text<'a>(n: Node<'_>, source: &'a str) -> &'a str {
    &source[n.byte_range()]
}
fn child<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut c = n.walk();
    n.named_children(&mut c).find(|n| n.kind() == kind)
}
fn field(n: Node<'_>, name: &str, source: &str) -> String {
    n.child_by_field_name(name)
        .map(|n| text(n, source).to_owned())
        .unwrap_or_default()
}
fn ancestor<'a>(mut n: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    while let Some(p) = n.parent() {
        if kinds.contains(&p.kind()) {
            return Some(p);
        }
        n = p;
    }
    None
}
fn modifiers(n: Node<'_>, s: &str) -> Vec<String> {
    let mut c = n.walk();
    n.named_children(&mut c)
        .filter(|n| n.kind() == "modifier")
        .map(|n| text(n, s).into())
        .collect()
}

pub fn extract(source: &str, defines: &[String]) -> Result<Facts> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into())?;
    let analysis = super::preprocess::active_source(source, defines)?;
    let tree = parser
        .parse(&analysis, None)
        .ok_or_else(|| anyhow::anyhow!("C# parsing cancelled"))?;
    let mut facts = Facts {
        csharp: Some(crate::csharp::syntax::FileSyntax::default()),
        errors: tree.root_node().has_error(),
        ..Facts::default()
    };
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let before = facts.declarations.len();
        declaration(node, &analysis, &mut facts);
        if facts.declarations.len() != before {
            let header = crate::csharp::lower::header(node, &analysis, &facts.declarations);
            facts.csharp.as_mut().unwrap().headers.push(header);
        }
        if node.kind() == "using_directive" {
            facts
                .csharp
                .as_mut()
                .unwrap()
                .imports
                .push(crate::csharp::lower::import(node, &analysis));
            let written = text(node, &analysis).trim_end_matches(';').trim();
            let global = written.starts_with("global ");
            let value = written
                .trim_start_matches("global ")
                .trim_start_matches("using ")
                .trim_start_matches("static ")
                .trim();
            let (alias, path) = value
                .split_once('=')
                .map_or(("", value), |(a, p)| (a.trim(), p.trim()));
            let scope = ancestor(node, &["namespace_declaration"])
                .map_or(0..source.len(), |n| n.byte_range());
            facts.imports.push(Import {
                alias: normalize_name(alias),
                path: path.trim_start_matches("global::").into(),
                scope,
                global,
            });
        }
        if node.kind() == "identifier" || node.kind() == "predefined_type" {
            occurrence(node, &analysis, &mut facts);
        }
        if node.kind() == "element_access_expression"
            && let Some(arguments) = node.child_by_field_name("subscript")
        {
            facts.occurrences.push(Occurrence {
                name: "Item".into(),
                span: arguments.start_byte()..arguments.start_byte() + 1,
                call: true,
                construction: false,
                write: node.parent().is_some_and(|p| {
                    p.kind() == "assignment_expression"
                        && p.child_by_field_name("left") == Some(node)
                }),
                receiver: field(node, "expression", &analysis),
                arguments: Some(arguments.named_child_count()),
                opaque: node.has_error(),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                let names: std::collections::HashSet<_> = facts
                    .declarations
                    .iter()
                    .map(|d| (d.name_span.start, d.name_span.end))
                    .collect();
                facts
                    .occurrences
                    .retain(|o| !names.contains(&(o.span.start, o.span.end)));
                crate::csharp::lower::bodies(
                    tree.root_node(),
                    &analysis,
                    facts.csharp.as_mut().unwrap(),
                );
                for expression in &facts.csharp.as_ref().unwrap().expressions {
                    if let crate::csharp::syntax::ExpressionKind::ImplicitCall { name, .. } =
                        &expression.kind
                    {
                        facts.occurrences.push(Occurrence {
                            name: name.clone(),
                            span: expression.span.clone(),
                            call: !name.starts_with("op_")
                                && name != "@operator"
                                && name != "Current",
                            construction: false,
                            write: false,
                            receiver: String::new(),
                            arguments: Some(0),
                            opaque: true,
                        });
                    }
                }
                return Ok(facts);
            }
        }
    }
}

fn declaration(n: Node<'_>, s: &str, facts: &mut Facts) {
    let kind = match n.kind() {
        "class_declaration" => "class",
        "struct_declaration" => "struct",
        "record_declaration" => "class",
        "interface_declaration" => "interface",
        "enum_declaration" => "enum",
        "delegate_declaration" => "delegate",
        "method_declaration" | "local_function_statement" => "method",
        "constructor_declaration" => "constructor",
        "property_declaration" | "indexer_declaration" => "property",
        "event_declaration" => "event",
        "operator_declaration" | "conversion_operator_declaration" => "operator",
        "namespace_declaration" | "file_scoped_namespace_declaration" => "namespace",
        "enum_member_declaration" => "variant",
        "parameter" | "implicit_parameter" => "parameter",
        "declaration_expression" | "declaration_pattern" => "local",
        "variable_declarator" => {
            if ancestor(n, &["event_field_declaration"]).is_some() {
                "event"
            } else if ancestor(n, &["field_declaration"]).is_some() {
                if ancestor(n, &["field_declaration"])
                    .is_some_and(|p| modifiers(p, s).iter().any(|m| m == "const"))
                {
                    "const"
                } else {
                    "field"
                }
            } else {
                "local"
            }
        }
        _ => return,
    };
    let name_node = n
        .child_by_field_name("name")
        .or_else(|| n.child_by_field_name("operator"))
        .or_else(|| {
            let mut cursor = n.walk();
            (n.kind() == "indexer_declaration")
                .then(|| n.children(&mut cursor).find(|n| n.kind() == "this"))
                .flatten()
        });
    let Some(name_node) = name_node else { return };
    let name = if n.kind() == "indexer_declaration" {
        "Item".into()
    } else {
        normalize_name(text(name_node, s))
    };
    if name.is_empty() {
        return;
    }
    let span = n.byte_range();
    let owner = facts.declarations.iter().rev().find(|d| {
        !d.local() && d.span.start <= span.start && d.span.end >= span.end && d.kind != "namespace"
    });
    let file_ns = facts.declarations.iter().find(|d| {
        d.kind == "namespace"
            && s[d.span.clone()].trim_start().starts_with("namespace ")
            && !s[d.span.clone()].contains('{')
    });
    let ns = facts
        .declarations
        .iter()
        .rev()
        .find(|d| d.kind == "namespace" && d.span.start <= span.start && d.span.end >= span.end)
        .or(file_ns);
    let namespace = ns.map(|d| d.qualified.clone()).unwrap_or_default();
    let owner_name = owner
        .map(|d| d.qualified.clone())
        .unwrap_or_else(|| namespace.clone());
    let qualified = if owner_name.is_empty() {
        name.clone()
    } else {
        format!("{owner_name}.{name}")
    };
    let outer = if matches!(kind, "field" | "event" | "const") && n.kind() == "variable_declarator"
    {
        ancestor(n, &["field_declaration", "event_field_declaration"]).unwrap_or(n)
    } else {
        n
    };
    let mods = modifiers(outer, s);
    let access = mods
        .iter()
        .filter(|m| matches!(m.as_str(), "public" | "private" | "protected" | "internal"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let access = if access.is_empty() {
        if kind == "namespace" || owner.is_some_and(|d| d.kind == "interface") {
            "public"
        } else if owner.is_some() {
            "private"
        } else {
            "internal"
        }
        .into()
    } else {
        access
    };
    let mut attributes = Vec::new();
    let mut c = outer.walk();
    for a in outer
        .named_children(&mut c)
        .filter(|n| n.kind() == "attribute_list")
    {
        let mut ac = a.walk();
        for a in a
            .named_children(&mut ac)
            .filter(|n| n.kind() == "attribute")
        {
            attributes.push(field(a, "name", s));
        }
    }
    let body = n
        .child_by_field_name("body")
        .or_else(|| child(n, "accessor_list"));
    let header = outer.start_byte()..body.map_or(outer.end_byte(), |b| b.start_byte());
    let mut ty = field(n, "type", s);
    if ty.is_empty() {
        ty = field(n, "returns", s);
    }
    if n.kind() == "variable_declarator"
        && let Some(v) = n.parent()
    {
        ty = field(v, "type", s);
    }
    if ty == "var"
        && let Some(init) = child(n, "object_creation_expression")
    {
        ty = field(init, "type", s);
    }
    let mut parameters = Vec::new();
    if let Some(p) = n.child_by_field_name("parameters") {
        let mut c = p.walk();
        for param in p.named_children(&mut c).filter(|n| n.kind() == "parameter") {
            let mut t = modifiers(param, s)
                .into_iter()
                .filter(|m| matches!(m.as_str(), "ref" | "out" | "in" | "params" | "this"))
                .collect::<Vec<_>>();
            t.push(field(param, "type", s));
            parameters.push(t.join(" "));
        }
    }
    let bases = child(n, "base_list")
        .map(|b| {
            let mut c = b.walk();
            b.named_children(&mut c)
                .map(|n| text(n, s).to_owned())
                .collect()
        })
        .unwrap_or_default();
    let scope = if kind == "parameter" {
        owner.map(|d| d.span.clone()).unwrap_or(0..s.len())
    } else if kind == "local" || n.kind() == "local_function_statement" {
        ancestor(
            n,
            &[
                "block",
                "for_statement",
                "foreach_statement",
                "switch_section",
            ],
        )
        .map_or(span.clone(), |n| n.byte_range())
    } else {
        span.clone()
    };
    facts.declarations.push(Declaration {
        name,
        qualified,
        kind: kind.into(),
        namespace,
        owner: owner_name,
        name_span: name_node.byte_range(),
        span,
        header,
        scope,
        parameters,
        ty,
        access,
        attributes,
        modifiers: mods,
        bases,
    });
}

fn occurrence(n: Node<'_>, s: &str, facts: &mut Facts) {
    let mut current = n;
    let mut receiver = String::new();
    if let Some(p) = current.parent()
        && p.kind() == "generic_name"
    {
        current = p;
    }
    if let Some(binding) = current
        .parent()
        .filter(|p| p.kind() == "member_binding_expression")
        && let Some(conditional) = binding
            .parent()
            .filter(|p| p.kind() == "conditional_access_expression")
    {
        receiver = field(conditional, "condition", s);
        current = conditional;
    }
    if let Some(p) = current.parent()
        && matches!(p.kind(), "member_access_expression" | "qualified_name")
        && p.child_by_field_name("name")
            .or_else(|| p.child_by_field_name("right"))
            .is_some_and(|name| name.byte_range().contains(&n.start_byte()))
    {
        receiver = field(p, "expression", s);
        if receiver.is_empty() {
            receiver = field(p, "qualifier", s);
        }
        current = p;
    }
    let mut call = false;
    let mut construction = false;
    let mut arguments = None;
    let mut write = false;
    if let Some(p) = current.parent() {
        if p.kind() == "invocation_expression"
            && p.child_by_field_name("function") == Some(current)
            && text(current, s) != "nameof"
        {
            call = true;
            arguments = p
                .child_by_field_name("arguments")
                .map(|a| a.named_child_count());
        }
        if p.kind() == "object_creation_expression"
            && p.child_by_field_name("type") == Some(current)
        {
            call = true;
            construction = true;
            arguments = p
                .child_by_field_name("arguments")
                .map(|a| a.named_child_count());
        }
        if p.kind() == "assignment_expression" && p.child_by_field_name("left") == Some(current) {
            write = true;
        }
        if matches!(
            p.kind(),
            "postfix_unary_expression" | "prefix_unary_expression"
        ) && (text(p, s).contains("++") || text(p, s).contains("--"))
        {
            write = true;
        }
    }
    facts.occurrences.push(Occurrence {
        name: normalize_name(text(n, s)),
        span: n.byte_range(),
        call,
        construction,
        write,
        receiver,
        arguments,
        opaque: n.has_error(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarations_and_explicit_roles() {
        let s = "namespace Game; class Parser { int a,b; public int Parse(int x) { a++; b = x; return Parse(x); } }";
        let f = extract(s, &[]).unwrap();
        assert!(
            f.declarations
                .iter()
                .any(|d| d.qualified == "Game.Parser.Parse")
        );
        assert_eq!(
            f.declarations.iter().filter(|d| d.kind == "field").count(),
            2
        );
        assert_eq!(f.occurrences.iter().filter(|o| o.call).count(), 1);
        assert_eq!(f.occurrences.iter().filter(|o| o.write).count(), 2);
        assert!(
            !f.occurrences
                .iter()
                .any(|o| f.declarations.iter().any(|d| d.name_span == o.span))
        );
    }
}
