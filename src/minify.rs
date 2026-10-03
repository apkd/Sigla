//! A read-only source rendering pass. Edits use original byte spans, including for line slices.
use crate::model::Language;
use ra_ap_syntax::{AstNode, NodeOrToken, SourceFile, SyntaxKind, ast};
use std::ops::Range;

struct Edit {
    range: Range<usize>,
    text: String,
}
struct Token {
    range: Range<usize>,
    opaque: bool,
}

pub fn render(source: &str, language: Language, range: Range<usize>) -> String {
    let (tokens, mut edits) = match language {
        Language::Rust => rust(source),
        Language::CSharp => csharp(source, &range),
        _ => return source[range].into(),
    };
    edits.extend(spacing(source, &tokens));
    if let Some(first) = tokens.iter().find(|t| t.range.end > range.start)
        && first.range.start >= range.start
        && first.range.start <= range.end
        && source[range.start..first.range.start]
            .chars()
            .all(char::is_whitespace)
    {
        edits.push(Edit {
            range: range.start..first.range.start,
            text: String::new(),
        });
    }
    apply(source, range, edits)
}

fn apply(source: &str, range: Range<usize>, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| (e.range.start, std::cmp::Reverse(e.range.end)));
    let mut cursor = range.start;
    let mut text = String::new();
    for edit in edits {
        if edit.range.start < cursor || edit.range.end > range.end {
            continue;
        }
        text.push_str(&source[cursor..edit.range.start]);
        text.push_str(&edit.text);
        cursor = edit.range.end;
    }
    text.push_str(&source[cursor..range.end]);
    text
}

fn compact(source: &str, range: Range<usize>, tokens: &[Token]) -> String {
    let start = tokens.partition_point(|t| t.range.end <= range.start);
    let end = tokens.partition_point(|t| t.range.start < range.end);
    apply(source, range, spacing(source, &tokens[start..end]))
}

fn spacing(source: &str, tokens: &[Token]) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut end = tokens.first().map_or(0, |t| t.range.start);
    let mut previous: Option<&Token> = None;
    for token in tokens {
        let gap = &source[end..token.range.start];
        if !gap.is_empty() && gap.chars().all(char::is_whitespace) {
            let left = previous.map(|t| &source[t.range.clone()]).unwrap_or("");
            let right = &source[token.range.clone()];
            let text = match previous {
                None => "",
                Some(previous) if gap.contains('\n') => {
                    // Keep comments and directives on their own lines; retain source statement layout.
                    if right == "{" && !previous.opaque {
                        ""
                    } else {
                        "\n"
                    }
                }
                Some(previous) if previous.opaque || token.opaque || separator(left, right) => " ",
                Some(_) => "",
            };
            edits.push(Edit {
                range: end..token.range.start,
                text: text.into(),
            });
        }
        end = token.range.end;
        previous = Some(token);
    }
    edits
}

