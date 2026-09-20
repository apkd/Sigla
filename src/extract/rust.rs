use crate::{model::*, query::normalize_name};
use ra_ap_syntax::{
    AstNode, Edition, SourceFile, SyntaxNode,
    ast::{self, HasArgList, HasName},
};
use std::ops::Range;

fn range(n: &SyntaxNode) -> Range<usize> {
    let r = n.text_range();
    usize::from(r.start())..usize::from(r.end())
}
fn kind(n: &SyntaxNode) -> String {
    format!("{:?}", n.kind())
}
fn child(n: &SyntaxNode, k: &str) -> Option<SyntaxNode> {
    n.children().find(|n| kind(n) == k)
}
fn ty(n: &SyntaxNode) -> String {
    n.children()
        .find_map(ast::Type::cast)
        .map(|t| t.syntax().text().to_string())
        .unwrap_or_default()
}
fn scope(n: &SyntaxNode) -> Range<usize> {
    n.ancestors()
        .skip(1)
        .find(|n| {
            matches!(
                kind(n).as_str(),
                "BLOCK_EXPR" | "FN" | "CLOSURE_EXPR" | "MATCH_ARM"
            )
        })
        .map(|n| range(&n))
        .unwrap_or_else(|| range(n))
}

pub fn extract(source: &str, edition: &str) -> anyhow::Result<Facts> {
    let edition = match edition {
        "2015" => Edition::Edition2015,
        "2018" => Edition::Edition2018,
        "2021" => Edition::Edition2021,
        _ => Edition::Edition2024,
    };
    let parsed = SourceFile::parse(source, edition);
    let root = parsed.tree().syntax().clone();
    let mut facts = Facts {
        errors: !parsed.errors().is_empty(),
        ..Facts::default()
    };
    for n in root.descendants() {
        let k = kind(&n);
        if k == "USE"
            && let Some(tree) = ast::Use::cast(n.clone()).and_then(|u| u.use_tree())
        {
            imports(tree, "", 0..source.len(), &mut facts);
        }
        if let Some(m) = ast::Module::cast(n.clone())
            && m.semicolon_token().is_some()
            && let Some(name) = m.name()
        {
            let inline = n
                .ancestors()
                .skip(1)
                .filter_map(ast::Module::cast)
                .filter_map(|m| m.name())
                .map(|n| n.text().to_string())
                .collect::<Vec<_>>();
            let path = n.children().filter(|n| kind(n) == "ATTR").find_map(|a| {
                let t = a.text().to_string();
                if t.contains("path") {
                    t.split('"').nth(1).map(str::to_owned)
                } else {
                    None
                }
            });
            facts.modules.push(ModuleFile {
                name: normalize_name(name.text().as_ref()),
                path,
                inline: inline.into_iter().rev().collect(),
            });
        }
        let dk = match k.as_str() {
            "STRUCT" => "struct",
            "ENUM" => "enum",
            "TRAIT" => "trait",
            "TYPE_ALIAS" => "type",
            "MODULE" => "module",
            "FN" => {
                if n.ancestors()
                    .skip(1)
                    .any(|n| matches!(kind(&n).as_str(), "IMPL" | "TRAIT"))
                {
                    "method"
                } else {
                    "function"
                }
            }
            "CONST" => "const",
            "STATIC" => "static",
            "VARIANT" => "variant",
            "RECORD_FIELD" => "field",
            "MACRO_RULES" | "MACRO_DEF" => "macro",
            "IDENT_PAT" => {
                if n.ancestors().skip(1).any(|n| kind(&n) == "PARAM") {
                    "parameter"
                } else {
                    "local"
                }
            }
            "IMPL" => "impl",
            _ => "",
        };
        if !dk.is_empty() {
            let name_node = child(&n, "NAME");
            let implementation = ast::Impl::cast(n.clone());
            let self_type = implementation
                .as_ref()
                .and_then(|i| i.self_ty())
                .map(|t| t.syntax().text().to_string());
            let name = name_node
                .as_ref()
                .map(|n| normalize_name(&n.text().to_string()))
                .or(self_type.clone());
            if let Some(name) = name {
                let span = range(&n);
                let owner =
                    facts.declarations.iter().rev().find(|d| {
                        d.span.start <= span.start && d.span.end >= span.end && !d.local()
                    });
                let namespace = facts
                    .declarations
                    .iter()
                    .rev()
                    .find(|d| {
                        d.kind == "module" && d.span.start <= span.start && d.span.end >= span.end
                    })
                    .map(|d| d.qualified.clone())
                    .unwrap_or_default();
                let owner_name = owner
                    .map(|d| {
                        if d.kind == "impl" {
                            d.ty.clone()
                        } else {
                            d.qualified.clone()
                        }
                    })
                    .unwrap_or_default();
                let qualified = if owner_name.is_empty() {
                    name.clone()
                } else {
                    format!("{owner_name}::{name}")
                };
                let body = n.children().find(|n| {
                    matches!(
                        kind(n).as_str(),
                        "BLOCK_EXPR"
                            | "ITEM_LIST"
                            | "ASSOC_ITEM_LIST"
                            | "RECORD_FIELD_LIST"
                            | "VARIANT_LIST"
                    )
                });
                let header = span.start..body.as_ref().map_or(span.end, |n| range(n).start);
                let access = child(&n, "VISIBILITY")
                    .map(|n| n.text().to_string())
                    .unwrap_or_else(|| {
                        if owner.is_some_and(|d| d.kind == "trait") {
                            "pub".into()
                        } else {
                            "private".into()
                        }
                    });
                let attributes = n
                    .children()
                    .filter(|n| kind(n) == "ATTR")
                    .map(|n| n.text().to_string())
                    .collect();
                let mut parameters = Vec::new();
                let mut decl_ty = ty(&n);
                if let Some(f) = ast::Fn::cast(n.clone()) {
                    if let Some(p) = f.param_list() {
                        parameters = p
                            .params()
                            .map(|p| {
                                p.ty()
                                    .map(|t| t.syntax().text().to_string())
                                    .unwrap_or_default()
                            })
                            .collect();
                    }
                    decl_ty = f
                        .ret_type()
                        .and_then(|r| r.ty())
                        .map(|t| t.syntax().text().to_string())
                        .unwrap_or_default();
                }
                if (dk == "local" || dk == "parameter")
                    && let Some(parent) = n
                        .ancestors()
                        .skip(1)
                        .find(|n| matches!(kind(n).as_str(), "LET_STMT" | "PARAM"))
                {
                    decl_ty = ty(&parent);
                }
                if let Some(t) = self_type {
                    decl_ty = if namespace.is_empty() || t.contains("::") {
                        t
                    } else {
                        format!("{namespace}::{t}")
                    };
                }
                let bases = implementation
                    .and_then(|i| i.trait_())
                    .map(|t| vec![t.syntax().text().to_string()])
                    .unwrap_or_default();
                let name_span = name_node.as_ref().map(range).unwrap_or(header.clone());
                let decl_scope = if dk == "parameter" {
                    n.ancestors()
                        .skip(1)
                        .find(|n| matches!(kind(n).as_str(), "FN" | "CLOSURE_EXPR"))
                        .map(|n| range(&n))
                        .unwrap_or_else(|| scope(&n))
                } else {
                    scope(&n)
                };
                facts.declarations.push(Declaration {
                    name,
                    qualified,
                    kind: dk.into(),
                    namespace,
                    owner: owner_name,
                    name_span,
                    span,
                    header,
                    scope: decl_scope,
                    parameters,
                    ty: decl_ty,
                    access,
                    attributes,
                    modifiers: Vec::new(),
                    bases,
                });
            }
        }
        if k == "NAME_REF" {
            let name = normalize_name(&n.text().to_string());
            let mut call = false;
            let mut receiver = String::new();
            let mut arguments = None;
            if let Some(p) = n.parent()
                && let Some(m) = ast::MethodCallExpr::cast(p.clone())
                && m.name_ref().is_some_and(|r| r.syntax() == &n)
            {
                call = true;
                receiver = m
                    .receiver()
                    .map(|r| r.syntax().text().to_string())
                    .unwrap_or_default();
                arguments = m.arg_list().map(|a| a.args().count());
            }
            let mut expr = n.clone();
            while let Some(p) = expr.parent() {
                if matches!(
                    kind(&p).as_str(),
                    "PATH_SEGMENT" | "PATH" | "PATH_EXPR" | "FIELD_EXPR"
                ) {
                    expr = p;
                } else {
                    break;
                }
            }
            if kind(&expr) == "PATH_EXPR" {
                let written = expr.text().to_string();
                if written.ends_with(&n.text().to_string()) {
                    receiver = written
                        .rsplit_once("::")
                        .map(|(p, _)| p.to_owned())
                        .unwrap_or_default();
                    if let Some(p) = expr.parent().and_then(ast::CallExpr::cast) {
                        call = true;
                        arguments = p.arg_list().map(|a| a.args().count());
                    }
                }
            }
            if kind(&expr) == "FIELD_EXPR" {
                let written = expr.text().to_string();
                if written.ends_with(&name) {
                    receiver = written
                        .rsplit_once('.')
                        .map(|(p, _)| p.to_owned())
                        .unwrap_or_default();
                }
            }
            let write = expr.parent().is_some_and(|p| {
                if kind(&p) != "BIN_EXPR" {
                    return false;
                }
                let children = p.children().collect::<Vec<_>>();
                children.first() == Some(&expr)
                    && p.children_with_tokens()
                        .filter_map(|t| t.into_token())
                        .any(|t| {
                            matches!(
                                t.text(),
                                "=" | "+="
                                    | "-="
                                    | "*="
                                    | "/="
                                    | "%="
                                    | "|="
                                    | "&="
                                    | "^="
                                    | "<<="
                                    | ">>="
                            )
                        })
            });
            let opaque = n
                .ancestors()
                .any(|n| matches!(kind(&n).as_str(), "TOKEN_TREE" | "MACRO_CALL"));
            facts.occurrences.push(Occurrence {
                name,
                span: range(&n),
                call,
                construction: false,
                write,
                receiver,
                arguments,
                opaque,
            });
        }
    }
    // keep impl ownership tied to its written type, including imported aliases.
    for declaration in &mut facts.declarations {
        let expand = |written: &str| {
            let first = written.split([':', '<']).next().unwrap_or(written);
            facts
                .imports
                .iter()
                .find(|i| i.alias == first && i.scope.contains(&declaration.name_span.start))
                .map(|i| format!("{}{}", i.path, &written[first.len()..]))
                .unwrap_or_else(|| written.into())
        };
        declaration.owner = expand(&declaration.owner);
        declaration.ty = expand(&declaration.ty);
        declaration.bases = declaration.bases.iter().map(|base| expand(base)).collect();
        if declaration.kind == "impl" {
            declaration.qualified = declaration
                .ty
                .split('<')
                .next()
                .unwrap_or(&declaration.ty)
                .into();
        } else if !declaration.owner.is_empty() {
            declaration.qualified = format!("{}::{}", declaration.owner, declaration.name);
        }
    }
    Ok(facts)
}

