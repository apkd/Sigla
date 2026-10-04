use anyhow::{Context, Result, bail, ensure};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Location {
    pub path: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Target {
    pub name: String,
    pub parameters: Option<Vec<String>>,
    pub location: Option<Location>,
    pub qualifiers: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Filter {
    pub key: String,
    pub value: String,
    pub negate: bool,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub selector: String,
    pub target: Target,
    pub filters: Vec<Filter>,
    pub loose: bool,
    pub limit: usize,
    pub wait_complete: bool,
}

impl Query {
    pub(crate) fn file_language_filters(&self, language: crate::model::Language) -> bool {
        use crate::model::Language;
        let dialects: &[Language] = match language {
            Language::Header => &[Language::C, Language::Cpp],
            Language::ShaderLab => &[Language::Hlsl, Language::Glsl, Language::ShaderLab],
            _ => std::slice::from_ref(&language),
        };
        dialects
            .iter()
            .any(|&dialect| self.language_filters(dialect, language))
    }
    pub(crate) fn language_filters(
        &self,
        dialect: crate::model::Language,
        container: crate::model::Language,
    ) -> bool {
        use crate::{model::Language, native::language_matches};
        self.filters.iter().filter(|f| f.key == "lang").all(|f| {
            (language_matches(&f.value, dialect)
                || matches!(container, Language::Header | Language::ShaderLab)
                    && language_matches(&f.value, container))
                != f.negate
        })
    }
}

const SELECTORS: &[&str] = &[
    "symbol",
    "type",
    "class",
    "interface",
    "struct",
    "union",
    "enum",
    "delegate",
    "method",
    "property",
    "field",
    "event",
    "constructor",
    "operator",
    "namespace",
    "function",
    "trait",
    "module",
    "macro",
    "const",
    "static",
    "variant",
    "uses",
    "calls",
    "writes",
    "derived",
    "impl",
    "text",
    "file",
    "instance",
    "references",
    "dependencies",
];
const FILTERS: &[&str] = &[
    "project",
    "namespace",
    "access",
    "path",
    "attr",
    "in",
    "unity-project",
    "type-match",
    "lang",
];

impl Query {
    pub fn parse(input: &str) -> Result<Self> {
        let tokens = lex(input)?;
        let mut primary = None;
        let mut namespaces = Vec::new();
        let mut filters = Vec::new();
        let mut loose = None;
        let mut limit = None;
        let mut offset = false;
        let mut wait_complete = false;
        for token in tokens {
            let split = token.find(':').filter(|&i| {
                !token.starts_with('@')
                    && !token[i..].starts_with("::")
                    && token[..i]
                        .chars()
                        .all(|c| c.is_ascii_alphabetic() || c == '-')
            });
            let (raw_key, value) = split.map_or(("symbol", token.as_str()), |i| {
                (&token[..i], &token[i + 1..])
            });
            let negate = raw_key.starts_with('-');
            let key = match raw_key.trim_start_matches('-') {
                "t" => "type",
                "c" => "class",
                "i" => "interface",
                "m" => "method",
                "x" => "text",
                "language" => "lang",
                x => x,
            };
            ensure!(!value.is_empty(), "`{key}:` needs a value.");
            ensure!(
                !negate || FILTERS.contains(&key),
                "Only metadata and containment filters can be negated."
            );
            match key {
                "wait" => {
                    ensure!(
                        value == "complete",
                        "Use `wait:complete` to wait for all relevant indexes."
                    );
                    wait_complete = true;
                }
                "offset" => offset = true,
                "match" => {
                    ensure!(loose.is_none(), "Duplicate `match:` control.");
                    loose = Some(match value {
                        "exact" => false,
                        "loose" => true,
                        _ => bail!("Use `match:exact` or `match:loose`."),
                    });
                }
                "limit" => {
                    ensure!(limit.is_none(), "Duplicate `limit:` control.");
                    let n = value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=MAX_LIMIT).contains(n));
                    limit = Some(n.ok_or_else(|| {
                        anyhow::anyhow!("`limit:` must be between `1` and `{MAX_LIMIT}`.")
                    })?);
                }
                "namespace" => namespaces.push(Filter {
                    key: key.into(),
                    value: value.into(),
                    negate,
                }),
                k if FILTERS.contains(&k) => filters.push(Filter {
                    key: k.into(),
                    value: value.into(),
                    negate,
                }),
                k if SELECTORS.contains(&k) => {
                    ensure!(primary.is_none(), "Use one primary selector per query.");
                    primary = Some((k.to_owned(), value.to_owned()));
                }
                _ => bail!("Unknown qualifier {}.", crate::render::inline(key)),
            }
        }
        if primary.is_none() && namespaces.first().is_some_and(|f| !f.negate) {
            let f = namespaces.remove(0);
            primary = Some(("namespace".into(), f.value));
        }
        filters.extend(namespaces);
        if primary.is_none() && filters.iter().any(|f| f.key == "path" && !f.negate) {
            primary = Some(("symbol".into(), "*".into()));
        }
        let (selector, value) = primary.ok_or_else(|| {
            anyhow::anyhow!("Provide a search target; use `symbol:*` with filters.")
        })?;
        if offset {
            let filters = if selector == "file" {
                "`path:` or `project:`"
            } else {
                "`path:` or `in:`"
            };
            bail!(
                "`offset:` is unsupported. Narrow the query with {filters}, or increase `limit:`."
            );
        }
        for f in &filters {
            if f.key == "path" {
                globset::Glob::new(&f.value)?;
            }
            if f.key == "in" {
                Target::parse(&f.value)?;
            }
        }
        if selector == "file" {
            globset::Glob::new(&value)?;
            ensure!(
                filters
                    .iter()
                    .all(|f| matches!(f.key.as_str(), "path" | "project" | "lang")),
                "`file:` supports `project:`, `path:`, and `lang:` filters."
            );
        }
        if selector == "text" {
            ensure!(
                filters
                    .iter()
                    .all(|f| matches!(f.key.as_str(), "path" | "project" | "in" | "lang")),
                "`text:` supports `project:`, `path:`, `in:`, and `lang:` filters."
            );
        }
        let asset_query = matches!(
            selector.as_str(),
            "instance" | "references" | "dependencies"
        );
        if asset_query {
            ensure!(
                filters
                    .iter()
                    .all(|f| matches!(f.key.as_str(), "path" | "unity-project" | "type-match")),
                "Asset queries support path:, unity-project:, and type-match: filters"
            );
            for filter in &filters {
                if filter.key == "type-match" {
                    ensure!(
                        !filter.negate && matches!(filter.value.as_str(), "exact" | "derived"),
                        "Use type-match:exact or type-match:derived"
                    );
                }
            }
        } else {
            ensure!(
                !filters
                    .iter()
                    .any(|f| matches!(f.key.as_str(), "unity-project" | "type-match")),
                "Unity filters require an asset selector"
            );
        }
        let target = if asset_query || matches!(selector.as_str(), "text" | "file") {
            Target {
                name: value,
                parameters: None,
                location: None,
                qualifiers: None,
            }
        } else if selector == "operator" && value.starts_with("()") {
            Target::parse(&format!("operator{value}"))?
        } else {
            Target::parse(&value)?
        };
        Ok(Self {
            selector,
            target,
            filters,
            loose: loose.unwrap_or(false),
            limit: limit.unwrap_or(DEFAULT_LIMIT),
            wait_complete,
        })
    }

    pub fn relationship(&self) -> bool {
        matches!(
            self.selector.as_str(),
            "uses" | "calls" | "writes" | "derived" | "impl"
        )
    }
}

