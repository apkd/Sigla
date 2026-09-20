use serde::{Deserialize, Serialize};
use std::{ops::Range, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    CSharp,
    Rust,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub qualified: String,
    pub kind: String,
    pub namespace: String,
    pub owner: String,
    pub name_span: Range<usize>,
    pub span: Range<usize>,
    pub header: Range<usize>,
    pub scope: Range<usize>,
    pub parameters: Vec<String>,
    pub ty: String,
    pub access: String,
    pub attributes: Vec<String>,
    pub modifiers: Vec<String>,
    pub bases: Vec<String>,
}

impl Declaration {
    pub fn local(&self) -> bool {
        matches!(self.kind.as_str(), "local" | "parameter")
    }
    pub fn callable(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "method" | "function" | "constructor" | "operator" | "lambda"
        )
    }
    pub fn named_type(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "class" | "struct" | "interface" | "enum" | "type" | "trait" | "delegate"
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Occurrence {
    pub name: String,
    pub span: Range<usize>,
    pub call: bool,
    pub construction: bool,
    pub write: bool,
    pub receiver: String,
    pub arguments: Option<usize>,
    pub opaque: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub alias: String,
    pub path: String,
    pub scope: Range<usize>,
    pub global: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleFile {
    pub name: String,
    pub path: Option<String>,
    pub inline: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Facts {
    #[serde(skip)]
    pub csharp: Option<crate::csharp::syntax::FileSyntax>,
    pub declarations: Vec<Declaration>,
    pub occurrences: Vec<Occurrence>,
    pub imports: Vec<Import>,
    pub modules: Vec<ModuleFile>,
    pub errors: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceInput {
    pub path: PathBuf,
    pub project: usize,
    pub module: String,
    pub language: Language,
    pub metadata: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub path: PathBuf,
    pub name: String,
    pub defines: Vec<String>,
    pub references: Vec<PathBuf>,
    pub assemblies: Vec<PathBuf>,
    pub edition: String,
    /// Explicit project settings retained in the compilation identity.
    pub compiler_options: std::collections::BTreeMap<String, String>,
}

pub fn offset(source: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += source.get(start..)?.find('\n')? + 1;
    }
    let row = source.get(start..)?.split_inclusive('\n').next()?;
    row.char_indices()
        .nth(column.checked_sub(1)?)
        .map(|(i, _)| start + i)
}

pub fn position(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte];
    (
        prefix.bytes().filter(|&b| b == b'\n').count() + 1,
        prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    )
}

pub fn line(source: &str, byte: usize) -> &str {
    let start = source[..byte].rfind('\n').map_or(0, |i| i + 1);
    let end = source[byte..].find('\n').map_or(source.len(), |i| byte + i);
    source[start..end].trim()
}

pub fn decode(bytes: &[u8], language: Language) -> anyhow::Result<String> {
    if language == Language::CSharp
        && (bytes.starts_with(&[0xff, 0xfe, 0, 0]) || bytes.starts_with(&[0, 0, 0xfe, 0xff]))
    {
        anyhow::ensure!(
            bytes.len().is_multiple_of(4),
            "Invalid UTF-32 source length"
        );
        let little = bytes[0] == 0xff;
        return bytes[4..]
            .chunks_exact(4)
            .map(|word| {
                let word: [u8; 4] = word.try_into().unwrap();
                let scalar = if little {
                    u32::from_le_bytes(word)
                } else {
                    u32::from_be_bytes(word)
                };
                char::from_u32(scalar).ok_or_else(|| anyhow::anyhow!("Invalid UTF-32 scalar"))
            })
            .collect();
    }
    if language == Language::CSharp
        && (bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]))
    {
        anyhow::ensure!(
            bytes.len().is_multiple_of(2),
            "Invalid UTF-16 source length"
        );
        let little = bytes[0] == 0xff;
        let words: Vec<_> = bytes[2..]
            .chunks_exact(2)
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        Ok(String::from_utf16(&words)?)
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.strip_prefix('\u{feff}').unwrap_or(text).to_owned()),
            Err(error) if language == Language::Rust || bytes.starts_with(&[0xef, 0xbb, 0xbf]) => {
                Err(error.into())
            }
            Err(_) => {
                let mut detector = chardetng::EncodingDetector::new();
                detector.feed(bytes, true);
                let encoding = detector.guess(None, false);
                let (text, _, malformed) = encoding.decode(bytes);
                anyhow::ensure!(
                    !malformed,
                    "Malformed source in detected encoding {}",
                    encoding.name()
                );
                tracing::debug!(
                    encoding = encoding.name(),
                    "Detected legacy C# source encoding"
                );
                Ok(text.into_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_encoding_precedence_and_detection() {
        use super::{Language, decode};
        let text = "class Café { string s = \"128 × 128 pixels\"; }";
        let legacy: Vec<_> = text.chars().map(|c| c as u8).collect();
        assert_eq!(decode(text.as_bytes(), Language::CSharp).unwrap(), text);
        assert_eq!(decode(&legacy, Language::CSharp).unwrap(), text);
        assert!(decode(&legacy, Language::Rust).is_err());
        let utf16: Vec<_> = [0xff, 0xfe]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode(&utf16, Language::CSharp).unwrap(), text);
        assert!(decode(&[0xef, 0xbb, 0xbf, 0xff], Language::CSharp).is_err());
    }
    use super::*;
    #[test]
    fn unicode_columns_round_trip() {
        let s = "// λ\r\n\tαβ x";
        for (i, _) in s.char_indices() {
            let (l, c) = position(s, i);
            assert_eq!(offset(s, l, c), Some(i));
        }
        assert_eq!(offset(s, 2, 5), s.find('x'));
    }
}
