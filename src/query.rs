use anyhow::{Context, Result, bail, ensure};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Location {
    pub path: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Target {
    pub name: String,
    pub parameters: Option<Vec<String>>,
    pub location: Option<Location>,
    pub qualifiers: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Filter {
    pub key: String,
    pub value: String,
    pub negate: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
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

fn qualifier(token: &str) -> Option<(&str, &str)> {
    let split = token.find(':').filter(|&i| {
        !token.starts_with('@')
            && !token[i..].starts_with("::")
            && token[..i]
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '-')
    })?;
    Some((&token[..split], &token[split + 1..]))
}

fn canonical_key(key: &str) -> &str {
    match key {
        "t" => "type",
        "c" => "class",
        "i" => "interface",
        "m" => "method",
        "x" => "text",
        "language" => "lang",
        "call" => "calls",
        "use" => "uses",
        "write" => "writes",
        "reference" => "references",
        "dependency" => "dependencies",
        "attribute" => "attr",
        other => other,
    }
}

fn token_target(token: &str) -> (&str, &str) {
    qualifier(token).map_or(("symbol", token), |(key, value)| {
        (canonical_key(key), value)
    })
}

/// Render suggestions through the same quoting rules as query input.
pub(crate) fn suggestion(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| {
            let token = if let Some((key, value)) = qualifier(token) {
                let sign = if key.starts_with('-') { "-" } else { "" };
                format!(
                    "{sign}{}:{value}",
                    canonical_key(key.trim_start_matches('-'))
                )
            } else {
                token.clone()
            };
            crate::navigation::quote(&token)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn conflict(tokens: &[String], second: usize) -> String {
    let is_target = |token: &str| {
        let (key, _) = token_target(token);
        key != "namespace" && SELECTORS.contains(&key)
    };
    let two_targets = tokens.iter().filter(|t| is_target(t)).count() == 2;
    let first = tokens[..second]
        .iter()
        .position(|t| {
            let (key, _) = token_target(t);
            key != "namespace" && SELECTORS.contains(&key)
        })
        .expect("a preceding primary selector");
    let (a, av) = token_target(&tokens[first]);
    let (b, bv) = token_target(&tokens[second]);
    let describe = |i: usize| {
        if qualifier(&tokens[i]).is_none() {
            format!("bare name {}", crate::render::inline(&tokens[i]))
        } else {
            crate::render::inline(&suggestion(&tokens[i..=i]))
        }
    };
    let mut message = format!(
        "{} and {} both specify search targets.",
        describe(first),
        describe(second)
    );
    let corrected = |replace: usize, value: String, remove: usize| {
        let mut copy = tokens.to_vec();
        copy[replace] = value;
        copy.remove(remove);
        suggestion(&copy)
    };
    let declaration = |key: &str| {
        !matches!(
            key,
            "uses"
                | "calls"
                | "writes"
                | "derived"
                | "impl"
                | "file"
                | "text"
                | "instance"
                | "references"
                | "dependencies"
        )
    };
    let wildcard =
        if two_targets && av == "*" && declaration(a) && qualifier(&tokens[second]).is_none() {
            Some((first, second, a, bv))
        } else if two_targets && bv == "*" && declaration(b) && qualifier(&tokens[first]).is_none()
        {
            Some((second, first, b, av))
        } else {
            None
        };
    if let Some((selector, bare, key, value)) = wildcard {
        message.push_str(&format!(
            " To find a {key} named {}, use: {}.",
            crate::render::inline(value),
            crate::render::inline(&corrected(selector, format!("{key}:{value}"), bare))
        ));
    } else if two_targets && ((a == "file" && declaration(b)) || (b == "file" && declaration(a))) {
        let (index, value) = if a == "file" {
            (first, av)
        } else {
            (second, bv)
        };
        let mut copy = tokens.to_vec();
        copy[index] = format!("path:{value}");
        message.push_str(&format!(
            " `file:` selects files. To restrict declaration search to a file, use: {}.",
            crate::render::inline(&suggestion(&copy))
        ));
    } else if a == "text"
        && tokens[second..]
            .iter()
            .all(|t| qualifier(t).is_none() || !is_target(t))
    {
        let words = tokens[second..]
            .iter()
            .filter(|t| qualifier(t).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let copy = tokens
            .iter()
            .enumerate()
            .filter(|(i, t)| *i < second || qualifier(t).is_some())
            .map(|(i, t)| {
                if i == first {
                    format!("text:{av} {}", words.join(" "))
                } else {
                    t.clone()
                }
            })
            .collect::<Vec<_>>();
        message.push_str(&format!(
            " The bare words are outside the text value. To search for the phrase, use: {}.",
            crate::render::inline(&suggestion(&copy))
        ));
    } else {
        let separate = |keep: usize| {
            tokens
                .iter()
                .enumerate()
                .filter(|(i, t)| *i == keep || !is_target(t))
                .map(|(_, t)| t.clone())
                .collect::<Vec<_>>()
        };
        let left = separate(first);
        let right = separate(second);
        message.push_str(&format!(
            " Search separately: {} or {}.",
            crate::render::inline(&suggestion(&left)),
            crate::render::inline(&suggestion(&right))
        ));
        if qualifier(&tokens[first]).is_none() && qualifier(&tokens[second]).is_none() {
            message.push_str(" To search a text phrase, use `text:` with a double-quoted value.");
        }
    }
    message
}

fn natural_language_guidance(input: &str, tokens: &[String]) -> &'static str {
    if input.contains('"') || tokens.iter().any(|t| qualifier(t).is_some()) {
        return "";
    }
    let words: Vec<_> = tokens
        .iter()
        .map(|t| t.trim_matches([',', '.', ';', '!', '?']))
        .filter(|t| !t.is_empty() && t.chars().all(char::is_alphabetic))
        .collect();
    let connectives = [
        "and", "or", "for", "with", "the", "to", "of", "how", "where",
    ]
    .iter()
    .filter(|word| words.iter().any(|t| t.eq_ignore_ascii_case(word)))
    .count();
    if words.len() >= 6 && connectives >= 2 {
        " This looks like natural language. Sigla uses structured queries. Split the request into separate searches, such as `symbol:Parser`, `text:\"TODO\"`, or `file:*.rs`; use `view` to inspect matching code."
    } else {
        ""
    }
}

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
        for (index, token) in tokens.iter().enumerate() {
            let (raw_key, value) = qualifier(token).unwrap_or(("symbol", token.as_str()));
            let negate = raw_key.starts_with('-');
            let key = canonical_key(raw_key.trim_start_matches('-'));
            if value.is_empty()
                && SELECTORS.contains(&key)
                && !matches!(key, "text" | "references" | "dependencies")
            {
                let mut copy = tokens.clone();
                copy[index] = format!("{key}:*");
                bail!(
                    "`{key}:` needs a name or wildcard. To enumerate matches, use: {}.",
                    crate::render::inline(&suggestion(&copy))
                );
            }
            ensure!(!value.is_empty(), "`{key}:` needs a value.");
            ensure!(
                !negate || FILTERS.contains(&key),
                "Search targets cannot be negated. Exclude a scope with `-path:` or `-in:` instead."
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
                        anyhow::anyhow!("`limit:` must be between `1` and `{MAX_LIMIT}`. Omit it to use the default limit.")
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
                    ensure!(
                        primary.is_none(),
                        "{}{}",
                        conflict(&tokens, index),
                        natural_language_guidance(input, &tokens)
                    );
                    primary = Some((k.to_owned(), value.to_owned()));
                }
                _ => bail!(
                    "Unknown qualifier {}. Select a target with `symbol:`, `method:`, `calls:`, `uses:`, `text:`, or `file:`; narrow it with `in:`, `path:`, `project:`, or `lang:`.",
                    crate::render::inline(key)
                ),
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
            let mut copy = vec!["symbol:*".into()];
            copy.extend(tokens.iter().cloned());
            anyhow::anyhow!("Filters specify where to search, but no search target was provided. To list declarations, use: {}.", crate::render::inline(&suggestion(&copy)))
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
            if filters.iter().any(|f| f.key == "in")
                && filters
                    .iter()
                    .all(|f| matches!(f.key.as_str(), "in" | "path" | "project" | "lang"))
            {
                let mut copy = vec!["symbol:*".into()];
                copy.extend(tokens.iter().map(|token| {
                    if token_target(token).0 == "file" {
                        format!("path:{value}")
                    } else {
                        token.clone()
                    }
                }));
                bail!(
                    "`in:` restricts declarations or text to a containing symbol. To explore declarations in these files, use: {}.",
                    crate::render::inline(&suggestion(&copy))
                );
            }
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
        if let Some(path) = value.strip_prefix('@').filter(|path| !path.contains("::")) {
            let location_error = || {
                anyhow::anyhow!(
                    "Search locations use `@path:line:column` with positive coordinates. To read lines instead, use `view` with a path such as `src/File.cs:20`."
                )
            };
            if !path.contains("::") && path.contains(':') && path.matches(':').count() < 2 {
                return Err(location_error());
            }
            let mut parts = path.rsplitn(3, ':');
            if let (Some(column), Some(line), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            {
                let line = line.parse::<usize>().map_err(|_| location_error())?;
                let column = column.parse::<usize>().map_err(|_| location_error())?;
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

pub(crate) fn lex(input: &str) -> Result<Vec<String>> {
    lex_with(input, false)
}

pub(crate) fn lex_history(input: &str) -> Result<Vec<String>> {
    lex_with(input, true)
}

fn lex_with(input: &str, literal_files: bool) -> Result<Vec<String>> {
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
        let literal = token.starts_with("operator:")
            || token.starts_with("text:")
            || token.starts_with("x:")
            || literal_files && token.starts_with("file:");
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
    fn conflict_corrections_preserve_scope_and_target() {
        for (input, expected) in [
            (
                "method:* in:Database open path:\"my source/**\"",
                "method:open in:Database path:\"my source/**\"",
            ),
            (
                "open path:src/** method:* in:Database",
                "method:open path:src/** in:Database",
            ),
            (
                "method:open file:src/db.rs lang:rust",
                "method:open path:src/db.rs lang:rust",
            ),
            (
                "text:connection failed again path:src/**",
                "text:\"connection failed again\" path:src/**",
            ),
            (
                "text:connection failed path:src/** again",
                "text:\"connection failed again\" path:src/**",
            ),
        ] {
            let expected = serde_json::to_value(Query::parse(expected).unwrap()).unwrap();
            let error = Query::parse(input).unwrap_err().to_string();
            assert!(
                error
                    .split('`')
                    .skip(1)
                    .step_by(2)
                    .filter_map(|s| Query::parse(s).ok())
                    .any(|q| serde_json::to_value(q).unwrap() == expected),
                "{error}"
            );
        }
    }

    #[test]
    fn recovery_names_share_canonical_semantics() {
        for (alias, canonical) in [
            ("call", "calls"),
            ("use", "uses"),
            ("write", "writes"),
            ("reference", "references"),
            ("dependency", "dependencies"),
        ] {
            assert_eq!(
                serde_json::to_value(Query::parse(&format!("{alias}:Target")).unwrap()).unwrap(),
                serde_json::to_value(Query::parse(&format!("{canonical}:Target")).unwrap())
                    .unwrap()
            );
        }
        let query = Query::parse("symbol:* -attribute:Hidden").unwrap();
        assert_eq!(query.filters[0].key, "attr");
        assert!(query.filters[0].negate);
    }

    #[test]
    fn incomplete_locations_do_not_become_symbol_searches() {
        for input in ["@src/db.rs:20", "@src/db.rs:bad:2", "@src/db.rs:2:bad"] {
            assert!(Target::parse(input).is_err());
        }
        assert!(Target::parse("@class").unwrap().location.is_none());
        assert!(Target::parse("module::item").unwrap().location.is_none());
        assert!(Target::parse("@src/db.rs:20:3").unwrap().location.is_some());
    }

    #[test]
    fn prose_errors_preserve_the_error_and_offer_valid_recovery_queries() {
        for input in [
            "Rust CLI argument parsing for --account and --to; grant summary/status output; authentication and execution identity handling; native error reporting and desktop session requirements",
            "How are fields and methods used in the project?",
        ] {
            let message = Query::parse(input).unwrap_err().to_string();
            assert!(message.contains("search targets"));
            let guidance = natural_language_guidance(input, &lex(input).unwrap());
            let suggestions: Vec<_> = guidance
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|s| s.contains(':'))
                .map(|s| Query::parse(s).unwrap())
                .collect();
            assert!(suggestions.iter().any(|q| q.selector == "symbol"));
            assert!(suggestions.iter().any(|q| q.selector == "text"));
        }
    }

    #[test]
    fn structured_and_ambiguous_errors_keep_the_ordinary_diagnostic() {
        for input in [
            "Alpha Beta",
            "How Parse",
            "Alpha Beta Gamma Delta Epsilon Zeta Eta Theta",
            "Alpha and Beta and Gamma and Delta and Epsilon",
            "Find(the, fields, and, methods, for, project) Other",
            "Find the fields and methods for a project \"name with spaces\"",
            "method:Parse text:hello",
        ] {
            assert!(Query::parse(input).is_err(), "{input}");
            assert!(
                natural_language_guidance(input, &lex(input).unwrap()).is_empty(),
                "{input}"
            );
        }
        for key in SELECTORS.iter().chain(FILTERS).chain(&[
            "t", "c", "i", "m", "x", "language", "wait", "offset", "match", "limit",
        ]) {
            let input = format!("Find the fields and methods for a project {key}:Example");
            assert!(Query::parse(&input).is_err(), "{input}");
            assert!(
                natural_language_guidance(&input, &lex(&input).unwrap()).is_empty(),
                "{input}"
            );
        }
        for input in [
            "\"find fields and methods for the project\"",
            "text:\"find fields and methods for the project\"",
            "method:Find(the, fields, and, methods, for, project)",
        ] {
            Query::parse(input).unwrap();
        }
    }

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
