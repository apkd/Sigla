//! Check scoped searches against asset paths before starting composition.
use super::*;
use crate::{cancellation::Cancellation, query::Query, repository::materialize::Prepared};

/// A sidecar may identify a file with no indexed language, such as an image.
fn untyped(query: &Query) -> bool {
    query.filters.iter().all(|f| f.key != "lang" || f.negate)
}

pub(crate) fn language_filter(query: &Query) -> impl Fn(&Path) -> bool {
    let text = query.file_language_filters(crate::model::Language::Text);
    let untyped = untyped(query);
    move |path| {
        if text == untyped || is_asset(path) {
            text
        } else {
            untyped
        }
    }
}

pub(crate) fn searches_files(query: &Query) -> bool {
    matches!(query.selector.as_str(), "file" | "text")
        && !query
            .filters
            .iter()
            .any(|f| matches!(f.key.as_str(), "in" | "project"))
        && (query.file_language_filters(crate::model::Language::Text)
            || query.selector == "file" && untyped(query))
}

pub(crate) fn matches(
    manifest: &Manifest,
    prepared: Option<&Prepared>,
    query: &Query,
    cancel: &dyn Cancellation,
) -> Result<bool> {
    let selected = super::query::path_selection(query);
    let language = language_filter(query);
    let target = (query.selector == "file")
        .then(|| {
            globset::GlobBuilder::new(&query.target.name)
                .literal_separator(true)
                .case_insensitive(query.loose)
                .build()
                .map(|g| g.compile_matcher())
        })
        .transpose()?;
    let eligible = |path: &str, project: &str| {
        let path = path.strip_suffix(".meta").unwrap_or(path);
        (query.selector == "file" || is_asset(Path::new(path)))
            && language(Path::new(path))
            && selected(path, project)
            && target.as_ref().is_none_or(|target| {
                target.is_match(if query.target.name.contains('/') {
                    path
                } else {
                    path.rsplit('/').next().unwrap()
                })
            })
    };
    let mut projects = Vec::new();
    roots(
        &manifest.root,
        &mut projects,
        &mut BTreeMap::new(),
        0,
        cancel,
    )?;
    if let Some(prepared) = prepared {
        for path in prepared.tracked.keys() {
            if let Some(prefix) = path.strip_suffix("ProjectSettings/ProjectVersion.txt") {
                let project = manifest.root.join(prefix);
                if !projects.contains(&project) {
                    projects.push(project);
                }
            }
        }
    }
    for project in projects {
        let name = project.strip_prefix(&manifest.root)?.to_string_lossy();
        let name = if name.is_empty() { "." } else { &name };
        let scopes = scopes(&manifest.root, &project, manifest);
        if let Some(prepared) = prepared {
            for path in prepared
                .selected
                .keys()
                .chain(prepared.deferred_assets.iter())
                .chain(prepared.omitted.keys())
            {
                cancel.check()?;
                let physical = manifest.root.join(path);
                if (is_asset(&physical) || physical.extension().is_some_and(|e| e == "meta"))
                    && scopes.iter().any(|scope| physical.starts_with(scope))
                    && eligible(path, name)
                {
                    return Ok(true);
                }
            }
        }
        for scope in scopes {
            if scope.is_dir()
                && walk(
                    &scope,
                    0,
                    &|path| {
                        let logical = logical_path(&manifest.root, &project, path, manifest);
                        if path.is_dir() {
                            (query.selector != "file"
                                || query.loose
                                || !query.target.name.contains('/')
                                || overlaps(&query.target.name, logical.trim_end_matches('/')))
                                && query
                                    .filters
                                    .iter()
                                    .filter(|f| f.key == "path" && !f.negate)
                                    .all(|f| overlaps(&f.value, logical.trim_end_matches('/')))
                        } else {
                            eligible(&logical, name)
                        }
                    },
                    cancel,
                )?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn overlaps(pattern: &str, directory: &str) -> bool {
    if pattern.contains('\\') {
        return true;
    }
    let prefix = pattern.split(['*', '?', '[', '{']).next().unwrap();
    let prefix = if prefix.len() == pattern.len() {
        prefix.trim_end_matches('/')
    } else {
        prefix.rsplit_once('/').map_or("", |(prefix, _)| prefix)
    };
    prefix.is_empty()
        || prefix == directory
        || prefix
            .strip_prefix(directory)
            .is_some_and(|s| s.starts_with('/'))
        || directory
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('/'))
}

fn walk(
    path: &Path,
    depth: usize,
    selected: &impl Fn(&Path) -> bool,
    cancel: &dyn Cancellation,
) -> Result<bool> {
    cancel.check()?;
    ensure!(
        depth < 128,
        "Directory nesting exceeds asset discovery limit"
    );
    if !selected(path) {
        return Ok(false);
    }
    for entry in std::fs::read_dir(path)? {
        cancel.check()?;
        let entry = entry?;
        let path = entry.path();
        if ignored(&path) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !is_root(&path)
                && !crate::discovery::linked_worktree(&path)
                && walk(&path, depth + 1, selected, cancel)?
            {
                return Ok(true);
            }
        } else if kind.is_file()
            && (is_asset(&path) || path.extension().is_some_and(|e| e == "meta"))
            && selected(&path)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_assets_match_without_materialized_project_directories() {
        let root = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            root: root.path().into(),
            ..Default::default()
        };
        let prepared = Prepared {
            selected: BTreeMap::from([("Assets/Editor/Code.cs".into(), "code".into())]),
            deferred_assets: BTreeSet::from([
                "Assets/Data/Settings.asset".into(),
                "Assets/Data/Icon.png.meta".into(),
            ]),
            tracked: BTreeMap::from([(
                "ProjectSettings/ProjectVersion.txt".into(),
                "version".into(),
            )]),
            omitted: Default::default(),
            unavailable: Default::default(),
            directories: Default::default(),
            transfer_bytes: 0,
            transport: None,
            resolved_target: None,
            branch: None,
            revision: String::new(),
        };
        for (query, expected) in [
            ("text:Name path:Assets/Data/**", true),
            ("text:Name path:Assets/Editor/**", false),
            ("file:Assets/Editor/**", false),
            ("file:*.PNG match:loose path:Assets/Data", true),
            ("file:* path:Assets/Data -path:Assets/Data", false),
        ] {
            assert_eq!(
                matches(
                    &manifest,
                    Some(&prepared),
                    &Query::parse(query).unwrap(),
                    &()
                )
                .unwrap(),
                expected,
                "{query}"
            );
        }
    }
}
