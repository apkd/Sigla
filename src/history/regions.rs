mod java;

use super::{
    cache::{self, Cache},
    sequence,
};
use crate::{
    extract::{HistoricalDeclaration, HistoricalPresence},
    model::{Declaration, Language},
    repository::history::Store,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
    sync::{Arc, LazyLock},
};

pub(super) const ADAPTER_VERSION: u32 = 6;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct Region {
    pub path: String,
    pub blob: String,
    pub span: Range<usize>,
    pub body: Range<usize>,
    pub name: String,
    pub owner: String,
    pub kind: String,
    pub parameters: Vec<String>,
    pub written: String,
    // Source-local presence; conditional alternatives cannot reserve a continuation.
    pub guard: usize,
    pub weak: bool,
    pub fragment: bool,
}
impl Region {
    pub fn heap_bytes(&self) -> usize {
        self.path.capacity()
            + self.blob.capacity()
            + self.name.capacity()
            + self.owner.capacity()
            + self.kind.capacity()
            + self.written.capacity()
            + self.parameters.capacity() * std::mem::size_of::<String>()
            + self.parameters.iter().map(String::capacity).sum::<usize>()
    }
    pub fn range(&self, source: &Source) -> Range<usize> {
        if self.kind == "file" {
            0..source.text.len()
        } else {
            self.span.clone()
        }
    }
    pub fn body_range(&self, source: &Source) -> Range<usize> {
        if self.kind == "file" {
            0..source.text.len()
        } else {
            self.body.clone()
        }
    }
    pub fn key(&self) -> (&str, &str, usize, usize) {
        (&self.path, &self.blob, self.span.start, self.span.end)
    }
    pub fn same_alternative(&self, other: &Self) -> bool {
        self.key() == other.key()
            && self.body == other.body
            && self.name == other.name
            && self.kind == other.kind
            && self.parameters == other.parameters
            && self.written == other.written
            && self.fragment == other.fragment
    }
    pub fn signature(&self) -> String {
        if matches!(
            self.kind.as_str(),
            "method" | "function" | "constructor" | "operator"
        ) {
            format!("{}({})", self.name, self.parameters.join(","))
        } else {
            self.name.clone()
        }
    }
    pub fn whole(path: &str, blob: &str, size: usize) -> Self {
        Self {
            path: path.into(),
            blob: blob.into(),
            span: 0..size,
            body: 0..size,
            name: String::new(),
            owner: String::new(),
            kind: "file".into(),
            parameters: Vec::new(),
            written: String::new(),
            guard: 1,
            weak: false,
            fragment: false,
        }
    }
    pub fn passage(path: &str, blob: &str, span: Range<usize>) -> Self {
        let mut region = Self::whole(path, blob, span.end);
        region.kind = "text".into();
        region.body = span.clone();
        region.span = span;
        region
    }
    /// Enrich only a complete implementation's exact line range. A containing
    /// declaration is not permission to widen an explicitly selected passage.
    pub fn selected(path: &str, blob: &str, span: Range<usize>, source: &Source) -> Self {
        if java::path(path)
            && let Some(declaration) = source
                .declarations
                .iter()
                .find(|d| java::lines(&source.text, d.span.clone()) == span)
        {
            return Self::declaration(path, blob, declaration, source);
        }
        Self::passage(path, blob, span)
    }
    pub fn declaration(
        path: &str,
        blob: &str,
        declaration: &HistoricalDeclaration,
        source: &Source,
    ) -> Self {
        let mut body = declaration.header.end.min(declaration.span.end)..declaration.span.end;
        let text = &source.text[body.clone()];
        if text.starts_with('{') && text.ends_with('}') {
            body.start += 1;
            body.end -= 1;
        } else if text.starts_with("=>") {
            body.start += 2;
        }
        Self {
            path: path.into(),
            blob: blob.into(),
            span: if java::path(path) {
                java::lines(&source.text, declaration.span.clone())
            } else {
                declaration.span.clone()
            },
            body,
            name: declaration.qualified.clone(),
            owner: declaration.owner.clone(),
            kind: declaration.kind.clone(),
            parameters: declaration.parameters.clone(),
            written: declaration.written.clone(),
            guard: declaration.guard,
            weak: declaration.uncertain,
            fragment: false,
        }
    }
}

