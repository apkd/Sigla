use crate::query::Query;
use anyhow::{Context, Result, ensure};

#[derive(Clone, Debug)]
pub(super) enum Command {
    Log {
        skip: usize,
        limit: usize,
    },
    Show {
        commit: String,
        parent: usize,
        offset: usize,
    },
    Diff {
        from: String,
        to: String,
        offset: usize,
    },
}
#[derive(Clone, Debug)]
pub(super) enum Selector {
    Repository,
    File(String),
    Paths(Vec<String>),
    Declaration(Query),
    Text {
        path: String,
        start: usize,
        end: usize,
    },
}
#[derive(Clone, Debug)]
pub(super) struct Request {
    pub command: Command,
    pub selector: Selector,
}

pub(super) fn path(value: &str) -> Result<()> {
    crate::repository::selection::validate_path(value)?;
    ensure!(!value.contains(['\n', '\r', '\0']), "Invalid history path");
    Ok(())
}
fn number(value: &str) -> Result<usize> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "Expected unsigned integer"
    );
    Ok(value.parse()?)
}
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
impl Request {
    pub fn parse(input: &str) -> Result<Self> {
        ensure!(input.len() <= 16 * 1024, "Query exceeds request size limit");
        let tokens = crate::query::lex(input)?;
        let mut tokens = tokens.into_iter();
        let verb = tokens
            .next()
            .context("Use log, show COMMIT, or diff FROM..TO")?;
        ensure!(
            matches!(verb.as_str(), "log" | "show" | "diff"),
            "Use log, show COMMIT, or diff FROM..TO"
        );
        let endpoint = if verb == "log" {
            None
        } else {
            Some(tokens.next().context("Missing comparison endpoint")?)
        };
        let mut controls = std::collections::BTreeMap::new();
        let mut selected = Vec::new();
        for token in tokens {
            if let Some((key, value)) = token.split_once(':')
                && matches!(key, "skip" | "limit" | "parent" | "offset")
            {
                let valid = match verb.as_str() {
                    "log" => matches!(key, "skip" | "limit"),
                    "show" => matches!(key, "parent" | "offset"),
                    _ => key == "offset",
                };
                ensure!(valid, "{key}: is not valid for {verb}");
                ensure!(
                    controls.insert(key.to_owned(), number(value)?).is_none(),
                    "Duplicate {key}: control"
                );
            } else {
                selected.push(token);
            }
        }
        let command = match verb.as_str() {
            "log" => {
                let limit = controls
                    .get("limit")
                    .copied()
                    .unwrap_or(crate::query::DEFAULT_LIMIT);
                ensure!(
                    (1..=crate::query::MAX_LIMIT).contains(&limit),
                    "Invalid history limit"
                );
                Command::Log {
                    skip: controls.get("skip").copied().unwrap_or(0),
                    limit,
                }
            }
            "show" => {
                let parent = controls.get("parent").copied().unwrap_or(1);
                ensure!(parent > 0, "Parent numbers are 1-based");
                Command::Show {
                    commit: endpoint.unwrap(),
                    parent,
                    offset: controls.get("offset").copied().unwrap_or(0),
                }
            }
            _ => {
                let endpoint = endpoint.unwrap();
                let (from, to) = endpoint.split_once("..").context("Use diff FROM..TO")?;
                ensure!(
                    !from.is_empty()
                        && !to.is_empty()
                        && !to.contains("..")
                        && !to.starts_with('.'),
                    "Use exactly two snapshot endpoints"
                );
                Command::Diff {
                    from: from.into(),
                    to: to.into(),
                    offset: controls.get("offset").copied().unwrap_or(0),
                }
            }
        };
        let selector = if selected.is_empty() {
            Selector::Repository
        } else if selected
            .iter()
            .all(|s| s.starts_with("path:") || s.starts_with("-path:"))
        {
            ensure!(
                selected.iter().any(|s| s.starts_with("path:")),
                "Path history needs a positive path"
            );
            for token in &selected {
                let (_, value) = token.split_once(':').unwrap();
                path(value.trim_end_matches('/'))?;
                globset::Glob::new(value)?;
            }
            Selector::Paths(selected)
        } else if selected.len() == 1 && selected[0].starts_with("file:") {
            let value = selected[0].strip_prefix("file:").unwrap();
            path(value)?;
            ensure!(
                !value.contains(['*', '?', '[']),
                "file: selects one literal path; use path: for patterns"
            );
            Selector::File(value.into())
        } else if selected.len() == 1
            && selected[0].starts_with('@')
            && selected[0]
                .rsplit(':')
                .next()
                .is_some_and(|s| s.contains('-'))
        {
            let (path_value, range) = selected[0][1..]
                .rsplit_once(':')
                .context("Use @path:start-end")?;
            path(path_value)?;
            let (start, end) = range.split_once('-').context("Use @path:start-end")?;
            let (start, end) = (number(start)?, number(end)?);
            ensure!(
                start > 0 && end >= start,
                "Text ranges are 1-based and inclusive"
            );
            Selector::Text {
                path: path_value.into(),
                start,
                end,
            }
        } else {
            let text = selected
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(" ");
            let query = Query::parse(&text)?;
            ensure!(
                !matches!(
                    query.selector.as_str(),
                    "uses"
                        | "calls"
                        | "writes"
                        | "derived"
                        | "impl"
                        | "instance"
                        | "references"
                        | "dependencies"
                        | "text"
                        | "file"
                ),
                "History requires a declaration or source region, not a search operation"
            );
            ensure!(!query.wait_complete, "History does not prepare a workspace");
            ensure!(
                query.filters.iter().all(|f| f.key == "path"),
                "Only path: narrows a history declaration anchor"
            );
            if selected.iter().any(|s| s.starts_with('@')) {
                ensure!(
                    query.target.location.is_some(),
                    "Use @path:line:column or @path:start-end"
                );
            }
            for filter in &query.filters {
                path(&filter.value)?;
                globset::Glob::new(&filter.value)?;
            }
            if let Some(location) = &query.target.location {
                path(&location.path)?;
            }
            Selector::Declaration(query)
        };
        Ok(Self { command, selector })
    }
}

