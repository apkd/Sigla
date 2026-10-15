//! Written-source adapter for NG graphics and block-format surface shaders.
//!
//! The language plus source bytes completely determine these facts. No paths,
//! Unity execution, generated passes, build variants, or new cache fields are used.
use super::{
    DeclarationInfo, File, OccurrenceInfo, Role,
    lex::{Kind, Lexer, Token},
};
use crate::model::{Declaration, Facts, Language, Occurrence, OccurrenceRole, WriteKind};
use anyhow::Result;
use std::{borrow::Cow, collections::BTreeSet, ops::Range};

pub fn extract(source: &str, language: Language) -> Result<Facts> {
    if !matches!(language, Language::Hlsl | Language::ShaderLab) {
        return super::extract::extract(source, language);
    }
    let prepared = prepare(source, language);
    let mut facts = super::extract::extract(&prepared.text, prepared.language)?;
    facts.errors |= prepared.errors;
    let custom_pragmas =
        prepared.text.contains("native_pass") || prepared.text.contains("native_mesh");
    if prepared.declarations.is_empty() && !custom_pragmas {
        return Ok(facts);
    }
    let mut native = facts
        .native
        .take()
        .expect("native extractor supplies native facts");
    for authored in prepared.declarations {
        // Masked block-format files have one full-file HLSL region. Never create
        // an out-of-region declaration: native navigation depends on this invariant.
        let Some(region) = native
            .regions
            .iter()
            .position(|region| region.span.contains(&authored.name.start))
        else {
            facts.errors = true;
            continue;
        };
        let name = source[authored.name.clone()].to_owned();
        facts.declarations.push(Declaration {
            name: name.clone(),
            qualified: name,
            kind: authored.kind.into(),
            namespace: String::new(),
            owner: String::new(),
            name_span: authored.name,
            span: authored.span.clone(),
            header: authored.span,
            scope: native.regions[region].span.clone(),
            parameters: Vec::new(),
            ty: authored.ty,
            access: "public".into(),
            attributes: authored.attributes,
            modifiers: Vec::new(),
            bases: Vec::new(),
        });
        native.declarations.push(DeclarationInfo {
            parent: None,
            region: region as u32,
            qualifiers: authored.qualifiers,
            conditional: authored.conditional,
        });
    }
    if custom_pragmas {
        let mut entries = Vec::new();
        for (region, info) in native.regions.iter().enumerate() {
            if info.language == Language::Hlsl {
                let (spans, errors) = entry_points(&prepared.text, info.span.clone());
                facts.errors |= errors;
                entries.extend(spans.into_iter().map(|span| (region as u32, span)));
            }
        }
        let mut occupied: BTreeSet<_> = facts
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.span.start, occurrence.span.end))
            .collect();
        let mut changed = false;
        for (region, span) in entries {
            if !occupied.insert((span.start, span.end)) {
                continue;
            }
            changed = true;
            facts.occurrences.push(Occurrence {
                role: OccurrenceRole::Value,
                name: source[span.clone()].to_owned(),
                span,
                call: false,
                construction: false,
                write: WriteKind::None,
                receiver: String::new(),
                arguments: None,
                opaque: false,
            });
            native.occurrences.push(OccurrenceInfo {
                region,
                role: Role::EntryPoint,
                ..Default::default()
            });
        }
        if changed {
            rebuild_occurrences(&mut facts, &mut native);
        }
    }
    facts.native = Some(native);
    Ok(facts)
}

/// Occurrence ordinals back both tables and the reverse indexes. Declaration
/// ordinals are deliberately NOT reordered (locals and parents refer to them).
fn rebuild_occurrences(facts: &mut Facts, native: &mut File) {
    let mut occurrences: Vec<_> = std::mem::take(&mut facts.occurrences)
        .into_iter()
        .zip(std::mem::take(&mut native.occurrences))
        .collect();
    occurrences.sort_by_key(|(occurrence, _)| (occurrence.span.start, occurrence.span.end));
    native.names.clear();
    native.calls.clear();
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
}

#[derive(Debug)]
struct Authored {
    kind: &'static str,
    name: Range<usize>,
    span: Range<usize>,
    ty: String,
    attributes: Vec<String>,
    qualifiers: String,
    conditional: bool,
}

