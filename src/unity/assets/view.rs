//! File contents need one asset record, without script binding or prefab composition.
use super::*;
use crate::{cancellation::Cancellation, navigation::Mode};
use tokio_util::sync::CancellationToken;

pub(crate) fn requested(path: &str) -> bool {
    !path.starts_with("unity@")
        && crate::navigation::location(path).is_ok_and(|(path, _)| supported(Path::new(path)))
}

pub(crate) fn supported(path: &Path) -> bool {
    is_asset(path)
        || path
            .extension()
            .is_some_and(|extension| extension == "meta")
}

pub(crate) struct Reader<'a> {
    pub root: &'a Path,
    pub cache: &'a Path,
    pub manifest: &'a Manifest,
    pub records: &'a crate::cache::artifacts::Store,
    pub leases: &'a crate::cache::views::LeaseSet,
    pub remote: bool,
    pub omitted: &'a BTreeMap<String, String>,
    pub unavailable: &'a BTreeSet<String>,
    pub cancel: &'a CancellationToken,
}

impl Reader<'_> {
    fn unavailable(&self, path: &str) -> Option<&str> {
        self.omitted.get(path).map(String::as_str).or_else(|| {
            self.unavailable
                .contains(path)
                .then_some("Source contents are unavailable")
        })
    }

    pub fn view(&self, path: &str, mode: Mode) -> Result<Option<String>> {
        self.leases.retain(self.root)?;
        self.leases.retain(self.cache)?;
        let normalized = crate::navigation::normalize(path, self.root, !self.remote)?;
        let path = normalized.as_str();
        let (selected, _) = crate::navigation::location(path)?;
        let mut projects = Vec::new();
        let mut observed = BTreeMap::new();
        roots(self.root, &mut projects, &mut observed, 0, self.cancel)?;
        let scopes: Vec<_> = projects
            .iter()
            .flat_map(|project| {
                scopes(self.root, project, self.manifest)
                    .into_iter()
                    .map(move |scope| (project, scope))
            })
            .collect();
        let mut candidates = BTreeMap::new();
        // An exact path needs only its ancestors, even in a very large asset tree.
        for (project, scope) in &scopes {
            self.cancel.check()?;
            self.leases.retain(scope)?;
            let prefix = logical_path(self.root, project, scope, self.manifest);
            if let Some(relative) = selected
                .strip_prefix(prefix.trim_end_matches('/'))
                .and_then(|rest| rest.strip_prefix('/'))
                && let Some(file) = exact(scope, relative, self.cancel)?
            {
                candidates
                    .entry(selected.to_owned())
                    .or_insert_with(BTreeSet::new)
                    .insert(file);
            }
        }
        if self.unavailable(selected).is_some()
            && projects
                .iter()
                .any(|project| self.root.join(selected).starts_with(project))
        {
            candidates
                .entry(selected.to_owned())
                .or_insert_with(BTreeSet::new)
                .insert(self.root.join(selected));
        }
        if candidates.is_empty() {
            for (project, scope) in &scopes {
                let mut files = BTreeSet::new();
                if scope.is_dir() {
                    inventory(scope, &mut files, &mut observed, 0, true, self.cancel)?;
                }
                for file in files.into_iter().filter(|file| supported(file)) {
                    let logical = logical_path(self.root, project, &file, self.manifest);
                    candidates
                        .entry(logical)
                        .or_insert_with(BTreeSet::new)
                        .insert(file);
                }
            }
            for logical in self
                .omitted
                .keys()
                .chain(self.unavailable.iter())
                .filter(|logical| supported(Path::new(logical)))
            {
                let file = self.root.join(logical);
                if projects.iter().any(|project| file.starts_with(project)) {
                    candidates
                        .entry(logical.clone())
                        .or_insert_with(BTreeSet::new)
                        .insert(file);
                }
            }
        }
        let matches =
            crate::navigation::file_matches(candidates.keys().map(String::as_str), selected, mode);
        if matches.is_empty() {
            ensure!(mode != Mode::Raw, "Asset source is unavailable");
            return Ok(None);
        }
        ensure!(
            matches.len() == 1 && candidates[matches[0]].len() == 1,
            "Asset path is ambiguous; use a full path or select a Unity project"
        );
        let logical = matches[0];
        let file = candidates[logical].first().unwrap();
        self.leases.retain(file)?;
        if file
            .extension()
            .is_some_and(|extension| extension == "meta")
        {
            ensure!(
                self.unavailable(logical).is_none(),
                "Metadata source is unavailable"
            );
            let size = std::fs::metadata(file)?.len();
            ensure!(
                size <= if self.remote {
                    crate::repository::selection::ASSET_LIMIT
                } else {
                    LOCAL_LIMIT
                },
                "Metadata exceeds size limit ({size} bytes)"
            );
            let source =
                crate::memory::Source::read(size, self.cancel, || Ok(std::fs::read(file)?))?
                    .map(|bytes| crate::model::decode_raw(bytes, Language::Text))?;
            return crate::navigation::sources::view(
                &BTreeMap::from([(logical.to_owned(), ())]),
                self.root,
                path,
                mode,
                !self.remote,
                self.cancel,
                |_| Ok(Some((Language::Text, source))),
            );
        }
        std::fs::create_dir_all(self.cache)?;
        let limit = if self.remote {
            crate::repository::selection::ASSET_LIMIT
        } else {
            LOCAL_LIMIT
        };
        let mut asset = Asset {
            project: String::new(),
            path: logical.into(),
            guid: String::new(),
            objects: super::records::empty(),
            content: None,
            unavailable: self.unavailable(logical).map(str::to_owned),
        };
        if asset.unavailable.is_none() {
            let stamp = Stamp::read(file)?;
            if stamp.size > limit {
                asset.unavailable =
                    Some(format!("Asset exceeds size limit ({} bytes)", stamp.size));
            } else {
                let record =
                    records::load(self.cache, Some(self.records), file, &stamp, self.cancel)?;
                asset.content = record.content;
                asset.unavailable = record.unavailable;
            }
            if mode != Mode::Raw && asset.unavailable.is_some() {
                asset.guid = metadata(&PathBuf::from(format!("{}.meta", file.display())))
                    .and_then(|(guid, _)| guid)
                    .unwrap_or_default();
            }
        }
        let mut index = Index::default();
        index.assets.insert(logical.into(), asset);
        if mode == Mode::Raw {
            index.view_raw(path, self.cancel)
        } else {
            index.view_with_cancel(path, self.cancel)
        }
    }
}

