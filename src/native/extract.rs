use super::{
    DeclarationInfo, File, Include, OccurrenceInfo, Region, Role,
    lex::{Kind, Lexer},
};
use crate::model::{Declaration, Facts, Language, Occurrence, WriteKind};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::Range,
};
use tree_sitter::{Node, Parser, Tree};

pub fn extract(source: &str, language: Language) -> Result<Facts> {
    let (regions, host_errors) = if language == Language::ShaderLab {
        shader_regions(source)
    } else {
        (
            vec![Region {
                span: 0..source.len(),
                language,
            }],
            false,
        )
    };
    let mut facts = Facts {
        errors: host_errors,
        ..Default::default()
    };
    let mut native = File::default();
    for mut region in regions {
        let text = &source[region.span.clone()];
        // Included numeric tables have no declarations or references. Feeding a
        // long comma list to a translation-unit grammar causes quadratic recovery.
        if numeric_fragment(text) {
            if region.language == Language::Header {
                region.language = Language::Cpp;
            }
            native.regions.push(region);
            facts.errors = true; // It is still not a complete translation unit.
            continue;
        }
        let mut tree = parse(text, region.language)?;
        if region.language == Language::Header {
            region.language = Language::Cpp;
            if tree.root_node().has_error() {
                let c = parse(text, Language::C)?;
                if error_size(c.root_node()) < error_size(tree.root_node()) {
                    tree = c;
                    region.language = Language::C;
                }
            }
        }
        facts.errors |= tree.root_node().has_error();
        let index = native.regions.len() as u32;
        let offset = region.span.start;
        let scope = region.span.clone();
        native.regions.push(region);
        Walker {
            source,
            offset,
            region: index,
            facts: &mut facts,
            native: &mut native,
            owners: Vec::new(),
            access: vec!["public".into()],
            scopes: vec![Scope {
                span: scope,
                locals: HashMap::new(),
            }],
            conditional: 0,
            calls: HashMap::new(),
            writes: HashMap::new(),
        }
        .walk(tree.root_node());
    }
    // Declarations and uses occupy separate records. Sort once, keeping all sites.
    let declared: BTreeSet<_> = facts
        .declarations
        .iter()
        .map(|d| d.name_span.start)
        .collect();
    let mut occurrences: Vec<_> = std::mem::take(&mut facts.occurrences)
        .into_iter()
        .zip(std::mem::take(&mut native.occurrences))
        .filter(|(o, _)| !declared.contains(&o.span.start))
        .collect();
    occurrences.sort_by_key(|(o, _)| (o.span.start, o.span.end));
    for (occurrence, info) in occurrences {
        let index = facts.occurrences.len() as u32;
        native
            .names
            .entry(occurrence.name.clone())
            .or_default()
            .push(index);
        if occurrence.call
            && let Some(owner) = info.owner
        {
            native.calls.entry(owner).or_default().push(index);
        }
        facts.occurrences.push(occurrence);
        native.occurrences.push(info);
    }
    facts.native = Some(native);
    Ok(facts)
}

fn parse(source: &str, language: Language) -> Result<Tree> {
    let grammar = match language {
        Language::C => tree_sitter_c::LANGUAGE,
        Language::Cpp | Language::Header => tree_sitter_cpp::LANGUAGE,
        Language::Hlsl => tree_sitter_hlsl::LANGUAGE_HLSL,
        Language::Glsl => tree_sitter_glsl::LANGUAGE_GLSL,
        _ => unreachable!("native parser language"),
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar.into())?;
    parser
        .parse(source, None)
        .context("Native parsing was interrupted")
}

fn numeric_fragment(source: &str) -> bool {
    let mut number = true;
    let mut seen = false;
    for token in Lexer::new(source, 0..source.len()) {
        let text = &source[token.span];
        if number {
            if token.kind != Kind::Literal
                || !text.starts_with(|c: char| c.is_ascii_digit())
                || !text.bytes().all(|c| {
                    c.is_ascii_hexdigit()
                        || matches!(c, b'x' | b'X' | b'u' | b'U' | b'l' | b'L' | b'.' | b'\'')
                })
            {
                return false;
            }
            seen = true;
        } else if text != "," {
            return false;
        }
        number = !number;
    }
    seen
}

fn error_size(node: Node<'_>) -> usize {
    if node.is_error() || node.is_missing() {
        return node.byte_range().len().max(1);
    }
    if !node.has_error() {
        return 0;
    }
    node.children(&mut node.walk()).map(error_size).sum()
}

