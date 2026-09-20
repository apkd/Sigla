use anyhow::{Result, bail, ensure};
use std::collections::HashSet;

enum Lexical {
    Normal,
    Comment,
    Verbatim,
    Raw(usize),
}
impl Lexical {
    fn scan(&mut self, line: &[u8]) {
        let mut i = 0;
        while i < line.len() {
            match self {
                Self::Comment => {
                    if line[i..].starts_with(b"*/") {
                        *self = Self::Normal;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Self::Verbatim => {
                    if line[i..].starts_with(b"\"\"") {
                        i += 2;
                    } else if line[i] == b'"' {
                        *self = Self::Normal;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                Self::Raw(count) => {
                    let quotes = line[i..].iter().take_while(|&&b| b == b'"').count();
                    if quotes >= *count {
                        *self = Self::Normal;
                        i += quotes;
                    } else {
                        i += quotes.max(1);
                    }
                }
                Self::Normal => {
                    if line[i..].starts_with(b"//") {
                        break;
                    }
                    if line[i..].starts_with(b"/*") {
                        *self = Self::Comment;
                        i += 2;
                        continue;
                    }
                    let quote = line[i];
                    if quote == b'"' {
                        let count = line[i..].iter().take_while(|&&b| b == b'"').count();
                        if count >= 3 {
                            *self = Self::Raw(count);
                            i += count;
                            continue;
                        }
                        if i > 0 && line[i - 1] == b'@' {
                            *self = Self::Verbatim;
                            i += 1;
                            continue;
                        }
                    }
                    i += 1;
                    if quote == b'"' || quote == b'\'' {
                        while i < line.len() {
                            if line[i] == b'\\' {
                                i += 2;
                            } else if line[i] == quote {
                                i += 1;
                                break;
                            } else {
                                i += 1;
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn active_source(source: &str, defines: &[String]) -> Result<String> {
    let mut symbols: HashSet<String> = defines.iter().cloned().collect();
    let mut output = Vec::with_capacity(source.len());
    let mut stack: Vec<(bool, bool)> = Vec::new();
    let mut active = true;
    let mut lexical = Lexical::Normal;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let directive = if matches!(lexical, Lexical::Normal) {
            trimmed.strip_prefix('#')
        } else {
            None
        };
        if let Some(d) = directive {
            let (key, rest) = d
                .trim()
                .split_once(char::is_whitespace)
                .unwrap_or((d.trim(), ""));
            let expr = rest.split("//").next().unwrap_or("").trim();
            match key {
                "if" => {
                    let yes = evaluate(expr, &symbols)?;
                    stack.push((active, yes));
                    active &= yes;
                }
                "elif" => {
                    let (parent, seen) = stack
                        .last_mut()
                        .ok_or_else(|| anyhow::anyhow!("Unmatched #elif"))?;
                    let yes = evaluate(expr, &symbols)?;
                    active = *parent && !*seen && yes;
                    *seen |= yes;
                }
                "else" => {
                    let (parent, seen) = stack
                        .last_mut()
                        .ok_or_else(|| anyhow::anyhow!("Unmatched #else"))?;
                    active = *parent && !*seen;
                    *seen = true;
                }
                "endif" => {
                    active = stack
                        .pop()
                        .ok_or_else(|| anyhow::anyhow!("Unmatched #endif"))?
                        .0;
                }
                "define" if active => {
                    symbols.insert(expr.into());
                }
                "undef" if active => {
                    symbols.remove(expr);
                }
                _ => {}
            }
        }
        if directive.is_some() || !active {
            output.extend(
                line.bytes()
                    .map(|b| if b == b'\n' || b == b'\r' { b } else { b' ' }),
            );
        } else {
            output.extend_from_slice(line.as_bytes());
            lexical.scan(line.as_bytes());
        }
    }
    ensure!(stack.is_empty(), "Unclosed C# #if directive");
    Ok(String::from_utf8(output)?)
}

fn evaluate(expr: &str, symbols: &HashSet<String>) -> Result<bool> {
    struct Parser<'a> {
        rest: &'a str,
        symbols: &'a HashSet<String>,
    }
    impl Parser<'_> {
        fn eat(&mut self, s: &str) -> bool {
            self.rest = self.rest.trim_start();
            if let Some(rest) = self.rest.strip_prefix(s) {
                self.rest = rest;
                true
            } else {
                false
            }
        }
        fn atom(&mut self) -> Result<bool> {
            if self.eat("!") {
                return Ok(!self.atom()?);
            }
            if self.eat("(") {
                let v = self.or()?;
                ensure!(self.eat(")"), "Unclosed preprocessor expression");
                return Ok(v);
            }
            self.rest = self.rest.trim_start();
            let len = self
                .rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(self.rest.len());
            if len == 0 {
                bail!("Invalid preprocessor expression");
            }
            let name = &self.rest[..len];
            self.rest = &self.rest[len..];
            Ok(name == "true" || (name != "false" && self.symbols.contains(name)))
        }
        fn equality(&mut self) -> Result<bool> {
            let mut v = self.atom()?;
            loop {
                if self.eat("==") {
                    v = self.atom()? == v;
                } else if self.eat("!=") {
                    v = self.atom()? != v;
                } else {
                    break;
                }
            }
            Ok(v)
        }
        fn and(&mut self) -> Result<bool> {
            let mut v = self.equality()?;
            while self.eat("&&") {
                v &= self.equality()?;
            }
            Ok(v)
        }
        fn or(&mut self) -> Result<bool> {
            let mut v = self.and()?;
            while self.eat("||") {
                v |= self.and()?;
            }
            Ok(v)
        }
    }
    let mut p = Parser {
        rest: expr,
        symbols,
    };
    let v = p.or()?;
    ensure!(
        p.rest.trim().is_empty(),
        "Unsupported preprocessor expression: {}",
        p.rest
    );
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn branches_preserve_offsets() {
        let s = "#if A && !B\nclass Yes {}\n#else\nclass Nø {}\n#endif\n";
        let a = active_source(s, &["A".into()]).unwrap();
        assert_eq!(a.len(), s.len());
        assert!(a.contains("Yes"));
        assert!(!a.contains("Nø"));
        assert_eq!(a.find("Yes"), s.find("Yes"));
    }
    #[test]
    fn directives_inside_literals_and_comments_are_text() {
        let s = "/*\n#if NEVER\n#endif*/\nclass P { string x = @\"\n#if TEXT\n\"; }\n#if YES\nclass Live {}\n#endif\n";
        let a = active_source(s, &["YES".into()]).unwrap();
        assert!(a.contains("class Live"));
        assert!(a.contains("#if TEXT"));
        assert_eq!(a.len(), s.len());
    }
}
