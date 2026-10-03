//! Decide which independent indexes a request needs before taking a query worker.
use super::Request;
use crate::{
    native::Group,
    query::Query,
    workspace::{FileEntry, Manifest},
};
use std::collections::BTreeMap;

pub(super) fn groups(
    request: &Request,
    query: Option<&Query>,
    manifest: &Manifest,
) -> BTreeMap<Group, bool> {
    if query.is_some_and(|q| {
        matches!(
            q.selector.as_str(),
            "instance" | "references" | "dependencies"
        )
    }) {
        return BTreeMap::new();
    }
    let matches = |file: &FileEntry| selected(request, query, manifest, file);
    let mut groups = BTreeMap::new();
    for file in manifest.deferred.values().filter(|f| matches(f)) {
        groups.insert(Group::of(file.language).unwrap(), false);
    }
    let narrowed = match request {
        Request::Search(_) => query.is_some_and(|q| {
            q.target.location.is_some()
                || q.selector == "file" && q.target.name != "*"
                || q.filters
                    .iter()
                    .any(|f| !f.negate && matches!(f.key.as_str(), "path" | "project" | "lang"))
                || q.filters.iter().any(|f| {
                    f.key == "in"
                        && !f.negate
                        && crate::query::Target::parse(&f.value).is_ok_and(|t| t.location.is_some())
                })
        }),
        Request::View(..) => true,
        Request::Browse(path) => !path.is_empty(),
    };
    let wait =
        query.is_some_and(|q| q.wait_complete) || narrowed && !manifest.files.values().any(matches);
    groups.values_mut().for_each(|value| *value = wait);
    groups
}

fn selected(
    request: &Request,
    query: Option<&Query>,
    manifest: &Manifest,
    file: &FileEntry,
) -> bool {
    if file.metadata {
        return false;
    }
    if let Some(q) = query {
        if file.language.document() && !matches!(q.selector.as_str(), "text" | "file") {
            return false;
        }
        if !crate::search::Search::file_language_filters(q, file.language) {
            return false;
        }
        if !file.memberships.iter().any(|m| {
            let path = manifest.display(file, m);
            q.filters.iter().all(|f| match f.key.as_str() {
                "path" => path_matches(&f.value, &path) != f.negate,
                "project" => {
                    crate::query::wildcard(&f.value, &manifest.projects[m.project].name) != f.negate
                }
                "in" if !f.negate => crate::query::Target::parse(&f.value)
                    .ok()
                    .and_then(|t| t.location)
                    .is_none_or(|loc| {
                        path.as_ref() == loc.path
                            || path.ends_with(&format!("/{}", loc.path.trim_start_matches("…/")))
                    }),
                _ => true,
            }) && q.target.location.as_ref().is_none_or(|loc| {
                path.as_ref() == loc.path
                    || path.ends_with(&format!("/{}", loc.path.trim_start_matches("…/")))
            }) && (q.selector != "file"
                || globset::Glob::new(&q.target.name).is_ok_and(|g| {
                    g.compile_matcher()
                        .is_match(if q.target.name.contains('/') {
                            path.as_ref()
                        } else {
                            path.rsplit('/').next().unwrap()
                        })
                }))
        }) {
            return false;
        }
    }
    match request {
        Request::Search(_) => true,
        Request::Browse(path) if path.is_empty() => true,
        Request::Browse(path) | Request::View(path, _) => {
            let path = crate::navigation::location(path)
                .map_or(path.as_str(), |(p, _)| p)
                .trim_start_matches("…/")
                .trim_end_matches('/');
            file.memberships.iter().any(|m| {
                let display = manifest.display(file, m);
                display == path
                    || display.ends_with(&format!("/{path}"))
                    || matches!(request, Request::Browse(_))
                        && display.starts_with(&format!("{path}/"))
            }) || file.path == std::path::Path::new(path)
        }
    }
}

fn path_matches(pattern: &str, path: &str) -> bool {
    globset::Glob::new(pattern).is_ok_and(|g| g.compile_matcher().is_match(path))
        || path
            .strip_prefix(pattern.trim_end_matches('/'))
            .is_some_and(|s| s.starts_with('/'))
}

pub(super) fn wait_assets(request: &Request, query: Option<&Query>) -> bool {
    match request {
        Request::View(..) => true,
        Request::Browse(path) => !path.is_empty(),
        Request::Search(_) => query.is_some_and(|q| {
            q.wait_complete
                || matches!(
                    q.selector.as_str(),
                    "instance" | "references" | "dependencies"
                )
                || q.selector == "file"
                    && crate::unity::assets::is_asset(std::path::Path::new(&q.target.name))
                || q.filters.iter().any(|f| {
                    f.key == "path"
                        && !f.negate
                        && crate::unity::assets::is_asset(std::path::Path::new(&f.value))
                })
        }),
    }
}