fn shader_regions(source: &str) -> (Vec<Region>, bool) {
    let mut regions = Vec::new();
    let mut open: Option<(usize, &str, Language)> = None;
    for token in Lexer::new(source, 0..source.len()) {
        if token.kind != Kind::Name {
            continue;
        }
        let word = &source[token.span.clone()];
        if let Some((start, end, language)) = open {
            if word == end {
                regions.push(Region {
                    span: start..token.span.start,
                    language,
                });
                open = None;
            }
        } else {
            open = match word {
                "HLSLPROGRAM" | "HLSLINCLUDE" => Some((token.span.end, "ENDHLSL", Language::Hlsl)),
                "CGPROGRAM" | "CGINCLUDE" => Some((token.span.end, "ENDCG", Language::Hlsl)),
                "GLSLPROGRAM" => Some((token.span.end, "ENDGLSL", Language::Glsl)),
                _ => None,
            };
        }
    }
    if let Some((start, _, language)) = open {
        regions.push(Region {
            span: start..source.len(),
            language,
        });
    }
    (regions, open.is_some())
}

struct Scope {
    span: Range<usize>,
    locals: HashMap<String, Vec<u32>>,
}

#[derive(Clone)]
struct Call {
    receiver: Option<Range<usize>>,
    qualified: bool,
    arguments: usize,
    indirect: bool,
    construction: bool,
}

#[derive(Clone)]
struct Write {
    expression: Range<usize>,
    indirect: bool,
}

struct Walker<'a> {
    source: &'a str,
    offset: usize,
    region: u32,
    facts: &'a mut Facts,
    native: &'a mut File,
    owners: Vec<u32>,
    access: Vec<String>,
    scopes: Vec<Scope>,
    conditional: usize,
    calls: HashMap<usize, Call>,
    writes: HashMap<usize, Write>,
}

