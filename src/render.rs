//! Markdown presentation shared by all search modes.
use crate::model::Language;

pub fn inline(value: &str) -> String {
    let fence = "`".repeat(value.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1);
    if value.starts_with('`') || value.ends_with('`') {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

pub fn result(symbol: &str, location: &str, source: &str, language: Language) -> String {
    let source = dedent(source);
    let fence = "`".repeat(3.max(source.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1));
    let language = match language {
        Language::CSharp => "cs",
        Language::Rust => "rust",
    };
    format!(
        "# {}\n{}\n\n{fence}{language}\n{source}\n{fence}",
        inline(symbol),
        inline(location)
    )
}

pub fn excerpt(source: &str) -> String {
    if source.chars().count() <= 1500 {
        return source.into();
    }
    let text = source.lines().take(12).collect::<Vec<_>>().join("\n");
    let text: String = text.chars().take(1500).collect();
    let mut text = dedent(&text);
    text.push_str("\n// ...");
    text
}

pub fn source_excerpt(source: &str, span: std::ops::Range<usize>) -> String {
    let line_start = source[..span.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let margin = &source[line_start..span.start];
    if margin.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
        excerpt(&format!("{margin}{}", &source[span]))
    } else {
        excerpt(&source[span])
    }
}

fn dedent(source: &str) -> String {
    let source = source.trim_matches(['\r', '\n']);
    let lines: Vec<_> = source.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    // Syntax spans often start at the first token, after the first line's margin.
    let skip_first = lines.first().is_some_and(|line| indent(line) == 0);
    let margin = lines
        .iter()
        .skip(usize::from(skip_first))
        .filter(|line| !line.trim().is_empty())
        .map(|line| indent(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 && skip_first {
                *line
            } else {
                &line[indent(line).min(margin)..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

pub fn error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    let message = message
        .rsplit_once(" (os error ")
        .filter(|(_, suffix)| suffix.ends_with(')'))
        .map_or(message.as_str(), |(text, _)| text)
        .trim_end_matches('.');
    message
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "."
}

pub fn omission(total: Option<usize>) -> String {
    match total {
        Some(total) => {
            format!("More matches omitted. Repeat with `limit:{total}` to retrieve all results.")
        }
        None => "More matches omitted. Repeat with a higher `limit:` value.".into(),
    }
}

pub fn metadata(source: &str) -> String {
    use std::sync::LazyLock;
    static ARITY: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"`([0-9]+)").unwrap());
    static PARAMETER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"!!?([0-9]+)").unwrap());
    static PRIMITIVE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\bSystem\.(Boolean|Byte|SByte|Int16|UInt16|Int32|UInt32|Int64|UInt64|Single|Double|Decimal|Char|String|Object|Void)\b").unwrap()
    });
    let source = ARITY.replace_all(source, |capture: &regex::Captures<'_>| {
        if source[capture.get(0).unwrap().end()..].starts_with('<') {
            return String::new();
        }
        let count = capture[1].parse::<usize>().unwrap_or(0).min(256);
        format!(
            "<{}>",
            (0..count)
                .map(|i| format!("T{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let source = PARAMETER.replace_all(&source, "T$1");
    PRIMITIVE
        .replace_all(&source, |capture: &regex::Captures<'_>| match &capture[1] {
            "Boolean" => "bool",
            "Byte" => "byte",
            "SByte" => "sbyte",
            "Int16" => "short",
            "UInt16" => "ushort",
            "Int32" => "int",
            "UInt32" => "uint",
            "Int64" => "long",
            "UInt64" => "ulong",
            "Single" => "float",
            "Double" => "double",
            "Decimal" => "decimal",
            "Char" => "char",
            "String" => "string",
            "Object" => "object",
            "Void" => "void",
            _ => unreachable!(),
        })
        .into_owned()
}

pub fn external_declaration(declaration: &crate::model::Declaration, signature: &str) -> String {
    if !declaration.named_type() {
        return metadata(signature);
    }
    let bases = declaration
        .bases
        .iter()
        .filter(|base| {
            !matches!(
                base.as_str(),
                "System.Object" | "System.ValueType" | "System.Enum"
            )
        })
        .map(|base| metadata(base))
        .collect::<Vec<_>>();
    let mut source = format!(
        "{} {} {}",
        declaration.access,
        declaration.kind,
        metadata(&declaration.name)
    );
    if !bases.is_empty() {
        source.push_str(" : ");
        source.push_str(&bases.join(", "));
    }
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn code_preserves_relative_indentation_and_embedded_fences() {
        let source = "    fn f() {\n        // ```\n    }";
        let rendered = result("a::f", "A folder/a.rs:1:5", source, Language::Rust);
        assert!(rendered.contains("`A folder/a.rs:1:5`"));
        assert!(rendered.contains("````rust\nfn f() {\n    // ```\n}\n````"));
        assert_eq!(
            dedent("void F() {\n        Run();\n    }"),
            "void F() {\n    Run();\n}"
        );
        let large = format!("    void F() {{\n{}    }}", "        Run();\n".repeat(200));
        let excerpt = source_excerpt(&large, large.find("void").unwrap()..large.len());
        assert!(excerpt.starts_with("void F() {\n    Run();\n"));
        assert!(excerpt.ends_with("// ..."));
    }
    #[test]
    fn metadata_uses_source_type_syntax() {
        assert_eq!(
            metadata("System.Collections.Generic.List`1<System.String> F(!!0, System.Int32)"),
            "System.Collections.Generic.List<string> F(T0, int)"
        );
    }

    #[test]
    fn errors_are_blockquotes_with_readable_values() {
        let error = anyhow::anyhow!(
            "Cannot open {}: No such file or directory (os error 2)",
            inline("/A folder/missing")
        );
        assert_eq!(
            super::error(&error),
            "> Cannot open `/A folder/missing`: No such file or directory."
        );
        let error = crate::query::Query::parse("unknownfilter:value").unwrap_err();
        assert_eq!(super::error(&error), "> Unknown qualifier `unknownfilter`.");
    }
}