/// Follow the same directory boundaries as inventory without walking siblings.
fn exact(scope: &Path, relative: &str, cancel: &dyn Cancellation) -> Result<Option<PathBuf>> {
    let mut path = scope.to_owned();
    let mut components = Path::new(relative).components().peekable();
    while let Some(component) = components.next() {
        cancel.check()?;
        let std::path::Component::Normal(name) = component else {
            return Ok(None);
        };
        path.push(name);
        if ignored(&path) {
            return Ok(None);
        }
        let kind = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if components.peek().is_none() {
            return Ok((kind.is_file() && supported(&path)).then_some(path));
        }
        if !kind.is_dir() || is_root(&path) || crate::discovery::linked_worktree(&path) {
            return Ok(None);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn exact_paths_respect_inventory_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        for file in [
            "Visible/File.prefab",
            ".hidden/File.prefab",
            "Nested/Assets/File.prefab",
            "Linked/File.prefab",
        ] {
            write(root.path(), file, "contents");
        }
        write(
            root.path(),
            "Nested/ProjectSettings/ProjectVersion.txt",
            "version",
        );
        write(root.path(), "Linked/.git", "gitdir: ../.linked\n");
        write(root.path(), ".linked/commondir", "..\n");
        std::os::unix::fs::symlink(root.path().join("Visible"), root.path().join("Alias")).unwrap();
        let mut files = BTreeSet::new();
        inventory(
            root.path(),
            &mut files,
            &mut BTreeMap::new(),
            0,
            false,
            &cancel,
        )
        .unwrap();
        for file in [
            "Visible/File.prefab",
            ".hidden/File.prefab",
            "Nested/Assets/File.prefab",
            "Linked/File.prefab",
            "Alias/File.prefab",
        ] {
            assert_eq!(
                exact(root.path(), file, &cancel).unwrap().is_some(),
                files.contains(&root.path().join(file)),
                "{file}"
            );
        }
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn file_views_resolve_external_packages_and_keep_suffix_ambiguity() {
        use crate::model::{Project, SourceRoot};
        let root = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write(root.path(), "ProjectSettings/ProjectVersion.txt", "version");
        let source = "--- !u!1 &1\nGameObject:\n  m_Name: Visible\n";
        write(root.path(), "Assets/Selected.prefab", source);
        write(package.path(), "Selected.prefab", source);
        let manifest = Manifest {
            root: root.path().into(),
            projects: vec![Project {
                identity: String::new(),
                origin: None,
                name: String::new(),
                defines: Vec::new(),
                references: Vec::new(),
                assemblies: Vec::new(),
                edition: String::new(),
                compiler_options: BTreeMap::new(),
                source_roots: vec![
                    SourceRoot {
                        physical: root.path().join("Assets"),
                        logical: "Assets".into(),
                    },
                    SourceRoot {
                        physical: package.path().into(),
                        logical: "Packages/example".into(),
                    },
                ],
            }],
            ..Manifest::default()
        };
        let records = crate::cache::artifacts::Store::open(cache.path()).unwrap();
        let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
        let cancel = CancellationToken::new();
        let reader = Reader {
            root: root.path(),
            cache: cache.path(),
            manifest: &manifest,
            records: &records,
            leases: &leases,
            remote: false,
            omitted: &BTreeMap::new(),
            unavailable: &BTreeSet::new(),
            cancel: &cancel,
        };
        for file in ["Assets/Selected.prefab", "Packages/example/Selected.prefab"] {
            assert_eq!(reader.view(file, Mode::Raw).unwrap().unwrap(), source);
        }
        assert!(reader.view("Selected.prefab", Mode::Raw).is_err());
        cancel.cancel();
        assert!(reader.view("Assets/Selected.prefab", Mode::Raw).is_err());
    }
}