impl Walker<'_> {
    fn span(&self, node: Node<'_>) -> Range<usize> {
        node.start_byte() + self.offset..node.end_byte() + self.offset
    }
    fn text(&self, node: Node<'_>) -> &str {
        &self.source[self.span(node)]
    }
    fn callable(&self) -> Option<u32> {
        self.owners
            .iter()
            .rev()
            .copied()
            .find(|&i| self.facts.declarations[i as usize].callable())
    }
    fn qualified_owner(&self) -> &str {
        self.owners
            .iter()
            .rev()
            .map(|&i| &self.facts.declarations[i as usize])
            .find(|d| d.kind != "scope")
            .map_or("", |d| d.qualified.as_str())
    }
    fn namespace(&self) -> String {
        self.owners
            .iter()
            .rev()
            .map(|&i| &self.facts.declarations[i as usize])
            .find(|d| d.kind == "namespace")
            .map_or_else(String::new, |d| d.qualified.clone())
    }
    fn children(&mut self, node: Node<'_>) {
        for child in node.named_children(&mut node.walk()) {
            self.walk(child);
        }
    }
    fn scoped(&mut self, node: Node<'_>, owner: Option<u32>, access: &str) {
        self.scopes.push(Scope {
            span: self.span(node),
            locals: HashMap::new(),
        });
        self.access.push(access.into());
        if let Some(owner) = owner {
            self.owners.push(owner);
        }
        self.children(node);
        if owner.is_some() {
            self.owners.pop();
        }
        self.access.pop();
        self.scopes.pop();
    }

    fn declare(&mut self, node: Node<'_>, name: Option<Node<'_>>, kind: &str) -> u32 {
        let written = name.map_or_else(String::new, |n| self.text(n).trim().into());
        let leaf = name
            .and_then(declarator_name)
            .unwrap_or_else(|| name.unwrap_or(node));
        let name_span = name.map_or_else(
            || self.span(node).start..self.span(node).start,
            |_| self.span(leaf),
        );
        let simple = super::simple_name(&written);
        let owner = self.qualified_owner().to_owned();
        let qualified =
            if super::written_components(&written).len() > 1 || written.starts_with("::") {
                let written = written.trim_start_matches("::");
                let namespace = self.namespace();
                if namespace.is_empty() || written.starts_with(&format!("{namespace}::")) {
                    written.into()
                } else {
                    format!("{namespace}::{written}")
                }
            } else if owner.is_empty() || written.is_empty() {
                written.clone()
            } else {
                format!("{owner}::{written}")
            };
        let mut parts = super::written_components(&qualified);
        parts.pop();
        let owner = parts.join("::");
        let span = self.span(node);
        let header_end = node
            .child_by_field_name("body")
            .map_or(span.end, |body| self.span(body).start);
        let ty = node
            .child_by_field_name("type")
            .map_or_else(String::new, |ty| self.text(ty).into());
        let modifiers = node
            .named_children(&mut node.walk())
            .filter(|n| {
                matches!(
                    n.kind(),
                    "storage_class_specifier"
                        | "type_qualifier"
                        | "virtual_specifier"
                        | "qualifiers"
                )
            })
            .map(|n| self.text(n).to_owned())
            .collect();
        let attributes = node
            .named_children(&mut node.walk())
            .filter(|n| {
                matches!(
                    n.kind(),
                    "attribute_declaration" | "attribute_specifier" | "hlsl_attribute"
                )
            })
            .map(|n| self.text(n).to_owned())
            .collect();
        let scope = self.scopes.last().unwrap().span.clone();
        let index = self.facts.declarations.len() as u32;
        self.facts.declarations.push(Declaration {
            name: simple,
            qualified,
            kind: kind.into(),
            namespace: self.namespace(),
            owner,
            name_span,
            span: span.clone(),
            header: span.start..header_end,
            scope,
            parameters: Vec::new(),
            ty,
            access: self.access.last().unwrap().clone(),
            attributes,
            modifiers,
            bases: Vec::new(),
        });
        self.native.declarations.push(DeclarationInfo {
            parent: self.owners.last().copied(),
            region: self.region,
            qualifiers: String::new(),
            conditional: self.conditional > 0,
        });
        if matches!(kind, "local" | "parameter") {
            let name = self.facts.declarations[index as usize].name.clone();
            self.scopes
                .last_mut()
                .unwrap()
                .locals
                .entry(name)
                .or_default()
                .push(index);
        }
        index
    }

    fn local(&self, name: &str, at: usize) -> Option<u32> {
        for scope in self.scopes.iter().rev() {
            let Some(locals) = scope.locals.get(name) else {
                continue;
            };
            let mut found = None;
            for &i in locals {
                if self.facts.declarations[i as usize].name_span.start > at {
                    continue;
                }
                if self.native.declarations[i as usize].conditional {
                    return None;
                }
                found = Some(i);
            }
            if found.is_some() {
                return found;
            }
        }
        None
    }

    fn occurrence(&mut self, name: String, span: Range<usize>, role: Role) {
        let call = self.calls.get(&span.start);
        let write = self.writes.get(&span.start);
        let receiver = call.and_then(|c| c.receiver.clone());
        let local = (receiver.is_none() && role != Role::MacroBody)
            .then(|| self.local(&name, span.start))
            .flatten();
        self.native.occurrences.push(OccurrenceInfo {
            owner: self.callable().or_else(|| self.owners.last().copied()),
            local,
            region: self.region,
            role,
            receiver,
            qualified: call.is_some_and(|c| c.qualified),
            assignment: write.map(|w| w.expression.clone()),
            indirect_write: write.is_some_and(|w| w.indirect),
        });
        self.facts.occurrences.push(Occurrence {
            role: crate::model::OccurrenceRole::Value,
            name,
            span,
            call: call.is_some(),
            construction: call.is_some_and(|c| c.construction),
            write: if write.is_some() {
                WriteKind::Direct
            } else {
                WriteKind::None
            },
            receiver: String::new(),
            arguments: call.map(|c| c.arguments),
            opaque: role == Role::MacroBody || call.is_some_and(|c| c.indirect),
        });
    }

    fn identifier(&mut self, node: Node<'_>) {
        let mut info = None;
        if let Some(parent) = node.parent() {
            if parent.kind() == "field_expression"
                && parent.child_by_field_name("field") == Some(node)
            {
                info = parent
                    .child_by_field_name("argument")
                    .map(|r| (self.span(r), false));
            } else if parent.kind() == "qualified_identifier"
                && parent.child_by_field_name("name") == Some(node)
            {
                info = parent
                    .child_by_field_name("scope")
                    .map(|r| (self.span(r), true));
            }
        }
        let role = match node.kind() {
            "type_identifier" | "namespace_identifier" => Role::Type,
            "statement_identifier" => Role::Label,
            _ => Role::Identifier,
        };
        self.occurrence(self.text(node).to_owned(), self.span(node), role);
        if let Some((receiver, qualified)) = info {
            let last = self.native.occurrences.last_mut().unwrap();
            last.receiver = Some(receiver);
            last.qualified = qualified;
            last.local = None;
        }
    }

    fn function(&mut self, node: Node<'_>, declarator: Node<'_>, definition: bool) -> Option<u32> {
        let name = declarator_name(declarator)?;
        let conversion = name.kind() == "operator_cast";
        let function = if conversion {
            let mut layer = declarator_child(name)?;
            while layer.kind() != "abstract_function_declarator" {
                layer = declarator_child(layer)?;
            }
            layer
        } else {
            function_layer(declarator, name)?
        };
        let written_name = qualified_declarator_name(declarator).unwrap_or(name);
        let leaf = self.text(name);
        let owner = self.qualified_owner().rsplit("::").next().unwrap_or("");
        let written_owner = self
            .text(written_name)
            .rsplit_once("::")
            .map(|(p, _)| p.rsplit("::").next().unwrap_or(p));
        let kind = if leaf.starts_with("operator") {
            "operator"
        } else if !leaf.is_empty() && (leaf == owner || written_owner == Some(leaf)) {
            "constructor"
        } else if self
            .owners
            .iter()
            .any(|&i| self.facts.declarations[i as usize].named_type())
        {
            "method"
        } else {
            "function"
        };
        let index = self.declare(node, Some(written_name), kind);
        self.facts.declarations[index as usize].name = super::simple_name(self.text(name));
        self.facts.declarations[index as usize].name_span = self.span(name);
        if let Some(parameters) = function.child_by_field_name("parameters") {
            if conversion {
                let name_span = self.span(name).start..self.span(parameters).start;
                let written = self.source[name_span.clone()].trim_end();
                let suffix = self.text(name).len() - written.len();
                let decl = &mut self.facts.declarations[index as usize];
                decl.qualified.truncate(decl.qualified.len() - suffix);
                decl.name = written.into();
                decl.name_span = name_span.start..name_span.start + written.len();
            }
            self.facts.declarations[index as usize].parameters = parameters
                .named_children(&mut parameters.walk())
                .filter(|p| {
                    matches!(
                        p.kind(),
                        "parameter_declaration"
                            | "optional_parameter_declaration"
                            | "variadic_parameter_declaration"
                            | "variadic_parameter"
                    )
                })
                .map(|p| self.parameter_type(p))
                .collect();
            let start = parameters.end_byte() + self.offset;
            self.native.declarations[index as usize].qualifiers = self.source
                [start..function.end_byte() + self.offset]
                .trim()
                .into();
            let qualifiers = &self.native.declarations[index as usize].qualifiers;
            if !qualifiers.is_empty() {
                self.facts.declarations[index as usize]
                    .modifiers
                    .push(qualifiers.clone());
            }
        }
        self.facts.declarations[index as usize].scope = self.span(node);
        if !definition {
            self.facts.declarations[index as usize].header = self.span(node);
        }
        Some(index)
    }

    fn parameter_type(&self, node: Node<'_>) -> String {
        let end = node
            .child_by_field_name("default_value")
            .map_or(node.end_byte(), |value| {
                let prefix =
                    &self.source[node.start_byte() + self.offset..value.start_byte() + self.offset];
                node.start_byte() + prefix.rfind('=').unwrap_or(prefix.len())
            })
            + self.offset;
        let span = node.start_byte() + self.offset..end;
        let Some(name) = node
            .child_by_field_name("declarator")
            .and_then(declarator_name)
        else {
            return self.source[span].trim().into();
        };
        let name = self.span(name);
        format!(
            "{}{}",
            &self.source[span.start..name.start],
            &self.source[name.end..span.end]
        )
        .trim()
        .into()
    }

    fn declaration(&mut self, node: Node<'_>, alias: bool) {
        let mut functions = BTreeMap::new();
        for i in 0..node.child_count() {
            if node.field_name_for_child(i as u32) != Some("declarator") {
                continue;
            }
            let declarator = node.child(i).unwrap();
            if let Some(binding) = structured_binding(declarator) {
                for name in binding.named_children(&mut binding.walk()) {
                    self.declare(
                        node,
                        Some(name),
                        if self.callable().is_some() {
                            "local"
                        } else {
                            "field"
                        },
                    );
                }
                continue;
            }
            let Some(name) = declarator_name(declarator) else {
                continue;
            };
            if !alias && let Some(function) = self.function(node, declarator, false) {
                functions.insert(declarator.id(), function);
                continue;
            }
            let kind = if alias {
                "type"
            } else if self.callable().is_some() {
                "local"
            } else if node
                .named_children(&mut node.walk())
                .any(|n| n.kind() == "storage_class_specifier" && self.text(n) == "static")
            {
                "static"
            } else {
                "field"
            };
            self.declare(node, Some(name), kind);
        }
        for child in node.named_children(&mut node.walk()) {
            if let Some(&owner) = functions.get(&child.id()) {
                self.scoped(child, Some(owner), "public");
            } else {
                self.walk(child);
            }
        }
    }

    fn call(&mut self, node: Node<'_>) {
        let construction = node.kind() == "new_expression";
        let Some(callee) = node.child_by_field_name(if construction { "type" } else { "function" })
        else {
            return;
        };
        let arguments = node.child_by_field_name("arguments").map_or(0, |args| {
            args.named_children(&mut args.walk())
                .filter(|n| n.kind() != "comment")
                .count()
        });
        if let Some((name, receiver, qualified, indirect)) = callee_name(callee) {
            self.calls.insert(
                self.span(name).start,
                Call {
                    receiver: receiver.map(|r| self.span(r)),
                    qualified,
                    arguments,
                    indirect,
                    construction,
                },
            );
        } else {
            // The second call in factory()() has no identifier of its own.
            let span = node
                .child_by_field_name("arguments")
                .map_or(self.span(callee), |a| {
                    self.span(a).start..self.span(a).start + 1
                });
            self.calls.insert(
                span.start,
                Call {
                    receiver: Some(self.span(callee)),
                    qualified: false,
                    arguments,
                    indirect: true,
                    construction,
                },
            );
            self.occurrence("<expression>".into(), span, Role::Identifier);
        }
    }

    fn write(&mut self, expression: Node<'_>) {
        if let Some((name, indirect)) = lvalue(expression) {
            self.writes.insert(
                self.span(name).start,
                Write {
                    expression: self.span(expression),
                    indirect,
                },
            );
        } else {
            let expression = self.span(expression);
            let span = expression.start..expression.start + 1;
            self.writes.insert(
                span.start,
                Write {
                    expression,
                    indirect: true,
                },
            );
            self.occurrence("<expression>".into(), span, Role::Identifier);
        }
    }

    fn macro_definition(&mut self, node: Node<'_>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let owner = self.declare(node, Some(name), "macro");
        self.owners.push(owner);
        self.scopes.push(Scope {
            span: self.span(node),
            locals: HashMap::new(),
        });
        if let Some(parameters) = node.child_by_field_name("parameters") {
            for parameter in parameters
                .named_children(&mut parameters.walk())
                .filter(|n| n.kind() == "identifier")
            {
                self.declare(parameter, Some(parameter), "parameter");
            }
        }
        if let Some(value) = node.child_by_field_name("value") {
            for token in Lexer::new(self.source, self.span(value)).filter(|t| t.kind == Kind::Name)
            {
                self.occurrence(
                    self.source[token.span.clone()].into(),
                    token.span,
                    Role::MacroBody,
                );
            }
        }
        self.scopes.pop();
        self.owners.pop();
    }

    fn directive(&mut self, node: Node<'_>) {
        if node.kind() == "preproc_include" {
            if let Some(path) = node.child_by_field_name("path") {
                let text = self.text(path);
                if text.starts_with(['\"', '<']) && text.len() >= 2 {
                    let span = self.span(path);
                    self.native.includes.push(Include {
                        path: text[1..text.len() - 1].into(),
                        span: span.start + 1..span.end - 1,
                        relative: text.starts_with('"'),
                    });
                } else {
                    self.walk(path);
                }
            }
            return;
        }
        let mut tokens = Lexer::new(self.source, self.span(node));
        let words: Vec<_> = tokens
            .by_ref()
            .filter(|t| t.kind == Kind::Name)
            .take(3)
            .collect();
        if words.len() == 3
            && &self.source[words[0].span.clone()] == "pragma"
            && matches!(
                &self.source[words[1].span.clone()],
                "vertex" | "fragment" | "geometry" | "hull" | "domain" | "kernel" | "surface"
            )
        {
            let name = &words[2];
            self.occurrence(
                self.source[name.span.clone()].into(),
                name.span.clone(),
                Role::EntryPoint,
            );
        }
    }

    fn walk(&mut self, node: Node<'_>) {
        match node.kind() {
            "namespace_definition" => {
                let owner = self.declare(node, node.child_by_field_name("name"), "namespace");
                self.scoped(node, Some(owner), "public");
                return;
            }
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
            | "cbuffer_specifier" => {
                let kind = match node.kind() {
                    "class_specifier" => "class",
                    "enum_specifier" => "enum",
                    "union_specifier" => "union",
                    _ => "struct",
                };
                let name = node.child_by_field_name("name");
                let owner = self.declare(node, name, if name.is_some() { kind } else { "scope" });
                if let Some(base) = node
                    .named_children(&mut node.walk())
                    .find(|n| n.kind() == "base_class_clause")
                {
                    self.facts.declarations[owner as usize].bases =
                        crate::query::split_parameters(self.text(base).trim_start_matches(':'))
                            .unwrap_or_default();
                }
                self.scoped(
                    node,
                    Some(owner),
                    if kind == "class" { "private" } else { "public" },
                );
                return;
            }
            "function_definition" => {
                if let Some(declarator) = node.child_by_field_name("declarator")
                    && let Some(owner) = self.function(node, declarator, true)
                {
                    self.scoped(node, Some(owner), "public");
                    return;
                }
            }
            "declaration" | "field_declaration" => {
                self.declaration(node, false);
                return;
            }
            "type_definition" => {
                self.declaration(node, true);
                return;
            }
            "alias_declaration" | "concept_definition" => {
                self.declare(node, node.child_by_field_name("name"), "type");
            }
            "enumerator" => {
                self.declare(node, node.child_by_field_name("name"), "variant");
            }
            "parameter_declaration"
            | "optional_parameter_declaration"
            | "variadic_parameter_declaration"
            | "type_parameter_declaration"
            | "variadic_type_parameter_declaration" => {
                let name = node
                    .child_by_field_name("declarator")
                    .and_then(declarator_name)
                    .or_else(|| node.child_by_field_name("name"))
                    .or_else(|| {
                        node.named_children(&mut node.walk()).find(|n| {
                            n.kind() == "type_identifier" && node.kind().contains("type_parameter")
                        })
                    });
                if let Some(name) = name {
                    let owner = self.declare(node, Some(name), "parameter");
                    self.facts.declarations[owner as usize].ty = self.parameter_type(node);
                }
            }
            "function_declarator" => {
                let current = self
                    .owners
                    .last()
                    .map(|&i| &self.facts.declarations[i as usize]);
                let name = declarator_name(node).map(|n| self.span(n));
                if !current.is_some_and(|d| d.callable() && Some(d.name_span.clone()) == name) {
                    self.scoped(node, None, "public");
                    return;
                }
            }
            "for_range_loop" => {
                self.scopes.push(Scope {
                    span: self.span(node),
                    locals: HashMap::new(),
                });
                let body = node.child_by_field_name("body");
                for child in node
                    .named_children(&mut node.walk())
                    .filter(|child| Some(*child) != body)
                {
                    self.walk(child);
                }
                if let Some(declarator) = node.child_by_field_name("declarator") {
                    if let Some(binding) = structured_binding(declarator) {
                        for name in binding.named_children(&mut binding.walk()) {
                            self.declare(node, Some(name), "local");
                        }
                    } else if let Some(name) = declarator_name(declarator) {
                        self.declare(node, Some(name), "local");
                    }
                }
                if let Some(body) = body {
                    self.walk(body);
                }
                self.scopes.pop();
                return;
            }
            "template_declaration"
            | "compound_statement"
            | "for_statement"
            | "while_statement"
            | "if_statement"
            | "switch_statement"
            | "catch_clause" => {
                self.scoped(node, None, self.access.last().unwrap().clone().as_str());
                return;
            }
            "lambda_expression" => {
                let owner = self.declare(node, None, "lambda");
                let at = self.span(node).start;
                self.facts.declarations[owner as usize].name = "<lambda>".into();
                self.facts.declarations[owner as usize].qualified =
                    format!("{}::<lambda@{at}>", self.qualified_owner());
                self.scoped(node, Some(owner), "public");
                return;
            }
            "preproc_def" | "preproc_function_def" => {
                self.macro_definition(node);
                return;
            }
            "preproc_include" | "preproc_call" => {
                self.directive(node);
                return;
            }
            "preproc_if" | "preproc_ifdef" | "preproc_elif" | "preproc_else" => {
                self.conditional += 1;
                self.children(node);
                self.conditional -= 1;
                return;
            }
            "call_expression" | "new_expression" => self.call(node),
            "assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    self.write(left);
                }
            }
            "update_expression" => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    self.write(argument);
                }
            }
            "access_specifier" => {
                *self.access.last_mut().unwrap() = self.text(node).into();
            }
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "namespace_identifier"
            | "statement_identifier" => {
                self.identifier(node);
                return;
            }
            "primitive_type" if self.calls.contains_key(&self.span(node).start) => {
                self.identifier(node);
                return;
            }
            "operator_name" | "destructor_name" => {
                self.occurrence(
                    super::simple_name(self.text(node)),
                    self.span(node),
                    Role::Identifier,
                );
                return;
            }
            _ => (),
        }
        self.children(node);
    }
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "type_identifier"
        | "namespace_identifier"
        | "operator_name"
        | "operator_cast"
        | "destructor_name" => Some(node),
        "qualified_identifier" | "template_function" | "template_type" | "template_method" => {
            node.child_by_field_name("name").and_then(declarator_name)
        }
        "dependent_name" => node.named_child(0).and_then(declarator_name),
        _ => declarator_child(node).and_then(declarator_name),
    }
}

