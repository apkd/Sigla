//! Prove that managed fallback cannot change the Rust input universe.
use super::*;
use std::sync::Arc;

pub(crate) struct Stage {
    pub discovery: Discovery,
    pub proof: Proof,
}

pub(crate) struct Proof {
    monitor: Arc<crate::watch::Monitor>,
    directories: BTreeSet<PathBuf>,
    watch: crate::watch::Snapshot,
}
impl Proof {
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.directories
            .iter()
            .map(|p| 2 * (p.as_os_str().len() as u64 + 64))
            .sum()
    }
    pub(crate) fn changed(&self) -> bool {
        self.watch.changed()
    }
}
impl Drop for Proof {
    fn drop(&mut self) {
        for path in &self.directories {
            self.monitor.unregister(path);
        }
    }
}

/// Accepted managed graph origins must be regular files in the immutable source
/// view. Cover every possible Project root, including arbitrary extensions, and
/// reject any root whose fallback could add a Rust file or membership.
pub(crate) fn stage(
    discovery: &Discovery,
    managed: &[PathBuf],
    policy: &Policy,
    monitor: Arc<crate::watch::Monitor>,
) -> Result<Option<Stage>> {
    let Some(remote) = &policy.remote else {
        return Ok(None);
    };
    if managed
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "sln" || e == "slnx"))
        || discovery.sources.is_empty()
        || discovery
            .sources
            .iter()
            .any(|s| s.language != Language::Rust)
        || discovery.projects.iter().any(|p| !p.assemblies.is_empty())
        || discovery.dependency_retry.is_some()
    {
        return Ok(None);
    }
    let files: BTreeSet<_> = remote
        .tracked
        .iter()
        .map(|p| remote.workspace.join(p))
        .collect();
    let relevant: BTreeSet<_> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .flat_map(|p| {
            p.ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(&remote.workspace))
                .map(Path::to_owned)
        })
        .collect();
    if managed
        .iter()
        .any(|p| p.parent().is_some_and(|p| relevant.contains(p)))
    {
        return Ok(None);
    }
    let mut candidates = BTreeSet::new();
    for file in &files {
        candidates.extend(
            file.ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(&remote.workspace))
                .map(Path::to_owned),
        );
    }
    let mut directories = BTreeSet::new();
    for directory in candidates {
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {
                directories.insert(directory);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => return Ok(None),
        }
    }
    for directory in &directories {
        monitor.register(directory);
    }
    let (_, fence) = monitor.fence(&directories, 0);
    let proof = Proof {
        watch: monitor.snapshot(&directories, fence),
        monitor,
        directories,
    };
    // Reject physical inputs outside the tracked view. Missing unrelated subtrees
    // cannot add Rust memberships; existing ancestors watch their later creation.
    for directory in &proof.directories {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !(kind.is_file() && files.contains(&entry.path())
                || kind.is_dir() && proof.directories.contains(&entry.path()))
            {
                return Ok(None);
            }
        }
    }
    let inspected: BTreeSet<_> = files
        .iter()
        .filter(|file| {
            file.parent()
                .is_some_and(|parent| relevant.contains(parent))
        })
        .cloned()
        .collect();
    for file in &inspected {
        use std::io::Read;
        let mut input = std::fs::File::open(file)?;
        let mut prefix = Vec::new();
        (&mut input).take(4096).read_to_end(&mut prefix)?;
        let mut project = project_prefix(&prefix);
        if project.is_none() && prefix.len() == 4096 {
            input.take(60 * 1024).read_to_end(&mut prefix)?;
            project = project_prefix(&prefix);
        }
        let Some(project) = project else {
            return Ok(None);
        };
        if project || file.extension().is_some_and(|e| e == "csproj") {
            return Ok(None);
        }
    }
    if proof.changed() {
        return Ok(None);
    }
    let mut discovery = discovery.rust_projection()?;
    discovery.metadata.extend(inspected);
    discovery.metadata.extend(proof.directories.iter().cloned());
    Ok(Some(Stage { discovery, proof }))
}

