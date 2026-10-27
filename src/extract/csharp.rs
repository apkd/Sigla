use crate::{model::*, query::normalize_name};
use anyhow::Result;
use std::ops::Range;
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
fn ancestor<'a>(parents: &[Node<'a>], kinds: &[&str]) -> Option<Node<'a>> {
    parents
        .iter()
        .rev()
        .copied()
        .find(|p| kinds.contains(&p.kind()))
}
fn modifiers(n: Node<'_>, s: &str) -> Vec<String> {
    let mut c = n.walk();
    n.named_children(&mut c)
        .filter(|n| n.kind() == "modifier")
        .map(|n| text(n, s).into())
        .collect()
}

/// Declaration spans arrive in tree preorder: ordinary spans nest or are disjoint.
/// Keep only enclosing declarations, so each index is pushed and popped once.
/// These indices are extraction state, not additional persisted relationships.
#[derive(Default)]
struct DeclarationScopes {
    owners: Vec<usize>,
    namespaces: Vec<usize>,
    file_namespace: Option<usize>,
    // Inserted before traversal, but may start after earlier types/namespaces and
    // end before later ones. It must not be popped like an ordinary declaration.
    top_level: Option<usize>,
}

impl DeclarationScopes {
    fn enclosing(
        &mut self,
        span: &Range<usize>,
        declarations: &[Declaration],
    ) -> (Option<usize>, Option<usize>) {
        let contains = |index: &usize| {
            let parent = &declarations[*index].span;
            parent.start <= span.start && parent.end >= span.end
        };
        while self.owners.last().is_some_and(|i| !contains(i)) {
            self.owners.pop();
        }
        while self.namespaces.last().is_some_and(|i| !contains(i)) {
            self.namespaces.pop();
        }
        (
            self.owners
                .last()
                .copied()
                .or_else(|| self.top_level.filter(contains)),
            self.namespaces.last().copied().or(self.file_namespace),
        )
    }

    fn push(&mut self, index: usize, declaration: &Declaration, source: &str) {
        if declaration.kind == "namespace" {
            self.namespaces.push(index);
            if self.file_namespace.is_none() {
                // Preserve the existing first-braceless-namespace fallback,
                // including its treatment of comments and recovered syntax.
                let written = &source[declaration.span.clone()];
                if written.trim_start().starts_with("namespace ") && !written.contains('{') {
                    self.file_namespace = Some(index);
                }
            }
        } else if !declaration.local() {
            // Local functions are method declarations and own declarations;
            // Header::local is deliberately not the eligibility predicate.
            self.owners.push(index);
        }
    }
}

pub fn extract(source: &str, defines: &[String]) -> Result<Facts> {
    let analysis = super::preprocess::active_source(source, defines)?;
    extract_analysis(&analysis)
}

mod history;
pub(crate) use history::conditions::Presence as HistoricalPresence;

pub(super) fn historical(source: &str) -> Result<super::HistoricalFacts> {
    history::extract(source)
}

