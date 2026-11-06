//! Validate published source files once, then use filesystem events to detect loss.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(super) struct Sources {
    root: PathBuf,
    directories: BTreeSet<PathBuf>,
    monitor: Arc<crate::watch::Monitor>,
    validated: Option<crate::watch::Snapshot>,
}

impl Sources {
    pub fn new(root: &Path, prepared: &super::materialize::Prepared) -> Self {
        let files = prepared
            .selected
            .keys()
            .filter(|p| !prepared.unavailable.contains(*p))
            .map(|p| root.join(p));
        let mut directories = BTreeSet::from([root.to_owned()]);
        for file in files {
            for parent in file
                .parent()
                .unwrap()
                .ancestors()
                .take_while(|p| p.starts_with(root))
            {
                directories.insert(parent.to_owned());
            }
        }
        let monitor = Arc::new(crate::watch::Monitor::default());
        for directory in &directories {
            monitor.register(directory);
        }
        Self {
            root: root.to_owned(),
            directories,
            monitor,
            validated: None,
        }
    }

    pub fn current(&mut self, prepared: &super::materialize::Prepared) -> bool {
        if self.validated.as_ref().is_some_and(|s| !s.changed()) {
            return true;
        }
        // Fence before checking files: a concurrent deletion must remain visible
        // to the next request, even if this scan already visited its path.
        let (_, generation) = self.monitor.fence(&self.directories, 0);
        if !prepared
            .selected
            .keys()
            .all(|p| prepared.unavailable.contains(p) || self.root.join(p).is_file())
        {
            self.validated = None;
            return false;
        }
        self.validated = Some(self.monitor.snapshot(&self.directories, generation));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(paths: &[&str]) -> super::super::materialize::Prepared {
        super::super::materialize::Prepared {
            selected: paths.iter().map(|p| ((*p).into(), "blob".into())).collect(),
            unavailable: Default::default(),
            deferred_assets: Default::default(),
            omitted: Default::default(),
            transfer_bytes: 0,
            transport: None,
            resolved_target: None,
            branch: None,
            revision: String::new(),
            tracked: Default::default(),
            directories: Default::default(),
        }
    }

    #[test]
    #[ignore = "measures warm source validation filesystem work"]
    fn warm_source_validation_probe() {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..10_000)
            .map(|i| format!("{}/{}.rs", i / 100, i))
            .collect();
        for path in &paths {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "fn source() {}").unwrap();
        }
        let prepared = prepared(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let mut sources = Sources::new(root.path(), &prepared);
        assert!(sources.current(&prepared));
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert!(
                prepared
                    .selected
                    .keys()
                    .all(|p| root.path().join(p).is_file())
            );
        }
        let full = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert!(sources.current(&prepared));
        }
        eprintln!(
            "20 requests / 10000 files: full={full:?}, watched={:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn publication_validates_new_inputs_even_without_a_generation_change() {
        let root = tempfile::tempdir().unwrap();
        let branch = super::super::manager::Branch::for_test(root.path().into());
        std::fs::create_dir_all(branch.source()).unwrap();
        let before = prepared(&[]);
        assert!(branch.sources_current(&before));
        // Optional asset publication preserves the code generation.
        let publication = branch.publish().await;
        drop(publication);
        let after = prepared(&["new.rs"]);
        assert!(!branch.sources_current(&after));
        std::fs::write(branch.source().join("new.rs"), "fn added() {}").unwrap();
        assert!(branch.sources_current(&after));
    }

    #[test]
    fn missing_files_and_removed_ancestors_invalidate_warm_sources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested/deeper/source.rs");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "fn source() {}").unwrap();
        let mut prepared = prepared(&["nested/deeper/source.rs", "unavailable.rs"]);
        prepared.unavailable.insert("unavailable.rs".into());
        let mut sources = Sources::new(root.path(), &prepared);
        assert!(sources.current(&prepared));
        assert!(sources.current(&prepared));
        std::fs::remove_file(&path).unwrap();
        assert!(!sources.current(&prepared));
        std::fs::write(&path, "fn restored() {}").unwrap();
        assert!(sources.current(&prepared));
        std::fs::rename(root.path().join("nested"), root.path().join("moved")).unwrap();
        assert!(!sources.current(&prepared));
    }
}