impl Target {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(path) = value.strip_prefix('@') {
            let mut parts = path.rsplitn(3, ':');
            if let (Some(column), Some(line), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            {
                let line = line.parse::<usize>()?;
                let column = column.parse::<usize>()?;
                ensure!(
                    line > 0 && column > 0 && !path.is_empty(),
                    "Locations use `@path:line:column` with positive coordinates."
                );
                ensure!(
                    !std::path::Path::new(path)
                        .components()
                        .any(|c| c == std::path::Component::ParentDir),
                    "Location paths must not contain parent-directory components."
                );
                return Ok(Self {
                    name: String::new(),
                    parameters: None,
                    location: Some(Location {
                        path: path.into(),
                        line,
                        column,
                    }),
                    qualifiers: None,
                });
            }
        }
        let opening = value.match_indices('(').find_map(|(i, _)| {
            (!(value[..i].ends_with("operator") && value[i..].starts_with("()"))).then_some(i)
        });
        let (name, parameters, qualifiers) = if let Some(i) = opening {
            let mut depth = 0;
            let end = value[i..]
                .char_indices()
                .find_map(|(j, c)| {
                    if c == '(' {
                        depth += 1;
                    }
                    if c == ')' {
                        depth -= 1;
                    }
                    (depth == 0).then_some(i + j)
                })
                .context("Unclosed parameter signature.")?;
            let suffix = value[end + 1..].trim();
            ensure!(
                suffix.is_empty()
                    || [
                        "const", "volatile", "&", "noexcept", "override", "final", "requires", "->"
                    ]
                    .iter()
                    .any(|p| suffix.starts_with(p)),
                "Invalid trailing member qualifiers."
            );
            (
                &value[..i],
                Some(split_parameters(&value[i + 1..end])?),
                (!suffix.is_empty()).then(|| suffix.to_owned()),
            )
        } else {
            (value, None, None)
        };
        ensure!(!name.is_empty(), "A target needs a name.");
        Ok(Self {
            name: normalize_name(name),
            parameters,
            location: None,
            qualifiers,
        })
    }
}