pub(super) struct PathPattern<'a> {
    directory: &'a str,
    glob: Option<globset::GlobMatcher>,
}
impl<'a> PathPattern<'a> {
    pub fn new(pattern: &'a str) -> Self {
        Self {
            directory: pattern.trim_end_matches('/'),
            // Rebased patterns retain literal-prefix matching even if their glob is invalid.
            glob: globset::Glob::new(pattern)
                .ok()
                .map(|glob| glob.compile_matcher()),
        }
    }
    pub fn matches(&self, path: &str) -> bool {
        path == self.directory
            || path
                .strip_prefix(self.directory)
                .is_some_and(|s| s.starts_with('/'))
            || self.glob.as_ref().is_some_and(|glob| glob.is_match(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_paths_preserve_prefix_and_glob_semantics() {
        for (pattern, accepted, rejected) in [
            ("src", "src/nested/a.rs", "src-other/a.rs"),
            ("src/", "src", "src-other"),
            ("src///", "src/a.rs", "src-other/a.rs"),
            ("src/a.rs", "src/a.rs", "src/b.rs"),
            ("src/*", "src/a.rs", "other/a.rs"),
            ("src/**", "src/nested/a.rs", "other/a.rs"),
            ("**/*.rs", "src/nested/a.rs", "src/a.cs"),
            ("src/{a,b}.rs", "src/b.rs", "src/c.rs"),
            ("src/[ab]?.rs", "src/a1.rs", "src/c1.rs"),
            (r"src/\*.rs", "src/*.rs", "src/a.rs"),
            ("src/λ", "src/λ/a.rs", "src/λ-other/a.rs"),
            ("src/[", "src/[/a.rs", "src/a.rs"),
        ] {
            let compiled = PathPattern::new(pattern);
            assert!(compiled.matches(accepted), "{pattern:?}: {accepted:?}");
            assert!(!compiled.matches(rejected), "{pattern:?}: {rejected:?}");
        }
    }
}
