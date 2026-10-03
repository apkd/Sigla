//! A source lexer for opaque directives and ShaderLab block boundaries.
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Name,
    Literal,
    Punctuation,
}

pub struct Token {
    pub span: Range<usize>,
    pub kind: Kind,
}

pub struct Lexer<'a> {
    source: &'a str,
    at: usize,
    end: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str, range: Range<usize>) -> Self {
        Self {
            source,
            at: range.start,
            end: range.end,
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        while self.at < self.end {
            let rest = &self.source[self.at..self.end];
            let ch = rest.chars().next()?;
            if ch.is_whitespace() {
                self.at += ch.len_utf8();
            } else if rest.starts_with("//") {
                self.at += rest.find('\n').unwrap_or(rest.len());
            } else if let Some(body) = rest.strip_prefix("/*") {
                self.at += body.find("*/").map_or(rest.len(), |n| n + 4);
            } else if rest.starts_with("\\\n") || rest.starts_with("\\\r\n") {
                self.at += if rest.as_bytes()[1] == b'\r' { 3 } else { 2 };
            } else {
                break;
            }
        }
        if self.at == self.end {
            return None;
        }
        let start = self.at;
        let rest = &self.source[start..self.end];
        for prefix in ["u8R\"", "uR\"", "UR\"", "LR\"", "R\""] {
            if let Some(body) = rest.strip_prefix(prefix) {
                self.at = self.end;
                if let Some(open) = body.find('(').filter(|&n| n <= 16) {
                    let close = format!("){}\"", &body[..open]);
                    if let Some(end) = body[open + 1..].find(&close) {
                        self.at = start + prefix.len() + open + 1 + end + close.len();
                    }
                }
                return Some(Token {
                    span: start..self.at,
                    kind: Kind::Literal,
                });
            }
        }
        for prefix in [
            "u8\"", "u\"", "U\"", "L\"", "\"", "u8'", "u'", "U'", "L'", "'",
        ] {
            if rest.starts_with(prefix) {
                let quote = prefix.as_bytes()[prefix.len() - 1];
                self.at += prefix.len();
                let mut escaped = false;
                for &byte in &self.source.as_bytes()[self.at..self.end] {
                    self.at += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == quote {
                        break;
                    }
                }
                return Some(Token {
                    span: start..self.at,
                    kind: Kind::Literal,
                });
            }
        }
        let first = rest.chars().next()?;
        let kind = if first == '_' || first.is_alphabetic() {
            self.at += rest
                .find(|c: char| c != '_' && !c.is_alphanumeric())
                .unwrap_or(rest.len());
            Kind::Name
        } else if first.is_ascii_digit() {
            self.at += rest
                .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '\''))
                .unwrap_or(rest.len());
            Kind::Literal
        } else {
            self.at += first.len_utf8();
            Kind::Punctuation
        };
        Some(Token {
            span: start..self.at,
            kind,
        })
    }
}