pub fn normalize_name(name: &str) -> String {
    name.split("::")
        .map(|part| part.trim_start_matches("r#"))
        .collect::<Vec<_>>()
        .join("::")
        .split('.')
        .map(|part| part.trim_start_matches('@'))
        .collect::<Vec<_>>()
        .join(".")
}

pub fn split_parameters(input: &str) -> Result<Vec<String>> {
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut depth = 0i32;
    let mut start = 0;
    let mut result = Vec::new();
    let mut previous = '\0';
    for (i, c) in input.char_indices() {
        match c {
            '(' | '[' | '<' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '>' if previous != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                result.push(input[start..i].trim().to_owned());
                start = i + 1;
            }
            _ => {}
        }
        ensure!(depth >= 0, "Unbalanced signature delimiters.");
        previous = c;
    }
    ensure!(depth == 0, "Unbalanced signature delimiters.");
    result.push(input[start..].trim().to_owned());
    ensure!(
        result.iter().all(|s| !s.is_empty()),
        "Empty signature parameter."
    );
    Ok(result)
}

fn lex(input: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut token = String::new();
    let mut stack = Vec::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                    token.push(chars.next().unwrap())
                }
                _ => token.push(c),
            }
            continue;
        }
        if c == '"' {
            quoted = true;
            continue;
        }
        if c == '\'' && (token.is_empty() || token.ends_with(':')) {
            let mut value = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '\'' {
                    closed = true;
                    break;
                }
                value.push(next);
            }
            if closed {
                let suggestion = format!("{token}{}", serde_json::to_string(&value)?);
                bail!("Use double quotes: {}.", crate::render::inline(&suggestion));
            }
            bail!("Use double quotes around query values, for example `text:\"hello world\"`.");
        }
        let literal =
            token.starts_with("operator:") || token.starts_with("text:") || token.starts_with("x:");
        if !literal {
            match c {
                '(' | '[' | '{' => stack.push(c),
                '<' if !stack.contains(&'{')
                    && !token.ends_with("operator")
                    && !token.ends_with("operator<") =>
                {
                    stack.push(c)
                }
                ')' | ']' | '}' => {
                    let expected = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    ensure!(
                        stack.pop() == Some(expected),
                        "Unbalanced query delimiters."
                    );
                }
                '>' if !token.ends_with('-') && stack.last() == Some(&'<') => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if c.is_whitespace() && stack.is_empty() {
            if !token.is_empty() {
                result.push(std::mem::take(&mut token));
            }
        } else {
            token.push(c);
        }
    }
    ensure!(!quoted, "Unclosed query quote.");
    ensure!(stack.is_empty(), "Unclosed query delimiter.");
    if !token.is_empty() {
        result.push(token);
    }
    Ok(result)
}