/// Existence is independent of selected bytes: an empty file is not a deletion.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Value {
    Absent,
    Present(String),
}
impl Value {
    pub fn text(&self) -> &str {
        match self {
            Self::Absent => "",
            Self::Present(text) => text,
        }
    }
}

pub(super) struct Call {
    pub span: Range<usize>,
    pub name: String,
    pub arguments: Option<usize>,
    pub receiver: String,
}
pub(super) struct Source {
    pub text: Arc<str>,
    pub calls: Vec<Call>,
    pub declarations: Vec<HistoricalDeclaration>,
    pub presence: HistoricalPresence,
    pub incomplete: Option<String>,
    cost: usize,
}
impl Source {
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.cost
    }
}
pub(super) struct Sources {
    cache: Arc<Cache>,
}
impl Default for Sources {
    fn default() -> Self {
        Self::new(Arc::new(Cache::new(crate::memory::cache_budget() / 4)))
    }
}
impl Sources {
    pub fn new(cache: Arc<Cache>) -> Self {
        Self { cache }
    }
    pub fn anchor(
        &mut self,
        store: &mut Store,
        path: &str,
        blob: &str,
        query: &crate::query::Query,
    ) -> Result<Option<Arc<Source>>> {
        if language(path) == Some(Language::CSharp) {
            let bytes = store.blob(blob)?;
            if let Ok(text) = std::str::from_utf8(&bytes)
                && !may_declare(query, text)
            {
                return Ok(None);
            }
        }
        self.get(store, path, blob).map(Some)
    }
    pub fn get(&mut self, store: &mut Store, path: &str, blob: &str) -> Result<Arc<Source>> {
        crate::repository::history::check(&store.cancel)?;
        let language = language(path).with_context(|| format!("unsupported source path {path}"))?;
        let java = java::path(path);
        // Java remains public Language::Text, but must not share transient facts
        // with an identical blob used as ordinary text (in either lookup order).
        let policy = if java {
            "Java/0.23".into()
        } else {
            format!("{language:?}/2024")
        };
        let key = cache::key(store.cache_scope(), &(blob, policy));
        if let Some(source) = self.cache.source(&key) {
            return Ok(source);
        }
        let bytes = store.blob(blob)?;
        let _admission = crate::memory::admit_file(bytes.len() as u64, false);
        crate::repository::history::check(&store.cancel)?;
        ensure!(
            !bytes.contains(&0),
            "binary source at {path}; contents were not followed"
        );
        ensure!(
            !bytes.starts_with(b"version https://git-lfs.github.com/spec/"),
            "Git LFS pointer at {path}; payload history is excluded"
        );
        let text: Arc<str> = String::from_utf8(bytes)
            .with_context(|| format!("non-UTF-8 source at {path}"))?
            .into();
        let facts = if java {
            java::extract(&text, &store.cancel)
        } else {
            crate::extract::historical(&text, language)
        };
        crate::repository::history::check(&store.cancel)?;
        let (declarations, calls, presence, incomplete) = match facts {
            Ok(facts) => (
                facts.declarations,
                facts
                    .occurrences
                    .into_iter()
                    .filter(|o| o.call && !o.opaque)
                    .map(|o| Call {
                        span: o.span,
                        name: o.name,
                        arguments: o.arguments,
                        receiver: o.receiver,
                    })
                    .collect::<Vec<_>>(),
                facts.presence,
                facts.incomplete,
            ),
            Err(error) => (
                Vec::new(),
                Vec::new(),
                HistoricalPresence::default(),
                Some(error.to_string()),
            ),
        };
        let cost = text.len()
            + presence.heap_bytes()
            + incomplete.as_ref().map_or(0, String::capacity)
            + calls.capacity() * std::mem::size_of::<Call>()
            + calls
                .iter()
                .map(|c| c.name.capacity() + c.receiver.capacity())
                .sum::<usize>()
            + declarations.capacity() * std::mem::size_of::<HistoricalDeclaration>()
            + declarations
                .iter()
                .map(HistoricalDeclaration::heap_bytes)
                .sum::<usize>();
        let source = Arc::new(Source {
            text,
            declarations,
            calls,
            presence,
            incomplete,
            cost,
        });
        Ok(self.cache.put_source(key, source))
    }
    pub fn text(&mut self, store: &mut Store, region: &Region) -> Result<String> {
        let source = self.get(store, &region.path, &region.blob)?;
        Ok(source
            .text
            .get(region.range(&source))
            .context("Invalid historical source range")?
            .into())
    }
    pub fn label(&mut self, store: &mut Store, region: &Region) -> Result<String> {
        if region.kind == "file" {
            return Ok(region.path.clone());
        }
        let source = self.get(store, &region.path, &region.blob)?;
        let start = crate::model::position(&source.text, region.span.start).0;
        let end = region.span.end - usize::from(source.text[region.span.clone()].ends_with('\n'));
        let end = crate::model::position(&source.text, end).0;
        Ok(
            format!("{}:{start}-{end} {}", region.path, region.signature())
                .trim_end()
                .into(),
        )
    }
}