fn project_prefix(bytes: &[u8]) -> Option<bool> {
    use quick_xml::events::Event;
    if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1")
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
        || bytes.starts_with(&[0, 0, 0xfe, 0xff])
        || bytes.starts_with(b"<\0")
        || bytes.starts_with(b"\0<")
        || bytes.starts_with(b"\0\0\0")
        || bytes.get(..2).is_some_and(|b| {
            b[0].is_ascii_whitespace() && b[1] == 0 || b[0] == 0 && b[1].is_ascii_whitespace()
        })
    {
        return None;
    }
    let mut reader = quick_xml::Reader::from_reader(bytes);
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag) | Event::Empty(tag)) => {
                return Some(tag.local_name().as_ref().eq_ignore_ascii_case(b"Project"));
            }
            Ok(Event::Text(text)) if text.iter().any(|b| !b.is_ascii_whitespace()) => {
                return Some(false);
            }
            Ok(Event::Eof) => {
                return (bytes.len() < 4096 && bytes.iter().all(u8::is_ascii_whitespace))
                    .then_some(false);
            }
            Ok(Event::Decl(declaration)) => {
                if let Some(encoding) = declaration.encoding() {
                    let encoding = encoding.ok()?;
                    if ![b"utf-8".as_slice(), b"us-ascii", b"ascii"]
                        .iter()
                        .any(|allowed| encoding.eq_ignore_ascii_case(allowed))
                    {
                        return None;
                    }
                }
            }
            Ok(Event::PI(_) | Event::Comment(_) | Event::Text(_)) => (),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_roots_include_arbitrary_extensions_and_fail_closed_on_uncertain_xml() {
        assert_eq!(
            project_prefix(b"<?xml version='1.0'?><!-- comment --><m:Project xmlns:m='x'/>"),
            Some(true)
        );
        assert_eq!(project_prefix(b"pub struct Rust;"), Some(false));
        assert_eq!(project_prefix(b"<!-- unfinished"), None);
        assert_eq!(project_prefix(&vec![b' '; 65536]), None);
        assert_eq!(project_prefix(b"\xff\xfe<\0P\0"), None);
        assert_eq!(project_prefix(b"\0\0\xfe\xff\0\0\0<"), None);
        assert_eq!(
            project_prefix(b"<?xml version='1.0' encoding='utf-7'?>+ADw-Project/+AD4-"),
            None
        );
        assert_eq!(project_prefix(b"\xef\xbb\xbf<Project/>"), Some(true));
        assert_eq!(
            project_prefix(b"version https://git-lfs.github.com/spec/v1\noid sha256:abc"),
            None
        );
    }

    #[test]
    fn completeness_proof_covers_transitive_roots_inventory_and_new_rust_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("source");
        std::fs::create_dir_all(root.join("managed/nested"))?;
        let files = [
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'",
            ),
            ("lib.rs", "pub struct Rust;"),
            (
                "managed/managed.csproj",
                "<Project><ItemGroup><Compile Include='Code.cs'/></ItemGroup></Project>",
            ),
            ("managed/global.json", "{}"),
            ("managed/nested/Code.cs", "class Managed {}"),
            ("candidate.txt", "ordinary text"),
        ];
        for (path, text) in files {
            std::fs::write(root.join(path), text)?;
        }
        let mut policy = Policy::new(vec![root.clone()])?;
        policy.remote = Some(crate::discovery::RemoteContext {
            workspace: root.clone(),
            writable: directory.path().join("generated"),
            shared: directory.path().join("cache"),
            repositories: vec![],
            selection_identity: String::new(),
            tracked: Arc::new(files.iter().map(|(p, _)| (*p).to_owned()).collect()),
        });
        let monitor = Arc::new(crate::watch::Monitor::default());
        let mut captured = None;
        let mut capture = |discovery: &Discovery, managed: &[PathBuf]| {
            captured = Some((discovery.clone(), managed.to_vec()));
        };
        crate::discovery::discover_staged(
            &root,
            &policy,
            &directory.path().join("cache"),
            crate::msbuild::Budget::default(),
            Some(&mut capture),
        )?;
        let (discovery, managed) = captured.unwrap();
        // Sparse archives and entire unmaterialized subtrees without Rust files
        // cannot introduce managed fallback memberships for Rust sources.
        Arc::make_mut(&mut policy.remote.as_mut().unwrap().tracked).extend([
            "managed/unused.tar.zst".into(),
            "unmaterialized/deep/data.bin".into(),
        ]);
        let valid = stage(&discovery, &managed, &policy, monitor.clone())?.unwrap();
        let mut incomplete = discovery.clone();
        incomplete.dependency_retry = Some(DependencyRetry::new());
        assert!(stage(&incomplete, &managed, &policy, monitor.clone())?.is_none());
        std::fs::write(root.join("managed/nested/New.rs"), "pub struct New;")?;
        assert!(valid.proof.changed());
        assert!(stage(&discovery, &managed, &policy, monitor.clone())?.is_none());
        std::fs::remove_file(root.join("managed/nested/New.rs"))?;
        // A managed ProjectReference may point to any extension, including one at
        // the repository root whose fallback would add the Cargo files again.
        std::fs::write(root.join("candidate.txt"), "<Project/>")?;
        assert!(stage(&discovery, &managed, &policy, monitor.clone())?.is_none());
        std::fs::write(root.join("candidate.txt"), "<!-- uncertain")?;
        assert!(stage(&discovery, &managed, &policy, monitor.clone())?.is_none());
        std::fs::write(root.join("candidate.txt"), "ordinary text")?;
        assert!(stage(&discovery, &managed, &policy, monitor.clone())?.is_some());
        assert!(
            stage(
                &discovery,
                &[root.join("managed/build.sln")],
                &policy,
                monitor
            )?
            .is_none()
        );
        Ok(())
    }
}
