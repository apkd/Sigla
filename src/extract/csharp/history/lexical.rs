//! Only lexical state needed to recognize directive lines. This is deliberately
//! separate from the build preprocessor: historical exploration has no defines.
use anyhow::{Result, ensure};

pub(super) const NESTING: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Regular,
    Verbatim,
    Raw,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Comment,
    Quoted {
        quote: u8,
        style: Style,
        interpolated: bool,
    },
    Raw {
        quotes: usize,
        dollars: usize,
    },
    Hole {
        braces: usize,
        parens: usize,
        brackets: usize,
        close: usize,
    },
    Format {
        close: usize,
        style: Style,
    },
}
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Lexical {
    stack: Vec<Frame>,
}
impl Lexical {
    pub(super) fn code(&self) -> bool {
        self.stack.is_empty() || matches!(self.stack.last(), Some(Frame::Hole { .. }))
    }
    pub(super) fn complete(&self) -> bool {
        self.stack.is_empty()
    }
    fn push(&mut self, frame: Frame) -> Result<()> {
        ensure!(
            self.stack.len() < NESTING,
            "historical C# lexical nesting limit reached"
        );
        self.stack.push(frame);
        Ok(())
    }
    pub(super) fn scan(&mut self, line: &str) -> Result<()> {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            let byte = bytes[i];
            match self.stack.last().copied() {
                Some(Frame::Comment) => {
                    if rest.starts_with(b"*/") {
                        self.stack.pop();
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Some(Frame::Quoted {
                    quote,
                    style,
                    interpolated,
                }) => {
                    if matches!(style, Style::Regular) && byte == b'\\' {
                        i = (i + 2).min(bytes.len());
                    } else if matches!(style, Style::Verbatim) && rest.starts_with(b"\"\"") {
                        i += 2;
                    } else if byte == quote {
                        self.stack.pop();
                        i += 1;
                    } else if interpolated && rest.starts_with(b"{{") {
                        i += 2;
                    } else if interpolated && byte == b'{' {
                        self.push(Frame::Hole {
                            braces: 0,
                            parens: 0,
                            brackets: 0,
                            close: 1,
                        })?;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                Some(Frame::Raw { quotes, dollars }) => {
                    let count = run(rest, byte);
                    if byte == b'"' && count >= quotes {
                        self.stack.pop();
                        i += count;
                    } else if dollars > 0 && byte == b'{' && count >= dollars {
                        self.push(Frame::Hole {
                            braces: 0,
                            parens: 0,
                            brackets: 0,
                            close: dollars,
                        })?;
                        i += count;
                    } else {
                        i += count;
                    }
                }
                Some(Frame::Format { close, style }) => {
                    if byte == b'}' && run(rest, b'}') >= close {
                        self.stack.pop();
                        i += close;
                    } else if byte == b'\\' && matches!(style, Style::Regular) {
                        i = (i + 2).min(bytes.len());
                    } else {
                        i += 1;
                    }
                }
                frame => {
                    if rest.starts_with(b"//") {
                        break;
                    }
                    if rest.starts_with(b"/*") {
                        self.push(Frame::Comment)?;
                        i += 2;
                        continue;
                    }
                    // Consume prefixes here, not by inspecting the byte before a
                    // quote: @$" and $@" are both valid, and holes contain code.
                    let dollars = run(rest, b'$');
                    let (prefix, style, interpolated) =
                        if rest.starts_with(b"@$\"") || rest.starts_with(b"$@\"") {
                            (2, Style::Verbatim, true)
                        } else if rest.starts_with(b"@\"") {
                            (1, Style::Verbatim, false)
                        } else {
                            (dollars, Style::Regular, dollars > 0)
                        };
                    if rest.get(prefix) == Some(&b'"') {
                        let quotes = run(&rest[prefix..], b'"');
                        if quotes >= 3 && !matches!(style, Style::Verbatim) {
                            self.push(Frame::Raw { quotes, dollars })?;
                            i += prefix + quotes;
                        } else {
                            self.push(Frame::Quoted {
                                quote: b'"',
                                style,
                                interpolated,
                            })?;
                            i += prefix + 1;
                        }
                        continue;
                    }
                    if byte == b'\'' {
                        self.push(Frame::Quoted {
                            quote: b'\'',
                            style: Style::Regular,
                            interpolated: false,
                        })?;
                        i += 1;
                        continue;
                    }
                    if let Some(Frame::Hole {
                        mut braces,
                        mut parens,
                        mut brackets,
                        close,
                    }) = frame
                    {
                        if byte == b'}' && braces == 0 && run(rest, b'}') >= close {
                            self.stack.pop();
                            i += close;
                            continue;
                        }
                        if byte == b':' && braces == 0 && parens == 0 && brackets == 0 {
                            if rest.starts_with(b"::") {
                                i += 2;
                                continue;
                            }
                            self.stack.pop();
                            let style = match self.stack.last() {
                                Some(Frame::Quoted { style, .. }) => *style,
                                _ => Style::Raw,
                            };
                            self.push(Frame::Format { close, style })?;
                            i += 1;
                            continue;
                        }
                        match byte {
                            b'{' => braces += 1,
                            b'}' => braces = braces.saturating_sub(1),
                            b'(' => parens += 1,
                            b')' => parens = parens.saturating_sub(1),
                            b'[' => brackets += 1,
                            b']' => brackets = brackets.saturating_sub(1),
                            _ => {}
                        }
                        *self.stack.last_mut().unwrap() = Frame::Hole {
                            braces,
                            parens,
                            brackets,
                            close,
                        };
                    }
                    i += 1;
                }
            }
        }
        if line.ends_with(newline) {
            ensure!(
                !matches!(
                    self.stack.last(),
                    Some(
                        Frame::Quoted {
                            style: Style::Regular,
                            ..
                        } | Frame::Format {
                            style: Style::Regular,
                            ..
                        }
                    )
                ),
                "unfinished historical C# regular literal"
            );
        }
        Ok(())
    }
}
fn run(bytes: &[u8], byte: u8) -> usize {
    bytes.iter().take_while(|&&b| b == byte).count()
}
pub(super) fn newline(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_like_lines_in_literals_and_comments_stay_text() {
        for (open, close) in [
            ("/*\n", "*/\n"),
            ("@\"\n", "\";\n"),
            ("$@\"{{literal}}\n", "\";\n"),
            ("@$\"{F(\"nested\")}\n", "\";\n"),
            ("\"\"\"\n", "\"\"\";\n"),
            ("\"\"\"\"\n\"\"\"\n", "\"\"\"\";\n"),
            ("$$\"\"\"\n{{F(\"nested\")}}\n", "\"\"\";\n"),
            ("$@\"{42:\n", "}\";\n"),
        ] {
            let mut lexical = Lexical::default();
            for line in open.split_inclusive(newline) {
                lexical.scan(line).unwrap();
            }
            assert!(!lexical.code(), "{open}");
            lexical.scan("#if NOT_A_DIRECTIVE\n").unwrap();
            assert!(!lexical.code(), "{open}");
            lexical.scan(close).unwrap();
            assert!(lexical.complete(), "{open}");
        }
    }

    #[test]
    fn escaped_quotes_characters_and_line_comments_restore_code_state() {
        for line in [
            "var s = \"\\\"/*\";\n",
            "char c = '\\\''; // \" /*\n",
            "var s = @\"\"\"value\";\n",
            "var s = $@\"{new { Text = \"}\" }.Text}\";\n",
        ] {
            let mut lexical = Lexical::default();
            lexical.scan(line).unwrap();
            assert!(lexical.complete(), "{line}");
        }
    }
}
