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
    pub(crate) size: u64,
    revision: Revision,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Revision {
    File {
        modified: u128,
        changed: i64,
        inode: u64,
    },
    Immutable(crate::cache::blobs::Id),
}
impl Stamp {
    pub fn read(path: &Path) -> Result<Self> {
        let m = std::fs::metadata(path)?;
        if let Some(id) = crate::cache::blobs::identity(path, &m)? {
            return Ok(Self {
                size: m.len(),
                revision: Revision::Immutable(id),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                size: m.len(),
                revision: Revision::File {
                    modified: m
                        .modified()?
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos(),
                    changed: m.ctime_nsec() ^ m.ctime(),
                    inode: m.ino(),
                },
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                size: m.len(),
                revision: Revision::File {
                    modified: 0,
                    changed: 0,
                    inode: 0,
                },
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
impl FileEntry {
    pub(crate) fn read_source(&self) -> Result<String> {
        let _admission = crate::memory::admit_file(self.stamp.size, false);
        ensure!(
            Stamp::read(&self.path)? == self.stamp,
            "Source changed; retry query"
        );
        let source = read_stable(&self.path, self.language)?;
        ensure!(
            Stamp::read(&self.path)? == self.stamp,
            "Source changed; retry query"
        );
        Ok(source)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub source_group: Option<crate::native::Group>,
    pub discovery_policy: [u8; 32],
    pub environment: [u8; 32],
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub files: BTreeMap<String, FileEntry>,
    /// Selected sources whose facts are published by the native/shader workers.
    pub deferred: BTreeMap<String, FileEntry>,
    pub metadata: BTreeMap<PathBuf, Stamp>,
    pub inputs: Vec<SourceInput>,
    pub dependencies: BTreeSet<PathBuf>,
    pub diagnostics: Vec<String>,
}

impl Manifest {
    pub(crate) fn sources_current(&self) -> bool {
        self.metadata
            .iter()
            .chain(
                self.files
                    .values()
                    .chain(self.deferred.values())
                    .map(|file| (&file.path, &file.stamp)),
            )
            .all(|(path, stamp)| Stamp::read(path).as_ref().ok() == Some(stamp))
    }
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
    _files: Arc<crate::cache::blobs::Store>,
    pub entry: PathBuf,
    pub store: Arc<Store>,
    pub manifest: Arc<Manifest>,
    policy: Policy,
    cache: PathBuf,
    pub builds: usize,
    monitor: Arc<crate::watch::Monitor>,
    directories: BTreeSet<PathBuf>,
    fence: u64,
    initialized: bool,
    materialization: Option<u64>,
}
pub(crate) struct IndexPlan {
    pub manifest: Arc<Manifest>,
    pub watch: crate::watch::Snapshot,
    directories: BTreeSet<PathBuf>,
    fence: u64,
    started: std::time::Instant,
    validation: std::time::Duration,
    discovery_time: std::time::Duration,
}
pub struct Preparation {
    discovery: discovery::Discovery,
    fence: u64,
    started: std::time::Instant,
    validation: std::time::Duration,
    discovery_time: std::time::Duration,
}
impl Workspace {
    pub(crate) fn watch_snapshot(&self) -> crate::watch::Snapshot {
        self.monitor.snapshot(&self.directories, self.fence)
    }
    pub fn update_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }
    pub fn open(
        entry: PathBuf,
        cache: &Path,
        policy: Policy,
        analysis: &Path,
        owner: Option<&Path>,
        monitor: Arc<crate::watch::Monitor>,
    ) -> Result<Self> {
        let key = *blake3::hash(&postcard::to_allocvec(&(
            crate::store::ANALYSIS_VERSION,
            &entry,
            policy.unity_platform,
            policy.remote.is_some(),
        ))?)
        .as_bytes();
        let store = Store::open_workspace(analysis, key, &entry, owner)?;
        let manifest = store.get_manifest()?.unwrap_or_default();
        Ok(Self {
            _files: crate::cache::blobs::Store::open(
                analysis.parent().context("Analysis has no cache root")?,
            )?,
            entry,
            store,
            manifest: Arc::new(manifest),
            policy,
            cache: cache.to_owned(),
            builds: 0,
            monitor,
            directories: BTreeSet::new(),
            fence: 0,
            initialized: false,
            materialization: None,
        })
    }
    pub fn refresh(&mut self) -> Result<()> {
        if let Some(prepared) = self.prepare()? {
            self.apply(prepared)?;
        }
        Ok(())
    }

    /// Managed materialization bypasses watcher timing, while still comparing input stamps.
    pub fn materialized(&mut self, generation: u64) {
        if self.materialization != Some(generation) {
            self.initialized = false;
            self.materialization = Some(generation);
        }
    }

    /// Dependency preparation does not consume an indexing worker.
    pub fn prepare(&mut self) -> Result<Option<Preparation>> {
        let start = std::time::Instant::now();
        if !self.initialized {
            for p in self
                .manifest
                .files
                .values()
                .chain(self.manifest.deferred.values())
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
        if self.initialized && !dirty && self.store.manifest_current()? {
            return Ok(None);
        }
        let cache_current = self.store.manifest_current()?;
        let policy_current = self.manifest.discovery_policy == self.policy.identity()?;
        let changed_metadata = self
            .manifest
            .metadata
            .iter()
            .find(|(p, s)| Stamp::read(p).as_ref().ok() != Some(s))
            .map(|(path, _)| path);
        let changed_source = self
            .manifest
            .files
            .values()
            .chain(self.manifest.deferred.values())
            .find(|f| Stamp::read(&f.path).as_ref().ok() != Some(&f.stamp))
            .map(|file| &file.path);
        let metadata_changed = !cache_current
            || !policy_current
            || self.manifest.projects.is_empty()
            || changed_metadata.is_some();
        let sources_changed = changed_source.is_some();
        if !metadata_changed && !sources_changed {
            self.fence = fence;
            self.initialized = true;
            tracing::debug!(workspace = %self.entry.display(), elapsed_ms = start.elapsed().as_millis(), "workspace cache validated");
            return Ok(None);
        }
        let validation = start.elapsed();
        let discovery_started = std::time::Instant::now();
        tracing::debug!(workspace = %self.entry.display(), cache_current, policy_current, ?changed_metadata, ?changed_source, "workspace refresh required");
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
            validation,
            discovery_time: discovery_started.elapsed(),
        }))
    }

    pub fn apply(&mut self, prepared: Preparation) -> Result<()> {
        let plan = self.plan(prepared)?;
        self.apply_plan(plan)
    }

    /// Resolve the selected paths before extracting semantic facts.
    pub(crate) fn plan(&mut self, prepared: Preparation) -> Result<IndexPlan> {
        let Preparation {
            discovery,
            fence,
            started: start,
            validation,
            discovery_time,
        } = prepared;
        let mut directories = BTreeSet::new();
        let mut manifest = Manifest {
            source_group: None,
            discovery_policy: self.policy.identity()?,
            environment: [0; 32],
            root: discovery.root,
            projects: discovery.projects,
            files: BTreeMap::new(),
            deferred: BTreeMap::new(),
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
                    let path = if manifest.dependencies.contains(&assembly.path) {
                        assembly.path.clone()
                    } else {
                        match self.policy.canonical(&assembly.path) {
                            Ok(path) => path,
                            Err(error) => {
                                manifest.diagnostics.push(format!(
                                    "Excluded reference {}: {error:#}",
                                    assembly.path.display()
                                ));
                                continue;
                            }
                        }
                    };
                    queue.push_back(SourceInput {
                        path,
                        project,
                        module: String::new(),
                        language: Language::CSharp,
                        metadata: true,
                    });
                }
            }
        }
        let mut visited = BTreeSet::new();
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
            let stamp = match Stamp::read(&input.path) {
                Ok(stamp) => stamp,
                Err(error) => {
                    manifest
                        .diagnostics
                        .push(format!("Skipped {}: {error}", input.path.display()));
                    continue;
                }
            };
            let key = input_key(&input, project, &stamp);
            let files = if input.language.native() {
                &mut manifest.deferred
            } else {
                &mut manifest.files
            };
            if let Some(file) = files.get_mut(&key) {
                file.memberships.push(Membership {
                    project: input.project,
                    module: input.module,
                });
                continue;
            }
            if !input.metadata && stamp.size as usize > MAX_SOURCE_BYTES {
                manifest.metadata.insert(input.path.clone(), stamp);
                manifest.diagnostics.push(format!(
                    "Skipped source exceeding {} MiB: {}",
                    MAX_SOURCE_BYTES / 1024 / 1024,
                    input.path.display()
                ));
                continue;
            }
            let modules = if input.language == Language::Rust {
                let modules = self.store.current(&key, &stamp).and_then(|cached| {
                    cached.map_or_else(
                        || {
                            Ok(crate::extract::rust_modules(
                                &read_stable(&input.path, input.language)?,
                                &project.edition,
                            ))
                        },
                        Ok,
                    )
                });
                match modules {
                    Ok(modules) => modules,
                    Err(error) => {
                        manifest.metadata.insert(input.path.clone(), stamp);
                        manifest
                            .diagnostics
                            .push(format!("Skipped {}: {error:#}", input.path.display()));
                        continue;
                    }
                }
            } else {
                Vec::new()
            };
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
                            let parent = match self.policy.canonical(parent) {
                                Ok(parent) => parent,
                                Err(error) => {
                                    manifest.diagnostics.push(format!(
                                        "Cannot watch Rust module {}: {error:#}",
                                        path.display()
                                    ));
                                    continue;
                                }
                            };
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
            let file = FileEntry {
                display: input
                    .path
                    .strip_prefix(&manifest.root)
                    .unwrap_or(&input.path)
                    .to_string_lossy()
                    .into_owned(),
                path: input.path,
                stamp,
                language: input.language,
                memberships: vec![Membership {
                    project: input.project,
                    module: input.module,
                }],
                modules,
                metadata: input.metadata,
            };
            if input.language.native() {
                manifest.deferred.insert(key, file);
            } else {
                manifest.files.insert(key, file);
            }
        }
        for directory in &directories {
            if let Ok(stamp) = Stamp::read(directory) {
                manifest.metadata.insert(directory.clone(), stamp);
            }
        }
        Ok(IndexPlan {
            manifest: Arc::new(manifest),
            watch: self.monitor.snapshot(&directories, fence),
            directories,
            fence,
            started: start,
            validation,
            discovery_time,
        })
    }