/// a linear-space wildcard matcher; only '*' is special.
pub fn wildcard(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if i < p.len() && p[i] == v[j] {
            i += 1;
            j += 1;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

pub fn name_rank(pattern: &str, value: &str, loose: bool) -> Option<u8> {
    if pattern == value || wildcard(pattern, value) {
        return Some(0);
    }
    if !loose {
        return None;
    }
    let p = pattern.to_lowercase();
    let v = value.to_lowercase();
    if p == v || wildcard(&p, &v) {
        return Some(1);
    }
    if v.starts_with(&p) {
        return Some(2);
    }
    let chars: Vec<_> = value.chars().collect();
    let initials: String = chars
        .iter()
        .enumerate()
        .filter(|&(i, c)| {
            c.is_alphanumeric()
                && (i == 0
                    || !chars[i - 1].is_alphanumeric()
                    || (c.is_uppercase()
                        && (chars[i - 1].is_lowercase()
                            || chars.get(i + 1).is_some_and(|n| n.is_lowercase()))))
        })
        .map(|(_, c)| c.to_ascii_lowercase())
        .collect();
    if initials.starts_with(&p) {
        return Some(3);
    }
    v.contains(&p).then_some(4)
}

/// Qualified targets may omit leading namespace or module components.
pub fn qualified_name_rank(pattern: &str, value: &str, loose: bool) -> Option<u8> {
    let separator = if pattern.contains("::") {
        "::"
    } else if pattern.contains('.') {
        "."
    } else {
        return name_rank(pattern, value, loose);
    };
    let mut best =
        name_rank(pattern, value, loose).map(|rank| if rank == 0 { 0 } else { rank + 2 });
    for (at, _) in value.match_indices(separator) {
        if let Some(rank) = name_rank(pattern, &value[at + separator.len()..], loose) {
            let rank = if rank == 0 { 1 } else { rank + 2 };
            best = Some(best.map_or(rank, |current| current.min(rank)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offset_recovery_respects_selector_in_any_order() {
        for query in ["file:*.cs offset:1", "offset:1 file:*.cs"] {
            let message = Query::parse(query).unwrap_err().to_string();
            assert!(
                message.contains("`path:`")
                    && message.contains("`project:`")
                    && !message.contains("`in:`"),
                "{message}"
            );
        }
        assert!(
            Query::parse("calls:Run offset:1")
                .unwrap_err()
                .to_string()
                .contains("`in:`")
        );
    }
    #[test]
    fn concrete_type_selectors_and_aliases() {
        for (selector, kind) in [
            ("c", "class"),
            ("i", "interface"),
            ("class", "class"),
            ("interface", "interface"),
            ("struct", "struct"),
            ("enum", "enum"),
            ("delegate", "delegate"),
        ] {
            assert_eq!(
                Query::parse(&format!("{selector}:Example"))
                    .unwrap()
                    .selector,
                kind
            );
        }
    }
    #[test]
    fn qualification_matches_whole_components_and_ranks_exact_names_first() {
        for (target, full, wrong) in [
            ("Type.Run", "Library.Type.Run", "Library.OtherType.Run"),
            (
                "inner::run",
                "crate_name::outer::inner::run",
                "crate_name::other_inner::run",
            ),
            (
                "Outer.Inner.Run",
                "Library.Outer.Inner.Run",
                "Library.OtherOuter.Inner.Run",
            ),
        ] {
            assert!(qualified_name_rank(target, full, false).is_some());
            assert!(qualified_name_rank(target, wrong, false).is_none());
            assert!(
                qualified_name_rank(target, target, false).unwrap()
                    < qualified_name_rank(target, full, false).unwrap()
            );
            assert!(
                qualified_name_rank(target, full, true).unwrap()
                    < qualified_name_rank(target, &full.to_uppercase(), true).unwrap()
            );
        }
        assert!(qualified_name_rank("Type.R*", "Library.Type.Run", false).is_some());
        assert!(qualified_name_rank("inner::*", "crate_name::inner::run", false).is_some());
        assert!(qualified_name_rank("Type.Run", "Library.Type.RunExtra", false).is_none());
    }
    #[test]
    fn contract_examples() {
        for q in [
            "Widget",
            "method:Parse(string, int)",
            "method:Parse(Dictionary<string,List<int>>, ref int)",
            "method:crate::Parser::parse(&'a str)",
            "method:<T as Trait>::parse(&str)",
            "method:f([u8; 32], fn(i32) -> bool)",
            "operator:*",
            "operator:<<",
            "@class",
            "@dir:name/File.cs:4:9",
            "uses:\"@Assets/My Folder/F.cs:4:9\"",
            "text:\"a:b c\"",
            "calls:* in:Parser.Parse",
            "type:Parser namespace:MyApp.Syntax",
            "path:src/** limit:5",
            "GameManager limit:1",
            "file:*.rs path:src",
            "file:\"src/My Folder/Code.cs\"",
        ] {
            Query::parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
        for q in [
            "",
            "limit:0",
            "limit:-1",
            "method:Parse text:hello",
            "case:ignore",
            "file:X method:Y",
            "file:X in:Y",
            "method:f(",
            "text:\"oops",
            "Widget limit:200 limit:300",
        ] {
            assert!(Query::parse(q).is_err(), "{q}");
        }
    }
    #[test]
    fn literal_escapes_and_unicode() {
        assert_eq!(
            Query::parse(r#"text:"a\nb\\c\"d""#).unwrap().target.name,
            "a\\nb\\c\"d"
        );
        assert!(wildcard("é*λ", "éabcλ"));
        assert!(wildcard("*", "*abc"));
        assert!(wildcard("*λ", "éλ"));
        assert!(!wildcard("A*", "a"));
        assert!(name_rank("hp", "HTTPParser", true).is_some());
        assert_eq!(Target::parse("f()").unwrap().parameters, Some(vec![]));
    }
}
