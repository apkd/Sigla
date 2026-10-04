//! Read-only cache reports. The service supplies live state; idle reads hold ownership.
use super::{
    owners::Catalog,
    policy::{self, Disk, Limits},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(crate) const SOCKET: &str = "inspect.sock";
pub(crate) const CONFIG: &str = "cache-policy.json";

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Configuration {
    pub limits: Limits,
    pub repositories: Option<crate::repository::retention::Policy>,
    pub editors: BTreeSet<String>,
}
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub max_bytes: Option<u64>,
    pub headroom_percent: Option<u8>,
}
impl Overrides {
    pub(crate) fn apply(self, mut limits: Limits) -> Result<Limits> {
        if let Some(bytes) = self.max_bytes {
            ensure!(bytes > 0, "Cache size must be positive");
            limits.max_bytes = bytes;
        }
        if let Some(percent) = self.headroom_percent {
            ensure!(percent <= 98, "Headroom must be at most 98%");
            limits.headroom_percent = percent;
        }
        Ok(limits)
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct Footprint {
    pub allocated_bytes: u64,
    pub hardlink_savings_bytes: u64,
    pub unreferenced_blob_bytes: u64,
    /// Shared inode costs are divided between their top-level directories.
    pub categories: BTreeMap<String, u64>,
}
#[derive(Serialize, Deserialize)]
pub struct Workspace {
    pub entry: PathBuf,
    pub repository: Option<String>,
    pub attributed_bytes: u64,
    pub last_used_ms: u64,
    pub priority: f64,
    pub protection: Option<String>,
    pub dependencies: BTreeSet<PathBuf>,
    pub action: String,
}
#[derive(Serialize, Deserialize)]
pub struct Dependency {
    pub path: PathBuf,
    pub retained_by: Vec<PathBuf>,
    pub pinned_by: Vec<PathBuf>,
    pub configured: bool,
}
#[derive(Serialize, Deserialize)]
pub struct Preview {
    pub reclaim_target_bytes: u64,
    pub cleanup_estimate_bytes: u64,
    pub eviction_estimate_bytes: u64,
    pub shortfall_estimate_bytes: u64,
    pub eviction_order: Vec<PathBuf>,
}
#[derive(Serialize, Deserialize)]
pub struct Report {
    pub cache: PathBuf,
    pub live_service: bool,
    pub measured_at_ms: u64,
    pub limits: Limits,
    pub disk: Disk,
    pub footprint: Footprint,
    pub analysis: Option<super::compaction::Stats>,
    pub workspaces: Vec<Workspace>,
    pub dependencies: Vec<Dependency>,
    pub preview: Preview,
    pub notes: Vec<String>,
}

pub(crate) struct Analysis {
    stats: super::compaction::Stats,
    unknown: Vec<PathBuf>,
}

pub(crate) fn seed_repositories(cache: &Path, owners: &mut Catalog) -> Result<()> {
    for (root, state) in crate::repository::retention::cached(cache)? {
        owners.seed(&root.join("source"), Some(&root), state.last_use);
    }
    Ok(())
}
impl Analysis {
    pub fn read(
        owners: &mut Catalog,
        read: impl FnOnce(
            &mut crate::store::shared::InspectionVisitor<'_>,
        ) -> Result<super::compaction::Stats>,
    ) -> Result<Self> {
        let mut unknown = Vec::new();
        let stats = read(&mut |info, bytes| {
            if info.phase == crate::store::shared::Phase::Deleting {
                return Ok(());
            }
            if let Some(manifest) = bytes
                .map(crate::store::inspect_manifest)
                .transpose()?
                .flatten()
            {
                if manifest.source_group.is_none() {
                    owners.observe(&info.entry, info.owner.as_deref(), &manifest, None);
                }
            } else {
                unknown.push(info.entry.clone());
            }
            Ok(())
        })?;
        Ok(Self { stats, unknown })
    }
}

pub(crate) fn footprint(cache: &Path) -> Result<Footprint> {
    struct Inode {
        bytes: u64,
        categories: BTreeSet<String>,
        views: u64,
        blob: bool,
        links: u64,
        regular: bool,
    }
    fn visit(path: &Path, cache: &Path, inodes: &mut BTreeMap<(u64, u64), Inode>) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let category = path
            .strip_prefix(cache)?
            .components()
            .next()
            .map_or("other".into(), |part| {
                part.as_os_str().to_string_lossy().into_owned()
            });
        let blob = category == "blobs"
            && metadata.is_file()
            && path.file_name().is_some_and(|name| name.len() == 62);
        let inode = inodes
            .entry((metadata.dev(), metadata.ino()))
            .or_insert_with(|| Inode {
                bytes: metadata.blocks() * 512,
                categories: BTreeSet::new(),
                views: 0,
                blob: false,
                links: metadata.nlink(),
                regular: metadata.is_file(),
            });
        inode.categories.insert(category);
        inode.blob |= blob;
        inode.views += u64::from(metadata.is_file() && !blob);
        if metadata.is_dir() {
            let entries = match fs::read_dir(path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && path.ends_with("work") =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                visit(&entry?.path(), cache, inodes)?;
            }
        }
        Ok(())
    }
    let mut inodes = BTreeMap::new();
    visit(cache, cache, &mut inodes)?;
    let mut result = Footprint::default();
    for inode in inodes.values() {
        result.allocated_bytes += inode.bytes;
        if inode.regular {
            result.hardlink_savings_bytes += inode.bytes * inode.views.saturating_sub(1);
        }
        if inode.blob && inode.links == 1 {
            result.unreferenced_blob_bytes += inode.bytes;
        }
        let count = inode.categories.len() as u64;
        for (i, category) in inode.categories.iter().enumerate() {
            *result.categories.entry(category.clone()).or_default() +=
                inode.bytes / count + u64::from((i as u64) < inode.bytes % count);
        }
    }
    Ok(result)
}

pub(crate) fn build(
    cache: &Path,
    config: Configuration,
    overrides: Overrides,
    mut owners: Catalog,
    active: BTreeSet<PathBuf>,
    analysis: Option<Analysis>,
    live_service: bool,
) -> Result<Report> {
    let now = super::now();
    let limits = overrides.apply(config.limits)?;
    let states = crate::repository::retention::cached(cache)?;
    for (root, state) in &states {
        owners.seed(&root.join("source"), Some(root), state.last_use);
    }
    let mut notes = vec!["Eviction uses the maintenance policy's age limits and frequency/recency ranking. Byte estimates are approximate: maintenance collects shared objects and rebuilds Git pools, then remeasures before evicting more.".into()];
    if let Some(analysis) = &analysis {
        for entry in &analysis.unknown {
            notes.push(format!(
                "Dependencies are unknown for {}; shared cleanup may be deferred.",
                entry.display()
            ));
        }
    }
    owners
        .entries
        .retain(|_, owner| owner.repository.as_deref().unwrap_or(&owner.entry).exists());
    let candidates = policy::candidates(cache, &owners, config.repositories.as_ref(), now)?;
    let footprint = footprint(cache)?;
    let disk = Disk::read(cache)?;
    let analysis = analysis.map(|a| a.stats);
    let reclaim_target_bytes = limits.reclaim(footprint.allocated_bytes, disk);
    let cleanup = footprint.unreferenced_blob_bytes
        + analysis
            .filter(|s| {
                s.eligible()
                    && disk.available
                        >= s.live
                            .saturating_add(s.live / 10)
                            .saturating_add(64 * 1024 * 1024)
            })
            .map_or(0, |s| s.reclaimable());
    let mut workspaces: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            let protection = if active.contains(&candidate.owner.entry) {
                Some(
                    candidate
                        .protection
                        .as_ref()
                        .map_or("active job or request".into(), |reason| {
                            format!("{reason}; active job or request")
                        }),
                )
            } else {
                candidate.protection.clone()
            };
            Workspace {
                entry: candidate.owner.entry.clone(),
                repository: candidate
                    .owner
                    .repository
                    .as_ref()
                    .and_then(|root| states.get(root))
                    .map(|s| format!("{}#{}", s.repository, s.branch)),
                attributed_bytes: candidate.bytes,
                last_used_ms: candidate.owner.usage.last_use,
                priority: candidate.owner.usage.priority(now, candidate.bytes),
                protection,
                dependencies: candidate
                    .owner
                    .dependencies
                    .iter()
                    .filter(|p| p.exists())
                    .cloned()
                    .collect(),
                action: "keep".into(),
            }
        })
        .collect();
    let mut eviction_order = Vec::new();
    let mut eviction_estimate_bytes = 0;
    for (i, candidate) in candidates.iter().enumerate() {
        if candidate.expired && workspaces[i].protection.is_none() {
            workspaces[i].action = "evict: age limit".into();
            eviction_order.push(candidate.owner.entry.clone());
            eviction_estimate_bytes += candidate.bytes;
        }
    }
    let freed = cleanup
        .saturating_add(eviction_estimate_bytes)
        .min(footprint.allocated_bytes);
    let mut remaining = limits.reclaim(
        footprint.allocated_bytes - freed,
        Disk {
            available: disk.available.saturating_add(freed),
            ..disk
        },
    );
    for (i, candidate) in candidates.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        if !candidate.expired && workspaces[i].protection.is_none() {
            workspaces[i].action = "evict: disk pressure".into();
            eviction_order.push(candidate.owner.entry.clone());
            eviction_estimate_bytes += candidate.bytes;
            remaining = remaining.saturating_sub(candidate.bytes);
        }
    }
    let mut dependencies: BTreeMap<PathBuf, Dependency> = BTreeMap::new();
    for workspace in &workspaces {
        for path in &workspace.dependencies {
            let dependency = dependencies
                .entry(path.clone())
                .or_insert_with(|| Dependency {
                    path: path.clone(),
                    retained_by: Vec::new(),
                    pinned_by: Vec::new(),
                    configured: false,
                });
            dependency.retained_by.push(workspace.entry.clone());
            if workspace.protection.is_some() {
                dependency.pinned_by.push(workspace.entry.clone());
            }
        }
    }
    for editor in config.editors {
        let path = cache.join("editors").join(editor);
        if path.exists() {
            dependencies
                .entry(path.clone())
                .or_insert_with(|| Dependency {
                    path,
                    retained_by: Vec::new(),
                    pinned_by: Vec::new(),
                    configured: false,
                })
                .configured = true;
        }
    }
    if live_service {
        notes.push("Live usage can change while filesystem sizes are measured. Active work is protected in this snapshot.".into());
    }
    Ok(Report {
        cache: cache.into(),
        live_service,
        measured_at_ms: now,
        limits,
        disk,
        footprint,
        analysis,
        workspaces,
        dependencies: dependencies.into_values().collect(),
        preview: Preview {
            reclaim_target_bytes,
            cleanup_estimate_bytes: cleanup,
            eviction_estimate_bytes,
            shortfall_estimate_bytes: remaining,
            eviction_order,
        },
        notes,
    })
}

