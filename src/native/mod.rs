//! Written-source facts shared by the C, C++, HLSL, and GLSL frontends.
mod extract;
pub(crate) mod jobs;
mod lex;

use crate::model::Language;
pub use extract::extract;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ops::Range, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Group {
    Native,
    Shaders,
}

impl Group {
    pub fn of(language: Language) -> Option<Self> {
        match language {
            Language::C | Language::Cpp | Language::Header => Some(Self::Native),
            Language::Hlsl | Language::Glsl | Language::ShaderLab => Some(Self::Shaders),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Shaders => "shaders",
        }
    }
}

pub fn language(path: &Path) -> Option<Language> {
    Some(match path.extension()?.to_str()? {
        "c" => Language::C,
        "h" => Language::Header,
        "C" | "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "inl" | "ipp"
        | "tpp" | "ixx" | "cppm" => Language::Cpp,
        "hlsl" | "hlsli" | "cginc" | "compute" => Language::Hlsl,
        "glsl" | "vert" | "frag" | "geom" | "tesc" | "tese" | "comp" | "mesh" | "task" | "rgen"
        | "rint" | "rahit" | "rchit" | "rmiss" | "rcall" => Language::Glsl,
        "shader" | "surfshader" => Language::ShaderLab,
        _ => return None,
    })
}

#[derive(
    Clone, Debug, Default, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct File {
    /// These tables have the same ordinals as Facts' declaration/occurrence tables.
    pub declarations: Vec<DeclarationInfo>,
    pub occurrences: Vec<OccurrenceInfo>,
    pub names: BTreeMap<String, Vec<u32>>,
    pub calls: BTreeMap<u32, Vec<u32>>,
    pub includes: Vec<Include>,
    pub regions: Vec<Region>,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct Region {
    pub span: Range<usize>,
    pub language: Language,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct DeclarationInfo {
    pub parent: Option<u32>,
    pub region: u32,
    pub qualifiers: String,
    pub conditional: bool,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum Role {
    #[default]
    Identifier,
    Type,
    MacroBody,
    EntryPoint,
    Label,
}

#[derive(
    Clone, Debug, Default, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct OccurrenceInfo {
    pub owner: Option<u32>,
    pub local: Option<u32>,
    pub region: u32,
    pub role: Role,
    pub receiver: Option<Range<usize>>,
    pub qualified: bool,
    pub assignment: Option<Range<usize>>,
    pub indirect_write: bool,
}

#[derive(
    Clone, Debug, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct Include {
    pub path: String,
    pub span: Range<usize>,
    pub relative: bool,
}

/// Compare token spellings, retaining native qualifiers and type names.
pub fn normalized(text: &str) -> String {
    lex::Lexer::new(text, 0..text.len())
        .map(|token| &text[token.span])
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn signature_matches(written: &[String], query: &[String]) -> bool {
    written.len() == query.len()
        && written
            .iter()
            .zip(query)
            .all(|(a, b)| normalized(a) == normalized(b))
}

pub fn compatible(a: Language, b: Language) -> bool {
    a == b
        || matches!(
            (Group::of(a), Group::of(b)),
            (Some(Group::Native), Some(Group::Native))
        )
        || a == Language::ShaderLab && Group::of(b) == Some(Group::Shaders)
        || b == Language::ShaderLab && Group::of(a) == Some(Group::Shaders)
}

pub fn language_matches(pattern: &str, language: Language) -> bool {
    match pattern {
        "c" | "cpp" if language == Language::Header => true,
        "c++" => matches!(language, Language::Cpp | Language::Header),
        "c#" | "csharp" => language == Language::CSharp,
        "rs" => language == Language::Rust,
        "native" => Group::of(language) == Some(Group::Native),
        "shader" | "shaders" => Group::of(language) == Some(Group::Shaders),
        _ => crate::query::wildcard(pattern, language.tag()),
    }
}

pub fn components(name: &str) -> Vec<String> {
    written_components(name)
        .into_iter()
        .map(|part| {
            let part = normalized(part);
            if part.starts_with("operator ") {
                return part;
            }
            let (name, arguments) = part.split_once(" < ").unwrap_or((&part, ""));
            let mut name = name.replace(' ', "");
            if !arguments.is_empty() {
                name.push_str(" < ");
                name.push_str(arguments);
            }
            name
        })
        .collect()
}

pub fn written_components(name: &str) -> Vec<&str> {
    let mut tokens = lex::Lexer::new(name, 0..name.len()).peekable();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut operator = false;
    while let Some(token) = tokens.next() {
        let text = &name[token.span.clone()];
        if !operator
            && depth == 0
            && text == ":"
            && tokens.peek().is_some_and(|t| &name[t.span.clone()] == ":")
        {
            let part = name[start..token.span.start].trim();
            if !part.is_empty() {
                parts.push(part);
            }
            start = tokens.next().unwrap().span.end;
            continue;
        }
        operator |= text == "operator";
        if !operator {
            if text == "<" {
                depth += 1;
            }
            if text == ">" {
                depth = depth.saturating_sub(1);
            }
        }
    }
    let part = name[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

pub fn simple_name(name: &str) -> String {
    let part = components(name).pop().unwrap_or_default();
    if let Some(operator) = part.strip_prefix("operator ") {
        let suffix = operator.replace(" : : ", "::");
        if suffix.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            format!("operator {suffix}")
        } else {
            format!("operator{}", suffix.replace(' ', ""))
        }
    } else {
        part.split(" < ").next().unwrap_or(&part).to_owned()
    }
}

pub fn name_rank(pattern: &str, written: &str, loose: bool) -> Option<u8> {
    let pattern = components(pattern);
    let written = components(written);
    if pattern.len() > written.len() || pattern.is_empty() {
        return None;
    }
    let mut rank = u8::from(pattern.len() != written.len());
    for (p, w) in pattern
        .iter()
        .zip(&written[written.len() - pattern.len()..])
    {
        let w = if !p.contains(" < ") && !p.starts_with("operator ") {
            w.split(" < ").next().unwrap_or(w)
        } else {
            w
        };
        let next = if p.starts_with("operator ") {
            (p == w).then_some(0)
        } else {
            crate::query::name_rank(p, w, loose)
        }?;
        rank = rank.max(next);
    }
    Some(rank)
}

pub fn base_name(written: &str) -> String {
    written
        .split_whitespace()
        .filter(|word| !matches!(*word, "public" | "protected" | "private" | "virtual"))
        .collect::<Vec<_>>()
        .join(" ")
}