/// A necessary spelling check, never evidence that a matching declaration exists.
fn may_declare(query: &crate::query::Query, text: &str) -> bool {
    if query.loose
        || query.target.location.is_some()
        || query.selector == "operator"
        || text.contains("\\u")
        || text.contains("\\U")
    {
        return true;
    }
    let parts: Vec<_> = query.target.name.split('.').collect();
    if parts.iter().any(|part| {
        part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }) {
        return true;
    }
    parts.iter().all(|part| text.contains(part))
}

pub(super) fn language(path: &str) -> Option<Language> {
    let file = Path::new(path);
    if let Some(language) =
        crate::native::language(file).or_else(|| crate::documents::language(file))
    {
        return Some(language);
    }
    let extension = file
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => Language::Rust,
        "cs" => Language::CSharp,
        "c" => Language::C,
        "cpp" | "cc" | "cxx" => Language::Cpp,
        "h" | "hpp" => Language::Header,
        "md" | "markdown" => Language::Markdown,
        "txt" | "toml" | "yaml" | "yml" | "json" | "xml" | "html" | "css" | "js" | "ts" | "tsx"
        | "jsx" | "py" | "java" | "go" | "sh" | "sql" | "ini" | "cfg" | "rst" | "csproj"
        | "props" | "targets" | "sln" | "lock" => Language::Text,
        "" if file.file_name().is_some_and(|s| {
            let s = s.to_string_lossy();
            s.starts_with('.')
                || ["LICENSE", "COPYING", "Makefile", "Dockerfile"].contains(&s.as_ref())
        }) =>
        {
            Language::Text
        }
        _ => return None,
    })
}
pub(super) fn compatible(a: &str, b: &str) -> bool {
    language(a).is_some() && language(a) == language(b)
}
pub(super) fn selectable(declaration: &Declaration) -> bool {
    !declaration.local() && !matches!(declaration.kind.as_str(), "scope" | "impl" | "lambda")
}
pub(super) fn matches_kind(kind: &str, declaration: &Declaration) -> bool {
    kind == "symbol" || (kind == "type" && declaration.named_type()) || kind == declaration.kind
}
// The anchor scan applies path filters before loading declarations.
pub(super) fn selected(query: &crate::query::Query, declaration: &Declaration) -> bool {
    if !selectable(declaration)
        // History-only callables are reachable by position, not new named queries.
        || matches!(declaration.kind.as_str(), "closure" | "accessor")
    {
        return false;
    }
    let kind = query.selector.as_str();
    if !matches_kind(kind, declaration) {
        return false;
    }
    let name = if kind == "constructor" {
        &declaration.owner
    } else if query.target.name.contains('.') || query.target.name.contains("::") {
        &declaration.qualified
    } else {
        &declaration.name
    };
    crate::query::qualified_name_rank(&query.target.name, name, query.loose).is_some()
        && query
            .target
            .parameters
            .as_ref()
            .is_none_or(|p| crate::search::signature_matches(p, &declaration.parameters))
}