pub async fn inspect(cache: &Path, overrides: Overrides) -> Result<Report> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cache = cache
        .canonicalize()
        .context("Cache directory does not exist")?;
    let ownership = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(cache.join("ownership.lock"))
        .context("No Sigla cache ownership file found")?;
    match ownership.try_lock() {
        Ok(()) => (),
        Err(fs::TryLockError::WouldBlock) => {
            let mut stream = tokio::net::UnixStream::connect(cache.join(SOCKET)).await
                .context("The running service has no inspection socket; deploy the new version or stop it before inspection")?;
            stream.write_all(&serde_json::to_vec(&overrides)?).await?;
            stream.shutdown().await?;
            let mut bytes = Vec::new();
            stream
                .take(64 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .await?;
            let reply: std::result::Result<Report, String> = serde_json::from_slice(&bytes)?;
            return reply.map_err(anyhow::Error::msg);
        }
        Err(fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    tokio::task::spawn_blocking(move || {
        let _ownership = super::Ownership(ownership);
        let config = match fs::read(cache.join(CONFIG)) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let unknown = config.is_none();
        let mut owners = Catalog::open(&cache)?;
        seed_repositories(&cache, &mut owners)?;
        let analysis = cache.join("analysis/data.mdb").is_file()
            .then(|| Analysis::read(&mut owners, |visit| crate::store::shared::inspect_idle(&cache.join("analysis"), visit))).transpose()?;
        let mut report = build(&cache, config.unwrap_or_default(), overrides, owners, BTreeSet::new(), analysis, false)?;
        if unknown {
            report.notes.push("No saved service configuration. Default limits are shown; repositories are conservatively protected. Start the updated service to save its policy.".into());
        }
        Ok(report)
    }).await?
}

impl Report {
    pub fn render(&self) -> String {
        fn size(bytes: u64) -> String {
            let mut value = bytes as f64;
            let mut unit = "B";
            for next in ["KiB", "MiB", "GiB", "TiB"] {
                if value < 1024.0 {
                    break;
                }
                value /= 1024.0;
                unit = next;
            }
            format!("{value:.2} {unit}")
        }
        let mut text = format!(
            "Cache: {} ({})\nPhysical disk: {}\nHardlink savings: {}\nFilesystem available: {} / {}\nLimits: {:.2} GB; {}% free headroom\n",
            self.cache.display(),
            if self.live_service {
                "live service"
            } else {
                "idle cache"
            },
            size(self.footprint.allocated_bytes),
            size(self.footprint.hardlink_savings_bytes),
            size(self.disk.available),
            size(self.disk.capacity),
            self.limits.max_bytes as f64 / 1_000_000_000.0,
            self.limits.headroom_percent
        );
        text.push_str("\nDisk by category (shared inodes divided between categories):\n");
        for (name, bytes) in &self.footprint.categories {
            text.push_str(&format!("  {:>12}  {name}\n", size(*bytes)));
        }
        if let Some(analysis) = self.analysis {
            text.push_str(&format!(
                "\nLMDB: {} file; {} live pages; {} reusable space; compaction {}\n",
                size(analysis.allocated),
                size(analysis.live),
                size(analysis.reclaimable()),
                if analysis.eligible() {
                    "eligible when temporary space permits"
                } else {
                    "below reclamation threshold"
                }
            ));
        }
        text.push_str("\nWorkspaces (least retention priority first):\n");
        for workspace in &self.workspaces {
            let label = workspace
                .repository
                .as_deref()
                .map(str::to_owned)
                .unwrap_or_else(|| workspace.entry.display().to_string());
            let status = workspace.protection.as_deref().unwrap_or(&workspace.action);
            text.push_str(&format!(
                "  {:>12}  last used {:.1} days ago  {label}\n                {status}\n",
                size(workspace.attributed_bytes),
                self.measured_at_ms.saturating_sub(workspace.last_used_ms) as f64 / 86_400_000.0
            ));
        }
        text.push_str("\nShared dependencies:\n");
        for dependency in &self.dependencies {
            text.push_str(&format!(
                "  {}: {} owners, {} protected{}\n",
                dependency.path.display(),
                dependency.retained_by.len(),
                dependency.pinned_by.len(),
                if dependency.configured {
                    "; configured editor"
                } else {
                    ""
                }
            ));
        }
        text.push_str(&format!("\nEviction preview: target {}; estimated cleanup {}; workspace share {}; remaining shortfall {}\n",
            size(self.preview.reclaim_target_bytes), size(self.preview.cleanup_estimate_bytes),
            size(self.preview.eviction_estimate_bytes), size(self.preview.shortfall_estimate_bytes)));
        for (i, path) in self.preview.eviction_order.iter().enumerate() {
            text.push_str(&format!("  {}. {}\n", i + 1, path.display()));
        }
        for note in &self.notes {
            text.push_str(&format!("\n{note}\n"));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_accounting_counts_hardlinks_once_and_excludes_the_blob_anchor_from_savings() {
        let cache = tempfile::tempdir().unwrap();
        let store = super::super::blobs::Store::open(cache.path()).unwrap();
        let first = cache.path().join("first");
        let second = cache.path().join("second");
        fs::write(&first, vec![42; 8192]).unwrap();
        {
            let _lease = store.lease().unwrap();
            store.import(&first).unwrap();
        }
        let before = footprint(cache.path()).unwrap();
        fs::hard_link(&first, &second).unwrap();
        let after = footprint(cache.path()).unwrap();
        let allocation = fs::metadata(&first).unwrap().blocks() * 512;
        assert_eq!(before.hardlink_savings_bytes, 0);
        assert_eq!(after.hardlink_savings_bytes, allocation);
        assert_eq!(
            after.allocated_bytes,
            super::super::blobs::allocated(cache.path()).unwrap()
        );
        assert_eq!(
            after.categories.values().sum::<u64>(),
            after.allocated_bytes
        );
        assert_eq!(after.unreferenced_blob_bytes, 0);
        fs::remove_file(&first).unwrap();
        fs::remove_file(&second).unwrap();
        assert_eq!(
            footprint(cache.path()).unwrap().unreferenced_blob_bytes,
            allocation
        );
    }
}