struct Prepared<'a> {
    text: Cow<'a, str>,
    language: Language,
    declarations: Vec<Authored>,
    errors: bool,
}

#[derive(Debug)]
struct Block {
    kind: String,
    span: Range<usize>,
    body: Range<usize>,
}

struct Marker {
    kind: String,
    open: bool,
    line: Range<usize>,
    syntax_end: usize,
}

fn marker_word(word: &str) -> Option<(bool, String)> {
    if word
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("BEGIN_"))
    {
        Some((true, word[6..].to_ascii_uppercase()))
    } else if word
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("END_"))
    {
        Some((false, word[4..].to_ascii_uppercase()))
    } else {
        None
    }
}

fn contains_ascii_case_insensitive(text: &str, pattern: &[u8]) -> bool {
    text.as_bytes()
        .windows(pattern.len())
        .any(|part| part.eq_ignore_ascii_case(pattern))
}

fn prepare(source: &str, language: Language) -> Prepared<'_> {
    let first =
        Lexer::new(source, 0..source.len()).find(|token| &source[token.span.clone()] != "\u{feff}");
    // Standard ShaderLab starts with Shader, not BEGIN_*. Leave its existing
    // embedded-code/scoping behavior entirely to the existing extractor.
    let surface = language == Language::ShaderLab
        && first.is_some_and(|token| {
            token.kind == Kind::Name && marker_word(&source[token.span]).is_some()
        });
    let properties = language == Language::Hlsl
        && (contains_ascii_case_insensitive(source, b"BEGIN_PROPERTIES")
            || contains_ascii_case_insensitive(source, b"END_PROPERTIES"));
    let mut prepared = Prepared {
        text: Cow::Borrowed(source),
        language,
        declarations: Vec::new(),
        errors: false,
    };
    if !surface && !properties {
        return prepared;
    }
    let (blocks, markers, errors) = scan_blocks(source, surface);
    prepared.errors = errors;
    let mut text = source.as_bytes().to_vec();
    if surface {
        blank(&mut text);
        prepared.language = Language::Hlsl;
    }
    for marker in markers {
        blank(&mut text[marker]);
    }
    for block in blocks {
        if !surface {
            blank(&mut text[block.span.clone()]);
        }
        match block.kind.as_str() {
            "CODE" | "DEFINES" | "CBUFFER" if surface => {
                text[block.body.clone()].copy_from_slice(&source.as_bytes()[block.body]);
            }
            "PROPERTIES" => {
                let (declarations, errors) = property_declarations(source, block.body);
                prepared.declarations.extend(declarations);
                prepared.errors |= errors;
            }
            "VARYINGS" if surface => {
                let (declarations, errors) = varying_declarations(source, block.body);
                prepared.declarations.extend(declarations);
                prepared.errors |= errors;
            }
            // Selector/render metadata, not HLSL declarations.
            "OPTIONS" | "CHANNELS" | "PASS" if surface => {}
            // Keep other code usable, but do not pretend to understand unknown
            // blocks or macro-generated legacy instancing/blackboard declarations.
            _ => prepared.errors = true,
        }
    }
    // Every removed byte is replaced, including every byte of a UTF-8 scalar;
    // retained slices end on line/token boundaries. Byte positions never move.
    prepared.text = Cow::Owned(String::from_utf8(text).expect("mask preserves UTF-8"));
    prepared
}

fn blank(bytes: &mut [u8]) {
    for byte in bytes {
        if !matches!(*byte, b'\r' | b'\n') {
            *byte = b' ';
        }
    }
}

fn physical_start(source: &str, at: usize) -> usize {
    source[..at].rfind('\n').map_or(0, |index| index + 1)
}

/// Beginning of the preprocessor logical line containing `at`.
fn logical_start(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut start = physical_start(source, at);
    while start > 0 {
        let mut previous = start - 1;
        if previous > 0 && bytes[previous - 1] == b'\r' {
            previous -= 1;
        }
        if previous == 0 || bytes[previous - 1] != b'\\' {
            break;
        }
        start = physical_start(source, previous - 1);
    }
    start
}

