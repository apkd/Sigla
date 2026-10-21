//! Completed asset generations. Objects are stored once; composed instances
//! refer back to their definitions instead of serializing the same graph again.
use super::{Asset, Index, ScriptType, compose, native};
use crate::workspace::Stamp;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Serialize, Deserialize)]
struct Instance {
    id: String,
    definition: String,
    object: i64,
    name: String,
    ty: Option<Arc<ScriptType>>,
    native_issue: Option<Arc<str>>,
    references: BTreeMap<String, compose::Target>,
    evidence: Vec<(String, i64)>,
    alive: bool,
}
#[derive(Serialize, Deserialize)]
struct Composed {
    objects: Vec<Instance>,
    aliases: BTreeMap<i64, String>,
    incomplete: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Cached {
    version: String,
    generation: [u8; 32],
    assets: BTreeMap<String, Asset>,
    scripts: BTreeMap<String, Arc<ScriptType>>,
    instances: BTreeMap<String, Composed>,
    native_types: BTreeMap<String, BTreeMap<i32, native::Resolved>>,
    observed: BTreeMap<PathBuf, Stamp>,
}
fn path(cache: &Path) -> PathBuf {
    cache.join("completed.zst")
}

fn intern<T: ?Sized + Eq + std::hash::Hash>(values: &mut HashSet<Arc<T>>, value: Arc<T>) -> Arc<T> {
    if let Some(shared) = values.get(&value) {
        shared.clone()
    } else {
        values.insert(value.clone());
        value
    }
}

pub(super) fn load(cache: &Path, generation: [u8; 32]) -> Option<Index> {
    let result = (|| -> Result<_> {
        let file = std::fs::File::open(path(cache))?;
        let mut bytes = Vec::new();
        zstd::stream::read::Decoder::new(file)?
            .take(512 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 512 * 1024 * 1024,
            "Asset snapshot exceeds size limit"
        );
        let cached: Cached = postcard::from_bytes(&bytes)?;
        ensure!(
            cached.version == env!("SIGLA_ASSET_FINGERPRINT") && cached.generation == generation,
            "Asset generation changed"
        );
        let mut index = Index {
            assets: cached.assets,
            scripts: cached.scripts,
            native_types: cached.native_types,
            observed: cached.observed,
            ..Default::default()
        };
        // Serialization preserves values, not Arc identities. Restore sharing
        // before publishing the graph, including script and native catalogs.
        let mut types = HashSet::new();
        let mut issues = HashSet::new();
        for ty in index.scripts.values_mut() {
            *ty = intern(&mut types, ty.clone());
        }
        for native in index
            .native_types
            .values_mut()
            .flat_map(|types| types.values_mut())
        {
            native.ty = native.ty.take().map(|ty| intern(&mut types, ty));
            native.issue = native.issue.take().map(|issue| intern(&mut issues, issue));
        }
        ensure!(index.current(), "Asset inputs changed");
        ensure!(
            index.assets.values().all(|asset| asset
                .content
                .as_deref()
                .is_none_or(super::records::available)),
            "Asset records are unavailable"
        );
        let objects: BTreeMap<_, _> = index
            .assets
            .iter()
            .flat_map(|(key, asset)| {
                asset
                    .objects
                    .iter()
                    .map(move |object| ((key.as_str(), object.id), object))
            })
            .collect();
        let diagnostics = tracing::enabled!(tracing::Level::DEBUG);
        let mut null_paths = 0;
        let mut null_path_capacity = 0;
        let mut null_path_estimated_bytes = 0;
        let empty_path_size = crate::memory::encoded_size(&"");
        for (key, composed) in cached.instances {
            let instances = composed
                .objects
                .into_iter()
                .map(|mut instance| -> Result<_> {
                    // Older snapshots retained destination paths for null
                    // pointers. Keep their raw evidence, but no destination.
                    for target in instance
                        .references
                        .values_mut()
                        .filter(|target| target.raw.null())
                    {
                        let path = std::mem::take(&mut target.asset);
                        if diagnostics && !path.is_empty() {
                            null_paths += 1;
                            null_path_capacity += path.capacity();
                            null_path_estimated_bytes +=
                                (crate::memory::encoded_size(&path) - empty_path_size) * 3;
                        }
                    }
                    Ok(compose::Instance {
                        object: Arc::clone(
                            objects
                                .get(&(instance.definition.as_str(), instance.object))
                                .context("Missing instance definition")?,
                        ),
                        id: instance.id,
                        definition: instance.definition,
                        name: instance.name,
                        ty: instance.ty.map(|ty| intern(&mut types, ty)),
                        native_issue: instance
                            .native_issue
                            .map(|issue| intern(&mut issues, issue)),
                        references: instance.references,
                        evidence: instance.evidence,
                        alive: instance.alive,
                    })
                })
                .collect::<Result<_>>()?;
            index.instances.insert(
                key,
                Arc::new(compose::Composed {
                    objects: instances,
                    aliases: composed.aliases,
                    incomplete: composed.incomplete,
                }),
            );
        }
        tracing::debug!(
            null_paths,
            null_path_capacity,
            null_path_estimated_bytes,
            "Discarded saved null asset destinations"
        );
        for (key, asset) in &index.assets {
            if !asset.guid.is_empty() {
                index
                    .guids
                    .entry((asset.project.clone(), asset.guid.clone()))
                    .or_default()
                    .push(key.clone());
            }
        }
        index.measure();
        Ok(index)
    })();
    result.ok()
}

pub(super) struct Pending {
    file: tempfile::NamedTempFile,
    destination: PathBuf,
}
impl Pending {
    pub(super) fn publish(self) -> Result<()> {
        self.file.persist(self.destination)?;
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn save(cache: &Path, generation: [u8; 32], index: &Index) -> Result<()> {
    stage(cache, generation, index)?.publish()
}

pub(super) fn stage(cache: &Path, generation: [u8; 32], index: &Index) -> Result<Pending> {
    let instances = index
        .instances
        .iter()
        .map(|(key, composed)| {
            (
                key.clone(),
                Composed {
                    objects: composed
                        .objects
                        .iter()
                        .map(|instance| Instance {
                            id: instance.id.clone(),
                            definition: instance.definition.clone(),
                            object: instance.object.id,
                            name: instance.name.clone(),
                            ty: instance.ty.clone(),
                            native_issue: instance.native_issue.clone(),
                            references: instance.references.clone(),
                            evidence: instance.evidence.clone(),
                            alive: instance.alive,
                        })
                        .collect(),
                    aliases: composed.aliases.clone(),
                    incomplete: composed.incomplete.clone(),
                },
            )
        })
        .collect();
    let cached = Cached {
        version: env!("SIGLA_ASSET_FINGERPRINT").into(),
        generation,
        assets: index.assets.clone(),
        scripts: index.scripts.clone(),
        instances,
        native_types: index.native_types.clone(),
        observed: index.observed.clone(),
    };
    std::fs::create_dir_all(cache)?;
    let mut file = tempfile::NamedTempFile::new_in(cache)?;
    let mut encoder = zstd::stream::write::Encoder::new(&mut file, 3)?;
    encoder.include_checksum(true)?;
    postcard::to_io(&cached, &mut encoder)?;
    encoder.finish()?;
    Ok(Pending {
        file,
        destination: path(cache),
    })
}