fn imports(tree: ast::UseTree, prefix: &str, scope: Range<usize>, facts: &mut Facts) {
    let part = tree
        .path()
        .map(|p| p.syntax().text().to_string())
        .unwrap_or_default();
    let path = if prefix.is_empty() {
        part
    } else if part.is_empty() || part == "self" {
        prefix.into()
    } else {
        format!("{prefix}::{part}")
    };
    if let Some(list) = tree.use_tree_list() {
        for t in list.use_trees() {
            imports(t, &path, scope.clone(), facts);
        }
    } else {
        let alias = tree
            .rename()
            .and_then(|r| r.name())
            .map(|n| n.text().to_string())
            .unwrap_or_else(|| {
                if tree.star_token().is_some() {
                    "*".into()
                } else {
                    path.rsplit("::").next().unwrap_or("").into()
                }
            });
        facts.imports.push(Import {
            alias: normalize_name(&alias),
            path,
            scope,
            global: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn methods_and_modules() {
        let f=extract("mod inner { pub struct P; impl P { pub fn new() -> Self { Self } fn parse(&self, x: &str) {} } } fn run(p: inner::P) { p.parse(\"x\"); }", "2024").unwrap();
        assert!(
            f.declarations
                .iter()
                .any(|d| d.qualified == "inner::P::new" && d.kind == "method")
        );
        assert!(
            f.occurrences
                .iter()
                .any(|o| o.name == "parse" && o.call && o.receiver == "p")
        );
        assert!(!f.occurrences.iter().any(|o| o.name == "x"));
    }
}