fn qualified_declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "qualified_identifier" | "template_function" | "template_type" => Some(node),
        "identifier" | "field_identifier" | "type_identifier" | "operator_name"
        | "operator_cast" | "destructor_name" => Some(node),
        _ => declarator_child(node).and_then(qualified_declarator_name),
    }
}

fn declarator_child(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("declarator").or_else(|| {
        matches!(
            node.kind(),
            "reference_declarator"
                | "abstract_reference_declarator"
                | "parenthesized_declarator"
                | "attributed_declarator"
                | "variadic_declarator"
        )
        .then(|| node.named_child(0))
        .flatten()
    })
}

fn structured_binding(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        if node.kind() == "structured_binding_declarator" {
            return Some(node);
        }
        node = declarator_child(node)?;
    }
}

fn function_layer<'a>(root: Node<'a>, name: Node<'a>) -> Option<Node<'a>> {
    let mut node = name;
    loop {
        if node.kind() == "function_declarator" {
            return Some(node);
        }
        if matches!(
            node.kind(),
            "pointer_declarator" | "reference_declarator" | "array_declarator"
        ) || node == root
        {
            return None;
        }
        node = node.parent()?;
    }
}

fn callee_name(node: Node<'_>) -> Option<(Node<'_>, Option<Node<'_>>, bool, bool)> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "primitive_type"
        | "operator_name" | "destructor_name" => Some((node, None, false, false)),
        "qualified_identifier" => Some((
            declarator_name(node)?,
            node.child_by_field_name("scope"),
            true,
            false,
        )),
        "field_expression" => Some((
            declarator_name(node.child_by_field_name("field")?)?,
            node.child_by_field_name("argument"),
            false,
            false,
        )),
        "template_function" | "template_method" | "template_type" => {
            callee_name(node.child_by_field_name("name")?)
        }
        "parenthesized_expression" => callee_name(node.named_child(0)?),
        "pointer_expression" | "subscript_expression" => {
            let (name, receiver, qualified, _) =
                callee_name(node.child_by_field_name("argument")?)?;
            Some((name, receiver, qualified, true))
        }
        _ => None,
    }
}