fn separator(left: &str, right: &str) -> bool {
    let a = left.chars().last().unwrap_or(' ');
    let b = right.chars().next().unwrap_or(' ');
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '@' || c == '#';
    if word(a) && word(b) {
        return true;
    }
    // Do not create a new operator, comment, lifetime, string prefix, or numeric token.
    matches!(
        (a, b),
        ('+', '+')
            | ('-', '-')
            | ('/', '/' | '*')
            | ('*', '/')
            | ('&', '&')
            | ('|', '|')
            | (':', ':')
            | ('.', '.')
            | ('?', '?' | '.' | '[')
            | ('<', '<' | '=')
            | ('>', '>' | '=')
            | ('=', '=' | '>')
            | ('!' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^', '=')
            | ('-', '>')
    ) || (a.is_ascii_digit() && b == '.')
        || (a == '.' && b.is_ascii_digit())
        || (matches!(a, '\'' | '"' | '#' | '@' | '$') && word(b))
        || (word(a) && matches!(b, '\'' | '"' | '#' | '@' | '$'))
}

fn rust(source: &str) -> (Vec<Token>, Vec<Edit>) {
    let parsed = SourceFile::parse(source, ra_ap_syntax::Edition::Edition2024);
    let root = parsed.syntax_node();
    let mut tokens = Vec::new();
    fn visit(node: ra_ap_syntax::SyntaxNode, tokens: &mut Vec<Token>) {
        if matches!(
            node.kind(),
            SyntaxKind::TOKEN_TREE | SyntaxKind::ERROR | SyntaxKind::ATTR
        ) {
            tokens.push(Token {
                range: node.text_range().into(),
                opaque: true,
            });
            return;
        }
        for child in node.children_with_tokens() {
            match child {
                NodeOrToken::Node(node) => visit(node, tokens),
                NodeOrToken::Token(t) if t.kind() != SyntaxKind::WHITESPACE => tokens.push(Token {
                    range: t.text_range().into(),
                    opaque: t.kind() == SyntaxKind::COMMENT,
                }),
                _ => (),
            }
        }
    }
    visit(root.clone(), &mut tokens);
    let mut edits = Vec::new();
    if !parsed.errors().is_empty() {
        return (tokens, edits);
    }
    for node in root.descendants() {
        if node
            .ancestors()
            .any(|n| matches!(n.kind(), SyntaxKind::TOKEN_TREE | SyntaxKind::ATTR))
        {
            continue;
        }
        if let Some(field) = ast::RecordExprField::cast(node.clone())
            && let (Some(name), Some(expr), Some(_)) =
                (field.name_ref(), field.expr(), field.colon_token())
        {
            let name = name.syntax().text().to_string();
            if expr.syntax().text().to_string() == name && !has_rust_comment(&node) {
                // Keep field attributes outside the replaced name/expression span.
                let start = field
                    .name_ref()
                    .unwrap()
                    .syntax()
                    .text_range()
                    .start()
                    .into();
                edits.push(Edit {
                    range: start..expr.syntax().text_range().end().into(),
                    text: name,
                });
            }
        }
        if let Some(tree) = ast::UseTree::cast(node.clone())
            && let Some(list) = tree.use_tree_list()
        {
            let children: Vec<_> = list.use_trees().collect();
            if children.len() == 1 && !has_rust_comment(&node) {
                let child = &children[0];
                // `prefix::{self}` is not `prefix::self`.
                if !child.syntax().text().to_string().contains("self") {
                    edits.push(Edit {
                        range: list.syntax().text_range().into(),
                        text: compact(source, child.syntax().text_range().into(), &tokens),
                    });
                }
            }
        }
        // Optional trailing commas; tuple and pattern punctuation remains untouched.
        if matches!(
            node.kind(),
            SyntaxKind::ARG_LIST
                | SyntaxKind::PARAM_LIST
                | SyntaxKind::ARRAY_EXPR
                | SyntaxKind::RECORD_EXPR_FIELD_LIST
                | SyntaxKind::RECORD_FIELD_LIST
                | SyntaxKind::VARIANT_LIST
                | SyntaxKind::USE_TREE_LIST
        ) {
            let significant: Vec<_> = node
                .children_with_tokens()
                .filter(|n| !matches!(n.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
                .collect();
            if significant.len() >= 2 {
                let comma = &significant[significant.len() - 2];
                if comma.kind() == SyntaxKind::COMMA {
                    edits.push(Edit {
                        range: comma.text_range().into(),
                        text: String::new(),
                    });
                }
            }
        }
    }
    // Adjacent plain imports in the same scope can share their prefix. Attributes are retained.
    for parent in root.descendants() {
        let children: Vec<_> = parent.children().collect();
        let mut i = 0;
        while i < children.len() {
            let Some(first) = plain_use(&children[i], source, &tokens) else {
                i += 1;
                continue;
            };
            let mut tails = vec![first.2];
            let mut end = children[i].text_range().end();
            let mut j = i + 1;
            while j < children.len() {
                let gap = &source[usize::from(end)..usize::from(children[j].text_range().start())];
                let Some(next) = plain_use(&children[j], source, &tokens) else {
                    break;
                };
                if !gap.chars().all(char::is_whitespace) || next.0 != first.0 || next.1 != first.1 {
                    break;
                }
                tails.push(next.2);
                end = children[j].text_range().end();
                j += 1;
            }
            if tails.len() > 1 {
                edits.push(Edit {
                    range: usize::from(children[i].text_range().start())..usize::from(end),
                    text: format!("{}use {}::{{{}}};", first.0, first.1, tails.join(",")),
                });
            }
            i = j;
        }
    }
    (tokens, edits)
}

fn has_rust_comment(node: &ra_ap_syntax::SyntaxNode) -> bool {
    node.descendants_with_tokens()
        .any(|n| n.kind() == SyntaxKind::COMMENT)
}
fn plain_use(
    node: &ra_ap_syntax::SyntaxNode,
    source: &str,
    tokens: &[Token],
) -> Option<(String, String, String)> {
    use ast::{HasAttrs, HasVisibility};
    let item = ast::Use::cast(node.clone())?;
    if item.attrs().next().is_some() || has_rust_comment(node) {
        return None;
    }
    let tree = item.use_tree()?;
    if tree.use_tree_list().is_some() {
        return None;
    }
    let text = compact(source, tree.syntax().text_range().into(), tokens);
    let (prefix, tail) = text.rsplit_once("::")?;
    // An alias path must not be mistaken for the imported prefix.
    if prefix.contains(" as ") {
        return None;
    }
    let visibility = item
        .visibility()
        .map(|v| format!("{} ", v.syntax().text()))
        .unwrap_or_default();
    Some((visibility, prefix.into(), tail.into()))
}

fn csharp(source: &str, selection: &Range<usize>) -> (Vec<Token>, Vec<Edit>) {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut tokens = Vec::new();
    fn visit(node: tree_sitter::Node<'_>, tokens: &mut Vec<Token>) {
        let kind = node.kind();
        let opaque = kind.contains("string")
            || kind == "character_literal"
            || kind == "comment"
            || kind.starts_with("preproc_")
            || node.is_error();
        if opaque || node.child_count() == 0 {
            if !node.is_missing() && node.end_byte() > node.start_byte() {
                tokens.push(Token {
                    range: node.byte_range(),
                    opaque,
                });
            }
        } else {
            for child in node.children(&mut node.walk()) {
                visit(child, tokens);
            }
        }
    }
    visit(tree.root_node(), &mut tokens);
    let mut edits = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let start = tokens[index].range.start;
        let text = &source[tokens[index].range.clone()];
        if text.starts_with("///") || text.starts_with("/**") {
            let mut end = tokens[index].range.end;
            if text.starts_with("///") {
                while let Some(next) = tokens.get(index + 1)
                    && source[end..next.range.start]
                        .chars()
                        .all(char::is_whitespace)
                    && source[next.range.clone()].starts_with("///")
                {
                    index += 1;
                    end = next.range.end;
                }
            }
            if let Some(text) = xml_doc(&source[start..end]) {
                edits.push(Edit {
                    range: start..end,
                    text,
                });
            }
        }
        index += 1;
    }
    fn walk(
        node: tree_sitter::Node<'_>,
        source: &str,
        tokens: &[Token],
        edits: &mut Vec<Edit>,
        selection: &Range<usize>,
    ) {
        if node.has_error() {
            // Still simplify independent valid siblings.
            if node.is_error() {
                return;
            }
        } else {
            simplify_cs(node, source, tokens, edits, selection);
        }
        if node.kind().contains("string")
            || node.kind().starts_with("preproc_")
            || node.kind() == "comment"
        {
            return;
        }
        for child in node.named_children(&mut node.walk()) {
            walk(child, source, tokens, edits, selection);
        }
    }
    walk(tree.root_node(), source, &tokens, &mut edits, selection);
    (tokens, edits)
}

fn clean_cs(node: tree_sitter::Node<'_>) -> bool {
    !node.has_error()
        && node.kind() != "comment"
        && !node.kind().starts_with("preproc_")
        && node.named_children(&mut node.walk()).all(clean_cs)
}
fn returned(body: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if body.kind() != "block" || body.named_child_count() != 1 || !clean_cs(body) {
        return None;
    }
    let statement = body.named_child(0)?;
    if statement.kind() != "return_statement" || statement.named_child_count() != 1 {
        return None;
    }
    statement.named_child(0)
}

fn simplify_cs(
    node: tree_sitter::Node<'_>,
    source: &str,
    tokens: &[Token],
    edits: &mut Vec<Edit>,
    selection: &Range<usize>,
) {
    let text = |n: tree_sitter::Node<'_>| &source[n.byte_range()];
    // Merge only adjacent lists with identical targets. Gaps containing comments
    // or directives are boundaries, as are selections cutting through a list.
    let mut previous = None;
    for child in node.named_children(&mut node.walk()) {
        if child.kind() != "attribute_list"
            || !clean_cs(child)
            || child.start_byte() < selection.start
            || child.end_byte() > selection.end
        {
            previous = None;
            continue;
        }
        let target = child
            .named_children(&mut child.walk())
            .find(|n| n.kind() == "attribute_target_specifier");
        let start = tokens.partition_point(|t| t.range.end <= child.start_byte());
        let end = tokens.partition_point(|t| t.range.start < child.end_byte());
        for pair in tokens[start..end].windows(2) {
            let gap = pair[0].range.end..pair[1].range.start;
            if source[gap.clone()].contains('\n')
                && source[gap.clone()].chars().all(char::is_whitespace)
            {
                edits.push(Edit {
                    range: gap,
                    text: if separator(
                        &source[pair[0].range.clone()],
                        &source[pair[1].range.clone()],
                    ) {
                        " "
                    } else {
                        ""
                    }
                    .into(),
                });
            }
        }
        if let Some((end, old_target)) = previous
            && old_target == target.map(text)
            && source[end..child.start_byte()]
                .chars()
                .all(char::is_whitespace)
        {
            edits.push(Edit {
                range: end - 1..target.map_or(child.start_byte() + 1, |n| n.end_byte()),
                text: ",".into(),
            });
        }
        previous = Some((child.end_byte(), target.map(text)));
    }
    match node.kind() {
        "method_declaration" => {
            if let Some(body) = node.child_by_field_name("body")
                && let Some(expr) = returned(body)
            {
                edits.push(Edit {
                    range: body.byte_range(),
                    text: format!("=>{};", compact(source, expr.byte_range(), tokens)),
                });
            }
        }
        "property_declaration" => {
            if let Some(list) = node.child_by_field_name("accessors")
                && list.named_child_count() == 1
                && clean_cs(list)
            {
                let getter = list.named_child(0).unwrap();
                if getter
                    .child_by_field_name("name")
                    .is_some_and(|name| text(name) == "get")
                    && getter.named_child_count() == 1
                    && let Some(expr) = getter.child_by_field_name("body").and_then(returned)
                {
                    edits.push(Edit {
                        range: list.byte_range(),
                        text: format!("=>{};", compact(source, expr.byte_range(), tokens)),
                    });
                }
            }
        }
        "namespace_declaration"
            if selection.start <= node.start_byte() && selection.end >= node.end_byte() =>
        {
            let parent = node.parent().unwrap();
            let siblings: Vec<_> = parent.named_children(&mut parent.walk()).collect();
            if parent.kind() == "compilation_unit"
                && siblings.iter().all(|n| {
                    n.id() == node.id()
                        || matches!(
                            n.kind(),
                            "using_directive" | "extern_alias_directive" | "comment"
                        )
                })
                && let Some(body) = node.child_by_field_name("body")
                && clean_cs(body)
                && !body
                    .named_children(&mut body.walk())
                    .any(|n| n.kind() == "namespace_declaration")
                && siblings.iter().all(|n| n.end_byte() <= node.end_byte())
            {
                edits.push(Edit {
                    range: body.start_byte()..body.start_byte() + 1,
                    text: ";".into(),
                });
                edits.push(Edit {
                    range: body.end_byte() - 1..body.end_byte(),
                    text: String::new(),
                });
            }
        }
        "variable_declaration" if clean_cs(node) => {
            if let Some(ty) = node.child_by_field_name("type") {
                for declarator in node
                    .named_children(&mut node.walk())
                    .filter(|n| n.kind() == "variable_declarator")
                {
                    for value in declarator.named_children(&mut declarator.walk()) {
                        if value.kind() == "default_expression"
                            && value
                                .child_by_field_name("type")
                                .is_some_and(|t| text(t) == text(ty))
                        {
                            edits.push(Edit {
                                range: value.byte_range(),
                                text: "default".into(),
                            });
                        }
                        // Retain the declared type. Exclude nullable types and aliases whose targets are unknown.
                        if value.kind() == "object_creation_expression"
                            && ty.kind() == "predefined_type"
                            && value.child_by_field_name("arguments").is_some()
                            && value
                                .child_by_field_name("type")
                                .is_some_and(|t| text(t) == text(ty))
                        {
                            let t = value.child_by_field_name("type").unwrap();
                            edits.push(Edit {
                                range: value.start_byte()..t.end_byte(),
                                text: "new".into(),
                            });
                        } else if value.kind() == "object_creation_expression"
                            && value
                                .child_by_field_name("type")
                                .is_some_and(|t| text(t) == text(ty))
                            && node
                                .parent()
                                .is_some_and(|p| p.kind() == "local_declaration_statement")
                            && node.named_child_count() == 2
                            && selection.start <= node.start_byte()
                            && selection.end >= node.end_byte()
                            && text(ty).len() > 3
                        {
                            // The constructor still spells the exact original type, including nullable types.
                            edits.push(Edit {
                                range: ty.byte_range(),
                                text: "var".into(),
                            });
                        }
                    }
                }
            }
        }
        "block" if clean_cs(node) && node.named_child_count() == 1 => {
            let parent = node.parent().unwrap();
            if matches!(
                parent.kind(),
                "if_statement"
                    | "for_statement"
                    | "for_each_statement"
                    | "while_statement"
                    | "do_statement"
            ) {
                let statement = node.named_child(0).unwrap();
                if matches!(
                    statement.kind(),
                    "expression_statement"
                        | "return_statement"
                        | "throw_statement"
                        | "break_statement"
                        | "continue_statement"
                ) {
                    edits.push(Edit {
                        range: node.byte_range(),
                        text: format!(" {}", compact(source, statement.byte_range(), tokens)),
                    });
                }
            }
        }
        _ => (),
    }
}

fn xml_doc(doc: &str) -> Option<String> {
    use std::sync::LazyLock;
    static TAG: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#"<[^>]*>"#).unwrap());
    let text = if let Some(block) = doc.strip_prefix("/**") {
        block
            .strip_suffix("*/")?
            .lines()
            .map(|line| line.trim().strip_prefix('*').unwrap_or(line.trim()).trim())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        doc.lines()
            .map(|line| line.trim().strip_prefix("///"))
            .collect::<Option<Vec<_>>>()?
            .join("\n")
    };
    let mut reduced = String::new();
    let mut end = 0;
    let mut elements = Vec::new();
    for tag in TAG.find_iter(&text) {
        let plain = &text[end..tag.start()];
        if plain.contains(['<', '>']) {
            return None;
        }
        reduced.push_str(&quick_xml::escape::unescape(plain).ok()?);
        let raw = tag.as_str();
        let closing = raw.starts_with("</");
        // Validate tags and nesting before replacing any part of the doc block.
        let normalized = if closing {
            raw.replacen("</", "<", 1)
        } else {
            raw.into()
        };
        let mut reader = quick_xml::Reader::from_str(&normalized);
        let element = match reader.read_event().ok()? {
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => e,
            _ => return None,
        };
        let name = element.name();
        let name = std::str::from_utf8(name.as_ref()).ok()?;
        let empty = raw.ends_with("/>");
        if closing {
            if elements.pop().as_deref() != Some(name) {
                return None;
            }
        } else if !empty {
            elements.push(name.to_owned());
        }
        let mut argument = None;
        for attr in element.attributes() {
            let attr = attr.ok()?;
            let allowed = match name {
                "see" | "seealso" => matches!(attr.key.as_ref(), b"cref" | b"langword" | b"href"),
                "param" | "typeparam" | "paramref" | "typeparamref" => attr.key.as_ref() == b"name",
                "exception" => attr.key.as_ref() == b"cref",
                "list" => attr.key.as_ref() == b"type",
                _ => false,
            };
            if !allowed || closing || argument.is_some() {
                return None;
            }
            argument = Some(attr.unescape_value().ok()?.into_owned());
        }
        match name {
            "summary" | "remarks" | "para" | "list" | "listheader" => reduced.push('\n'),
            "param" | "typeparam" | "exception" if !closing => {
                reduced.push('\n');
                reduced.push_str(&argument?);
                reduced.push_str(": ");
            }
            "returns" | "value" if !closing => {
                reduced.push('\n');
                reduced.push_str(name);
                reduced.push_str(": ");
            }
            "see" | "seealso" | "paramref" | "typeparamref" if empty => {
                reduced.push_str(&argument?)
            }
            "see" | "seealso" | "paramref" | "typeparamref" if !closing => {
                reduced.push_str(&argument?);
                reduced.push_str(" (");
            }
            "see" | "seealso" | "paramref" | "typeparamref" => reduced.push(')'),
            "item" if !closing => reduced.push_str("\n- "),
            "term" if closing => reduced.push_str(": "),
            "c" | "description" | "term" | "item" | "param" | "typeparam" | "returns" | "value"
            | "exception" => (),
            // Code examples and unknown/inherited/included docs stay verbatim.
            _ => return None,
        }
        end = tag.end();
    }
    let tail = &text[end..];
    if !elements.is_empty() || tail.contains(['<', '>']) {
        return None;
    }
    reduced.push_str(&quick_xml::escape::unescape(tail).ok()?);
    Some(
        reduced
            .lines()
            .filter_map(|line| {
                let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
                (!line.is_empty()).then(|| format!("// {line}"))
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_docs_keep_meaning_and_preserve_unsupported_markup() {
        let source = r#"class C {
/// <summary>Returns <see langword="null"/> &amp; <paramref name="value"/>.</summary>
/// <exception cref="InvalidOperationException">When unavailable.</exception>
/// <seealso cref="Other"/>
/** <summary>Block documentation.</summary>
 * <returns>A <c>value</c>.</returns> */
int Value;
/// <include file="docs.xml" path="x"/>
/// <unknown>Preserve this.</unknown>
int Other;
}"#;
        let output = min(source, Language::CSharp);
        assert!(output.contains("Returns null & value."), "{output}");
        assert!(
            output.contains("InvalidOperationException: When unavailable."),
            "{output}"
        );
        assert!(output.contains("// Other"), "{output}");
        assert!(output.contains("// Block documentation."), "{output}");
        assert!(output.contains("returns: A value."), "{output}");
        assert!(output.contains("<include file=\"docs.xml\""), "{output}");
        assert!(
            output.contains("<unknown>Preserve this.</unknown>"),
            "{output}"
        );
        let code = "/// <code>\n///   var x =  1;\n/// </code>\nclass C {}";
        assert!(min(code, Language::CSharp).contains("///   var x =  1;"));
        assert!(xml_doc("/// <see cref='Broken></see>").is_none());
        assert!(xml_doc("/// <summary>Broken</remarks>").is_none());
        let list = xml_doc("/// <list type='bullet'><item><term>A</term><description>First</description></item><item>Second</item></list>").unwrap();
        assert!(
            list.contains("- A: First") && list.contains("- Second"),
            "{list}"
        );
        let link = xml_doc("/// <see href='https://example.com'>The guide</see>").unwrap();
        assert!(link.contains("The guide") && link.contains("https://example.com"));
    }

    #[test]
    fn attributes_merge_without_changing_arguments_targets_or_boundaries() {
        let source = "[A(\"a,b\")]\n[B(typeof(int))]\nclass C {\n[return: A]\n[return: B]\nint M()=>1;\n[field: A]\n[property: B]\nint P{get;set;}\n[A]\n// keep\n[B]\nint X;\n}";
        let output = min(source, Language::CSharp);
        assert!(output.contains("[A(\"a,b\"),B(typeof(int))]"), "{output}");
        assert!(output.contains("[return:A,B]"), "{output}");
        assert!(output.contains("[field:A]\n[property:B]"), "{output}");
        assert!(output.contains("[A]\n// keep\n[B]"), "{output}");
        let range = source.find("[B(typeof").unwrap()..source.len();
        assert!(render(source, Language::CSharp, range).starts_with("[B(typeof(int))]"));
        let multiline = "[A(\n1,\n2)]\n[B(@\"first\n  second\")]\nclass C {}";
        let output = min(multiline, Language::CSharp);
        assert!(
            output.contains("[A(1,2),B(@\"first\n  second\")]"),
            "{output}"
        );
        let directives = "[A]\n#if DEBUG\n[B]\n#endif\nclass C {}";
        let output = min(directives, Language::CSharp);
        assert!(output.contains("#if DEBUG\n[B]\n#endif"), "{output}");
    }
    #[test]
    #[ignore = "manual token measurement; requires m-count-tokens"]
    fn measure_tokens() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        fn count(text: &str) -> usize {
            let mut child = Command::new("m-count-tokens")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        }
        for (name, source, language) in [
            (
                "C# documentation and attributes",
                "/// <summary>Returns <see langword=\"null\"/>.</summary>\n[Serializable]\n[Obsolete(\"Use NewType\")]\nclass Example {}\n",
                Language::CSharp,
            ),
            ("Rust search", include_str!("search.rs"), Language::Rust),
            ("Rust service", include_str!("service.rs"), Language::Rust),
            (
                "C# fixture",
                include_str!("../tests/metadata-fixture/Fixture.cs"),
                Language::CSharp,
            ),
            (
                "C# Unity exporter",
                include_str!("../tests/unity-reference/ExportCompilation.cs"),
                Language::CSharp,
            ),
        ] {
            let output = min(source, language);
            let before = count(source);
            let after = count(&output);
            println!(
                "{name}: {before} -> {after} tokens ({:.1}% saved)",
                100.0 * (before as f64 - after as f64) / before as f64
            );
        }
    }
    fn min(source: &str, language: Language) -> String {
        render(source, language, 0..source.len())
    }
    #[test]
    fn rust_shortens_code_without_touching_macros_literals_or_tuple_shape() {
        let source = r####"use std::collections::HashMap;
use std::collections::HashSet;
struct Point { x: i32, y: i32 }
fn make(x: i32, y: i32) -> Point {
    let literal = r#" keep   all spaces "#;
    let tuple = (x,);
    let separate = x - -y;
    custom! { x : x,  keep   spacing };
    Point { x: x, y: y, }
}
"####;
        let result = min(source, Language::Rust);
        assert!(
            result.contains("use std::collections::{HashMap,HashSet};"),
            "{result}"
        );
        assert!(result.contains("Point{x,y}"), "{result}");
        assert!(result.contains("(x,)"), "{result}");
        assert!(result.contains("r#\" keep   all spaces \"#"), "{result}");
        assert!(result.contains("{ x : x,  keep   spacing }"), "{result}");
        assert!(result.contains("- -"), "{result}");
        assert!(result.len() < source.len());
    }
    #[test]
    fn csharp_preserves_types_comments_strings_and_directives() {
        let source = r####"using Alias = System.Int32?;
class Sample {
    /// <summary>Keep the value.</summary>
    public int Value { get { return 42; } }
    int Add(int a, int b) { return a + b; }
    void Run(bool ready) {
        int? nullable = new int?();
        Alias alias = new Alias();
        System.IO.Stream stream = new System.IO.MemoryStream();
        int number = default(int);
        int initialized = new int {};
        int[] values = ready ? [1] : [2];
        if (ready) { number++; }
        // Keep this invariant.
        string text = "a   b";
    }
#if ACTIVE
    string raw = " stay   exact ";
#endif
}
"####;
        let result = min(source, Language::CSharp);
        assert!(
            result.contains("Value=>42;") && result.contains("=>a+b;"),
            "{result}"
        );
        assert!(
            result.contains("// Keep the value.") && result.contains("// Keep this invariant."),
            "{result}"
        );
        assert!(result.contains("nullable=new int?()"), "{result}");
        assert!(result.contains("alias=new Alias()"), "{result}");
        assert!(
            result.contains("System.IO.Stream stream=new System.IO.MemoryStream()"),
            "{result}"
        );
        assert!(result.contains("number=default;"), "{result}");
        assert!(result.contains("new int{}"), "{result}");
        assert!(result.contains("? [1]"), "{result}");
        assert!(
            result.contains("\"a   b\"") && result.contains("\" stay   exact \""),
            "{result}"
        );
        assert!(result.len() < source.len());
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        assert!(
            !parser.parse(&result, None).unwrap().root_node().has_error(),
            "{result}"
        );
    }
    #[test]
    fn selected_lines_inside_literals_remain_unchanged() {
        for (language, source) in [
            (Language::Rust, "fn f(){let x=r#\"\n  a   b\n  c   d\n\"#;}"),
            (
                Language::CSharp,
                "class C { string s=@\"\n  a   b\n  c   d\n\"; }",
            ),
        ] {
            let (range, _, _) = crate::navigation::lines(source, Some((2, 2))).unwrap();
            assert_eq!(render(source, language, range.clone()), source[range]);
        }
    }
    #[test]
    fn ranges_do_not_apply_half_a_namespace_conversion() {
        let source = "namespace Example\n{\nclass C { int Get() { return 1; } }\n}\n";
        let (range, _, _) = crate::navigation::lines(source, Some((2, 2))).unwrap();
        assert_eq!(render(source, Language::CSharp, range), "{\n");
        let full = min(source, Language::CSharp);
        assert!(
            full.contains("namespace Example;") && full.contains("=>1;"),
            "{full}"
        );
    }
}