fn extract_analysis(analysis: &str) -> Result<Facts> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c_sharp::LANGUAGE.into())?;
    let tree = parser
        .parse(analysis, None)
        .ok_or_else(|| anyhow::anyhow!("C# parsing cancelled"))?;
    let mut facts = Facts {
        csharp: Some(crate::csharp::syntax::FileSyntax::default()),
        errors: tree.root_node().has_error(),
        ..Facts::default()
    };
    let mut scopes = DeclarationScopes::default();
    let root = tree.root_node();
    let globals: Vec<_> = root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "global_statement")
        .collect();
    if let (Some(first), Some(last)) = (globals.first(), globals.last()) {
        let span = first.start_byte()..last.end_byte();
        // An internal lexical context for top-level statements, not a searchable symbol.
        facts.declarations.push(Declaration {
            name: String::new(),
            qualified: String::new(),
            kind: "scope".into(),
            namespace: String::new(),
            owner: String::new(),
            name_span: span.start..span.start,
            header: span.start..span.start,
            scope: span.clone(),
            span,
            parameters: vec![],
            ty: String::new(),
            access: "internal".into(),
            attributes: vec![],
            modifiers: vec![],
            bases: vec![],
        });
        facts
            .csharp
            .as_mut()
            .unwrap()
            .headers
            .push(crate::csharp::lower::header(
                root,
                analysis,
                &facts.declarations,
                None,
            ));
        scopes.top_level = Some(facts.declarations.len() - 1);
    }
    let mut cursor = tree.walk();
    let mut parents = Vec::new();
    loop {
        let node = cursor.node();
        declaration(node, &parents, analysis, &mut facts, &mut scopes);
        if node.kind() == "using_directive" {
            facts
                .csharp
                .as_mut()
                .unwrap()
                .imports
                .push(crate::csharp::lower::import(node, analysis));
            let written = text(node, analysis).trim_end_matches(';').trim();
            let global = written.starts_with("global ");
            let value = written
                .trim_start_matches("global ")
                .trim_start_matches("using ")
                .trim_start_matches("static ")
                .trim();
            let (alias, path) = value
                .split_once('=')
                .map_or(("", value), |(a, p)| (a.trim(), p.trim()));
            let scope = ancestor(&parents, &["namespace_declaration"])
                .map_or(0..analysis.len(), |n| n.byte_range());
            facts.imports.push(Import {
                alias: normalize_name(alias),
                path: path.trim_start_matches("global::").into(),
                scope,
                global,
            });
        }
        if node.kind() == "identifier" || node.kind() == "predefined_type" {
            occurrence(node, &parents, analysis, &mut facts);
        }
        if node.kind() == "element_access_expression"
            && let Some(arguments) = node.child_by_field_name("subscript")
        {
            facts.occurrences.push(Occurrence {
                role: crate::model::OccurrenceRole::Value,
                name: "Item".into(),
                span: arguments.start_byte()..arguments.start_byte() + 1,
                call: true,
                construction: false,
                write: if parents.last().is_some_and(|p| {
                    p.kind() == "assignment_expression"
                        && p.child_by_field_name("left") == Some(node)
                }) {
                    crate::model::WriteKind::Direct
                } else {
                    crate::model::WriteKind::None
                },
                receiver: field(node, "expression", analysis),
                arguments: Some(arguments.named_child_count()),
                opaque: node.has_error(),
            });
        }
        if cursor.goto_first_child() {
            parents.push(node);
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
                    analysis,
                    facts.csharp.as_mut().unwrap(),
                );
                for expression in &facts.csharp.as_ref().unwrap().expressions {
                    if let crate::csharp::syntax::ExpressionKind::ImplicitCall { name, .. } =
                        &expression.kind
                    {
                        facts.occurrences.push(Occurrence {
                            role: crate::model::OccurrenceRole::Value,
                            name: name.clone(),
                            span: expression.span.clone(),
                            call: !name.starts_with("op_")
                                && name != "@operator"
                                && name != "Current",
                            construction: false,
                            write: crate::model::WriteKind::None,
                            receiver: String::new(),
                            arguments: Some(0),
                            opaque: true,
                        });
                    }
                }
                return Ok(facts);
            }
            parents.pop();
        }
    }
}

