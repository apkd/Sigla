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

/// A positive literal path can be established without reading asset contents.
/// Other queries retain the normal index completeness and collision checks.
pub(crate) fn exact_request(query: &Query) -> bool {
    query.selector == "file"
        && is_asset(Path::new(&query.target.name))
        && !query.wait_complete
        && !query.loose
        && query.limit > 0
        && searches_files(query)
        && query.target.name.contains('/')
        && query
            .target
            .name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !query.target.name.contains(['*', '?', '[', '{', '}', '\\'])
        && Path::new(&query.target.name)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && !query.target.name.ends_with(".meta")
}

pub(crate) fn exact_file(
    manifest: &Manifest,
    prepared: Option<&Prepared>,
    query: &Query,
    cancel: &dyn Cancellation,
) -> Result<bool> {
    exact_file_with_memberships(manifest, prepared, query, cancel, prepared.is_none())
}

pub(crate) fn exact_workspace_file(
    manifest: &Manifest,
    prepared: Option<&Prepared>,
    query: &Query,
    cancel: &dyn Cancellation,
) -> Result<bool> {
    exact_file_with_memberships(manifest, prepared, query, cancel, true)
}

fn exact_file_with_memberships(
    manifest: &Manifest,
    prepared: Option<&Prepared>,
    query: &Query,
    cancel: &dyn Cancellation,
    memberships_known: bool,
) -> Result<bool> {
    if !exact_request(query) {
        return Ok(false);
    }
    let path = &query.target.name;
    if !crate::navigation::sources::file_path_filters(query)?(path) {
        return Ok(false);
    }
    // Sidecars alone can describe folders or missing files. Local visual graphs
    // also need sidecar interpretation, so leave those cases to the asset index.
    let typed = crate::repository::selection::asset(Path::new(path))
        || prepared.is_some() && crate::repository::selection::visual_graph(Path::new(path));
    if !typed {
        return Ok(false);
    }
    if query.filters.iter().any(|filter| filter.key == "lang") {
        // A compiler can include an asset-named source. Remote inventories do
        // not yet have those memberships, so defer language-sensitive queries.
        if !memberships_known {
            return Ok(false);
        }
        let paths = crate::navigation::sources::source_paths(manifest, &manifest.root);
        let source = paths.get(path).and_then(|key| {
            manifest
                .files
                .get(key)
                .or_else(|| manifest.deferred.get(key))
        });
        if !source.map_or_else(
            || language_filter(query)(Path::new(path)),
            |file| query.file_language_filters(file.language),
        ) {
            return Ok(false);
        }
    }
    let known = prepared.is_some_and(|prepared| {
        prepared.selected.contains_key(path) || prepared.deferred_assets.contains(path)
    });
    let mut projects = Vec::new();
    if let Some(prepared) = prepared {
        if prepared.omitted.contains_key(path)
            || prepared.unavailable.contains(path)
            || !known && !memberships_known
        {
            return Ok(false);
        }
        projects.extend(
            prepared
                .directories
                .iter()
                .filter(|path| {
                    Path::new(path)
                        .file_name()
                        .is_some_and(|name| name == "Assets")
                })
                .filter_map(|path| Path::new(path).parent())
                .filter(|project| {
                    prepared.tracked.contains_key(
                        &project
                            .join("ProjectSettings/ProjectVersion.txt")
                            .to_string_lossy()
                            .into_owned(),
                    )
                })
                .map(|project| manifest.root.join(project)),
        );
    }
    if memberships_known {
        if let Some(project) = local_project(&manifest.root, path, cancel)?
            && !projects.contains(&project)
        {
            projects.push(project);
        }
        // Packages inside the workspace keep their physical display path, even
        // when their Unity project lives in a sibling directory.
        let physical = manifest.root.join(path);
        for project in manifest.projects.iter().filter(|project| {
            project
                .source_roots
                .iter()
                .any(|source| physical.starts_with(&source.physical))
        }) {
            for source in &project.source_roots {
                if !source.physical.ends_with("Assets") {
                    continue;
                }
                let candidate = source.physical.parent().unwrap();
                if projects.iter().any(|project| project == candidate) {
                    continue;
                }
                let Ok(relative) = source.physical.strip_prefix(&manifest.root) else {
                    continue;
                };
                if local_project(&manifest.root, &relative.to_string_lossy(), cancel)?.as_deref()
                    == Some(candidate)
                {
                    projects.push(candidate.to_owned());
                }
            }
        }
    }
    for project in &projects {
        cancel.check()?;
        if prepared.is_some()
            && (project
                .strip_prefix(&manifest.root)?
                .ancestors()
                .any(ignored)
                || projects
                    .iter()
                    .any(|parent| parent != project && project.starts_with(parent)))
        {
            continue;
        }
        for scope in scopes(&manifest.root, project, manifest) {
            let prefix = logical_path(&manifest.root, project, &scope, manifest);
            let Some(relative) = path
                .strip_prefix(prefix.trim_end_matches('/'))
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                continue;
            };
            if known {
                let physical = scope.join(relative);
                if physical.strip_prefix(&scope)?.ancestors().any(ignored)
                    || projects.iter().any(|other| {
                        other != project && other.starts_with(&scope) && physical.starts_with(other)
                    })
                {
                    continue;
                }
                return Ok(true);
            }
            if super::view::exact(&scope, relative, cancel)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn local_project(root: &Path, path: &str, cancel: &dyn Cancellation) -> Result<Option<PathBuf>> {
    let mut project = root.to_owned();
    for (depth, component) in Path::new(path).components().enumerate() {
        cancel.check()?;
        ensure!(
            depth < 128,
            "Directory nesting exceeds asset discovery limit"
        );
        if is_root(&project) {
            return Ok(Some(project));
        }
        project.push(component);
        if ignored(&project) || crate::discovery::linked_worktree(&project) {
            return Ok(None);
        }
        match std::fs::symlink_metadata(&project) {
            Ok(metadata) if metadata.is_dir() => (),
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
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
    fn exact_filenames_ignore_unrelated_directory_trees() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Game/ProjectSettings")).unwrap();
        std::fs::write(
            root.path().join("Game/ProjectSettings/ProjectVersion.txt"),
            "version",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("Game/Assets")).unwrap();
        std::fs::write(root.path().join("Game/Assets/Known.prefab"), "asset").unwrap();
        std::fs::create_dir_all(root.path().join("Shared/pkg")).unwrap();
        std::fs::write(root.path().join("Shared/pkg/Known.prefab"), "asset").unwrap();
        // A full workspace scan would exceed its nesting bound here.
        std::fs::create_dir_all(root.path().join("Unrelated").join("nested/".repeat(128))).unwrap();
        let manifest = Manifest {
            root: root.path().into(),
            projects: vec![crate::model::Project {
                identity: "game".into(),
                origin: None,
                name: "game".into(),
                defines: vec![],
                references: vec![],
                assemblies: vec![],
                edition: String::new(),
                compiler_options: BTreeMap::new(),
                source_roots: vec![
                    crate::model::SourceRoot {
                        physical: root.path().join("Game/Assets"),
                        logical: "Game/Assets".into(),
                    },
                    crate::model::SourceRoot {
                        physical: root.path().join("Shared/pkg"),
                        logical: "Game/Packages/example".into(),
                    },
                ],
            }],
            ..Default::default()
        };
        let prepared = Prepared {
            selected: BTreeMap::new(),
            deferred_assets: BTreeSet::from(["Shared/pkg/Deferred.prefab".into()]),
            tracked: BTreeMap::from([(
                "Game/ProjectSettings/ProjectVersion.txt".into(),
                "version".into(),
            )]),
            directories: BTreeSet::from(["Game/Assets".into()]),
            omitted: Default::default(),
            unavailable: Default::default(),
            transfer_bytes: 0,
            transport: None,
            resolved_target: None,
            branch: None,
            revision: String::new(),
        };
        for (query, expected) in [
            ("file:Game/Assets/Known.prefab", true),
            ("file:Game/Assets/Known.prefab -path:Game", false),
            ("file:Shared/pkg/Known.prefab", true),
            ("file:Game/Packages/example/Known.prefab", false),
            ("file:Other/Missing.prefab", false),
        ] {
            assert_eq!(
                exact_file(&manifest, None, &Query::parse(query).unwrap(), &()).unwrap(),
                expected
            );
            assert_eq!(
                exact_workspace_file(
                    &manifest,
                    Some(&prepared),
                    &Query::parse(query).unwrap(),
                    &()
                )
                .unwrap(),
                expected
            );
        }
        assert!(
            exact_workspace_file(
                &manifest,
                Some(&prepared),
                &Query::parse("file:Shared/pkg/Deferred.prefab lang:text").unwrap(),
                &()
            )
            .unwrap()
        );
    }

    #[test]
    fn exact_filenames_preserve_source_language_precedence() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("ProjectSettings")).unwrap();
        std::fs::write(
            root.path().join("ProjectSettings/ProjectVersion.txt"),
            "version",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("Assets")).unwrap();
        let path = root.path().join("Assets/Known.asset");
        std::fs::write(&path, "class C {}").unwrap();
        let mut manifest = Manifest {
            root: root.path().into(),
            ..Default::default()
        };
        manifest.projects.push(crate::model::Project {
            identity: "project".into(),
            origin: None,
            name: "project".into(),
            defines: vec![],
            references: vec![],
            assemblies: vec![],
            edition: String::new(),
            compiler_options: BTreeMap::new(),
            source_roots: vec![],
        });
        manifest.files.insert(
            "source-key".into(),
            crate::workspace::FileEntry {
                stamp: Stamp::read(&path).unwrap(),
                path,
                display: "Assets/Known.asset".into(),
                language: crate::model::Language::CSharp,
                memberships: vec![crate::workspace::Membership {
                    project: 0,
                    module: String::new(),
                }],
                modules: vec![],
                metadata: false,
            },
        );
        for (filter, expected) in [
            ("", true),
            ("-lang:cs", false),
            ("lang:text", false),
            ("-lang:rust", true),
        ] {
            let query = Query::parse(&format!("file:Assets/Known.asset {filter}")).unwrap();
            assert_eq!(exact_file(&manifest, None, &query, &()).unwrap(), expected);
            let cancel = tokio_util::sync::CancellationToken::new();
            let sources = crate::navigation::sources::Sources {
                manifest: &manifest,
                assets: None,
                cancel: &cancel,
            };
            assert_eq!(
                sources
                    .files(&query, root.path())
                    .unwrap()
                    .contains("Known.asset"),
                expected
            );
        }
    }

    #[test]
    fn exact_filenames_require_file_proof_beyond_a_sidecar() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("ProjectSettings")).unwrap();
        std::fs::write(
            root.path().join("ProjectSettings/ProjectVersion.txt"),
            "version",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("Assets/Textures")).unwrap();
        std::fs::write(
            root.path().join("Assets/Textures.meta"),
            "folderAsset: yes\n",
        )
        .unwrap();
        std::fs::write(root.path().join("Assets/Graph.shadergraph"), "{}").unwrap();
        let manifest = Manifest {
            root: root.path().into(),
            ..Default::default()
        };
        for path in ["Assets/Textures", "Assets/Graph.shadergraph"] {
            assert!(
                !exact_file(
                    &manifest,
                    None,
                    &Query::parse(&format!("file:{path}")).unwrap(),
                    &()
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn deferred_assets_match_without_materialized_project_directories() {
        let root = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            root: root.path().into(),
            ..Default::default()
        };
        let mut prepared = Prepared {
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
            directories: BTreeSet::from(["Assets".into()]),
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
        for (query, expected) in [
            ("file:Assets/Data/Settings.asset", true),
            ("file:Assets/Data/Settings.asset -lang:cs", false),
            ("file:Assets/Data/Settings.asset lang:cs", false),
            ("file:Assets/Data/Settings.asset project:*", false),
            ("file:Assets/Data/Settings.asset -path:Assets/Data", false),
            (
                "file:Assets/Data/Settings.asset -path:Assets/Data/Settings.asset/",
                false,
            ),
            ("file:Assets/Data/Settings.asset/", false),
            ("file:Assets//Data/Settings.asset", false),
            ("file:Assets/./Data/Settings.asset", false),
            ("file:Assets/Data/Settings.asset wait:complete", false),
            ("file:Assets/Data/Icon.png", false),
            ("file:Assets/Data/Missing.asset", false),
            ("file:Assets/Data/*.asset", false),
        ] {
            assert_eq!(
                exact_file(
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
        let query = Query::parse("file:Assets/Data/Settings.asset").unwrap();
        assert!(
            exact_workspace_file(
                &manifest,
                Some(&prepared),
                &Query::parse("file:Assets/Data/Settings.asset lang:text").unwrap(),
                &()
            )
            .unwrap()
        );
        prepared
            .omitted
            .insert(query.target.name.clone(), "limit".into());
        assert!(!exact_file(&manifest, Some(&prepared), &query, &()).unwrap());
        prepared.omitted.clear();
        prepared.unavailable.insert(query.target.name.clone());
        assert!(!exact_file(&manifest, Some(&prepared), &query, &()).unwrap());
        prepared.unavailable.clear();
        for root in ["Assets/Nested", "Library/Hidden"] {
            let path = format!("{root}/Assets/Other.asset");
            prepared.directories.insert(format!("{root}/Assets"));
            prepared.tracked.insert(
                format!("{root}/ProjectSettings/ProjectVersion.txt"),
                "version".into(),
            );
            prepared.deferred_assets.insert(path.clone());
            assert!(
                !exact_file(
                    &manifest,
                    Some(&prepared),
                    &Query::parse(&format!("file:{path}")).unwrap(),
                    &()
                )
                .unwrap()
            );
        }
        prepared.directories.clear();
        assert!(!exact_file(&manifest, Some(&prepared), &query, &()).unwrap());
    }
}
