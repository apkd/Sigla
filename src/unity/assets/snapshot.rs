//! Completed asset generations. Objects are stored once; composed instances
//! refer back to their definitions instead of serializing the same graph again.
use super::{Asset, Index, ScriptType, compose, native};
use crate::{cancellation::Cancellation, workspace::Stamp};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
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

#[derive(Serialize)]
struct Snapshot<'a> {
    version: &'a str,
    generation: [u8; 32],
    assets: &'a BTreeMap<String, Asset>,
    scripts: &'a BTreeMap<String, Arc<ScriptType>>,
    #[serde(serialize_with = "serialize_compositions")]
    instances: &'a BTreeMap<String, Arc<compose::Composed>>,
    native_types: &'a BTreeMap<String, BTreeMap<i32, native::Resolved>>,
    observed: &'a BTreeMap<PathBuf, Stamp>,
}
impl<'a> Snapshot<'a> {
    fn new(generation: [u8; 32], index: &'a Index) -> Self {
        Self {
            version: env!("SIGLA_ASSET_FINGERPRINT"),
            generation,
            assets: &index.assets,
            scripts: &index.scripts,
            instances: &index.instances,
            native_types: &index.native_types,
            observed: &index.observed,
        }
    }
}

fn serialize_compositions<S: serde::Serializer>(
    values: &BTreeMap<String, Arc<compose::Composed>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Composition<'a> {
        #[serde(serialize_with = "serialize_instances")]
        objects: &'a [compose::Instance],
        aliases: &'a BTreeMap<i64, String>,
        incomplete: &'a [String],
    }
    serializer.collect_map(values.iter().map(|(key, value)| {
        (
            key,
            Composition {
                objects: &value.objects,
                aliases: &value.aliases,
                incomplete: &value.incomplete,
            },
        )
    }))
}

fn serialize_instances<S: serde::Serializer>(
    values: &[compose::Instance],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // Postcard structs and tuples encode fields in the same order. Preserve
    // Instance's format while borrowing the live graph and storing object IDs.
    serializer.collect_seq(values.iter().map(|value| {
        (
            &value.id,
            &value.definition,
            value.object.id,
            &value.name,
            &value.ty,
            &value.native_issue,
            &value.references,
            &value.evidence,
            value.alive,
        )
    }))
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

pub(super) fn load(
    cache: &Path,
    generation: [u8; 32],
    cancel: &dyn Cancellation,
) -> Result<Option<Index>> {
    cancel.check()?;
    let started = std::time::Instant::now();
    let result = (|| -> Result<_> {
        let cached: Cached = super::records::read(&path(cache), cancel)?;
        let decoded_ms = started.elapsed().as_millis();
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
        ensure!(index.current(cancel)?, "Asset inputs changed");
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
            cancel.check()?;
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
        tracing::debug!(
            cache = %cache.display(),
            decoded_ms,
            elapsed_ms = started.elapsed().as_millis(),
            assets = index.assets.len(),
            "Unity asset snapshot loaded"
        );
        Ok(index)
    })();
    cancel.check()?;
    if let Err(error) = &result {
        tracing::debug!(cache = %cache.display(), error = %error, "Asset snapshot unavailable");
    }
    Ok(result.ok())
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
    std::fs::create_dir_all(cache)?;
    let mut file = tempfile::NamedTempFile::new_in(cache)?;
    let mut encoder = zstd::stream::write::Encoder::new(&mut file, 3)?;
    encoder.include_checksum(true)?;
    postcard::to_io(&Snapshot::new(generation, index), &mut encoder)?;
    encoder.finish()?;
    Ok(Pending {
        file,
        destination: path(cache),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_load_does_not_become_a_cache_miss() {
        let cache = tempfile::tempdir().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        assert!(load(cache.path(), [0; 32], &cancel).is_err());
        assert!(load(cache.path(), [0; 32], &()).unwrap().is_none());
    }

    #[test]
    fn borrowed_snapshot_preserves_the_owned_record_format() {
        let object = Arc::new(super::super::parse::Object {
            id: 7,
            class: 1,
            stripped: false,
            name: "Object".into(),
            span: 0..10,
            line: 1,
            references: BTreeMap::new(),
            prefab: None,
        });
        let mut index = Index::default();
        index.assets.insert(
            "asset".into(),
            Asset {
                project: "project".into(),
                path: "Assets/Object.prefab".into(),
                guid: "guid".into(),
                objects: vec![object.clone()],
                content: None,
                unavailable: None,
            },
        );
        let reference = compose::Target {
            asset: "asset".into(),
            object: Some("7".into()),
            raw: super::super::parse::Pointer {
                guid: "guid".into(),
                id: 7,
            },
        };
        index.instances.insert(
            "asset".into(),
            Arc::new(compose::Composed {
                objects: vec![compose::Instance {
                    id: "instance".into(),
                    definition: "asset".into(),
                    object,
                    name: "Instance".into(),
                    ty: Some(Arc::new(ScriptType {
                        name: "Type".into(),
                        assembly: "Assembly".into(),
                        ancestry: vec!["Base".into()],
                    })),
                    native_issue: Some("diagnostic".into()),
                    references: BTreeMap::from([("field".into(), reference)]),
                    evidence: vec![("source".into(), 3)],
                    alive: true,
                }],
                aliases: BTreeMap::from([(7, "instance".into())]),
                incomplete: vec!["incomplete".into()],
            }),
        );
        let bytes = postcard::to_allocvec(&Snapshot::new([1; 32], &index)).unwrap();
        let owned: Cached = postcard::from_bytes(&bytes).unwrap();
        let instance = &owned.instances["asset"].objects[0];
        assert_eq!(instance.id, "instance");
        assert_eq!(instance.definition, "asset");
        assert_eq!(instance.object, index.assets["asset"].objects[0].id);
        assert_eq!(instance.name, "Instance");
        assert_eq!(
            instance.references,
            index.instances["asset"].objects[0].references
        );
        assert_eq!(postcard::to_allocvec(&owned).unwrap(), bytes);
    }
}