// Inspect each inter-token gap only once on ordinary input; do not rescan an
// arbitrarily long line for every stray # or BEGIN_* token in malformed code.
fn line_initial(source: &str, previous_end: usize, at: usize, floor: usize) -> bool {
    previous_end == floor
        || source[previous_end..at].contains('\n')
            && previous_end <= logical_start(source, at).max(floor)
}

fn physical_end(source: &str, at: usize, end: usize) -> usize {
    source[at..end]
        .find('\n')
        .map_or(end, |offset| at + offset + 1)
}

fn logical_end(source: &str, at: usize, limit: usize) -> usize {
    let bytes = source.as_bytes();
    let mut end = physical_end(source, at, limit);
    while end < limit {
        let mut previous = end - 1;
        if previous > 0 && bytes[previous - 1] == b'\r' {
            previous -= 1;
        }
        if previous == 0 || bytes[previous - 1] != b'\\' {
            break;
        }
        end = physical_end(source, end, limit);
    }
    end
}

/// The whole-file lexer skips comments and strings, including multiline ones.
/// `previous_end` distinguishes a marker/directive from a name inside code or a
/// continued macro. A marker's optional argument is opaque (CBUFFER/PASS names).
fn scan_blocks(source: &str, surface: bool) -> (Vec<Block>, Vec<Range<usize>>, bool) {
    let mut markers = Vec::new();
    let mut errors = false;
    let mut previous_end = 0;
    for token in Lexer::new(source, 0..source.len()) {
        if &source[token.span.clone()] == "\u{feff}" && token.span.start == 0 {
            continue;
        }
        let previous = previous_end;
        previous_end = token.span.end;
        if token.kind != Kind::Name {
            continue;
        }
        let Some((open, kind)) = marker_word(&source[token.span.clone()]) else {
            continue;
        };
        if !surface && kind != "PROPERTIES" {
            continue;
        }
        if !line_initial(source, previous, token.span.start, 0) {
            continue;
        }
        let end = physical_end(source, token.span.end, source.len());
        let tail: Vec<_> = Lexer::new(source, token.span.end..end).collect();
        if !(tail.is_empty()
            || open
                && surface
                && matches!(kind.as_str(), "CBUFFER" | "INSTANCING_BUFFER" | "PASS")
                && spelling(source, &tail, 0) == Some("(")
                && closing(source, &tail, 0).ok() == Some(tail.len() - 1))
        {
            errors |= surface;
            continue;
        }
        let syntax_end = tail.last().map_or(token.span.end, |last| last.span.end);
        markers.push(Marker {
            kind,
            open,
            line: token.span.start..end,
            syntax_end,
        });
    }
    let spans = markers
        .iter()
        .map(|marker| marker.line.start..marker.syntax_end)
        .collect();
    let mut blocks = Vec::new();
    let mut active: Option<Marker> = None;
    for marker in markers {
        if marker.open {
            if let Some(previous) = active.take() {
                // Recover at the next opener rather than swallowing all later code.
                errors = true;
                blocks.push(Block {
                    kind: previous.kind,
                    body: previous.syntax_end..marker.line.start,
                    span: previous.line.start..marker.line.start,
                });
            }
            active = Some(marker);
        } else if let Some(previous) = active.take() {
            errors |= previous.kind != marker.kind;
            blocks.push(Block {
                kind: previous.kind,
                body: previous.syntax_end..marker.line.start,
                span: previous.line.start..marker.syntax_end,
            });
        } else {
            errors = true;
        }
    }
    if let Some(previous) = active {
        errors = true;
        blocks.push(Block {
            kind: previous.kind,
            body: previous.syntax_end..source.len(),
            span: previous.line.start..source.len(),
        });
    }
    (blocks, spans, errors)
}

fn spelling<'a>(source: &'a str, tokens: &[Token], index: usize) -> Option<&'a str> {
    tokens.get(index).map(|token| &source[token.span.clone()])
}