fn declaration<'a>(
    n: Node<'a>,
    parents: &[Node<'a>],
    s: &str,
    facts: &mut Facts,
    scopes: &mut DeclarationScopes,
) {
    let implicit_value_type = crate::csharp::lower::accessor_value_type(n);
    let kind = match n.kind() {
        "accessor_declaration" if implicit_value_type.is_some() => "parameter",
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
            if ancestor(parents, &["event_field_declaration"]).is_some() {
                "event"
            } else if ancestor(parents, &["field_declaration"]).is_some() {
                if ancestor(parents, &["field_declaration"])
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
            (n.kind() == "conversion_operator_declaration")
                .then(|| {
                    n.children(&mut cursor)
                        .find(|token| matches!(token.kind(), "implicit" | "explicit"))
                })
                .flatten()
        })
        .or_else(|| {
            let mut cursor = n.walk();
            (n.kind() == "indexer_declaration")
                .then(|| n.children(&mut cursor).find(|n| n.kind() == "this"))
                .flatten()
        });
    let Some(name_node) = name_node else { return };
    let name = if implicit_value_type.is_some() {
        "value".into()
    } else if n.kind() == "indexer_declaration" {
        "Item".into()
    } else if n.kind() == "conversion_operator_declaration" {
        let checked = n
            .children(&mut n.walk())
            .any(|token| token.kind() == "checked");
        match (text(name_node, s), checked) {
            ("implicit", false) => "op_Implicit",
            ("explicit", false) => "op_Explicit",
            ("implicit", true) => "op_CheckedImplicit",
            _ => "op_CheckedExplicit",
        }
        .into()
    } else {
        normalize_name(text(name_node, s))
    };
    if name.is_empty() {
        return;
    }
    let span = n.byte_range();
    let (owner_index, namespace_index) = scopes.enclosing(&span, &facts.declarations);
    let owner = owner_index.map(|i| &facts.declarations[i]);
    let namespace = namespace_index
        .map(|i| facts.declarations[i].qualified.clone())
        .unwrap_or_default();
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
        ancestor(parents, &["field_declaration", "event_field_declaration"]).unwrap_or(n)
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
    let mut ty = implicit_value_type
        .map(|node| text(node, s).to_owned())
        .unwrap_or_else(|| field(n, "type", s));
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
    let scope = if implicit_value_type.is_some() {
        span.clone()
    } else if kind == "parameter" {
        crate::csharp::lower::parameter_scope(n)
    } else if kind == "local" || n.kind() == "local_function_statement" {
        crate::csharp::lower::local_scope(n).unwrap_or_else(|| span.clone())
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
    if let Some(syntax) = facts.csharp.as_mut() {
        let header =
            crate::csharp::lower::header(n, s, &facts.declarations, owner_index.map(|i| i as u32));
        syntax.headers.push(header);
    }
    let index = facts.declarations.len() - 1;
    scopes.push(index, &facts.declarations[index], s);
}

fn occurrence<'tree>(n: Node<'tree>, mut parents: &[Node<'tree>], s: &str, facts: &mut Facts) {
    let mut current = n;
    let mut receiver = String::new();
    if let Some((&p, rest)) = parents.split_last()
        && p.kind() == "generic_name"
    {
        current = p;
        parents = rest;
    }
    if let Some((&binding, rest)) = parents.split_last()
        && binding.kind() == "member_binding_expression"
        && let Some((&conditional, rest)) = rest.split_last()
        && conditional.kind() == "conditional_access_expression"
    {
        receiver = field(conditional, "condition", s);
        current = conditional;
        parents = rest;
    }
    if let Some((&p, rest)) = parents.split_last()
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
        parents = rest;
    }
    let mut call = false;
    let mut construction = false;
    let mut arguments = None;
    let mut write = crate::model::WriteKind::None;
    if let Some(&p) = parents.last() {
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
            write = crate::model::WriteKind::Direct;
        }
        if matches!(
            p.kind(),
            "postfix_unary_expression" | "prefix_unary_expression"
        ) && (text(p, s).contains("++") || text(p, s).contains("--"))
        {
            write = crate::model::WriteKind::Direct;
        }
        if p.kind() == "argument" {
            write = p
                .children(&mut p.walk())
                .find_map(|token| match token.kind() {
                    "out" => Some(crate::model::WriteKind::Out),
                    "ref" => Some(crate::model::WriteKind::Ref),
                    _ => None,
                })
                .unwrap_or(write);
        }
    }
    facts.occurrences.push(Occurrence {
        role: crate::model::OccurrenceRole::Value,
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
    fn required_modifier_and_identifier_preserve_indexed_facts() {
        let facts = extract(
            "class Worker { public required string Name { get; init; } int required; void Run(int required) { this.required = required; } }",
            &[],
        )
        .unwrap();
        assert!(!facts.errors);
        assert!(facts.declarations.iter().any(|d| d.name == "Name"
            && d.kind == "property"
            && d.modifiers.iter().any(|m| m == "required")));
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.name == "required" && d.kind == "field")
        );
        assert!(
            facts.occurrences.iter().any(|o| o.name == "required"
                && o.receiver == "this"
                && o.write == WriteKind::Direct)
        );
    }

    #[test]
    fn nested_receivers_keep_calls_and_write_roles() {
        let source = r#"class Reader { void Read() {
            factory.Make<Item>(value);
            factory?.Make<Item>(value);
            Make<Item>(value);
            var item = new Item(value);
            holder.Value += 1;
            holder.Items[0] = value;
            Touch(ref value, out result);
        } }"#;
        let facts = extract(source, &[]).unwrap();
        assert!(!facts.errors);
        let calls: Vec<_> = facts
            .occurrences
            .iter()
            .filter(|o| o.name == "Make" && o.call)
            .collect();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|o| o.arguments == Some(1)));
        assert_eq!(
            calls
                .iter()
                .map(|o| o.receiver.as_str())
                .collect::<Vec<_>>(),
            ["factory", "factory", ""]
        );
        assert!(
            facts
                .occurrences
                .iter()
                .any(|o| o.name == "Item" && o.construction && o.arguments == Some(1))
        );
        for (name, role) in [
            ("Value", WriteKind::Direct),
            ("Item", WriteKind::Direct),
            ("value", WriteKind::Ref),
            ("result", WriteKind::Out),
        ] {
            assert!(
                facts
                    .occurrences
                    .iter()
                    .any(|o| o.name == name && o.write == role)
            );
        }
        let historical = historical(source).unwrap();
        let calls: Vec<_> = historical
            .occurrences
            .iter()
            .filter(|o| o.name == "Make")
            .collect();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .map(|o| o.receiver.as_str())
                .collect::<Vec<_>>(),
            ["factory", "factory", ""]
        );
    }

    #[test]
    fn sibling_scopes_keep_their_own_ancestors() {
        let source = "namespace Outer { using System; class A { const int X = 1; event Action E; void M() { int local = 0; } } namespace Inner { using Other; class B { int field; } } } namespace Next { class C { int value; } }";
        let facts = extract(source, &[]).unwrap();
        for (name, kind) in [
            ("X", "const"),
            ("E", "event"),
            ("local", "local"),
            ("field", "field"),
            ("value", "field"),
        ] {
            assert!(
                facts
                    .declarations
                    .iter()
                    .any(|d| d.name == name && d.kind == kind)
            );
        }
        let outer = facts.imports.iter().find(|i| i.path == "System").unwrap();
        let inner = facts.imports.iter().find(|i| i.path == "Other").unwrap();
        assert!(outer.scope.contains(&source.find("class A").unwrap()));
        assert!(!inner.scope.contains(&source.find("class A").unwrap()));
        assert!(inner.scope.contains(&source.find("class B").unwrap()));
        assert!(!outer.scope.contains(&source.find("class C").unwrap()));
    }

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
        assert_eq!(
            f.occurrences
                .iter()
                .filter(|o| o.write != crate::model::WriteKind::None)
                .count(),
            2
        );
        assert!(
            !f.occurrences
                .iter()
                .any(|o| f.declarations.iter().any(|d| d.name_span == o.span))
        );
    }

    // Independent oracle for the base's prefix scans. Build expected names from
    // earlier expected names, so a wrong namespace cannot validate itself.
    fn assert_prefix_owners(source: &str, facts: &Facts) {
        let syntax = facts.csharp.as_ref().unwrap();
        assert_eq!(facts.declarations.len(), syntax.headers.len());
        let mut names: Vec<String> = Vec::new();
        for (index, (declaration, header)) in
            facts.declarations.iter().zip(&syntax.headers).enumerate()
        {
            let previous = &facts.declarations[..index];
            let contains = |d: &Declaration| {
                d.span.start <= declaration.span.start && d.span.end >= declaration.span.end
            };
            let owner = previous
                .iter()
                .rposition(|d| !d.local() && d.kind != "namespace" && contains(d));
            let namespace = previous
                .iter()
                .rposition(|d| d.kind == "namespace" && contains(d))
                .or_else(|| {
                    previous.iter().position(|d| {
                        d.kind == "namespace"
                            && source[d.span.clone()]
                                .trim_start()
                                .starts_with("namespace ")
                            && !source[d.span.clone()].contains('{')
                    })
                });
            let namespace = namespace.map_or("", |i| names[i].as_str());
            let owner_name = owner.map_or(namespace, |i| names[i].as_str());
            let qualified = if owner_name.is_empty() {
                declaration.name.clone()
            } else {
                format!("{owner_name}.{}", declaration.name)
            };
            assert_eq!(header.declaration, index as u32, "{qualified}");
            assert_eq!(header.owner, owner.map(|i| i as u32), "{qualified}");
            assert_eq!(declaration.namespace, namespace, "{qualified}");
            assert_eq!(declaration.owner, owner_name, "{qualified}");
            assert_eq!(declaration.qualified, qualified, "declaration {index}");
            let explicit_access = declaration
                .modifiers
                .iter()
                .filter(|m| matches!(m.as_str(), "public" | "private" | "protected" | "internal"))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let access = if !explicit_access.is_empty() {
                explicit_access.as_str()
            } else if declaration.kind == "namespace"
                || owner.is_some_and(|i| previous[i].kind == "interface")
            {
                "public"
            } else if owner.is_some() {
                "private"
            } else {
                "internal"
            };
            assert_eq!(declaration.access, access, "{qualified}");
            names.push(qualified);
        }
    }

    fn owner_facts(source: &str) -> Facts {
        let facts = extract(source, &[]).unwrap();
        assert_prefix_owners(source, &facts);
        facts
    }

    fn assert_owner(facts: &Facts, qualified: &str, owner: Option<&str>, local: bool) {
        let mut found = false;
        for (declaration, header) in facts
            .declarations
            .iter()
            .zip(&facts.csharp.as_ref().unwrap().headers)
            .filter(|(d, _)| d.qualified == qualified)
        {
            found = true;
            assert_eq!(
                header
                    .owner
                    .map(|i| facts.declarations[i as usize].qualified.as_str()),
                owner,
                "{}",
                declaration.qualified
            );
            assert_eq!(header.local, local, "{qualified}");
        }
        assert!(found, "missing declaration {qualified}");
    }

    #[test]
    fn owner_links_follow_nested_types_members_and_namespaces() {
        let source = r#"
namespace Outer {
    interface I {
        int P { get; set; }
        void M(int argument);
        class Nested { int field; }
    }
    class C {
        const int A = 1, B = 2;
        event System.Action E, F;
        enum Kind { First, Second }
        delegate void Callback(int item);
        record R(int primary) { int field; }
        struct S { int field; }
        class Inner { int x, y; }
        int tail;
    }
    namespace Inner { class C { int field; } }
}
namespace Next { class C { int field; } }
"#;
        let facts = owner_facts(source);
        assert!(!facts.errors);
        for name in [
            "Outer",
            "Outer.I",
            "Outer.C",
            "Outer.Inner",
            "Outer.Inner.C",
            "Next.C",
        ] {
            assert_owner(&facts, name, None, false);
        }
        for (name, owner) in [
            ("Outer.I.P", "Outer.I"),
            ("Outer.I.Nested", "Outer.I"),
            ("Outer.I.Nested.field", "Outer.I.Nested"),
            ("Outer.C.A", "Outer.C"),
            ("Outer.C.B", "Outer.C"),
            ("Outer.C.E", "Outer.C"),
            ("Outer.C.F", "Outer.C"),
            ("Outer.C.Kind.First", "Outer.C.Kind"),
            ("Outer.C.Kind.Second", "Outer.C.Kind"),
            ("Outer.C.Callback", "Outer.C"),
            ("Outer.C.R.field", "Outer.C.R"),
            ("Outer.C.S.field", "Outer.C.S"),
            ("Outer.C.Inner.x", "Outer.C.Inner"),
            ("Outer.C.Inner.y", "Outer.C.Inner"),
            ("Outer.C.tail", "Outer.C"),
            ("Outer.Inner.C.field", "Outer.Inner.C"),
            ("Next.C.field", "Next.C"),
        ] {
            assert_owner(&facts, name, Some(owner), false);
        }
        for (name, owner) in [
            ("Outer.I.P.value", "Outer.I.P"),
            ("Outer.I.M.argument", "Outer.I.M"),
            ("Outer.C.Callback.item", "Outer.C.Callback"),
            ("Outer.C.R.primary", "Outer.C.R"),
        ] {
            assert_owner(&facts, name, Some(owner), true);
        }
    }

    #[test]
    fn owner_links_distinguish_local_functions_parameters_and_accessors() {
        let source = r#"
namespace N;
class C {
    System.Func<int, int> first = (int fieldArg) => fieldArg,
                          second = (int siblingFieldArg) => siblingFieldArg;
    C(int ctor) {}
    public static implicit operator int(C converted) => 0;
    public int this[int index] {
        get { int got = 0; return got; }
        set { int setLocal = value; void InSet(int setArg) {} }
    }
    public int P { init { int initLocal = value; } }
    event System.Action E {
        add { System.Action addLocal = value; }
        remove { System.Action removeLocal = value; }
    }
    void M(int argument) {
        int a = 0, b = 1;
        System.Func<int, int> lambda = (int lambdaArg) => lambdaArg;
        System.Func<int, int> anonymous = delegate(int anonymousArg) {
            int anonymousLocal = 0; return anonymousArg;
        };
        void Local<T>(T localArg) {
            int inner = 0;
            void Nested(int nestedArg) {}
        }
        void Sibling(int siblingArg) {}
        if (argument is int pattern) {}
        int.TryParse("", out int output);
        int after = 0;
    }
}
"#;
        let facts = owner_facts(source);
        assert!(!facts.errors);
        for (name, owner) in [
            ("N.C.first.fieldArg", "N.C.first"),
            ("N.C.second.siblingFieldArg", "N.C.second"),
            ("N.C.C.ctor", "N.C.C"),
            ("N.C.op_Implicit.converted", "N.C.op_Implicit"),
            ("N.C.Item.index", "N.C.Item"),
            ("N.C.Item.value", "N.C.Item"),
            ("N.C.Item.got", "N.C.Item"),
            ("N.C.Item.setLocal", "N.C.Item"),
            ("N.C.Item.InSet", "N.C.Item"),
            ("N.C.Item.InSet.setArg", "N.C.Item.InSet"),
            ("N.C.P.value", "N.C.P"),
            ("N.C.P.initLocal", "N.C.P"),
            ("N.C.E.value", "N.C.E"),
            ("N.C.E.addLocal", "N.C.E"),
            ("N.C.E.removeLocal", "N.C.E"),
            ("N.C.M.lambdaArg", "N.C.M"),
            ("N.C.M.anonymousArg", "N.C.M"),
            ("N.C.M.anonymousLocal", "N.C.M"),
            ("N.C.M.Local", "N.C.M"),
            ("N.C.M.Local.localArg", "N.C.M.Local"),
            ("N.C.M.Local.inner", "N.C.M.Local"),
            ("N.C.M.Local.Nested", "N.C.M.Local"),
            ("N.C.M.Local.Nested.nestedArg", "N.C.M.Local.Nested"),
            ("N.C.M.Sibling", "N.C.M"),
            ("N.C.M.Sibling.siblingArg", "N.C.M.Sibling"),
            ("N.C.M.pattern", "N.C.M"),
            ("N.C.M.output", "N.C.M"),
            ("N.C.M.after", "N.C.M"),
        ] {
            assert_owner(&facts, name, Some(owner), true);
        }
        // A setter's synthetic value declaration owns nothing; lexical locals
        // still have the same scopes as expression lowering, not owner spans.
        let syntax = facts.csharp.as_ref().unwrap();
        for declaration in facts.declarations.iter().filter(|d| d.kind == "local") {
            let local = syntax
                .locals
                .iter()
                .find(|l| l.span == declaration.name_span)
                .unwrap();
            assert_eq!(declaration.scope, local.scope, "{}", declaration.qualified);
        }
        let values: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.qualified == "N.C.E.value")
            .collect();
        assert_eq!(values.len(), 2);
        assert!(values[0].scope.end <= values[1].scope.start);
    }

    #[test]
    fn owner_links_limit_the_synthetic_top_level_range() {
        let source = "int first = 0; void Top(int arg) { int local = arg; void Inner(int nested) {} } int last = first; class After { int field; }";
        let facts = owner_facts(source);
        assert!(!facts.errors);
        assert_owner(&facts, "", None, false);
        for name in ["first", "Top", "last"] {
            assert_owner(&facts, name, Some(""), true);
        }
        assert_owner(&facts, "Top.arg", Some("Top"), true);
        assert_owner(&facts, "Top.local", Some("Top"), true);
        assert_owner(&facts, "Top.Inner", Some("Top"), true);
        assert_owner(&facts, "Top.Inner.nested", Some("Top.Inner"), true);
        assert_owner(&facts, "After", None, false);
        assert_owner(&facts, "After.field", Some("After"), false);

        // Recovery can interleave types and global statements. Earlier types
        // must not evict the synthetic owner before its range is reached.
        let source = "class Before { int field; } int first = 0; class Between { int field; } int last = 0; class After { int field; }";
        let facts = owner_facts(source);
        assert_owner(&facts, "Before", None, false);
        assert_owner(&facts, "first", Some(""), true);
        assert_owner(&facts, "Between", Some(""), false);
        assert_owner(&facts, "last", Some(""), true);
        assert_owner(&facts, "After", None, false);
        for name in ["Before", "Between", "After"] {
            assert_owner(&facts, &format!("{name}.field"), Some(name), false);
        }
    }

    #[test]
    fn owner_links_match_prefix_scans_after_recovery_and_preprocessing() {
        for source in [
            "",
            "namespace N;",
            "namespace /*{*/ N; class C { int field; }",
            "namespace N; namespace M; class C { int field; }",
            "namespace N { class C { void M( { int x = ; } }",
            "namespace N { class C {} } int x = 0;",
        ] {
            owner_facts(source);
        }
        let source = "namespace Ω { class C { int a, b; int P { get; set { int x = value; } } void M(int p) { void L(int q) {} int z = p; } } }";
        for (offset, character) in source.char_indices() {
            owner_facts(&source[..offset]);
            let mut damaged = source.to_owned();
            damaged.replace_range(offset..offset + character.len_utf8(), "");
            owner_facts(&damaged);
        }
        let source =
            "#if LEFT\nnamespace Left;\n#else\nnamespace Right;\n#endif\nclass C { int field; }";
        for (defines, namespace) in [(vec![], "Right"), (vec!["LEFT".into()], "Left")] {
            let analysis = crate::extract::preprocess::active_source(source, &defines).unwrap();
            let facts = extract(source, &defines).unwrap();
            assert!(!facts.errors);
            assert_prefix_owners(&analysis, &facts);
            assert_owner(&facts, &format!("{namespace}.C"), None, false);
            assert_owner(
                &facts,
                &format!("{namespace}.C.field"),
                Some(&format!("{namespace}.C")),
                false,
            );
        }
    }

    #[test]
    fn owner_links_do_not_leak_across_many_sibling_declarations() {
        use std::fmt::Write;
        let mut source = String::from("namespace N; class Wide {");
        for i in 0..256 {
            write!(source, "int f{i}, g{i}; void M{i}(int p) {{ int x = p; }}").unwrap();
        }
        source.push('}');
        let facts = owner_facts(&source);
        assert!(!facts.errors);
        assert_eq!(facts.declarations.len(), 2 + 5 * 256);
        for i in 0..256 {
            for field in [format!("N.Wide.f{i}"), format!("N.Wide.g{i}")] {
                assert_owner(&facts, &field, Some("N.Wide"), false);
            }
            let method = format!("N.Wide.M{i}");
            assert_owner(&facts, &method, Some("N.Wide"), false);
            for name in ["p", "x"] {
                assert_owner(&facts, &format!("{method}.{name}"), Some(&method), true);
            }
        }
    }

    #[test]
    fn owner_links_survive_an_owned_declaration_payload_roundtrip() {
        use crate::csharp::syntax::{DeclarationFile, FileSyntax};
        let source =
            "int x = 0; void F(int p) { void G(int q) {} } class C { int P { get; set; } }";
        let facts = owner_facts(source);
        let syntax = facts.csharp.unwrap();
        let payload = DeclarationFile {
            declarations: facts.declarations,
            headers: syntax.headers,
            imports: syntax.imports,
        };
        let bytes = postcard::to_allocvec(&payload).unwrap();
        let decoded: DeclarationFile = postcard::from_bytes(&bytes).unwrap();
        let recovered = Facts {
            declarations: decoded.declarations,
            csharp: Some(FileSyntax {
                headers: decoded.headers,
                imports: decoded.imports,
                ..FileSyntax::default()
            }),
            ..Facts::default()
        };
        assert_prefix_owners(source, &recovered);
        assert_owner(&recovered, "F.G.q", Some("F.G"), true);
        assert_owner(&recovered, "C.P.value", Some("C.P"), true);
        assert_owner(&recovered, "x", Some(""), true);
        assert_owner(&recovered, "C", None, false);
    }
}
