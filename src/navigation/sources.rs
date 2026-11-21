//! Navigation over selected paths, independent of semantic index readiness.
use crate::{
    query::{Query, wildcard},
    workspace::{FileEntry, Manifest, Membership},
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// Navigate materialized repository inputs while dependency discovery is pending.
/// Unknown paths fall through to discovery, which may supply package or generated files.
pub(crate) fn repository(
    root: &std::path::Path,
    contents: Option<&std::path::Path>,
    prepared: &crate::repository::materialize::Prepared,
    path: &str,
    mode: Option<crate::navigation::Mode>,
    cancel: &dyn crate::cancellation::Cancellation,
    read_remote: impl FnOnce(&str) -> Result<Option<crate::memory::Source<Vec<u8>>>>,
) -> Result<Option<String>> {
    let metadata = mode.is_some()
        && crate::navigation::location(path).is_ok_and(|(path, _)| {
            std::path::Path::new(path)
                .extension()
                .is_some_and(|extension| extension == "meta")
        });
    let language = |path: &std::path::Path| match path.extension().and_then(|e| e.to_str()) {
        Some("cs") => Some(crate::model::Language::CSharp),
        Some("rs") => Some(crate::model::Language::Rust),
        Some("meta") if metadata => Some(crate::model::Language::Text),
        _ => crate::native::language(path).or_else(|| crate::documents::language(path)),
    };
    let files: BTreeMap<_, _> = prepared
        .selected
        .keys()
        .chain(prepared.deferred_assets.iter())
        .filter(|p| !prepared.unavailable.contains(*p))
        .filter(|p| !prepared.omitted.contains_key(*p))
        .filter_map(|p| {
            let path = std::path::Path::new(p);
            let language = language(path);
            (language.is_some() || crate::unity::assets::is_asset(path))
                .then(|| (p.clone(), language))
        })
        .collect();
    let Some(mode) = mode else {
        let dirs = crate::navigation::Directory::new(files.keys().map(String::as_str)).paths();
        let normalized = crate::navigation::normalize_indexed(path, root, false, |p| {
            files.contains_key(p) || dirs.iter().any(|d| d == p)
        })?;
        if !normalized.is_empty()
            && !files.contains_key(&normalized)
            && crate::navigation::matches(dirs.iter().map(String::as_str), &normalized).is_empty()
        {
            return Ok(None);
        }
        return browse(&files, root, path, false).map(Some);
    };
    let files: BTreeMap<_, _> = files
        .into_iter()
        .filter_map(|(p, l)| {
            l.or_else(|| {
                (mode == crate::navigation::Mode::Raw).then_some(crate::model::Language::Text)
            })
            .map(|l| (p.clone(), (p, l)))
        })
        .collect();
    let mut resolved = false;
    let text = view(
        &files,
        root,
        path,
        mode,
        false,
        cancel,
        |(path, language)| {
            resolved = true;
            let local = contents
                .filter(|_| prepared.selected.contains_key(path))
                .map(|contents| contents.join(path))
                .filter(|path| path.is_file());
            let bytes = if let Some(path) = local {
                let size = std::fs::metadata(&path)?.len();
                ensure!(
                    !crate::unity::assets::view::supported(&path)
                        || size <= crate::repository::selection::ASSET_LIMIT,
                    "Asset exceeds size limit ({size} bytes)"
                );
                crate::memory::Source::read(size, cancel, || Ok(std::fs::read(path)?))?
            } else if let Some(bytes) = read_remote(path)? {
                bytes
            } else {
                return Ok(None);
            };
            if bytes.starts_with(b"version https://git-lfs.github.com/spec/") {
                return Ok(None);
            }
            Ok(Some((
                *language,
                bytes.map(|bytes| {
                    if mode == crate::navigation::Mode::Raw {
                        crate::model::decode_raw(bytes, *language)
                    } else {
                        crate::model::decode_owned(bytes, *language)
                    }
                })?,
            )))
        },
    )?;
    Ok(resolved.then_some(text).flatten())
}

pub(crate) struct Sources<'a> {
    pub manifest: &'a Manifest,
    pub assets: Option<&'a crate::unity::assets::Index>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

pub(crate) fn source_paths(
    manifest: &Manifest,
    root: &std::path::Path,
) -> std::sync::Arc<BTreeMap<String, String>> {
    manifest.derived.navigation(root, || {
        manifest
            .files
            .iter()
            .chain(manifest.deferred.iter())
            .filter(|(_, file)| !file.metadata)
            .flat_map(|(key, file)| {
                file.memberships.iter().filter_map(move |m| {
                    if let Ok(path) = file.path.strip_prefix(root) {
                        Some((path.to_string_lossy().into_owned(), key.clone()))
                    } else {
                        let display = manifest.display(file, m);
                        (!std::path::Path::new(display.as_ref()).is_absolute())
                            .then(|| (display.into_owned(), key.clone()))
                    }
                })
            })
            .collect()
    })
}

pub(crate) fn file_path_filters(q: &Query) -> Result<impl Fn(&str) -> bool + '_> {
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
    Ok(move |path: &str| {
        paths.iter().all(|(f, pattern)| {
            (pattern.is_match(path)
                || path
                    .strip_prefix(f.value.trim_end_matches('/'))
                    .is_some_and(|suffix| suffix.starts_with('/')))
                != f.negate
        })
    })
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
            if filter.key == "namespace"
                && let Some(prefix) = filter
                    .value
                    .strip_suffix(".*")
                    .or_else(|| filter.value.strip_suffix("::*"))
            {
                return Some(format!(
                    "No matches. Namespace filters use a component prefix. To include nested namespaces, use {}.",
                    crate::render::inline(&crate::query::suggestion(&[format!(
                        "namespace:{prefix}"
                    )]))
                ));
            }
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
        if q.filters.iter().filter(|f| f.key == "lang").count() > 1 {
            use crate::model::Language::*;
            // Use file semantics too: headers and ShaderLab can contain several dialects.
            if ![
                CSharp, Rust, Markdown, Text, C, Cpp, Header, Hlsl, Glsl, ShaderLab,
            ]
            .into_iter()
            .any(|language| q.file_language_filters(language))
            {
                return Some("No matches. The language filters have no common language. Filters combine with AND; use separate queries for alternatives.".into());
            }
        }
        None
    }
    fn source_paths(&self, root: &std::path::Path) -> std::sync::Arc<BTreeMap<String, String>> {
        let mut paths = source_paths(self.manifest, root);
        if let Some(assets) = self.assets {
            let paths = std::sync::Arc::make_mut(&mut paths);
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
        let paths = file_path_filters(q)?;
        let mut found = Vec::new();
        let mut total = 0;
        let filter_languages = q.filters.iter().any(|f| f.key == "lang");
        let asset_language = crate::unity::assets::selection::language_filter(q);
        let source_paths = self.source_paths(root);
        for (path, key) in source_paths.iter() {
            self.check()?;
            let target = if q.target.name.contains('/') {
                path.as_str()
            } else {
                path.rsplit('/').next().unwrap()
            };
            if !pattern.is_match(target) || !paths(path) {
                continue;
            }
            let file = self
                .manifest
                .files
                .get(key)
                .or_else(|| self.manifest.deferred.get(key));
            if filter_languages
                && !match file {
                    Some(file) => q.file_language_filters(file.language),
                    None => asset_language(std::path::Path::new(path)),
                }
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
                found.push(crate::navigation::quote(path));
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
        browse(&self.source_paths(root), root, path, absolute)
    }
    pub fn view(
        &self,
        root: &std::path::Path,
        path: &str,
        mode: crate::navigation::Mode,
        absolute: bool,
    ) -> Result<String> {
        self.check()?;
        let normalized = if path.starts_with("unity@") {
            path.to_owned()
        } else {
            crate::navigation::normalize(path, root, absolute)?
        };
        let path = normalized.as_str();
        if let Some(assets) = self.assets
            && let Some(text) = if mode == crate::navigation::Mode::Raw {
                assets.view_raw(path, self.cancel)?
            } else {
                assets.view_with_cancel(path, self.cancel)?
            }
        {
            return Ok(text);
        }
        view(
            &self.source_paths(root),
            root,
            path,
            mode,
            absolute,
            self.cancel,
            |key| {
                let file = self
                    .manifest
                    .files
                    .get(key)
                    .or_else(|| self.manifest.deferred.get(key))
                    .unwrap();
                Ok(Some((
                    file.language,
                    file.read_source(self.cancel, mode == crate::navigation::Mode::Raw)?,
                )))
            },
        )?
        .context("Source contents are unavailable")
    }
}

fn browse<T>(
    files: &BTreeMap<String, T>,
    root: &std::path::Path,
    path: &str,
    absolute: bool,
) -> Result<String> {
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
        return Ok(crate::navigation::choices(&matches, "directories", &path));
    }
    Ok(tree.at(matches[0]).render(matches[0]))
}
pub(crate) fn view<T>(
    files: &BTreeMap<String, T>,
    root: &std::path::Path,
    path: &str,
    mode: crate::navigation::Mode,
    absolute: bool,
    cancel: &dyn crate::cancellation::Cancellation,
    read: impl FnOnce(&T) -> Result<Option<(crate::model::Language, crate::memory::Source<String>)>>,
) -> Result<Option<String>> {
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
    let (path, file) = if let Some((path, file)) = files.get_key_value(&path) {
        (path.as_str(), file)
    } else {
        let matches =
            crate::navigation::file_matches(files.keys().map(String::as_str), &path, mode);
        if mode == crate::navigation::Mode::Raw && matches.is_empty() {
            return Ok(None);
        }
        if matches.len() != 1 {
            ensure!(
                mode != crate::navigation::Mode::Raw,
                "{}",
                crate::navigation::choices(&matches, "files", &path)
            );
            return Ok(Some(crate::navigation::choices(&matches, "files", &path)));
        }
        let path = matches[0];
        (path, &files[path])
    };
    let Some((language, source)) = read(file)? else {
        return Ok(None);
    };
    let minify = match mode {
        crate::navigation::Mode::Raw => {
            return source
                .render(|text| crate::navigation::raw_lines(text, requested))
                .map(Some);
        }
        crate::navigation::Mode::Exact => false,
        crate::navigation::Mode::Minified => true,
    };
    let (range, first, last) = crate::navigation::lines(&source, requested)?;
    let (body, tag) = if minify {
        (
            crate::minify::render_admitted(&source, language, range, cancel)?,
            "",
        )
    } else {
        (source[range].to_owned(), language.tag())
    };
    let fence = "`".repeat(3.max(body.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1));
    let separator = if body.ends_with('\n') { "" } else { "\n" };
    Ok(Some(format!(
        "{}\n{fence}{tag}\n{body}{separator}{fence}",
        crate::render::inline(&crate::render::location(
            &crate::navigation::quote(path),
            first,
            last
        ))
    )))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filter_hints_respect_language_overlap_and_namespace_prefixes() {
        let manifest = Manifest::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let sources = Sources {
            manifest: &manifest,
            assets: None,
            cancel: &cancel,
        };
        assert!(
            sources
                .filter_hint(&Query::parse("symbol:* lang:rust lang:cs").unwrap())
                .is_some()
        );
        for input in [
            "symbol:* lang:c lang:cpp",
            "symbol:* lang:native lang:cpp",
            "symbol:* namespace:Example",
        ] {
            assert!(
                sources.filter_hint(&Query::parse(input).unwrap()).is_none(),
                "{input}"
            );
        }
        assert!(
            sources
                .filter_hint(&Query::parse("symbol:* namespace:Example.*").unwrap())
                .is_some()
        );
    }

    #[test]
    fn exact_view_remains_available_while_parser_is_busy() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::Duration;

        let parser = crate::memory::admit_file(u64::MAX, false, &()).unwrap();
        let files = BTreeMap::from([("Code.cs".to_owned(), ())]);
        let text = view(
            &files,
            std::path::Path::new("/repo"),
            "Code.cs",
            crate::navigation::Mode::Exact,
            false,
            &(),
            |_| {
                Ok(Some((
                    crate::model::Language::CSharp,
                    crate::memory::Source::read(16, &(), || Ok("class Example {}".to_owned()))?,
                )))
            },
        )
        .unwrap()
        .unwrap();
        assert!(text.contains("class Example {}"));

        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancelled.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(crate::minify::render_admitted(
                    "class Example {}",
                    crate::model::Language::CSharp,
                    0..16,
                    &*worker_cancel,
                ))
                .unwrap();
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        cancelled.store(true, Ordering::Release);
        let result = receiver.recv_timeout(Duration::from_secs(5));
        drop(parser);
        assert!(result.unwrap().is_err());
        worker.join().unwrap();
    }

    #[test]
    fn view_prefers_literal_filenames_before_location_suffixes() {
        let files: BTreeMap<_, _> = [
            "src/Code.cs",
            "other/src/Code.cs",
            "src/Code.cs:2",
            r"src\Code.cs:2",
        ]
        .into_iter()
        .map(|path| (path.to_owned(), path))
        .collect();
        for (path, expected) in [
            ("src/Code.cs", "src/Code.cs"),
            ("src/Code.cs:2", "src/Code.cs:2"),
            ("src/Code.cs:2:2", "src/Code.cs:2"),
            (r"src\Code.cs:2", r"src\Code.cs:2"),
            (r"src\Code.cs:2:2", r"src\Code.cs:2"),
        ] {
            let mut selected = None;
            view(
                &files,
                std::path::Path::new("/repo"),
                path,
                crate::navigation::Mode::Exact,
                false,
                &(),
                |file| {
                    selected = Some(*file);
                    Ok(Some((
                        crate::model::Language::CSharp,
                        crate::memory::Source::read(13, &(), || Ok("first\nsecond\n".into()))?,
                    )))
                },
            )
            .unwrap();
            assert_eq!(selected, Some(expected), "{path}");
        }
    }

    #[test]
    fn raw_assets_and_metadata_read_selected_or_deferred_bytes_without_code_preparation() {
        let root = tempfile::tempdir().unwrap();
        for path in ["Assets/Selected.prefab", "Assets/Selected.prefab.meta"] {
            let source = "%YAML 1.1\n--- !u!1 &1\nGameObject:\n  m_Name: Selected\n";
            let mut prepared = crate::repository::materialize::Prepared {
                deferred_assets: BTreeSet::from([path.into()]),
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                branch: None,
                revision: String::new(),
                selected: Default::default(),
                tracked: BTreeMap::from([(path.into(), "object".into())]),
                directories: Default::default(),
            };
            for selected in [false, true] {
                if selected {
                    prepared.selected = prepared.tracked.clone();
                    prepared.deferred_assets.clear();
                }
                let raw = repository(
                    root.path(),
                    Some(root.path()),
                    &prepared,
                    path,
                    Some(crate::navigation::Mode::Raw),
                    &(),
                    |resolved| {
                        assert_eq!(resolved, path);
                        crate::memory::Source::read(source.len() as u64, &(), || {
                            Ok(source.as_bytes().to_vec())
                        })
                        .map(Some)
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(raw, source);
            }
            prepared.omitted.insert(path.into(), "too large".into());
            assert!(
                repository(
                    root.path(),
                    None,
                    &prepared,
                    path,
                    Some(crate::navigation::Mode::Raw),
                    &(),
                    |_| panic!("omitted asset fetched")
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn repository_view_reads_only_the_resolved_source_and_can_defer_contents() {
        let root = tempfile::tempdir().unwrap();
        let prepared = crate::repository::materialize::Prepared {
            deferred_assets: Default::default(),
            omitted: Default::default(),
            transfer_bytes: 0,
            unavailable: Default::default(),
            transport: None,
            resolved_target: None,
            branch: None,
            revision: String::new(),
            selected: BTreeMap::from([("src/Code.cs".into(), "object".into())]),
            tracked: Default::default(),
            directories: Default::default(),
        };
        let text = repository(
            root.path(),
            None,
            &prepared,
            "Code.cs:2",
            Some(crate::navigation::Mode::Exact),
            &(),
            |path| {
                assert_eq!(path, "src/Code.cs");
                Ok(Some(crate::memory::Source::read(32, &(), || {
                    Ok(b"// first line\nclass Example {}\n".to_vec())
                })?))
            },
        )
        .unwrap()
        .unwrap();
        assert!(text.contains("class Example {}"));
        assert!(!text.contains("first line"));
        let raw_source = "\u{feff}// first line\r\nclass Example {}";
        let raw = repository(
            root.path(),
            None,
            &prepared,
            "Code.cs",
            Some(crate::navigation::Mode::Raw),
            &(),
            |_| {
                Ok(Some(crate::memory::Source::read(
                    raw_source.len() as u64,
                    &(),
                    || Ok(raw_source.as_bytes().to_vec()),
                )?))
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw, raw_source);
        assert!(
            repository(
                root.path(),
                None,
                &prepared,
                "Code.cs",
                Some(crate::navigation::Mode::Exact),
                &(),
                |_| Ok(None),
            )
            .unwrap()
            .is_none()
        );
        assert!(
            repository(
                root.path(),
                None,
                &prepared,
                "Missing.cs",
                Some(crate::navigation::Mode::Exact),
                &(),
                |_| panic!("Unknown files must not be downloaded"),
            )
            .unwrap()
            .is_none()
        );
    }
}
