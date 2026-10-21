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
    if matches!(request, Request::Browse(_))
        || query.is_some_and(|q| q.selector == "file" && !q.wait_complete)
    {
        return BTreeMap::new();
    }
    if query.is_some_and(|q| {
        matches!(
            q.selector.as_str(),
            "instance" | "references" | "dependencies"
        )
    }) {
        return BTreeMap::new();
    }
    let selection = query.map(Selection::new);
    let matches = |file: &FileEntry| selected(request, selection.as_ref(), manifest, file);
    let mut groups = BTreeMap::new();
    for file in manifest.deferred.values().filter(|f| matches(f)) {
        groups.insert(Group::of(file.language).unwrap(), false);
    }
    let narrowed = match request {
        Request::History(_) => unreachable!("history bypasses snapshot indexes"),
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

pub(super) fn source_only(request: &Request, query: Option<&Query>) -> bool {
    let source_path = |path: &str| {
        let path = std::path::Path::new(path);
        matches!(path.extension().and_then(|s| s.to_str()), Some("cs" | "rs"))
            || crate::native::language(path).is_some()
            || crate::documents::language(path).is_some()
    };
    match request {
        Request::History(_) => unreachable!("history bypasses snapshot indexes"),
        Request::View(path, _) => {
            crate::navigation::location(path).is_ok_and(|(path, _)| source_path(path))
        }
        Request::Search(_) => query.is_some_and(|q| {
            q.selector == "file"
                && !q.wait_complete
                && (source_path(&q.target.name)
                    || q.filters
                        .iter()
                        .any(|f| matches!(f.key.as_str(), "lang" | "project")))
        }),
        Request::Browse(_) => false,
    }
}

struct Selection<'a> {
    query: &'a Query,
    filters: Vec<(&'a crate::query::Filter, Option<globset::GlobMatcher>)>,
    target: Option<globset::GlobMatcher>,
}

impl<'a> Selection<'a> {
    fn new(query: &'a Query) -> Self {
        Self {
            query,
            filters: query
                .filters
                .iter()
                .map(|filter| {
                    let glob = if filter.key == "path" {
                        globset::Glob::new(&filter.value)
                            .ok()
                            .map(|g| g.compile_matcher())
                    } else {
                        None
                    };
                    (filter, glob)
                })
                .collect(),
            target: if query.selector == "file" {
                globset::Glob::new(&query.target.name)
                    .ok()
                    .map(|g| g.compile_matcher())
            } else {
                None
            },
        }
    }
}

fn selected(
    request: &Request,
    selection: Option<&Selection<'_>>,
    manifest: &Manifest,
    file: &FileEntry,
) -> bool {
    if file.metadata {
        return false;
    }
    if let Some(selection) = selection {
        let q = selection.query;
        if file.language.document() && !matches!(q.selector.as_str(), "text" | "file") {
            return false;
        }
        if !crate::search::Search::file_language_filters(q, file.language) {
            return false;
        }
        if !file.memberships.iter().any(|m| {
            let path = manifest.display(file, m);
            selection
                .filters
                .iter()
                .all(|(f, glob)| match f.key.as_str() {
                    "path" => path_matches(&f.value, glob.as_ref(), &path) != f.negate,
                    "project" => {
                        crate::query::wildcard(&f.value, &manifest.projects[m.project].name)
                            != f.negate
                    }
                    "in" if !f.negate => crate::query::Target::parse(&f.value)
                        .ok()
                        .and_then(|t| t.location)
                        .is_none_or(|loc| {
                            path.as_ref() == loc.path
                                || path
                                    .ends_with(&format!("/{}", loc.path.trim_start_matches("…/")))
                        }),
                    _ => true,
                })
                && q.target.location.as_ref().is_none_or(|loc| {
                    path.as_ref() == loc.path
                        || path.ends_with(&format!("/{}", loc.path.trim_start_matches("…/")))
                })
                && (q.selector != "file"
                    || selection.target.as_ref().is_some_and(|glob| {
                        glob.is_match(if q.target.name.contains('/') {
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
        Request::History(_) => unreachable!("history bypasses snapshot indexes"),
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

fn path_matches(pattern: &str, glob: Option<&globset::GlobMatcher>, path: &str) -> bool {
    glob.is_some_and(|g| g.is_match(path))
        || path
            .strip_prefix(pattern.trim_end_matches('/'))
            .is_some_and(|s| s.starts_with('/'))
}

pub(super) fn wait_assets(request: &Request, query: Option<&Query>) -> bool {
    match request {
        Request::History(_) => unreachable!("history bypasses snapshot indexes"),
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
/// Source navigation only needs selected paths and stable source bytes.
pub(super) fn navigation(request: &Request, query: Option<&Query>) -> bool {
    match request {
        Request::History(_) => unreachable!("history bypasses snapshot indexes"),
        Request::Browse(_) => true,
        Request::Search(_) => query.is_some_and(|q| {
            q.selector == "file" && !q.wait_complete && !wait_assets(request, Some(q))
        }),
        Request::View(path, _) => crate::navigation::location(path).is_ok_and(|(path, _)| {
            let path = std::path::Path::new(path);
            matches!(path.extension().and_then(|s| s.to_str()), Some("cs" | "rs"))
                || crate::native::language(path).is_some()
                || crate::documents::language(path).is_some()
        }),
    }
}
pub(super) fn has_unity(manifest: &Manifest) -> bool {
    crate::unity::assets::is_root(&manifest.root)
        || manifest
            .projects
            .iter()
            .any(|p| p.compiler_options.contains_key("UnityVersion"))
        || manifest.files.values().any(|file| {
            file.path.ancestors().any(|path| {
                path.file_name()
                    .is_some_and(|name| name == "Assets" || name == "ProjectSettings")
                    && path.parent().is_some_and(crate::unity::assets::is_root)
            })
        })
}

impl super::App {
    pub(super) fn start_assets(
        &self,
        workspace: std::sync::Arc<tokio::sync::Mutex<Option<crate::workspace::Workspace>>>,
        branch: Option<std::sync::Arc<crate::repository::manager::Branch>>,
        manifest: &Manifest,
        revision: Option<String>,
        refresh: bool,
        activity: std::sync::Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> anyhow::Result<crate::unity::assets::jobs::Ticket> {
        if !has_unity(manifest) {
            return Ok(crate::unity::assets::jobs::empty());
        }
        let cache = branch
            .as_ref()
            .map(|b| b.root.join("unity-assets"))
            .unwrap_or_else(|| {
                self.cache.join("unity-assets").join(
                    blake3::hash(manifest.root.as_os_str().as_encoded_bytes())
                        .to_hex()
                        .as_str(),
                )
            });
        let leases = crate::cache::views::LeaseSet::new(&self.cache)?;
        leases.retain(&cache)?;
        Ok(self.assets.start(crate::unity::assets::jobs::Request {
            workspace,
            branch,
            root: manifest.root.clone(),
            cache,
            records: self.asset_records.clone(),
            expected: crate::unity::assets::jobs::generation(manifest)?,
            revision,
            refresh,
            activity,
            leases,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Language, Project, SourceRoot},
        workspace::{Membership, Stamp},
    };

    fn manifest() -> Manifest {
        let source = tempfile::NamedTempFile::new().unwrap();
        let stamp = Stamp::read(source.path()).unwrap();
        let file = |path: &str, language| FileEntry {
            path: path.into(),
            display: path.into(),
            stamp: stamp.clone(),
            language,
            memberships: vec![Membership {
                project: 0,
                module: String::new(),
            }],
            modules: Vec::new(),
            metadata: false,
        };
        Manifest {
            projects: vec![Project {
                identity: "app".into(),
                origin: None,
                name: "app".into(),
                defines: Vec::new(),
                references: Vec::new(),
                assemblies: Vec::new(),
                edition: String::new(),
                compiler_options: BTreeMap::new(),
                source_roots: Vec::new(),
            }],
            files: BTreeMap::from([(
                "ordinary".into(),
                file("native/ordinary.cs", Language::CSharp),
            )]),
            deferred: BTreeMap::from([
                (
                    "native".into(),
                    file("native/nested/engine.cpp", Language::Cpp),
                ),
                (
                    "shader".into(),
                    file("native/generated/effect.hlsl", Language::Hlsl),
                ),
            ]),
            ..Manifest::default()
        }
    }

    fn search(text: &str, manifest: &Manifest) -> BTreeMap<Group, bool> {
        groups(
            &Request::Search(text.into()),
            Some(&Query::parse(text).unwrap()),
            manifest,
        )
    }

    #[test]
    fn groups_preserve_file_path_filters_and_wait_decisions() {
        use Group::{Native, Shaders};
        let manifest = manifest();
        let cases: &[(&str, &[(Group, bool)])] = &[
            (
                "file:engine.cpp path:native/ wait:complete",
                &[(Native, true)],
            ),
            ("file:native/*/engine.cpp wait:complete", &[(Native, true)]),
            ("file:nested/engine.cpp wait:complete", &[]),
            (
                "file:* path:native/ -path:native/generated/ wait:complete",
                &[(Native, true)],
            ),
            (
                "symbol:* path:native/ -path:native/generated/",
                &[(Native, false)],
            ),
            ("symbol:* path:native/*/engine.cpp", &[(Native, true)]),
            ("symbol:@engine.cpp:1:1", &[(Native, true)]),
            ("symbol:* in:@engine.cpp:1:1", &[(Native, true)]),
            (
                "symbol:* path:native/** -path:**/*.cs",
                &[(Native, true), (Shaders, true)],
            ),
            ("symbol:* path:native/ lang:hlsl", &[(Shaders, true)]),
            ("symbol:*", &[(Native, false), (Shaders, false)]),
            ("symbol:* wait:complete", &[(Native, true), (Shaders, true)]),
            ("file:*", &[]),
        ];
        for &(text, expected) in cases {
            assert_eq!(
                search(text, &manifest),
                expected.iter().copied().collect::<BTreeMap<_, _>>(),
                "{text}"
            );
        }
    }

    #[test]
    fn groups_require_filters_to_match_the_same_project_membership() {
        let mut manifest = manifest();
        let mut project = manifest.projects[0].clone();
        project.name = "tools".into();
        project.source_roots = vec![SourceRoot {
            physical: "native".into(),
            logical: "tools".into(),
        }];
        manifest.projects.push(project);
        manifest
            .deferred
            .get_mut("native")
            .unwrap()
            .memberships
            .push(Membership {
                project: 1,
                module: String::new(),
            });
        assert_eq!(
            search("symbol:* path:tools/** project:tools", &manifest),
            BTreeMap::from([(Group::Native, true)])
        );
        assert!(search("symbol:* path:tools/** project:app", &manifest).is_empty());
        assert!(search("symbol:* path:native/** project:tools", &manifest).is_empty());
    }

    #[test]
    fn groups_preserve_invalid_glob_fallbacks() {
        let mut manifest = manifest();
        manifest.deferred.get_mut("native").unwrap().display = "native[/engine.cpp".into();
        let request = Request::Search(String::new());
        let mut query = Query::parse("symbol:* path:native/").unwrap();
        query.filters[0].value = "native[/".into();
        assert_eq!(
            groups(&request, Some(&query), &manifest),
            BTreeMap::from([(Group::Native, true)])
        );
        query.filters[0].negate = true;
        assert_eq!(
            groups(&request, Some(&query), &manifest),
            BTreeMap::from([(Group::Shaders, false)])
        );
        query.selector = "file".into();
        query.target.name = "[".into();
        query.wait_complete = true;
        assert!(groups(&request, Some(&query), &manifest).is_empty());
    }
}
