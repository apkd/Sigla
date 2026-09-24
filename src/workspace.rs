use crate::{
    discovery::{self, Policy},
    model::*,
    store::{FileData, MAX_SOURCE_BYTES, Store},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    size: u64,
    modified: u128,
    changed: i64,
    inode: u64,
}
impl Stamp {
    pub fn read(path: &Path) -> Result<Self> {
        let m = std::fs::metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                size: m.len(),
                modified: m
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                changed: m.ctime_nsec() ^ m.ctime(),
                inode: m.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                size: m.len(),
                modified: 0,
                changed: 0,
                inode: 0,
            })
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Membership {
    pub project: usize,
    pub module: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display: String,
    pub stamp: Stamp,
    pub language: Language,
    pub memberships: Vec<Membership>,
    pub modules: Vec<ModuleFile>,
    pub metadata: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub discovery_policy: [u8; 32],
    pub environment: [u8; 32],
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub files: BTreeMap<String, FileEntry>,
    pub metadata: BTreeMap<PathBuf, Stamp>,
    pub inputs: Vec<SourceInput>,
    pub dependencies: BTreeSet<PathBuf>,
    pub diagnostics: Vec<String>,
}

impl Manifest {
    pub fn metadata_visible(&self, file: &FileEntry, project: usize) -> bool {
        self.projects[project].assemblies.iter().any(|reference| {
            reference.path == file.path
                && (reference.aliases.is_empty() || reference.aliases.iter().any(|a| a == "global"))
        })
    }
    pub fn display<'a>(
        &'a self,
        file: &'a FileEntry,
        membership: &Membership,
    ) -> std::borrow::Cow<'a, str> {
        let mapping = self.projects[membership.project]
            .source_roots
            .iter()
            .filter(|root| file.path.starts_with(&root.physical))
            .max_by_key(|root| root.physical.components().count());
        match mapping {
            Some(root) => {
                let relative = file
                    .path
                    .strip_prefix(&root.physical)
                    .unwrap()
                    .to_string_lossy();
                std::borrow::Cow::Owned(if root.logical.is_empty() {
                    relative.into_owned()
                } else {
                    format!("{}/{relative}", root.logical)
                })
            }
            None => std::borrow::Cow::Borrowed(&file.display),
        }
    }
}