/// Match balanced punctuation without recursion. On failure return the point
/// already inspected, so malformed input cannot force quadratic rescanning.
fn closing(source: &str, tokens: &[Token], start: usize) -> std::result::Result<usize, usize> {
    let mut stack = Vec::new();
    for (index, token) in tokens.iter().enumerate().skip(start) {
        let word = &source[token.span.clone()];
        match word {
            "(" => stack.push(")"),
            "[" => stack.push("]"),
            "{" => stack.push("}"),
            ")" | "]" | "}" => {
                if stack.pop() != Some(word) {
                    return Err(index + 1);
                }
                if stack.is_empty() {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(tokens.len())
}

fn property_declarations(source: &str, body: Range<usize>) -> (Vec<Authored>, bool) {
    let tokens: Vec<_> = Lexer::new(source, body.clone()).collect();
    let mut result = Vec::new();
    let mut errors = false;
    let mut at = 0;
    while at < tokens.len() {
        match property(source, &tokens, at) {
            Ok((declaration, next)) => {
                result.push(declaration);
                at = next;
            }
            Err(inspected) => {
                errors = true;
                // An unsupported line does not prevent later well-formed properties
                // from being indexed. Never revisit an already-inspected token.
                let end = physical_end(source, tokens[at].span.start, body.end);
                at = inspected.max(at + 1);
                while at < tokens.len() && tokens[at].span.start < end {
                    at += 1;
                }
            }
        }
    }
    (result, errors)
}

fn property(
    source: &str,
    tokens: &[Token],
    start: usize,
) -> std::result::Result<(Authored, usize), usize> {
    let mut at = start;
    let mut attributes = Vec::new();
    while spelling(source, tokens, at) == Some("[") {
        let end = closing(source, tokens, at)?;
        attributes.push(source[tokens[at].span.start..tokens[end].span.end].to_owned());
        at = end + 1;
    }
    let name = tokens.get(at).ok_or(at)?;
    if name.kind != Kind::Name || spelling(source, tokens, at + 1) != Some("(") {
        return Err(at + 1);
    }
    let header_end = closing(source, tokens, at + 1)?;
    if tokens
        .get(at + 2)
        .is_none_or(|token| token.kind != Kind::Literal)
        || !spelling(source, tokens, at + 2).is_some_and(|word| word.starts_with('"'))
        || spelling(source, tokens, at + 3) != Some(",")
        || header_end <= at + 4
        || spelling(source, tokens, header_end + 1) != Some("=")
    {
        return Err(header_end + 1);
    }
    let type_start = at + 4;
    let kind = spelling(source, tokens, type_start).ok_or(header_end + 1)?;
    if kind == "Range" {
        if spelling(source, tokens, type_start + 1) != Some("(")
            || closing(source, tokens, type_start + 1)? != header_end - 1
        {
            return Err(header_end + 1);
        }
    } else if header_end != type_start + 1 {
        return Err(header_end + 1);
    }
    let default = header_end + 2;
    let end = match kind {
        "Vector" | "Color" => {
            if spelling(source, tokens, default) != Some("(") {
                return Err(default);
            }
            closing(source, tokens, default)?
        }
        "2D" | "2DArray" | "3D" | "Cube" | "CubeArray" => {
            if !spelling(source, tokens, default).is_some_and(|word| word.starts_with('"')) {
                return Err(default);
            }
            if spelling(source, tokens, default + 1) == Some("{") {
                closing(source, tokens, default + 1)?
            } else {
                default
            }
        }
        "Float" | "Range" | "Int" | "Integer" => {
            let mut next = default;
            let mut number = String::new();
            while let Some(token) = tokens.get(next) {
                let word = &source[token.span.clone()];
                if word == "+"
                    || word == "-"
                    || word == "."
                    || token.kind == Kind::Literal && word.starts_with(|c: char| c.is_ascii_digit())
                {
                    number.push_str(word);
                    next += 1;
                } else {
                    break;
                }
            }
            if next == default || number.parse::<f64>().is_err() {
                return Err(next);
            }
            next - 1
        }
        _ => return Err(default),
    };
    let ty = source[tokens[type_start].span.start..tokens[header_end - 1].span.end].to_owned();
    let next = end + 1 + (spelling(source, tokens, end + 1) == Some(";")) as usize;
    Ok((
        Authored {
            kind: "property",
            name: name.span.clone(),
            span: tokens[start].span.start..tokens[end].span.end,
            ty,
            attributes,
            qualifiers: "authored shader material property".into(),
            conditional: false,
        },
        next,
    ))
}

fn varying_type(word: &str) -> bool {
    ["float", "half", "double", "int", "uint", "bool"]
        .into_iter()
        .any(|prefix| {
            word.strip_prefix(prefix)
                .is_some_and(|suffix| suffix.is_empty() || matches!(suffix, "1" | "2" | "3" | "4"))
        })
}

fn varying_declarations(source: &str, body: Range<usize>) -> (Vec<Authored>, bool) {
    let tokens: Vec<_> = Lexer::new(source, body.clone()).collect();
    let mut result = Vec::new();
    let mut errors = false;
    let mut at = 0;
    while at < tokens.len() {
        let end = logical_end(source, tokens[at].span.start, body.end);
        let mut next = at + 1;
        while next < tokens.len() && tokens[next].span.start < end {
            next += 1;
        }
        let row = &tokens[at..next];
        at = next;
        let Some(colon) = row
            .iter()
            .position(|token| &source[token.span.clone()] == ":")
        else {
            continue;
        };
        let Some(ty) = spelling(source, row, colon + 1) else {
            continue;
        };
        // "GBuffer KEYWORD : TangentBasis" is a conditional semantic selection,
        // NOT a field called TangentBasis. Only explicit scalar/vector types qualify.
        if !varying_type(ty) {
            continue;
        }
        if colon == 0
            || row
                .get(colon + 2)
                .is_none_or(|token| token.kind != Kind::Name)
            || row.iter().skip(colon + 3).any(|token| {
                !matches!(
                    &source[token.span.clone()],
                    "linear" | "nointerpolation" | "centroid" | "sample" | "noperspective"
                )
            })
        {
            errors = true;
            continue;
        }
        let prefix = source[row[0].span.start..row[colon].span.start].trim();
        let interpolation = row.get(colon + 3).map_or("", |token| {
            source[token.span.start..row.last().unwrap().span.end].trim()
        });
        result.push(Authored {
            kind: "field",
            name: row[colon + 2].span.clone(),
            span: row[0].span.start..row.last().unwrap().span.end,
            ty: ty.into(),
            attributes: Vec::new(),
            qualifiers: format!("authored shader varying ({prefix}) {interpolation}"),
            conditional: prefix != "*",
        });
    }
    (result, errors)
}

fn entry_points(source: &str, range: Range<usize>) -> (Vec<Range<usize>>, bool) {
    let mut result = Vec::new();
    let mut errors = false;
    let mut previous_end = range.start;
    for token in Lexer::new(source, range.clone()) {
        if token.span.start == range.start && &source[token.span.clone()] == "\u{feff}" {
            continue;
        }
        let previous = previous_end;
        previous_end = token.span.end;
        if &source[token.span.clone()] != "#"
            || !line_initial(source, previous, token.span.start, range.start)
        {
            continue;
        }
        let end = logical_end(source, token.span.end, range.end);
        let words: Vec<_> = Lexer::new(source, token.span.end..end).collect();
        if spelling(source, &words, 0) != Some("pragma") {
            continue;
        }
        let name = spelling(source, &words, 1);
        let selected: &[usize] = match name {
            // words = pragma native_pass pass light-mode vertex fragment [geometry]
            Some("native_pass") if words.len() == 6 => &[4, 5],
            Some("native_pass") if words.len() == 7 => &[4, 5, 6],
            // words = pragma native_mesh pass entry keyword
            Some("native_mesh") if words.len() == 5 => &[3],
            Some("native_pass" | "native_mesh") => {
                errors = true;
                continue;
            }
            _ => continue,
        };
        // Pass names and mesh-selection keywords must be identifiers too. The
        // light-mode slot is not a function and may contain an opaque literal.
        let valid = words.iter().enumerate().skip(2).all(|(index, word)| {
            name == Some("native_pass") && index == 3 || word.kind == Kind::Name
        });
        if !valid {
            errors = true;
            continue;
        }
        result.extend(selected.iter().map(|&index| words[index].span.clone()));
    }
    (result, errors)
}

#[cfg(test)]
mod tests;
