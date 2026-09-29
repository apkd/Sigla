//! Repository path lookup and compact directory rendering. Never reads the filesystem.
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, ops::Range, path::Path, sync::LazyLock};

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Exact,
    #[default]
    Minified,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_tiers_do_not_hide_ambiguity() {
        let paths = ["a/Model.cs", "b/Model.cs", "a/Models.cs", "a/Other.cs"];
        assert_eq!(matches(paths.into_iter(), "a/Model.cs"), vec!["a/Model.cs"]);
        assert_eq!(
            matches(paths.into_iter(), "Model.cs"),
            vec!["a/Model.cs", "b/Model.cs"]
        );
        assert_eq!(
            matches(paths.into_iter(), "model.CS"),
            vec!["a/Model.cs", "b/Model.cs"]
        );
        assert_eq!(matches(paths.into_iter(), "Othre.cs"), vec!["a/Other.cs"]);
        assert!(matches(paths.into_iter(), "NoSuchSource.rs").is_empty());
    }
    #[test]
    fn tree_is_complete_when_small_and_reports_omitted_descendants() {
        let small = [
            "src/a/deep/One.rs",
            "src/a/deep/Two.rs",
            "src/b/line\nbreak.rs",
        ];
        let rendered = Directory::new(small.into_iter()).render("");
        assert!(
            rendered.contains("a/deep/") && rendered.contains("One.rs Two.rs"),
            "{rendered}"
        );
        assert!(rendered.contains(r#""line\nbreak.rs""#), "{rendered}");
        assert!(!rendered.contains('…'));
        let paths: Vec<_> = (0..2000)
            .map(|i| format!("big/File{i:04}.cs"))
            .chain(["small/Useful.cs".into()])
            .collect();
        let tree = Directory::new(paths.iter().map(String::as_str));
        let root = tree.render("");
        assert!(
            root.contains("big/ …2000 files") && root.contains("Useful.cs"),
            "{root}"
        );
        let expanded = tree.at("big").render("big");
        for path in &paths[..2000] {
            assert!(expanded.contains(path.strip_prefix("big/").unwrap()));
        }
        assert!(!expanded.contains('…'));
    }
    #[test]
    fn lines_keep_crlf_and_suffixes_are_unambiguous() {
        for suffix in [":2", ":2:3", "#L2", "(2,3)"] {
            assert_eq!(
                location(&format!("Code.cs{suffix}")).unwrap().1,
                Some((2, 2))
            );
        }
        for suffix in [":2-3", "#L2-L3", ":2:1-3:5", ":2..3"] {
            assert_eq!(
                location(&format!("Code.cs{suffix}")).unwrap().1,
                Some((2, 3))
            );
        }
        assert!(location("Code.cs:0").is_err());
        assert!(location("Code.cs:9-2").is_err());
        let source = "one\r\ntwo\r\nthree";
        let (range, first, last) = lines(source, Some((2, 2))).unwrap();
        assert_eq!(&source[range], "two\r\n");
        assert_eq!((first, last), (2, 2));
    }
}
impl std::str::FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "exact" => Ok(Self::Exact),
            "minified" => Ok(Self::Minified),
            _ => anyhow::bail!("Use mode exact or minified"),
        }
    }
}

pub fn quote(name: &str) -> String {
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '`'))
    {
        serde_json::to_string(name).unwrap()
    } else {
        name.into()
    }
}

pub fn normalize(path: &str, root: &Path, absolute: bool) -> Result<String> {
    let path = Path::new(path);
    ensure!(
        absolute || !path.is_absolute(),
        "Absolute paths are only accepted in local mode"
    );
    let path = if path.is_absolute() {
        path.strip_prefix(root)
            .map_err(|_| anyhow::anyhow!("Path is outside this repository"))?
    } else {
        path
    };
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => (),
            _ => anyhow::bail!("Paths must stay inside the repository"),
        }
    }
    Ok(parts.join("/"))
}

