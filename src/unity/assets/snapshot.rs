//! Completed asset generations. Objects are stored once; composed instances
//! refer back to their definitions instead of serializing the same graph again.
use super::{Asset, Index, ScriptType, compose, native};
use crate::workspace::Stamp;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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
    scripts: BTreeMap<String, ScriptType>,
    instances: BTreeMap<String, Composed>,
    native_types: BTreeMap<String, BTreeMap<i32, native::Resolved>>,
    observed: BTreeMap<PathBuf, Stamp>,
}
fn path(cache: &Path) -> PathBuf {
    cache.join("completed.zst")
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
        for (key, composed) in cached.instances {
            let instances = composed
                .objects
                .into_iter()
                .map(|instance| -> Result<_> {
                    Ok(compose::Instance {
                        object: Arc::clone(
                            objects
                                .get(&(instance.definition.as_str(), instance.object))
                                .context("Missing instance definition")?,
                        ),
                        id: instance.id,
                        definition: instance.definition,
                        name: instance.name,
                        ty: instance.ty,
                        native_issue: instance.native_issue,
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

pub(super) fn save(cache: &Path, generation: [u8; 32], index: &Index) -> Result<()> {
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
    file.persist(path(cache))?;
    Ok(())
}