pub struct Workspace {
    pub entry: PathBuf,
    pub store: Arc<Store>,
    pub assemblies: Arc<Store>,
    pub manifest: Arc<Manifest>,
    policy: Policy,
    cache: PathBuf,
    pub builds: usize,
    monitor: Arc<crate::watch::Monitor>,
    directories: BTreeSet<PathBuf>,
    fence: u64,
    initialized: bool,
}
pub struct Preparation {
    discovery: discovery::Discovery,
    fence: u64,
    started: std::time::Instant,
}
impl Workspace {
    pub fn update_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }
    pub fn open(
        entry: PathBuf,
        cache: &Path,
        policy: Policy,
        assemblies: Arc<Store>,
        monitor: Arc<crate::watch::Monitor>,
    ) -> Result<Self> {
        let key = blake3::hash(&postcard::to_allocvec(&(
            &entry,
            policy.unity_platform,
            policy.remote.is_some(),
        ))?)
        .to_hex()
        .to_string();
        let store = Arc::new(Store::open(&cache.join(key))?);
        let manifest = store.get_manifest()?.unwrap_or_default();
        Ok(Self {
            entry,
            store,
            assemblies,
            manifest: Arc::new(manifest),
            policy,
            cache: cache.to_owned(),
            builds: 0,
            monitor,
            directories: BTreeSet::new(),
            fence: 0,
            initialized: false,
        })
    }
    pub fn refresh(&mut self) -> Result<()> {
        if let Some(prepared) = self.prepare()? {
            self.apply(prepared)?;
        }
        Ok(())
    }

    /// Managed materialization bypasses watcher timing, while still comparing input stamps.
    pub fn materialized(&mut self) {
        self.initialized = false;
    }

    /// Dependency preparation does not consume an indexing worker.
    pub fn prepare(&mut self) -> Result<Option<Preparation>> {
        if !self.initialized {
            for p in self
                .manifest
                .files
                .values()
                .map(|f| &f.path)
                .chain(self.manifest.metadata.keys())
            {
                if let Some(dir) = if p.is_dir() {
                    Some(p.as_path())
                } else {
                    p.parent()
                } && self.directories.insert(dir.into())
                {
                    self.monitor.register(dir);
                }
            }
        }
        let (dirty, fence) = self.monitor.fence(&self.directories, self.fence);
        if self.initialized && !dirty {
            return Ok(None);
        }
        let metadata_changed = !self.store.manifest_current()?
            || self.manifest.discovery_policy != self.policy.identity()?
            || self.manifest.projects.is_empty()
            || self
                .manifest
                .metadata
                .iter()
                .any(|(p, s)| Stamp::read(p).as_ref().ok() != Some(s));
        let sources_changed = self
            .manifest
            .files
            .values()
            .any(|f| Stamp::read(&f.path).as_ref().ok() != Some(&f.stamp));
        if !metadata_changed && !sources_changed {
            self.fence = fence;
            self.initialized = true;
            return Ok(None);
        }
        let start = std::time::Instant::now();
        let discovery = if metadata_changed {
            discovery::discover_cached(&self.entry, &self.policy, &self.cache)?
        } else {
            discovery::Discovery {
                root: self.manifest.root.clone(),
                projects: self.manifest.projects.clone(),
                sources: self.manifest.inputs.clone(),
                metadata: self.manifest.metadata.keys().cloned().collect(),
                dependencies: self.manifest.dependencies.clone(),
                diagnostics: self.manifest.diagnostics.clone(),
            }
        };
        Ok(Some(Preparation {
            discovery,
            fence,
            started: start,
        }))
    }

    pub fn apply(&mut self, prepared: Preparation) -> Result<()> {
        let Preparation {
            discovery,
            fence,
            started: start,
        } = prepared;
        let mut directories = BTreeSet::new();
        let mut manifest = Manifest {
            discovery_policy: self.policy.identity()?,
            environment: [0; 32],
            root: discovery.root,
            projects: discovery.projects,
            files: BTreeMap::new(),
            metadata: BTreeMap::new(),
            inputs: discovery.sources.clone(),
            dependencies: discovery.dependencies,
            diagnostics: discovery.diagnostics,
        };
        for p in discovery.metadata {
            if let Some(dir) = if p.is_dir() {
                Some(p.as_path())
            } else {
                p.parent()
            } {
                directories.insert(dir.to_owned());
                if self.directories.insert(dir.into()) {
                    self.monitor.register(dir);
                }
            }
            match Stamp::read(&p) {
                Ok(stamp) => {
                    manifest.metadata.insert(p, stamp);
                }
                Err(error) => manifest.diagnostics.push(format!(
                    "Cannot read project input {}: {error}",
                    p.display()
                )),
            }
        }
        let mut queue: VecDeque<_> = discovery.sources.into();
        for (project, p) in manifest.projects.iter().enumerate() {
            for assembly in &p.assemblies {
                if !assembly.path.is_file() {
                    manifest.diagnostics.push(format!("Reference is unavailable: {}. References to this assembly remain unresolved.", assembly.path.display()));
                }
                if assembly.path.is_file() {
                    queue.push_back(SourceInput {
                        path: if manifest.dependencies.contains(&assembly.path) {
                            assembly.path.clone()
                        } else {
                            self.policy.canonical(&assembly.path)?
                        },
                        project,
                        module: String::new(),
                        language: Language::CSharp,
                        metadata: true,
                    });
                }
            }
        }
        let mut visited = BTreeSet::new();
        let mut parsed = 0;
        while let Some(input) = queue.pop_front() {
            if let Some(dir) = input.path.parent() {
                directories.insert(dir.to_owned());
                if self.directories.insert(dir.into()) {
                    self.monitor.register(dir);
                }
            }
            if !visited.insert((input.path.clone(), input.project, input.module.clone())) {
                continue;
            }
            if visited.len() >= 1_000_000 {
                manifest.diagnostics.push(
                    "Workspace source limit reached; remaining files were not analyzed.".into(),
                );
                break;
            }
            let project = &manifest.projects[input.project];
            let profile = if input.metadata {
                String::new()
            } else if input.language == Language::CSharp {
                project.defines.join(";")
            } else {
                format!("rust-2:{}", project.edition)
            };
            let stamp = match Stamp::read(&input.path) {
                Ok(stamp) => stamp,
                Err(error) => {
                    manifest
                        .diagnostics
                        .push(format!("Skipped {}: {error}", input.path.display()));
                    continue;
                }
            };
            // Assembly records are immutable. Another workspace may publish a newer
            // revision without invalidating this workspace's pinned catalog.
            let revision = if input.metadata {
                format!("{stamp:?}")
            } else {
                String::new()
            };
            let key =
                blake3::hash(format!("{}\0{profile}\0{revision}", input.path.display()).as_bytes())
                    .to_hex()
                    .to_string();
            if let Some(f) = manifest.files.get_mut(&key) {
                f.memberships.push(Membership {
                    project: input.project,
                    module: input.module,
                });
                continue;
            }
            if !input.metadata && stamp.size as usize > MAX_SOURCE_BYTES {
                manifest.diagnostics.push(format!(
                    "Skipped source exceeding {} MiB: {}",
                    MAX_SOURCE_BYTES / 1024 / 1024,
                    input.path.display()
                ));
                continue;
            }
            let store = if input.metadata {
                &self.assemblies
            } else {
                &self.store
            };
            let admission = crate::memory::admit_file(stamp.size, input.metadata);
            let extracted = store.ensure_revision(&key, &stamp, || {
                let data = if input.metadata {
                    crate::metadata::file_data(&input.path).with_context(|| {
                        format!(
                            "Cannot extract metadata {}",
                            crate::render::inline(&input.path.to_string_lossy())
                        )
                    })?
                } else {
                    let source = read_stable(&input.path, input.language)?;
                    let facts = crate::extract::extract(
                        &source,
                        input.language,
                        &project.defines,
                        &project.edition,
                    )
                    .with_context(|| {
                        format!(
                            "Cannot extract {}",
                            crate::render::inline(&input.path.to_string_lossy())
                        )
                    })?;
                    FileData {
                        source,
                        facts,
                        assembly: None,
                    }
                };
                ensure!(
                    Stamp::read(&input.path)? == stamp,
                    "File changed during extraction; retry query"
                );
                Ok(data)
            });
            drop(admission);
            let (changed, modules) = match extracted {
                Ok(value) => value,
                Err(error) => {
                    manifest
                        .diagnostics
                        .push(format!("Skipped {}: {error:#}", input.path.display()));
                    continue;
                }
            };
            parsed += usize::from(changed);
            if input.language == Language::Rust {
                let base = input.path.parent().unwrap();
                let stem = input.path.file_stem().unwrap().to_string_lossy();
                let module_base = if matches!(stem.as_ref(), "lib" | "main" | "mod") {
                    base.to_owned()
                } else {
                    base.join(stem.as_ref())
                };
                for m in &modules {
                    let mut dir = module_base.clone();
                    for part in &m.inline {
                        dir.push(part);
                    }
                    let path = if let Some(path) = &m.path {
                        if m.inline.is_empty() {
                            base.join(path)
                        } else {
                            dir.join(path)
                        }
                    } else {
                        let p = dir.join(format!("{}.rs", m.name));
                        if p.exists() {
                            p
                        } else {
                            dir.join(&m.name).join("mod.rs")
                        }
                    };
                    if !path.exists() {
                        // watch the nearest existing parent so later module creation
                        // triggers discovery, even when its directory is created first.
                        if let Some(parent) = path.ancestors().skip(1).find(|p| p.is_dir()) {
                            let parent = self.policy.canonical(parent)?;
                            directories.insert(parent.clone());
                            if self.directories.insert(parent.clone()) {
                                self.monitor.register(&parent);
                            }
                        }
                        continue;
                    } // cfg/build-generated module files may not exist without a build.
                    let path = match self.policy.canonical(&path) {
                        Ok(path) => path,
                        Err(error) => {
                            manifest
                                .diagnostics
                                .push(format!("Skipped module: {error:#}"));
                            continue;
                        }
                    };
                    let module = std::iter::once(input.module.as_str())
                        .chain(m.inline.iter().map(String::as_str))
                        .chain(std::iter::once(m.name.as_str()))
                        .collect::<Vec<_>>()
                        .join("::");
                    queue.push_back(SourceInput {
                        path,
                        project: input.project,
                        module,
                        language: Language::Rust,
                        metadata: false,
                    });
                }
            }
            let display = input
                .path
                .strip_prefix(&manifest.root)
                .unwrap_or(&input.path)
                .to_string_lossy()
                .into_owned();
            manifest.files.insert(
                key,
                FileEntry {
                    path: input.path,
                    display,
                    stamp,
                    language: input.language,
                    memberships: vec![Membership {
                        project: input.project,
                        module: input.module,
                    }],
                    modules,
                    metadata: input.metadata,
                },
            );
        }
        for key in self
            .manifest
            .files
            .keys()
            .filter(|k| !manifest.files.contains_key(*k))
        {
            if !self.manifest.files[key].metadata {
                self.store.remove(key)?;
            }
        }
        for directory in &directories {
            if let Ok(stamp) = Stamp::read(directory) {
                manifest.metadata.insert(directory.clone(), stamp);
            }
        }
        let mut environment = blake3::Hasher::new();
        environment.update(&postcard::to_allocvec(&manifest.projects)?);
        for (key, file) in &manifest.files {
            environment.update(key.as_bytes());
            let store = if file.metadata {
                &self.assemblies
            } else {
                &self.store
            };
            environment.update(&store.declaration_revision(key)?);
        }
        manifest.environment = *environment.finalize().as_bytes();
        self.store.save_manifest(&manifest)?;
        for obsolete in self.directories.difference(&directories) {
            self.monitor.unregister(obsolete);
        }
        self.directories = directories;
        self.manifest = Arc::new(manifest);
        self.fence = fence;
        self.initialized = true;
        self.builds += 1;
        tracing::info!(
            files = self.manifest.files.len(),
            projects = self.manifest.projects.len(),
            parsed,
            elapsed_ms = start.elapsed().as_millis(),
            "workspace refreshed"
        );
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        for path in &self.directories {
            self.monitor.unregister(path);
        }
    }
}

fn read_stable(path: &Path, language: Language) -> Result<String> {
    for _ in 0..2 {
        let before = Stamp::read(path)?;
        let bytes = std::fs::read(path)?;
        ensure!(
            !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1"),
            "Source input is an unavailable Git LFS object: {}",
            path.display()
        );
        let after = Stamp::read(path)?;
        if before == after {
            return tracing::debug_span!("decode_source",path=%path.display())
                .in_scope(|| decode(&bytes, language))
                .with_context(|| {
                    format!(
                        "Cannot decode source {}",
                        crate::render::inline(&path.to_string_lossy())
                    )
                });
        }
    }
    anyhow::bail!(
        "File changed while reading {}; retry the query",
        crate::render::inline(&path.to_string_lossy())
    )
}
