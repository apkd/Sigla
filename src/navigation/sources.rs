//! Navigation over selected paths, independent of semantic index readiness.
use crate::{
    query::{Query, wildcard},
    workspace::{FileEntry, Manifest, Membership},
};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Sources<'a> {
    pub manifest: &'a Manifest,
    pub assets: Option<&'a crate::unity::assets::Index>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}
impl Sources<'_> {
    fn check(&self) -> Result<()> {
        ensure!(!self.cancel.is_cancelled(), "Query cancelled");
        Ok(())
    }
    pub fn project_membership(&self, q: &Query, file: &FileEntry, m: &Membership) -> bool {
        // A document's repository context is not a build project. It must not
        // satisfy project wildcards or bypass its real project memberships.
        !((file.language.document() || file.language.native())
            && self.manifest.projects[m.project].name.is_empty()
            && q.filters
                .iter()
                .any(|f| f.key == "project" && (!f.negate || file.memberships.len() > 1)))
    }
    pub fn filter_hint(&self, q: &Query) -> Option<String> {
        for filter in q.filters.iter().filter(|f| !f.negate) {
            if filter.key == "project"
                && !self
                    .manifest
                    .projects
                    .iter()
                    .any(|p| wildcard(&filter.value, &p.name))
            {
                let names: BTreeSet<_> = self
                    .manifest
                    .projects
                    .iter()
                    .filter(|p| !p.name.is_empty())
                    .map(|p| p.name.as_str())
                    .collect();
                if names.is_empty() {
                    continue;
                }
                return Some(format!(
                    "No build project matched {}. `project:` filters build-project names. Available: {}{}.",
                    crate::render::inline(&filter.value),
                    names
                        .iter()
                        .take(4)
                        .map(|s| crate::render::inline(s))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if names.len() > 4 { ", …" } else { "" }
                ));
            }
        }
        None
    }
    fn source_paths(&self, root: &std::path::Path) -> BTreeMap<String, String> {
        let mut paths: BTreeMap<String, String> = self
            .manifest
            .files
            .iter()
            .chain(self.manifest.deferred.iter())
            .filter(|(_, file)| !file.metadata)
            .flat_map(|(key, file)| {
                file.memberships.iter().filter_map(move |m| {
                    if let Ok(path) = file.path.strip_prefix(root) {
                        Some((path.to_string_lossy().into_owned(), key.clone()))
                    } else {
                        let display = self.manifest.display(file, m);
                        (!std::path::Path::new(display.as_ref()).is_absolute())
                            .then(|| (display.into_owned(), key.clone()))
                    }
                })
            })
            .collect();
        if let Some(assets) = self.assets {
            for asset in assets.assets.values() {
                paths.entry(asset.path.clone()).or_default();
            }
        }
        paths
    }
    pub fn files(&self, q: &Query, root: &std::path::Path) -> Result<String> {
        let pattern = globset::GlobBuilder::new(&q.target.name)
            .case_insensitive(q.loose)
            .literal_separator(true)
            .build()?
            .compile_matcher();
        let paths = q
            .filters
            .iter()
            .filter(|f| f.key == "path")
            .map(|f| {
                Ok((
                    f,
                    globset::Glob::new(f.value.trim_end_matches('/'))?.compile_matcher(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut found = Vec::new();
        let mut total = 0;
        for (path, key) in self.source_paths(root) {
            self.check()?;
            let target = if q.target.name.contains('/') {
                path.as_str()
            } else {
                path.rsplit('/').next().unwrap()
            };
            if !pattern.is_match(target)
                || !paths.iter().all(|(f, p)| {
                    (p.is_match(&path)
                        || path
                            .strip_prefix(f.value.trim_end_matches('/'))
                            .is_some_and(|suffix| suffix.starts_with('/')))
                        != f.negate
                })
            {
                continue;
            }
            let file = self
                .manifest
                .files
                .get(&key)
                .or_else(|| self.manifest.deferred.get(&key));
            if q.filters.iter().any(|f| f.key == "lang")
                && file.is_none_or(|file| !q.file_language_filters(file.language))
            {
                continue;
            }
            if file.is_some_and(|file| {
                !file.memberships.iter().any(|m| {
                    if !self.project_membership(q, file, m) {
                        return false;
                    }
                    q.filters.iter().filter(|f| f.key == "project").all(|f| {
                        wildcard(&f.value, &self.manifest.projects[m.project].name) != f.negate
                    })
                })
            }) || file.is_none() && q.filters.iter().any(|f| f.key == "project")
            {
                continue;
            }
            total += 1;
            if found.len() < q.limit {
                found.push(crate::navigation::quote(&path));
            }
        }
        if total == 0 {
            return Ok(self.filter_hint(q).unwrap_or_else(|| "No matches.".into()));
        }
        let mut text = found.join("\n");
        if total > found.len() {
            text.push_str(&format!("\n{}", crate::render::omission(Some(total))));
        }
        Ok(text)
    }
    pub fn browse(&self, root: &std::path::Path, path: &str, absolute: bool) -> Result<String> {
        self.check()?;
        let files = self.source_paths(root);
        let tree = crate::navigation::Directory::new(files.keys().map(String::as_str));
        let dirs = tree.paths();
        let path = crate::navigation::normalize_indexed(path, root, absolute, |p| {
            files.contains_key(p) || dirs.iter().any(|d| d == p)
        })?;
        if files.contains_key(&path) {
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            return Ok(format!(
                "This is a file. Use {}, or {}.",
                crate::render::inline(&format!("view({})", serde_json::to_string(&path)?)),
                crate::render::inline(&format!("browse({})", serde_json::to_string(parent)?))
            ));
        }
        if path.is_empty() {
            return Ok(tree.render(""));
        }
        let matches = crate::navigation::matches(dirs.iter().map(String::as_str), &path);
        if matches.len() != 1 {
            return Ok(crate::navigation::choices(&matches, "directories"));
        }
        Ok(tree.at(matches[0]).render(matches[0]))
    }
    pub fn view(
        &self,
        root: &std::path::Path,
        path: &str,
        mode: crate::navigation::Mode,
        absolute: bool,
    ) -> Result<String> {
        self.check()?;
        if let Some(assets) = self.assets
            && let Some(text) = assets.view(path)?
        {
            return Ok(text);
        }
        let files = self.source_paths(root);
        let indexed = |p: &str| {
            files.contains_key(p)
                || files.keys().any(|f| {
                    f.strip_suffix(p)
                        .is_some_and(|prefix| prefix.ends_with('/'))
                })
        };
        let literal = crate::navigation::normalize_indexed(path, root, absolute, indexed)?;
        let (path, requested) = if files.contains_key(&literal)
            || files.keys().any(|p| {
                p.strip_suffix(&literal)
                    .is_some_and(|prefix| prefix.ends_with('/'))
            }) {
            (literal, None)
        } else {
            // An indexed filename can itself contain location delimiters.
            // Split after the longest known filename before validating a suffix.
            let boundary = literal.char_indices().rev().find_map(|(i, c)| {
                (matches!(c, ':' | '#' | '(') && indexed(&literal[..i])).then_some(i)
            });
            let (path, lines) = if let Some(i) = boundary {
                let (_, lines) = crate::navigation::location(&format!("file{}", &literal[i..]))?;
                ensure!(
                    lines.is_some(),
                    "Invalid line range; use `path:1-20` (1-based, inclusive)"
                );
                (&literal[..i], lines)
            } else {
                crate::navigation::location(path)?
            };
            let path = if files.contains_key(path) {
                path
            } else {
                path.strip_prefix("…/").unwrap_or(path)
            };
            (
                crate::navigation::normalize_indexed(path, root, absolute, indexed)?,
                lines,
            )
        };
        let matches = crate::navigation::matches(files.keys().map(String::as_str), &path);
        if matches.len() != 1 {
            return Ok(crate::navigation::choices(&matches, "files"));
        }
        let path = matches[0];
        let key = &files[path];
        let file = self
            .manifest
            .files
            .get(key)
            .or_else(|| self.manifest.deferred.get(key))
            .unwrap();
        let language = file.language;
        let source = file.read_source()?;
        let (range, first, last) = crate::navigation::lines(&source, requested)?;
        let (body, tag) = match mode {
            crate::navigation::Mode::Exact => (source[range].to_owned(), language.tag()),
            crate::navigation::Mode::Minified => {
                (crate::minify::render(&source, language, range), "")
            }
        };
        let fence =
            "`".repeat(3.max(body.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1));
        let separator = if body.ends_with('\n') { "" } else { "\n" };
        Ok(format!(
            "{}\n{fence}{tag}\n{body}{separator}{fence}",
            crate::render::inline(&crate::render::location(
                &crate::navigation::quote(path),
                first,
                last
            ))
        ))
    }
}
