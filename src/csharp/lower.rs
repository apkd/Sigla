//! Lower from the existing parse, without retaining parser nodes.
use super::{syntax::*, types::*};
use crate::model::Declaration;
use std::collections::HashMap;
use tree_sitter::Node;

/// Postorder lowering gives children stable arena IDs without recursive traversal.
pub fn bodies(root: Node<'_>, source: &str, syntax: &mut FileSyntax) {
    let mut ids = HashMap::new();
    let mut out_calls = Vec::new();
    let mut cursor = root.walk();
    let mut entering = true;
    loop {
        if entering && cursor.goto_first_child() {
            continue;
        }
        let node = cursor.node();
        let get = |n: Node<'_>| ids.get(&n.id()).copied();
        let field = |name: &str| node.child_by_field_name(name).and_then(get);
        let arguments = |list: Option<Node<'_>>| -> Vec<Argument> {
            list.map(|list| {
                children(list)
                    .into_iter()
                    .filter(|n| n.kind() == "argument")
                    .filter_map(|n| {
                        let value = children(n).into_iter().rev().find_map(get)?;
                        let prefix = text(n, source).trim_start();
                        Some(Argument {
                            name: n.child_by_field_name("name").map(|n| name(n, source)),
                            mode: if prefix.starts_with("ref ") {
                                PassingMode::Ref
                            } else if prefix.starts_with("out ") {
                                PassingMode::Out
                            } else if prefix.starts_with("in ") {
                                PassingMode::In
                            } else {
                                PassingMode::Value
                            },
                            value,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
        };
        let kind = match node.kind() {
            "declaration_expression" => Some(ExpressionKind::OutVariable {
                name: node
                    .child_by_field_name("name")
                    .map(|n| name(n, source))
                    .unwrap_or_default(),
                ty: node
                    .child_by_field_name("type")
                    .map(|n| written(n, source))
                    .unwrap_or(WrittenType::Inferred),
            }),
            "identifier" | "generic_name" | "this_expression" | "base_expression"
            | "predefined_type" => Some(ExpressionKind::Name {
                name: if node.kind() == "generic_name" {
                    node.named_child(0)
                        .map(|n| name(n, source))
                        .unwrap_or_default()
                } else {
                    name(node, source)
                },
                arguments: child(node, "type_argument_list")
                    .map(|n| {
                        children(n)
                            .into_iter()
                            .map(|n| written(n, source))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "member_access_expression" => {
                field("expression")
                    .zip(field("name"))
                    .map(|(receiver, name)| ExpressionKind::Member {
                        receiver,
                        name,
                        conditional: false,
                    })
            }
            "invocation_expression" => field("function").map(|function| ExpressionKind::Call {
                function,
                arguments: arguments(node.child_by_field_name("arguments")),
            }),
            "object_creation_expression"
            | "implicit_object_creation_expression"
            | "array_creation_expression" => Some(ExpressionKind::New {
                ty: node
                    .child_by_field_name("type")
                    .map(|n| written(n, source))
                    .unwrap_or(WrittenType::Inferred),
                arguments: arguments(node.child_by_field_name("arguments")),
            }),
            "conditional_access_expression" => {
                let receiver = field("condition");
                let member = child(node, "member_binding_expression")
                    .and_then(|n| n.child_by_field_name("name"))
                    .and_then(get);
                receiver
                    .zip(member)
                    .map(|(receiver, name)| ExpressionKind::Member {
                        receiver,
                        name,
                        conditional: true,
                    })
            }
            "cast_expression" => field("value")
                .or_else(|| children(node).into_iter().rev().find_map(get))
                .map(|value| ExpressionKind::Cast {
                    ty: node
                        .child_by_field_name("type")
                        .map(|n| written(n, source))
                        .unwrap_or_else(|| WrittenType::Unsupported(node.kind().into())),
                    value,
                }),
            "element_access_expression" => {
                field("expression").map(|receiver| ExpressionKind::Index {
                    receiver,
                    arguments: arguments(
                        node.child_by_field_name("subscript")
                            .or_else(|| child(node, "bracketed_argument_list")),
                    ),
                })
            }
            "assignment_expression" => field("left")
                .zip(field("right"))
                .map(|(left, right)| ExpressionKind::Assign { left, right }),
            "parenthesized_expression" => node
                .named_child(0)
                .and_then(get)
                .map(ExpressionKind::Wrapped),
            "await_expression" => node.named_child(0).and_then(get).map(ExpressionKind::Await),
            "lambda_expression" => field("body").map(|body| ExpressionKind::Lambda {
                parameters: node
                    .child_by_field_name("parameters")
                    .map(|n| {
                        if n.kind() == "implicit_parameter" {
                            vec![parameter(n, source)]
                        } else {
                            children(n)
                                .into_iter()
                                .map(|n| parameter(n, source))
                                .collect()
                        }
                    })
                    .unwrap_or_default(),
                body,
            }),
            kind if kind.ends_with("_literal") => Some(ExpressionKind::Literal {
                kind: kind.into(),
                value: text(node, source).into(),
            }),
            kind if kind.ends_with("_expression") || kind == "block" || kind == "ERROR" => {
                Some(ExpressionKind::Unsupported(kind.into()))
            }
            _ => None,
        };
        if let Some(kind) = kind {
            let id = syntax.expressions.len() as ExprId;
            syntax.expressions.push(Expression {
                span: node.byte_range(),
                kind,
            });
            ids.insert(node.id(), id);
        }
        if matches!(
            node.kind(),
            "variable_declarator"
                | "foreach_statement"
                | "declaration_expression"
                | "declaration_pattern"
        ) {
            let iteration = node.kind() == "foreach_statement";
            let name_node = node.child_by_field_name(if iteration { "left" } else { "name" });
            if let Some(name_node) = name_node {
                let type_node = if iteration
                    || matches!(
                        node.kind(),
                        "declaration_expression" | "declaration_pattern"
                    ) {
                    node.child_by_field_name("type")
                } else {
                    node.parent().and_then(|n| n.child_by_field_name("type"))
                };
                let value = if matches!(
                    node.kind(),
                    "declaration_expression" | "declaration_pattern"
                ) {
                    None
                } else if iteration {
                    node.child_by_field_name("right")
                } else {
                    children(node).into_iter().rev().find(|n| *n != name_node)
                }
                .and_then(|n| ids.get(&n.id()).copied());
                let mut scope = node;
                while !matches!(
                    scope.kind(),
                    "block"
                        | "foreach_statement"
                        | "for_statement"
                        | "switch_section"
                        | "declaration_list"
                ) {
                    let Some(parent) = scope.parent() else { break };
                    scope = parent;
                }
                if scope.kind() != "declaration_list" {
                    let out_argument = if node.kind() == "declaration_expression" {
                        let mut parent = node.parent();
                        while let Some(n) = parent {
                            if n.kind() == "invocation_expression" {
                                out_calls.push((syntax.locals.len(), n.id()));
                                break;
                            }
                            parent = n.parent();
                        }
                        ids.get(&node.id()).copied()
                    } else {
                        None
                    };
                    syntax.locals.push(Local {
                        name: name(name_node, source),
                        span: name_node.byte_range(),
                        scope: scope.byte_range(),
                        ty: type_node
                            .map(|n| written(n, source))
                            .unwrap_or(WrittenType::Inferred),
                        value,
                        iteration,
                        out_argument,
                    });
                }
            }
        }
        if node.kind() == "foreach_statement" {
            let receiver = node
                .child_by_field_name("right")
                .and_then(|n| ids.get(&n.id()).copied());
            let span = node.start_byte()..node.start_byte() + "foreach".len();
            let enumerator = implicit(syntax, span.clone(), "GetEnumerator", receiver);
            implicit(syntax, span.clone(), "MoveNext", Some(enumerator));
            implicit(syntax, span, "Current", Some(enumerator));
        } else if node.kind() == "await_expression" {
            let receiver = node.named_child(0).and_then(|n| ids.get(&n.id()).copied());
            let span = node.start_byte()..node.start_byte() + "await".len();
            let awaiter = implicit(syntax, span.clone(), "GetAwaiter", receiver);
            implicit(syntax, span, "GetResult", Some(awaiter));
        } else if node.kind() == "using_statement" {
            let get = |n: Node<'_>| ids.get(&n.id()).copied();
            let resource = children(node).into_iter().find(|n| n.kind() != "block");
            let receiver = resource.and_then(|resource| {
                if resource.kind() != "variable_declaration" {
                    return get(resource);
                }
                let variable = child(resource, "variable_declarator")?;
                let value = children(variable).into_iter().rev().find_map(get)?;
                let ty = resource
                    .child_by_field_name("type")
                    .map(|n| written(n, source))
                    .unwrap_or(WrittenType::Inferred);
                if matches!(ty, WrittenType::Inferred) {
                    Some(value)
                } else {
                    let id = syntax.expressions.len() as ExprId;
                    syntax.expressions.push(Expression {
                        span: resource.byte_range(),
                        kind: ExpressionKind::Cast { ty, value },
                    });
                    Some(id)
                }
            });
            implicit(
                syntax,
                node.start_byte()..node.start_byte() + "using".len(),
                "Dispose",
                receiver,
            );
        } else if node.kind() == "binary_expression"
            && let Some(operator) = node.child_by_field_name("operator")
        {
            let name = match text(operator, source) {
                "+" => "op_Addition",
                "-" => "op_Subtraction",
                "*" => "op_Multiply",
                "/" => "op_Division",
                "==" => "op_Equality",
                "!=" => "op_Inequality",
                _ => "@operator",
            };
            implicit(syntax, operator.byte_range(), name, None);
        }
        if cursor.goto_next_sibling() {
            entering = true;
        } else if cursor.goto_parent() {
            entering = false;
        } else {
            break;
        }
    }
    for (local, call) in out_calls {
        syntax.locals[local].value = ids.get(&call).copied();
    }
}

fn implicit(
    syntax: &mut FileSyntax,
    span: std::ops::Range<usize>,
    name: &str,
    receiver: Option<ExprId>,
) -> ExprId {
    let id = syntax.expressions.len() as ExprId;
    syntax.expressions.push(Expression {
        span,
        kind: ExpressionKind::ImplicitCall {
            name: name.into(),
            receiver,
        },
    });
    id
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn child<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|n| n.kind() == kind)
}
fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}
fn name(node: Node<'_>, source: &str) -> String {
    text(node, source).trim_start_matches('@').into()
}

pub fn written(node: Node<'_>, source: &str) -> WrittenType {
    written_at(node, source, 0)
}
fn written_at(node: Node<'_>, source: &str, depth: usize) -> WrittenType {
    if depth > 64 {
        return WrittenType::Unsupported(node.kind().into());
    }
    let nested = |n| written_at(n, source, depth + 1);
    let value = text(node, source);
    match node.kind() {
        "identifier" | "predefined_type" | "implicit_type" => match value {
            "dynamic" => WrittenType::Dynamic,
            "var" => WrittenType::Inferred,
            _ => WrittenType::Name {
                alias: None,
                parts: vec![NamePart {
                    name: name(node, source),
                    arguments: vec![],
                }],
            },
        },
        "generic_name" => WrittenType::Name {
            alias: None,
            parts: vec![NamePart {
                name: node
                    .named_child(0)
                    .map(|n| name(n, source))
                    .unwrap_or_default(),
                arguments: child(node, "type_argument_list")
                    .map(|n| children(n).into_iter().map(nested).collect())
                    .unwrap_or_default(),
            }],
        },
        "qualified_name" | "alias_qualified_name" => {
            let left = node
                .child_by_field_name("qualifier")
                .or_else(|| node.child_by_field_name("alias"))
                .or_else(|| node.named_child(0));
            let right = node
                .child_by_field_name("name")
                .or_else(|| node.named_child(1));
            if let (Some(left), Some(right)) = (left, right)
                && let WrittenType::Name { alias, mut parts } = nested(right)
            {
                if node.kind() == "alias_qualified_name" {
                    return WrittenType::Name {
                        alias: Some(name(left, source)),
                        parts,
                    };
                }
                if let WrittenType::Name {
                    alias: prefix_alias,
                    parts: mut prefix,
                } = nested(left)
                {
                    prefix.append(&mut parts);
                    return WrittenType::Name {
                        alias: prefix_alias.or(alias),
                        parts: prefix,
                    };
                }
            }
            WrittenType::Unsupported(value.into())
        }
        "nullable_type" => node
            .named_child(0)
            .map(|n| WrittenType::Nullable(Box::new(nested(n))))
            .unwrap_or_else(|| WrittenType::Unsupported(value.into())),
        "pointer_type" => node
            .named_child(0)
            .map(|n| WrittenType::Pointer(Box::new(nested(n))))
            .unwrap_or_else(|| WrittenType::Unsupported(value.into())),
        "array_type" => {
            let Some(element) = node
                .child_by_field_name("type")
                .or_else(|| node.named_child(0))
            else {
                return WrittenType::Unsupported(value.into());
            };
            let mut ty = nested(element);
            for rank in children(node)
                .into_iter()
                .filter(|n| n.kind() == "array_rank_specifier")
                .rev()
            {
                ty = WrittenType::Array(
                    Box::new(ty),
                    text(rank, source).bytes().filter(|b| *b == b',').count() as u32 + 1,
                );
            }
            ty
        }
        "tuple_type" => WrittenType::Tuple(
            children(node)
                .into_iter()
                .map(|n| {
                    (
                        n.child_by_field_name("type")
                            .map(nested)
                            .unwrap_or_else(|| WrittenType::Unsupported(text(n, source).into())),
                        n.child_by_field_name("name").map(|n| name(n, source)),
                    )
                })
                .collect(),
        ),
        _ => WrittenType::Unsupported(value.into()),
    }
}

fn parameter(node: Node<'_>, source: &str) -> Parameter {
    let words: Vec<_> = children(node)
        .into_iter()
        .filter(|n| n.kind() == "modifier")
        .map(|n| text(n, source))
        .collect();
    Parameter {
        name: node
            .child_by_field_name("name")
            .map(|n| name(n, source))
            .unwrap_or_else(|| name(node, source)),
        ty: node
            .child_by_field_name("type")
            .map(|n| written(n, source))
            .unwrap_or(WrittenType::Inferred),
        mode: if words.contains(&"out") {
            PassingMode::Out
        } else if words.contains(&"ref") {
            PassingMode::Ref
        } else if words.contains(&"in") {
            PassingMode::In
        } else {
            PassingMode::Value
        },
        default: {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|n| n.kind() == "=")
                .and_then(|n| n.next_named_sibling())
                .map(|n| text(n, source).into())
        },
        variadic: words.contains(&"params"),
        receiver: words.contains(&"this"),
    }
}

fn parameters(list: Node<'_>, source: &str) -> Vec<Parameter> {
    let mut result: Vec<_> = children(list)
        .into_iter()
        .filter(|n| n.kind() == "parameter")
        .map(|n| parameter(n, source))
        .collect();
    // Tree-sitter represents a trailing params parameter directly on the list.
    if let (Some(ty), Some(name_node)) = (
        list.child_by_field_name("type"),
        list.child_by_field_name("name"),
    ) {
        result.push(Parameter {
            name: name(name_node, source),
            ty: written(ty, source),
            mode: PassingMode::Value,
            default: None,
            variadic: true,
            receiver: false,
        });
    }
    result
}

pub fn header(node: Node<'_>, source: &str, declarations: &[Declaration]) -> Header {
    let index = declarations.len() - 1;
    let declaration = &declarations[index];
    let owner = declarations[..index]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, d)| {
            !d.local()
                && d.kind != "namespace"
                && d.span.start <= declaration.span.start
                && d.span.end >= declaration.span.end
        })
        .map(|(i, _)| i as u32);
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| node.child_by_field_name("returns"))
        .or_else(|| {
            (node.kind() == "variable_declarator")
                .then(|| node.parent()?.child_by_field_name("type"))
                .flatten()
        });
    let mut generics: Vec<_> = child(node, "type_parameter_list")
        .map(|list| {
            children(list)
                .into_iter()
                .map(|n| GenericParameter {
                    name: n
                        .child_by_field_name("name")
                        .or_else(|| child(n, "identifier"))
                        .map(|n| name(n, source))
                        .unwrap_or_default(),
                    variance: if text(n, source).starts_with("out ") {
                        Variance::Out
                    } else if text(n, source).starts_with("in ") {
                        Variance::In
                    } else {
                        Variance::Invariant
                    },
                    constraints: vec![],
                    special_constraints: vec![],
                })
                .collect()
        })
        .unwrap_or_default();
    for clause in children(node)
        .into_iter()
        .filter(|n| n.kind() == "type_parameter_constraints_clause")
    {
        let parts = children(clause);
        if let Some(generic) = parts
            .first()
            .and_then(|n| generics.iter_mut().find(|g| g.name == text(*n, source)))
        {
            for constraint in parts.into_iter().skip(1) {
                let value = text(constraint, source);
                if matches!(
                    value,
                    "class" | "class?" | "struct" | "notnull" | "unmanaged" | "new()"
                ) {
                    generic.special_constraints.push(value.into());
                } else {
                    generic.constraints.push(written(
                        constraint.named_child(0).unwrap_or(constraint),
                        source,
                    ));
                }
            }
        }
    }
    Header {
        local: declaration.local() || node.kind() == "local_function_statement",
        accessors: child(node, "accessor_list")
            .map(|list| {
                children(list)
                    .into_iter()
                    .filter(|n| n.kind() == "accessor_declaration")
                    .map(|n| {
                        let value = text(n, source);
                        let role = value
                            .split(|c: char| !c.is_alphabetic())
                            .find(|word| matches!(*word, "get" | "set" | "init" | "add" | "remove"))
                            .unwrap_or("unknown");
                        let access = children(n)
                            .into_iter()
                            .filter(|n| n.kind() == "modifier")
                            .map(|n| text(n, source))
                            .collect::<Vec<_>>()
                            .join(" ");
                        Accessor {
                            role: role.into(),
                            access: if access.is_empty() {
                                declaration.access.clone()
                            } else {
                                access
                            },
                            metadata_method: None,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        implementations: vec![],
        constant: if declaration.kind == "const" {
            Some(
                children(node)
                    .into_iter()
                    .rev()
                    .find(|n| n.kind() == "integer_literal")
                    .and_then(|n| integer_literal(text(n, source)))
                    .map(Constant::Integer)
                    .unwrap_or(Constant::Unsupported),
            )
        } else {
            None
        },
        declaration: index as u32,
        owner,
        ty: type_node
            .map(|n| written(n, source))
            .unwrap_or(WrittenType::Inferred),
        parameters: node
            .child_by_field_name("parameters")
            .map(|n| parameters(n, source))
            .unwrap_or_default(),
        generics,
        bases: child(node, "base_list")
            .map(|n| {
                children(n)
                    .into_iter()
                    .map(|n| written(n, source))
                    .collect()
            })
            .unwrap_or_default(),
        explicit_interface: child(node, "explicit_interface_specifier")
            .and_then(|n| n.named_child(0))
            .map(|n| written(n, source)),
    }
}

pub fn import(node: Node<'_>, source: &str) -> Import {
    let value = text(node, source);
    let alias = node.child_by_field_name("name");
    let target = children(node).into_iter().rev().find(|n| Some(*n) != alias);
    let mut scope = 0..source.len();
    let mut parent = node.parent();
    while let Some(n) = parent {
        if n.kind() == "namespace_declaration" {
            scope = n.byte_range();
            break;
        }
        parent = n.parent();
    }
    Import {
        kind: if let Some(alias) = alias {
            ImportKind::Alias(name(alias, source))
        } else if value.split_whitespace().any(|s| s == "static") {
            ImportKind::Static
        } else {
            ImportKind::Namespace
        },
        ty: target
            .map(|n| written(n, source))
            .unwrap_or_else(|| WrittenType::Unsupported(value.into())),
        scope,
        global: value.starts_with("global "),
    }
}