/// All matches from the best qualifying tier; ranking never hides ambiguity.
pub fn matches<'a>(paths: impl Iterator<Item = &'a str>, query: &str) -> Vec<&'a str> {
    if query.is_empty() {
        return Vec::new();
    }
    let lower = query.to_lowercase();
    let mut best = u8::MAX;
    let mut found = Vec::new();
    for path in paths {
        let candidate = path.to_lowercase();
        let (tier, distance) = if path == query {
            (0, 0)
        } else if path.ends_with(&format!("/{query}")) {
            (1, 0)
        } else if candidate == lower || candidate.ends_with(&format!("/{lower}")) {
            (2, 0)
        } else if candidate.contains(&lower) {
            (3, candidate.len() - lower.len())
        } else {
            let segments = query.bytes().filter(|&b| b == b'/').count();
            let candidate = candidate
                .rmatch_indices('/')
                .nth(segments)
                .map_or(candidate.as_str(), |(i, _)| &candidate[i + 1..]);
            let cutoff = (lower.chars().count() / 5).clamp(1, 3);
            let Some(distance) = edit_distance(candidate, &lower, cutoff) else {
                continue;
            };
            (4, distance)
        };
        if tier < best {
            found.clear();
            best = tier;
        }
        if tier == best {
            found.push((distance, path));
        }
    }
    found.sort_unstable();
    found.into_iter().map(|(_, path)| path).collect()
}

fn edit_distance(a: &str, b: &str, cutoff: usize) -> Option<usize> {
    let a: Vec<_> = a.chars().collect();
    let b: Vec<_> = b.chars().collect();
    if a.len().abs_diff(b.len()) > cutoff {
        return None;
    }
    let mut row: Vec<_> = (0..=b.len()).collect();
    let mut older = row.clone();
    for (i, x) in a.iter().enumerate() {
        let old_row = row.clone();
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (diagonal + usize::from(x != y))
                .min(row[j] + 1)
                .min(old + 1);
            if i > 0 && j > 0 && a[i] == b[j - 1] && a[i - 1] == b[j] {
                row[j + 1] = row[j + 1].min(older[j - 1] + 1);
            }
            diagonal = old;
        }
        if *row.iter().min().unwrap() > cutoff {
            return None;
        }
        older = old_row;
    }
    (row[b.len()] <= cutoff).then_some(row[b.len()])
}

pub fn choices(paths: &[&str], kind: &str) -> String {
    if paths.is_empty() {
        return "No matching indexed path.".into();
    }
    let mut text = String::from("Choose a path:\n");
    text.push_str(
        &paths
            .iter()
            .take(4)
            .map(|p| quote(p))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if paths.len() > 4 {
        text.push_str(&format!(
            "\n{} other {kind} matched a similar path; be more specific.",
            paths.len() - 4
        ));
    }
    text
}

/// Literal indexed filenames are resolved before calling this parser.
pub fn location(path: &str) -> Result<(&str, Option<(usize, usize)>)> {
    static SUFFIX: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"^(.*?)(?::L?(\d+)(?::\d+)?(?:(?:-|\.\.)L?(\d+)(?::\d+)?)?|#L?(\d+)(?:-L?(\d+))?|\((\d+)(?:,\s*\d+)?\))$").unwrap()
    });
    let Some(c) = SUFFIX.captures(path) else {
        if path
            .rsplit_once(':')
            .is_some_and(|(_, suffix)| suffix.starts_with(|c: char| c.is_ascii_digit() || c == '-'))
            || path.contains("#L")
        {
            anyhow::bail!("Invalid line range; use path:1-20 (1-based, inclusive)");
        }
        return Ok((path, None));
    };
    let first = c
        .get(2)
        .or_else(|| c.get(4))
        .or_else(|| c.get(6))
        .unwrap()
        .as_str()
        .parse::<usize>()?;
    let last = c
        .get(3)
        .or_else(|| c.get(5))
        .map(|m| m.as_str().parse::<usize>())
        .transpose()?
        .unwrap_or(first);
    ensure!(
        first > 0 && last >= first,
        "Lines are 1-based inclusive; the end must follow the start"
    );
    Ok((c.get(1).unwrap().as_str(), Some((first, last))))
}