static TOKENS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
    r#""(?:\\.|[^"\\])*"|'(?:\\(?:u\{[0-9a-fA-F]+\}|u[0-9a-fA-F]{4}|U[0-9a-fA-F]{8}|x[0-9a-fA-F]{2}|.)|[^'\\\r\n])'|[\p{L}\p{N}_]+|[^\s]"#).unwrap()
});
pub(super) fn tokens(text: &str) -> Vec<Range<usize>> {
    TOKENS.find_iter(text).map(|m| m.range()).collect()
}
pub(super) fn words<'a>(text: &'a str, tokens: &[Range<usize>]) -> Vec<&'a str> {
    tokens.iter().map(|r| &text[r.clone()]).collect()
}
pub(super) fn distinctive(text: &str) -> usize {
    tokens(text)
        .iter()
        .map(|r| &text[r.clone()])
        .filter(|word| {
            word.len() >= 3
                && word.chars().any(char::is_alphanumeric)
                && ![
                    "return", "self", "Self", "this", "true", "false", "null", "let", "mut", "pub",
                ]
                .contains(word)
        })
        .collect::<BTreeSet<_>>()
        .len()
}
pub(super) fn same_tokens(a: &str, b: &str) -> bool {
    TOKENS
        .find_iter(a)
        .map(|m| m.as_str())
        .eq(TOKENS.find_iter(b).map(|m| m.as_str()))
}
/// Corroborate a whole edited body with 80% ordered token coverage and four
/// disjoint 16-token anchors unique in both bodies. Repetition and overlapping
/// windows cannot inflate the independent source evidence.
pub(super) fn ordered_body_anchors(a: &str, b: &str) -> bool {
    const WIDTH: usize = 16;
    const REQUIRED: usize = 4;
    let at = tokens(a);
    let bt = tokens(b);
    if at.len().min(bt.len()) < WIDTH * REQUIRED {
        return false;
    }
    let aw = words(a, &at);
    let bw = words(b, &bt);
    let pairs = sequence::matches(&aw, &bw);
    if pairs.len() * 100 < aw.len().max(bw.len()) * 80 {
        return false;
    }
    let mut counts = BTreeMap::<&[&str], (usize, usize)>::new();
    for window in aw.windows(WIDTH) {
        counts.entry(window).or_default().0 += 1;
    }
    for window in bw.windows(WIDTH) {
        if let Some(count) = counts.get_mut(window) {
            count.1 += 1;
        }
    }
    let (mut end, mut count) = (0, 0);
    for run in pairs.windows(WIDTH) {
        let (i, j) = run[0];
        // Alignment is strictly increasing on both sides: these endpoints
        // require consecutive tokens, and disjoint runs stay disjoint in both.
        if i < end || run[WIDTH - 1] != (i + WIDTH - 1, j + WIDTH - 1) {
            continue;
        }
        if counts.get(&aw[i..i + WIDTH]) == Some(&(1, 1)) {
            count += 1;
            if count == REQUIRED {
                return true;
            }
            end = i + WIDTH;
        }
    }
    false
}

pub(super) fn similarity(a: &str, b: &str) -> usize {
    let at = tokens(a);
    let bt = tokens(b);
    let aw = words(a, &at);
    let bw = words(b, &bt);
    if aw == bw {
        return 100;
    }
    if aw.is_empty() || bw.is_empty() {
        return 0;
    }
    let a: BTreeSet<_> = aw.windows(3).collect();
    let b: BTreeSet<_> = bw.windows(3).collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    200 * a.intersection(&b).count() / (a.len() + b.len())
}