    pub(crate) fn apply_plan(&mut self, plan: IndexPlan) -> Result<()> {
        self.initialized = false;
        self.store.begin_refresh()?;
        let IndexPlan {
            manifest,
            directories,
            fence,
            started: start,
            validation,
            discovery_time,
            ..
        } = plan;
        let mut manifest = Arc::unwrap_or_clone(manifest);
        let mut parsed = 0;
        let mut objects_reused = 0;
        let mut new_object_bytes = 0u64;
        let mut reused_object_bytes = 0u64;
        let mut extraction_time = std::time::Duration::ZERO;
        let mut storage_time = std::time::Duration::ZERO;
        let indexing_started = std::time::Instant::now();
        let inputs: VecDeque<_> = manifest
            .files
            .values()
            .map(|file| SourceInput {
                path: file.path.clone(),
                project: file.memberships[0].project,
                module: file.memberships[0].module.clone(),
                language: file.language,
                metadata: file.metadata,
            })
            .collect();
        let mut indexed = index_sources(&self.store, &inputs, &manifest.projects)?;
        for input in inputs {
            let project = &manifest.projects[input.project];
            let stamp = Stamp::read(&input.path)?;
            let key = input_key(&input, project, &stamp);
            ensure!(
                manifest.files.get(&key).is_some_and(|f| f.stamp == stamp),
                "File changed during indexing; retry query"
            );
            let extracted = indexed
                .remove(&key)
                .filter(|(before, _)| before == &stamp)
                .map_or_else(
                    || index_input(&self.store, &key, &input, project, &stamp),
                    |(_, result)| result,
                );
            let Indexed {
                changed,
                reused,
                encoded_bytes,
                extraction,
                storage,
                ..
            } = match extracted {
                Ok(value) => value,
                Err(error) => {
                    manifest.files.remove(&key);
                    manifest.metadata.insert(input.path.clone(), stamp);
                    manifest
                        .diagnostics
                        .push(format!("Skipped {}: {error:#}", input.path.display()));
                    continue;
                }
            };
            extraction_time += extraction;
            storage_time += storage;
            parsed += usize::from(changed);
            objects_reused += usize::from(reused);
            if changed {
                new_object_bytes += encoded_bytes;
            }
            if reused {
                reused_object_bytes += encoded_bytes;
            }
        }
        for key in self
            .manifest
            .files
            .keys()
            .filter(|k| !manifest.files.contains_key(*k))
        {
            self.store.remove(key)?;
        }
        let mut environment = blake3::Hasher::new();
        environment.update(&postcard::to_allocvec(&manifest.projects)?);
        for key in manifest.files.keys() {
            environment.update(key.as_bytes());
            environment.update(&self.store.declaration_revision(key)?);
        }
        manifest.environment = *environment.finalize().as_bytes();
        self.store.save_manifest(&manifest)?;
        for obsolete in self.directories.difference(&directories) {
            self.monitor.unregister(obsolete);
        }
        self.directories = directories;
        for diagnostic in &manifest.diagnostics {
            if !self.manifest.diagnostics.contains(diagnostic) {
                tracing::warn!("{diagnostic}");
            }
        }
        self.manifest = Arc::new(manifest);
        self.fence = fence;
        self.initialized = true;
        self.builds += 1;
        if parsed > 0 {
            crate::memory::reclaim_after_indexing();
        }
        tracing::info!(
            workspace = %self.entry.display(),
            files = self.manifest.files.len(),
            projects = self.manifest.projects.len(),
            parsed,
            objects_reused,
            new_object_bytes,
            reused_object_bytes,
            validation_ms = validation.as_millis(),
            discovery_ms = discovery_time.as_millis(),
            indexing_ms = indexing_started.elapsed().as_millis(),
            extraction_worker_ms = extraction_time.as_millis(),
            storage_worker_ms = storage_time.as_millis(),
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

#[cfg(test)]
#[path = "workspace_validation.rs"]
mod validation;

fn input_key(input: &SourceInput, project: &Project, stamp: &Stamp) -> String {
    let profile = if input.metadata {
        String::new()
    } else if input.language.document() {
        "document".into()
    } else if input.language == Language::CSharp {
        crate::store::canonical_defines(&project.defines).join(";")
    } else if input.language.native() {
        format!("native:{:?}", input.language)
    } else {
        format!("rust-2:{}", project.edition)
    };
    // Shared assembly records remain immutable across workspace revisions.
    let revision = if input.metadata {
        format!("{stamp:?}")
    } else {
        String::new()
    };
    blake3::hash(format!("{}\0{profile}\0{revision}", input.path.display()).as_bytes())
        .to_hex()
        .to_string()
}

struct Indexed {
    changed: bool,
    reused: bool,
    encoded_bytes: u64,
    extraction: std::time::Duration,
    storage: std::time::Duration,
}

fn index_input(
    store: &Store,
    key: &str,
    input: &SourceInput,
    project: &Project,
    stamp: &Stamp,
) -> Result<Indexed> {
    let _admission = crate::memory::admit_file(stamp.size, input.metadata);
    let started = std::time::Instant::now();
    let verify = || {
        ensure!(
            Stamp::read(&input.path)? == *stamp,
            "File changed during extraction; retry query"
        );
        Ok(())
    };
    if store.current(key, stamp)?.is_some() {
        verify()?;
        return Ok(Indexed {
            changed: false,
            reused: false,
            encoded_bytes: 0,
            extraction: std::time::Duration::ZERO,
            storage: started.elapsed(),
        });
    }
    let mut extraction = std::time::Duration::ZERO;
    let installed = (|| -> Result<_> {
        if input.metadata {
            verify()?;
            let bytes = std::fs::read(&input.path)?;
            verify()?;
            let stem = input
                .path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let id = crate::store::metadata_id(&bytes, &stem)?;
            store.install(key, stamp, id, verify, || {
                let start = std::time::Instant::now();
                let data = crate::metadata::file_data_bytes(bytes, &stem);
                extraction = start.elapsed();
                data
            })
        } else {
            let source = read_stable(&input.path, input.language)?;
            verify()?;
            let defines = crate::store::canonical_defines(&project.defines);
            let id = crate::store::source_id(&source, input.language, &defines, &project.edition)?;
            store.install(key, stamp, id, verify, || {
                let start = std::time::Instant::now();
                let facts =
                    crate::extract::extract(&source, input.language, &defines, &project.edition);
                extraction = start.elapsed();
                Ok(FileData {
                    source,
                    facts: facts?,
                    assembly: None,
                })
            })
        }
    })()
    .with_context(|| {
        format!(
            "Cannot index {}",
            crate::render::inline(&input.path.to_string_lossy())
        )
    })?;
    // `changed` was only used to count parsing. It now counts genuinely built objects.
    // Installed also provides reused/encoded_bytes for the existing aggregate refresh log.
    Ok(Indexed {
        changed: installed.built,
        reused: installed.reused,
        encoded_bytes: installed.encoded_bytes,
        extraction,
        storage: started.elapsed().saturating_sub(extraction),
    })
}

type IndexedSources = BTreeMap<String, (Stamp, Result<Indexed>)>;

pub(crate) fn index_native(
    store: &Store,
    key: &str,
    file: &FileEntry,
    projects: &[Project],
) -> Result<()> {
    let started = std::time::Instant::now();
    tracing::debug!(path = %file.path.display(), "indexing native source");
    let membership = &file.memberships[0];
    let input = SourceInput {
        path: file.path.clone(),
        project: membership.project,
        module: String::new(),
        language: file.language,
        metadata: false,
    };
    index_input(
        store,
        key,
        &input,
        &projects[membership.project],
        &file.stamp,
    )?;
    tracing::debug!(path = %file.path.display(), elapsed_ms = started.elapsed().as_millis(), "native source indexed");
    Ok(())
}

/// C# discovery already supplies all sources. Extract independent compilation
/// contexts concurrently, then assemble the manifest in discovery order.
fn index_sources(
    store: &Store,
    inputs: &VecDeque<SourceInput>,
    projects: &[Project],
) -> Result<IndexedSources> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut jobs = BTreeMap::new();
    for input in inputs
        .iter()
        .filter(|i| !i.metadata && i.language == Language::CSharp)
        .take(1_000_000)
    {
        let Ok(stamp) = Stamp::read(&input.path) else {
            continue;
        };
        if stamp.size as usize > MAX_SOURCE_BYTES {
            continue;
        }
        let project = &projects[input.project];
        let key = input_key(input, project, &stamp);
        jobs.entry(key).or_insert((stamp, input, project));
    }
    let jobs: Vec<_> = jobs.into_iter().collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min(jobs.len());
    let build = |(key, (stamp, input, project)): &(String, (Stamp, &SourceInput, &Project))| {
        (
            key.clone(),
            (
                stamp.clone(),
                index_input(store, key, input, project, stamp),
            ),
        )
    };
    if workers <= 1 {
        return Ok(jobs.iter().map(build).collect());
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut results = Vec::new();
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        results.push(build(job));
                    }
                    results
                })
            })
            .collect();
        let mut indexed = BTreeMap::new();
        for handle in handles {
            indexed.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("Source indexing worker panicked"))?,
            );
        }
        Ok(indexed)
    })
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
                .in_scope(|| decode_owned(bytes, language))
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