pub fn lines(
    source: &str,
    requested: Option<(usize, usize)>,
) -> Result<(Range<usize>, usize, usize)> {
    let mut starts = vec![0];
    starts.extend(
        source
            .match_indices('\n')
            .map(|(i, _)| i + 1)
            .filter(|&i| i < source.len()),
    );
    let (first, last) = requested.unwrap_or((1, starts.len()));
    ensure!(first <= starts.len(), "File has {} lines", starts.len());
    let last = last.min(starts.len());
    Ok((
        starts[first - 1]..starts.get(last).copied().unwrap_or(source.len()),
        first,
        last,
    ))
}

#[derive(Default)]
pub struct Directory {
    files: Vec<String>,
    dirs: BTreeMap<String, Directory>,
    count: usize,
    cost: usize,
}
impl Directory {
    pub fn new<'a>(paths: impl Iterator<Item = &'a str>) -> Self {
        let mut tree = Self::default();
        for path in paths {
            let mut node = &mut tree;
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                node.count += 1;
                if parts.peek().is_some() {
                    node = node.dirs.entry(part.into()).or_default();
                } else {
                    node.files.push(part.into());
                }
            }
        }
        tree.measure();
        tree
    }
    fn measure(&mut self) {
        self.cost = self.files.iter().map(|f| quote(f).len() + 2).sum();
        for (name, child) in &mut self.dirs {
            child.measure();
            self.cost += quote(name).len() + 24 + child.cost + child.count * 2;
        }
    }
    pub fn paths(&self) -> Vec<String> {
        fn visit(node: &Directory, prefix: &str, out: &mut Vec<String>) {
            for (name, child) in &node.dirs {
                let path = format!("{prefix}{name}");
                out.push(path.clone());
                visit(child, &format!("{path}/"), out);
            }
        }
        let mut paths = Vec::new();
        visit(self, "", &mut paths);
        paths
    }
    pub fn at(&self, path: &str) -> &Self {
        path.split('/')
            .filter(|p| !p.is_empty())
            .fold(self, |node, part| &node.dirs[part])
    }
    pub fn render(&self, path: &str) -> String {
        if self.count == 0 {
            return "No indexed source files.".into();
        }
        let mut text = String::new();
        let depth = if path.is_empty() {
            0
        } else {
            text.push_str(&format!("{}\n", quote(&format!("{path}/"))));
            1
        };
        self.write(depth, 24_000, &mut text);
        text.trim_end().into()
    }
    fn write(&self, depth: usize, budget: usize, text: &mut String) {
        let indent = "  ".repeat(depth);
        let mut width = 0;
        for name in &self.files {
            let name = quote(name);
            if width == 0 {
                text.push_str(&indent);
            } else if width + name.len() > 100 {
                text.push('\n');
                text.push_str(&indent);
                width = 0;
            } else {
                text.push(' ');
            }
            text.push_str(&name);
            width += name.len() + 1;
        }
        if width > 0 {
            text.push('\n');
        }
        // Spend first on complete cheap subtrees, then share the rest among large branches.
        let mut remaining = budget.saturating_sub(self.files.iter().map(|f| f.len() + 1).sum());
        let mut costs: Vec<_> = self
            .dirs
            .iter()
            .map(|(name, dir)| (dir.cost, name))
            .collect();
        costs.sort_unstable();
        let mut allocations = BTreeMap::new();
        for (i, (cost, name)) in costs.iter().enumerate() {
            let share = remaining / (costs.len() - i);
            let allocation = (*cost).min(share);
            allocations.insert(*name, allocation);
            remaining -= allocation;
        }
        for (name, child) in &self.dirs {
            let allocation = allocations[name];
            let mut name = name.clone();
            let mut child = child;
            while child.files.is_empty() && child.dirs.len() == 1 {
                let (next, node) = child.dirs.first_key_value().unwrap();
                name.push('/');
                name.push_str(next);
                child = node;
            }
            text.push_str(&indent);
            text.push_str(&quote(&format!("{name}/")));
            let direct_cost: usize = child.files.iter().map(|f| f.len() + 1).sum::<usize>()
                + child.dirs.keys().map(|d| d.len() + 24).sum::<usize>();
            if direct_cost <= allocation {
                text.push('\n');
                child.write(depth + 1, allocation, text);
            } else {
                text.push_str(&format!(" …{} files\n", child.count));
            }
        }
    }
}