/// Textual range mapping includes changed gaps inside retained surrounding anchors.
pub(super) fn map_range(from: &str, to: &str, range: Range<usize>) -> Option<Range<usize>> {
    let ft = tokens(from);
    let tt = tokens(to);
    let fw = words(from, &ft);
    let tw = words(to, &tt);
    let pairs = sequence::matches(&fw, &tw);
    let inside: Vec<_> = pairs
        .iter()
        .filter(|&&(i, _)| ft[i].start >= range.start && ft[i].end <= range.end)
        .collect();
    if let (Some(first), Some(last)) = (inside.first(), inside.last()) {
        return Some(tt[first.1].start..tt[last.1].end);
    }
    let before = pairs.iter().rev().find(|&&(i, _)| ft[i].end <= range.start);
    let after = pairs.iter().find(|&&(i, _)| ft[i].start >= range.end);
    match (before, after) {
        (Some(&(_, left)), Some(&(_, right))) if left < right => {
            Some(tt[left].end..tt[right].start)
        }
        _ => None,
    }
}

pub(super) fn line_range(text: &str, start: usize, end: usize) -> Result<Range<usize>> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let lines = if text.ends_with('\n') {
        starts.len() - 1
    } else {
        starts.len()
    };
    ensure!(
        start > 0 && end >= start && end <= lines,
        "Text range is outside the anchor source"
    );
    Ok(starts[start - 1]..starts.get(end).copied().unwrap_or(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_body_anchors_require_whole_ordered_nonrepeating_source() {
        let check = |a: &str, b: &str, expected: bool| {
            assert_eq!(ordered_body_anchors(a, b), expected);
            assert_eq!(ordered_body_anchors(b, a), expected);
        };
        let tokens: Vec<_> = (0..128).map(|i| format!("token{i}")).collect();
        let original = tokens.join(" ");
        let edited = original.replace("token64", "changed");
        check(&original, &edited, true);
        check(&original, &edited.replace(' ', "\r\n"), true);

        let reordered = tokens
            .chunks(32)
            .rev()
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        check(&original, &reordered, false);
        check(
            &original,
            &format!("{edited} {}", "unrelated ".repeat(128)),
            false,
        );

        let template = "var adjusted = value + 10; var scaled = adjusted * 3; return scaled ^ 127;";
        check(template, &template.replace("127", "255"), false);
        let repeated = template.repeat(20);
        let changed = repeated.replacen("127", "255", 1);
        check(&repeated, &changed, false);

        // Many unique windows in one short prefix are still overlapping evidence.
        let prefix = tokens[..20].join(" ");
        let padded = format!("{prefix} {}", "repeat ".repeat(256));
        check(&format!("{padded} old"), &format!("{padded} new"), false);
    }

    #[test]
    fn exact_body_evidence_requires_tokens_not_just_equal_trigram_sets() {
        let a = "begin alpha beta alpha beta alpha end";
        let b = "begin alpha beta alpha beta alpha beta alpha end";
        assert_eq!(similarity(a, b), 100);
        assert!(!same_tokens(a, b));
        assert!(same_tokens(a, &a.replace(' ', "\r\n")));
    }

    #[test]
    fn history_only_callables_are_positional_not_named_symbols() {
        let facts = crate::extract::historical(
            "class Worker { int Value { get => 1; set {} } void Run() { var f = () => 1; } }",
            Language::CSharp,
        )
        .unwrap();
        let query = crate::query::Query::parse("*").unwrap();
        for declaration in &facts.declarations {
            assert!(selectable(declaration));
            assert_eq!(
                selected(&query, declaration),
                !matches!(declaration.kind.as_str(), "closure" | "accessor")
            );
        }
    }

    #[test]
    fn existing_native_lambda_scopes_remain_unselectable() {
        let facts =
            crate::extract::historical("void run() { auto f = [] { return 1; }; }", Language::Cpp)
                .unwrap();
        let scopes: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind == "lambda")
            .collect();
        assert!(!scopes.is_empty());
        assert!(scopes.iter().all(|d| !selectable(d)));
    }
}