fn lvalue(node: Node<'_>) -> Option<(Node<'_>, bool)> {
    match node.kind() {
        "identifier" | "field_identifier" => Some((node, false)),
        "qualified_identifier" => Some((declarator_name(node)?, false)),
        "field_expression" => Some((declarator_name(node.child_by_field_name("field")?)?, false)),
        "parenthesized_expression" => lvalue(node.named_child(0)?),
        "subscript_expression" | "pointer_expression" => {
            Some((lvalue(node.child_by_field_name("argument")?)?.0, true))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_include_fragments_preserve_content_without_error_recovery() {
        let source = "/* data */ 0xff, 0x01, 0u, 12,\n".repeat(1024);
        let facts = extract(&source, Language::Cpp).unwrap();
        assert!(facts.declarations.is_empty() && facts.occurrences.is_empty());
        assert_eq!(facts.native.unwrap().regions[0].span, 0..source.len());
        assert!(!numeric_fragment("1, lookup(), 2"));
        assert!(!numeric_fragment("[]{}()"));
        assert!(!numeric_fragment("12_user_literal,"));
    }

    #[test]
    fn operators_templates_bindings_and_indirect_writes() {
        let source = r#"
template<class T> struct Box {
    T value;
    T& operator[](int i) & { return value; }
    explicit operator bool() const { return true; }
    template<class U> U get() { return U{}; }
};
template<> struct Box<int> { int value; };
void work(int *out, Box<int> &box) {
    auto [key, value] = pair();
    value++;
    for (auto item : items) { item++; }
    auto f = [&](int value) { return compute(value); };
    out[0] = 1;
    *out = 2;
    box.value++;
    box.template get<int>();
}
"#;
        let facts = extract(source, Language::Cpp).unwrap();
        assert!(
            !facts.errors,
            "{}",
            parse(source, Language::Cpp).unwrap().root_node().to_sexp()
        );
        let native = facts.native.as_ref().unwrap();
        assert!(
            facts.declarations.iter().any(|d| d.name == "operator bool"
                && d.parameters.is_empty()
                && d.kind == "operator")
        );
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.name == "operator[]" && d.kind == "operator")
        );
        let boxes: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.name == "Box")
            .collect();
        assert_eq!(boxes.len(), 2);
        assert_ne!(boxes[0].qualified, boxes[1].qualified);
        for name in ["item", "value"] {
            assert!(
                facts
                    .occurrences
                    .iter()
                    .zip(&native.occurrences)
                    .any(|(o, i)| o.name == name
                        && o.write == WriteKind::Direct
                        && i.local.is_some())
            );
        }
        assert!(
            facts.occurrences.iter().any(|o| o.name == "get" && o.call),
            "{}",
            parse(source, Language::Cpp).unwrap().root_node().to_sexp()
        );
        assert_eq!(
            facts
                .occurrences
                .iter()
                .zip(&native.occurrences)
                .filter(|(o, i)| o.name == "out"
                    && o.write == WriteKind::Direct
                    && i.indirect_write)
                .count(),
            2
        );
        let lambda = facts
            .declarations
            .iter()
            .position(|d| d.kind == "lambda")
            .unwrap() as u32;
        assert!(
            native.calls[&lambda]
                .iter()
                .any(|&i| facts.occurrences[i as usize].name == "compute")
        );
    }

    #[test]
    fn declarations_bodies_shadowing_and_writes() {
        let source = "namespace game { struct Renderer { void flush(); void flush() const; int position; }; void render(Renderer& r, void (*callback)()) { int x = 0; x++; { int x = 1; x += 2; } r.position = x; r.flush(); callback(); } }";
        let facts = extract(source, Language::Cpp).unwrap();
        let native = facts.native.as_ref().unwrap();
        assert!(!facts.errors);
        let flush: Vec<_> = facts
            .declarations
            .iter()
            .enumerate()
            .filter(|(_, d)| d.name == "flush")
            .collect();
        assert_eq!(flush.len(), 2);
        assert_ne!(flush[0].1.name_span, flush[1].1.name_span);
        assert_ne!(
            native.declarations[flush[0].0].qualifiers,
            native.declarations[flush[1].0].qualifiers
        );
        let render = facts
            .declarations
            .iter()
            .position(|d| d.name == "render")
            .unwrap() as u32;
        let calls: Vec<_> = native.calls[&render]
            .iter()
            .map(|&i| facts.occurrences[i as usize].name.as_str())
            .collect();
        assert_eq!(calls, ["flush", "callback"]);
        let writes: Vec<_> = facts
            .occurrences
            .iter()
            .zip(&native.occurrences)
            .filter(|(o, _)| o.name == "x" && o.write != WriteKind::None)
            .collect();
        assert_eq!(writes.len(), 2);
        assert!(writes.iter().all(|(_, info)| info.local.is_some()));
        assert_ne!(writes[0].1.local, writes[1].1.local);
        assert!(
            facts
                .occurrences
                .iter()
                .filter(|o| o.name == "r")
                .all(|o| o.write == WriteKind::None)
        );
        assert!(
            facts
                .occurrences
                .iter()
                .any(|o| o.name == "position" && o.write == WriteKind::Direct)
        );
    }

    #[test]
    fn macro_bodies_are_written_references_not_calls() {
        let source = "#define TRACE(x) log(x), \"ignored\", R\"tag(raw_name)tag\" /* comment_name */\nvoid f() { TRACE(value); external(); }\n";
        let facts = extract(source, Language::Cpp).unwrap();
        let native = facts.native.as_ref().unwrap();
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.kind == "macro" && d.name == "TRACE")
        );
        assert!(
            facts
                .occurrences
                .iter()
                .zip(&native.occurrences)
                .any(|(o, i)| o.name == "log" && !o.call && i.role == Role::MacroBody)
        );
        for hidden in ["ignored", "raw_name", "comment_name"] {
            assert!(!native.names.contains_key(hidden));
        }
        assert!(
            facts
                .occurrences
                .iter()
                .any(|o| o.name == "external" && o.call)
        );
    }

    #[test]
    fn shader_blocks_keep_original_positions_and_distinct_scopes() {
        let source = "Shader \"Test\" { // HLSLPROGRAM ignored\nHLSLINCLUDE\nfloat shade(float x) { return x; }\nENDHLSL\nPass { HLSLPROGRAM\n#pragma fragment frag\nfloat4 frag(float2 uv : TEXCOORD0) : SV_Target { return shade(uv.x); }\nENDHLSL }\nPass { GLSLPROGRAM\nlayout(location=0) out vec4 color;\nvoid main() { color = vec4(1); }\nENDGLSL } }";
        let facts = extract(source, Language::ShaderLab).unwrap();
        let native = facts.native.as_ref().unwrap();
        assert_eq!(native.regions.len(), 3);
        for declaration in facts.declarations.iter().filter(|d| !d.name.is_empty()) {
            assert_eq!(&source[declaration.name_span.clone()], declaration.name);
        }
        assert_eq!(
            facts
                .declarations
                .iter()
                .filter(|d| d.name == "shade")
                .count(),
            1
        );
        assert!(
            facts
                .occurrences
                .iter()
                .zip(&native.occurrences)
                .any(|(o, i)| o.name == "frag" && i.role == Role::EntryPoint)
        );
        assert!(
            facts
                .occurrences
                .iter()
                .any(|o| o.name == "shade" && o.call)
        );
        assert!(
            facts
                .occurrences
                .iter()
                .any(|o| o.name == "color" && o.write == WriteKind::Direct)
        );
    }
}
