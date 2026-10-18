//! Compose object presence and references without evaluating arbitrary field values.
use super::{
    Index, ScriptType,
    parse::{Object, Pointer},
};
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Target {
    pub asset: String,
    pub object: Option<String>,
    pub raw: Pointer,
}
#[derive(Clone, Debug)]
pub struct Instance {
    pub id: String,
    pub definition: String,
    pub object: Arc<Object>,
    pub name: String,
    pub ty: Option<Arc<ScriptType>>,
    pub native_issue: Option<Arc<str>>,
    pub references: BTreeMap<String, Target>,
    pub evidence: Vec<(String, i64)>,
    pub alive: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Composed {
    pub objects: Vec<Instance>,
    pub aliases: BTreeMap<i64, String>,
    pub incomplete: Vec<String>,
}

fn resolve(
    index: &Index,
    asset: &str,
    aliases: &BTreeMap<i64, String>,
    pointer: &Pointer,
) -> Target {
    let source = &index.assets[asset];
    let destination = if pointer.guid.is_empty() {
        Some(asset)
    } else {
        index.resolve(&source.project, &pointer.guid)
    };
    let object = if pointer.null() {
        None
    } else if destination == Some(asset) {
        aliases.get(&pointer.id).cloned()
    } else {
        destination.map(|_| pointer.id.to_string())
    };
    Target {
        asset: destination.unwrap_or("").into(),
        object,
        raw: pointer.clone(),
    }
}
fn lift(target: &mut Target, source: &str, destination: &str, prefix: &str) {
    if target.asset == source {
        target.asset = destination.into();
        if let Some(object) = &mut target.object {
            *object = format!("{prefix}/{object}");
        }
    }
}
fn mapped_target(
    index: &Index,
    source: &str,
    composition: &Composed,
    target: &Pointer,
) -> Option<String> {
    let asset = &index.assets[source];
    if target.guid.is_empty() || target.guid == asset.guid {
        return composition.aliases.get(&target.id).cloned();
    }
    let destination = index.resolve(&asset.project, &target.guid)?;
    let mut matches = composition
        .objects
        .iter()
        .filter(|o| o.definition == destination && o.object.id == target.id);
    let found = matches.next()?;
    matches.next().is_none().then(|| found.id.clone())
}

fn compose(
    index: &Index,
    key: &str,
    stack: &mut BTreeSet<String>,
    memo: &mut BTreeMap<String, Arc<Composed>>,
) -> Result<Arc<Composed>> {
    if let Some(result) = memo.get(key).or_else(|| index.instances.get(key)) {
        return Ok(result.clone());
    }
    if stack.len() >= 64 || !stack.insert(key.into()) {
        return Ok(Arc::new(Composed {
            incomplete: vec![format!(
                "Prefab cycle or depth limit at {}",
                index.assets[key].path
            )],
            ..Default::default()
        }));
    }
    let asset = &index.assets[key];
    let mut result = Composed::default();
    if let Some(reason) = &asset.unavailable {
        result.incomplete.push(format!("{}: {reason}", asset.path));
    }
    for object in &asset.objects {
        if object.stripped || object.prefab.is_some() {
            continue;
        }
        let id = object.id.to_string();
        result.aliases.insert(object.id, id.clone());
        let native = index
            .native_types
            .get(&asset.project)
            .and_then(|types| types.get(&object.class));
        result.objects.push(Instance {
            id,
            definition: key.into(),
            object: object.clone(),
            name: object.name.clone(),
            ty: if object.class == 114 {
                index.script(asset, object).cloned()
            } else {
                native.and_then(|resolved| resolved.ty.clone())
            },
            native_issue: native.and_then(|resolved| resolved.issue.clone()),
            references: BTreeMap::new(),
            evidence: Vec::new(),
            alive: true,
        });
    }
    let mut groups = Vec::new();
    for control in asset.objects.iter().filter(|o| o.prefab.is_some()) {
        let prefab = control.prefab.as_ref().unwrap();
        let Some(source) = index.resolve(&asset.project, &prefab.source.guid) else {
            result.incomplete.push(format!(
                "{}: unresolved source prefab {}",
                asset.path, prefab.source.guid
            ));
            continue;
        };
        let child = compose(index, source, stack, memo)?;
        ensure!(
            result.objects.len() + child.objects.len() <= 1_000_000,
            "Scene prefab expansion exceeds object limit"
        );
        let prefix = control.id.to_string();
        result.incomplete.extend(child.incomplete.iter().cloned());
        for mut object in child.objects.iter().filter(|o| o.alive).cloned() {
            object.id = format!("{prefix}/{}", object.id);
            for reference in object.references.values_mut() {
                lift(reference, source, key, &prefix);
            }
            object.evidence.push((key.into(), control.id));
            result.objects.push(object);
        }
        for stripped in asset
            .objects
            .iter()
            .filter(|o| o.stripped && o.pointer("m_PrefabInstance").id == control.id)
        {
            if let Some(id) = mapped_target(
                index,
                source,
                &child,
                &stripped.pointer("m_CorrespondingSourceObject"),
            ) {
                result.aliases.insert(stripped.id, format!("{prefix}/{id}"));
            } else {
                result.incomplete.push(format!(
                    "Unresolved stripped object {} in {}",
                    stripped.id, asset.path
                ));
            }
        }
        groups.push((control, source.to_owned(), child));
    }
    for object in result
        .objects
        .iter_mut()
        .filter(|o| o.definition == key && o.evidence.is_empty())
    {
        for (field, reference) in &object.object.references {
            object.references.insert(
                field.clone(),
                resolve(index, key, &result.aliases, &reference.target),
            );
        }
    }
    let positions: BTreeMap<_, _> = result
        .objects
        .iter()
        .enumerate()
        .map(|(i, o)| (o.id.clone(), i))
        .collect();
    for (control, source, child) in groups {
        let prefab = control.prefab.as_ref().unwrap();
        let destination = |target: &Pointer| {
            mapped_target(index, &source, &child, target).map(|id| format!("{}/{id}", control.id))
        };
        if !prefab.parent.null() {
            let prefix = format!("{}/", control.id);
            for object in result
                .objects
                .iter_mut()
                .filter(|o| o.id.starts_with(&prefix) && matches!(o.object.class, 4 | 224))
            {
                if object
                    .references
                    .get("m_Father")
                    .is_some_and(|r| r.raw.null())
                {
                    object.references.insert(
                        "m_Father".into(),
                        resolve(index, key, &result.aliases, &prefab.parent),
                    );
                }
            }
        }
        for removed in &prefab.removed {
            if let Some(id) = destination(removed) {
                if let Some(&position) = positions.get(&id) {
                    let object = &mut result.objects[position];
                    object.alive = false;
                }
            } else {
                result
                    .incomplete
                    .push(format!("Unresolved removal in {}", asset.path));
            }
        }
        for modification in &prefab.modifications {
            let Some(id) = destination(&modification.target) else {
                result
                    .incomplete
                    .push(format!("Unresolved override target in {}", asset.path));
                continue;
            };
            let Some(&position) = positions.get(&id) else {
                continue;
            };
            let object = &mut result.objects[position];
            let property = &modification.property;
            if property == "m_Name" {
                object.name = modification.value.clone();
            }
            if let Some(prefix) = property.strip_suffix(".Array.size") {
                if let Ok(size) = modification.value.parse::<usize>() {
                    let prefix = format!("{prefix}.Array.data[");
                    object.references.retain(|field, _| {
                        field
                            .strip_prefix(&prefix)
                            .and_then(|s| s.split_once(']'))
                            .and_then(|(i, _)| i.parse::<usize>().ok())
                            .is_none_or(|i| i < size)
                    });
                }
            } else if object.references.contains_key(property) || !modification.reference.null() {
                object.references.insert(
                    property.clone(),
                    resolve(index, key, &result.aliases, &modification.reference),
                );
                if property == "m_Script" && object.object.class == 114 {
                    object.ty = index
                        .resolve(&asset.project, &modification.reference.guid)
                        .and_then(|k| index.scripts.get(k))
                        .cloned();
                }
            }
        }
        for (target, added) in &prefab.added {
            if let (Some(owner), Some(id)) = (destination(target), result.aliases.get(added))
                && let Some(&position) = positions.get(id)
                && result.objects[position].object.class != 1
            {
                let object = &mut result.objects[position];
                object.references.insert(
                    "m_GameObject".into(),
                    Target {
                        asset: key.into(),
                        object: Some(owner),
                        raw: target.clone(),
                    },
                );
            }
        }
    }
    // Removing a GameObject removes its components and descendant GameObjects.
    let mut children = BTreeMap::<String, Vec<String>>::new();
    for object in &result.objects {
        for field in ["m_GameObject", "m_Father"] {
            if let Some(target) = object.references.get(field)
                && target.asset == key
                && let Some(id) = &target.object
            {
                children
                    .entry(id.clone())
                    .or_default()
                    .push(object.id.clone());
                if field == "m_GameObject" && matches!(object.object.class, 4 | 224) {
                    children
                        .entry(object.id.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
    }
    let mut pending: Vec<_> = result
        .objects
        .iter()
        .filter(|o| !o.alive)
        .map(|o| o.id.clone())
        .collect();
    let mut removed = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !removed.insert(id.clone()) {
            continue;
        }
        if let Some(&position) = positions.get(&id) {
            result.objects[position].alive = false;
        }
        if let Some(children) = children.get(&id) {
            pending.extend(children.iter().cloned());
        }
    }
    stack.remove(key);
    let result = Arc::new(result);
    memo.insert(key.into(), result.clone());
    Ok(result)
}

pub fn build(index: &mut Index) -> Result<()> {
    let mut memo = std::mem::take(&mut index.instances);
    for key in index.assets.keys() {
        if index.assets[key].path.ends_with(".prefab") {
            compose(index, key, &mut BTreeSet::new(), &mut memo)?;
        }
    }
    index.instances = memo;
    Ok(())
}

pub(super) fn reuse(index: &mut Index, previous: &Index) {
    // Retained compositions point at these definitions. Share them with the
    // new inventory rather than retaining two decoded copies of each object.
    for (key, asset) in &mut index.assets {
        if asset.content.is_some()
            && let Some(old) = previous.assets.get(key)
            && old.content == asset.content
            && old.unavailable == asset.unavailable
        {
            asset.objects.clone_from(&old.objects);
        }
    }
    if index.guids != previous.guids
        || index.scripts != previous.scripts
        || index.native_types != previous.native_types
    {
        return;
    }
    let mut changed: BTreeSet<_> = index
        .assets
        .iter()
        .filter(|(key, asset)| {
            previous.assets.get(*key).is_none_or(|old| {
                old.content != asset.content || old.unavailable != asset.unavailable
            })
        })
        .map(|(key, _)| key.clone())
        .collect();
    changed.extend(
        previous
            .assets
            .keys()
            .filter(|key| !index.assets.contains_key(*key))
            .cloned(),
    );
    let dependencies: BTreeMap<_, BTreeSet<_>> = index
        .assets
        .iter()
        .map(|(key, asset)| {
            let dependencies = asset
                .objects
                .iter()
                .flat_map(|object| {
                    object
                        .references
                        .values()
                        .map(|r| &r.target)
                        .chain(object.prefab.iter().map(|p| &p.source))
                })
                .filter_map(|pointer| {
                    index
                        .resolve(&asset.project, &pointer.guid)
                        .map(str::to_owned)
                })
                .collect();
            (key.clone(), dependencies)
        })
        .collect();
    loop {
        let next: Vec<_> = dependencies
            .iter()
            .filter(|(key, dependencies)| {
                !changed.contains(*key) && !dependencies.is_disjoint(&changed)
            })
            .map(|(key, _)| key.clone())
            .collect();
        if next.is_empty() {
            break;
        }
        changed.extend(next);
    }
    index.instances.extend(
        previous
            .instances
            .iter()
            .filter(|(key, _)| !changed.contains(*key))
            .map(|(key, instance)| (key.clone(), instance.clone())),
    );
}

impl Index {
    pub fn composed(&self, key: &str) -> Result<Arc<Composed>> {
        compose(self, key, &mut BTreeSet::new(), &mut BTreeMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unity::assets::{Asset, parse};

    fn asset(index: &mut Index, name: &str, guid: &str, text: &str) {
        index.assets.insert(
            name.into(),
            Asset {
                project: ".".into(),
                path: name.into(),
                guid: guid.into(),
                objects: parse::parse(text)
                    .unwrap()
                    .objects
                    .into_iter()
                    .map(Arc::new)
                    .collect(),
                content: None,
                unavailable: None,
            },
        );
        index
            .guids
            .insert((".".into(), guid.into()), vec![name.into()]);
    }
    const PREFAB: &str = "--- !u!1 &1\nGameObject:\n  m_Name: Enemy\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n  m_Father: {fileID: 0}\n--- !u!20 &3\nCamera:\n  m_GameObject: {fileID: 1}\n  nested:\n    target: {fileID: 1, guid: sword, type: 2}\n";
    #[test]
    fn snapshots_restore_shared_types_without_merging_different_definitions() {
        let cache = tempfile::tempdir().unwrap();
        let mut index = Index::default();
        for (path, guid, assembly) in [
            ("Actor.cs", "actor", "First"),
            ("Other.cs", "other", "Second"),
        ] {
            asset(&mut index, path, guid, "");
            index.scripts.insert(
                path.into(),
                Arc::new(ScriptType {
                    name: "Example.Actor".into(),
                    assembly: assembly.into(),
                    ancestry: vec!["Example.Actor".into(), "UnityEngine.Component".into()],
                }),
            );
        }
        let camera = Arc::new(ScriptType {
            name: "UnityEngine.Camera".into(),
            assembly: "UnityEngine".into(),
            ancestry: vec!["UnityEngine.Camera".into(), "UnityEngine.Component".into()],
        });
        index.native_types.insert(
            ".".into(),
            BTreeMap::from([
                (
                    4,
                    super::super::native::Resolved {
                        ty: None,
                        issue: Some("Native coverage unavailable".into()),
                    },
                ),
                (
                    20,
                    super::super::native::Resolved {
                        ty: Some(camera),
                        issue: Some("Native coverage unavailable".into()),
                    },
                ),
            ]),
        );
        let mut source = PREFAB.to_owned();
        for (id, script) in [(4, "actor"), (5, "actor"), (6, "other")] {
            source.push_str(&format!(
                "--- !u!114 &{id}\nMonoBehaviour:\n  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {script}, type: 3}}\n"
            ));
        }
        asset(&mut index, "Base.prefab", "base", &source);
        let changes = "\n    - target: {fileID: 4, guid: base, type: 3}\n      propertyPath: m_Script\n      value: \n      objectReference: {fileID: 11500000, guid: other, type: 3}";
        asset(
            &mut index,
            "Variant.prefab",
            "variant",
            &placement(10, "base", changes, "[]"),
        );
        build(&mut index).unwrap();
        let base = &index.instances["Base.prefab"];
        for id in ["4", "5"] {
            let ty = base
                .objects
                .iter()
                .find(|object| object.id == id)
                .unwrap()
                .ty
                .as_ref()
                .unwrap();
            assert!(Arc::ptr_eq(ty, &index.scripts["Actor.cs"]));
        }
        let overridden = index.instances["Variant.prefab"]
            .objects
            .iter()
            .find(|object| object.object.id == 4)
            .unwrap();
        assert!(Arc::ptr_eq(
            overridden.ty.as_ref().unwrap(),
            &index.scripts["Other.cs"]
        ));

        super::super::snapshot::save(cache.path(), [3; 32], &index).unwrap();
        let mut saved = super::super::snapshot::load(cache.path(), [3; 32]).unwrap();
        for (key, composed) in &index.instances {
            for (expected, actual) in composed.objects.iter().zip(&saved.instances[key].objects) {
                assert_eq!(actual.ty, expected.ty);
                assert_eq!(actual.native_issue, expected.native_issue);
                if let Some(ty) = &actual.ty {
                    let catalog = if actual.object.class == 114 {
                        saved
                            .scripts
                            .values()
                            .find(|candidate| *candidate == ty)
                            .unwrap()
                    } else {
                        saved.native_types["."][&actual.object.class]
                            .ty
                            .as_ref()
                            .unwrap()
                    };
                    assert!(Arc::ptr_eq(ty, catalog));
                }
                if let Some(issue) = &actual.native_issue {
                    assert!(Arc::ptr_eq(
                        issue,
                        saved.native_types["."][&4].issue.as_ref().unwrap()
                    ));
                }
            }
        }
        assert!(!Arc::ptr_eq(
            &saved.scripts["Actor.cs"],
            &saved.scripts["Other.cs"]
        ));
        for query in [
            "instance:Example.Actor limit:100",
            "dependencies:Variant.prefab limit:100",
        ] {
            let query = crate::query::Query::parse(query).unwrap();
            assert_eq!(saved.search(&query).unwrap(), index.search(&query).unwrap());
        }

        let shared_bytes = saved.retained_bytes;
        for composed in saved.instances.values_mut() {
            for object in &mut Arc::make_mut(composed).objects {
                object.ty = object.ty.as_ref().map(|ty| Arc::new((**ty).clone()));
                object.native_issue = object
                    .native_issue
                    .as_ref()
                    .map(|issue| Arc::from(issue.as_ref()));
            }
        }
        saved.measure();
        assert!(
            saved.retained_bytes > shared_bytes,
            "Separate allocations must consume more budget"
        );
    }

    #[test]
    fn completed_snapshots_restore_shared_definitions_and_invalidate_dependent_prefabs() {
        let cache = tempfile::tempdir().unwrap();
        let mut index = Index::default();
        asset(&mut index, "Base.prefab", "base", PREFAB);
        asset(
            &mut index,
            "Variant.prefab",
            "variant",
            &placement(10, "base", "", ""),
        );
        asset(&mut index, "Independent.prefab", "independent", PREFAB);
        build(&mut index).unwrap();
        super::super::snapshot::save(cache.path(), [1; 32], &index).unwrap();
        let saved = super::super::snapshot::load(cache.path(), [1; 32]).unwrap();
        assert!(super::super::snapshot::load(cache.path(), [2; 32]).is_none());
        for (key, expected) in &index.instances {
            let actual = &saved.instances[key];
            assert_eq!(actual.aliases, expected.aliases);
            assert_eq!(actual.objects.len(), expected.objects.len());
            for (actual, expected) in actual.objects.iter().zip(&expected.objects) {
                assert_eq!(actual.references, expected.references);
                assert_eq!(actual.name, expected.name);
                let definition = saved.assets[&actual.definition]
                    .objects
                    .iter()
                    .find(|o| o.id == actual.object.id)
                    .unwrap();
                assert!(Arc::ptr_eq(&actual.object, definition));
            }
        }
        let mut changed = Index {
            assets: saved.assets.clone(),
            guids: saved.guids.clone(),
            ..Default::default()
        };
        asset(
            &mut changed,
            "Base.prefab",
            "base",
            &PREFAB.replace("Enemy", "Changed"),
        );
        changed.assets.get_mut("Base.prefab").unwrap().content = Some("changed-record".into());
        reuse(&mut changed, &saved);
        assert!(changed.instances.contains_key("Independent.prefab"));
        assert!(!changed.instances.contains_key("Variant.prefab"));
        build(&mut changed).unwrap();
        assert!(Arc::ptr_eq(
            &changed.instances["Independent.prefab"],
            &saved.instances["Independent.prefab"]
        ));
        assert!(
            changed.instances["Variant.prefab"]
                .objects
                .iter()
                .any(|o| o.name == "Changed")
        );
        index.assets.get_mut("Base.prefab").unwrap().content =
            Some(cache.path().join("missing.text.zst"));
        super::super::snapshot::save(cache.path(), [1; 32], &index).unwrap();
        assert!(super::super::snapshot::load(cache.path(), [1; 32]).is_none());
    }
    fn placement(id: i64, guid: &str, changes: &str, removed: &str) -> String {
        format!(
            "--- !u!1001 &{id}\nPrefabInstance:\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}, type: 3}}\n  m_Modification:\n    m_Modifications: {changes}\n    m_RemovedComponents: {removed}\n"
        )
    }

    #[test]
    fn reused_compositions_share_inventory_definitions() {
        let prepare = || {
            let mut index = Index::default();
            asset(&mut index, "Stable.prefab", "stable", PREFAB);
            index.assets.get_mut("Stable.prefab").unwrap().content = Some("content-record".into());
            index
        };
        let mut before = prepare();
        build(&mut before).unwrap();
        let mut after = prepare();
        reuse(&mut after, &before);
        build(&mut after).unwrap();
        assert!(Arc::ptr_eq(
            &before.instances["Stable.prefab"],
            &after.instances["Stable.prefab"]
        ));
        for instance in &after.instances["Stable.prefab"].objects {
            let definition = after.assets[&instance.definition]
                .objects
                .iter()
                .find(|object| object.id == instance.object.id)
                .unwrap();
            assert!(Arc::ptr_eq(&instance.object, definition));
        }
    }
    #[test]
    fn placements_apply_reference_overrides_and_component_removals() {
        let mut index = Index::default();
        asset(&mut index, "Enemy.prefab", "enemy", PREFAB);
        asset(&mut index, "Sword.asset", "sword", "name: Sword\n");
        asset(&mut index, "Bow.asset", "bow", "name: Bow\n");
        let changes = "\n    - target: {fileID: 3, guid: enemy, type: 3}\n      propertyPath: nested.target\n      value: \n      objectReference: {fileID: 1, guid: bow, type: 2}";
        let scene = placement(10, "enemy", changes, "[]")
            + &placement(20, "enemy", "[]", "[{fileID: 3, guid: enemy, type: 3}]");
        asset(&mut index, "Level.unity", "scene", &scene);
        build(&mut index).unwrap();
        let scene = index.composed("Level.unity").unwrap();
        let cameras: Vec<_> = scene
            .objects
            .iter()
            .filter(|o| o.alive && o.object.class == 20)
            .collect();
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].references["nested.target"].asset, "Bow.asset");
        assert_eq!(
            index.instances["Enemy.prefab"]
                .objects
                .iter()
                .find(|o| o.object.class == 20)
                .unwrap()
                .references["nested.target"]
                .asset,
            "Sword.asset"
        );
        assert_eq!(
            cameras[0].references["m_GameObject"].object.as_deref(),
            Some("10/1")
        );
    }
    #[test]
    fn nested_variants_use_stripped_aliases_and_can_clear_references() {
        let mut index = Index::default();
        asset(&mut index, "Enemy.prefab", "enemy", PREFAB);
        let variant = placement(10, "enemy", "[]", "[]")
            + "--- !u!20 &99 stripped\nCamera:\n  m_CorrespondingSourceObject: {fileID: 3, guid: enemy, type: 3}\n  m_PrefabInstance: {fileID: 10}\n";
        asset(&mut index, "Variant.prefab", "variant", &variant);
        let changes = "\n    - target: {fileID: 99, guid: variant, type: 3}\n      propertyPath: nested.target\n      value: \n      objectReference: {fileID: 0}";
        asset(
            &mut index,
            "Level.unity",
            "scene",
            &placement(20, "variant", changes, "[]"),
        );
        build(&mut index).unwrap();
        let scene = index.composed("Level.unity").unwrap();
        let cameras: Vec<_> = scene
            .objects
            .iter()
            .filter(|o| o.alive && o.object.class == 20)
            .collect();
        assert_eq!(cameras.len(), 1);
        assert!(cameras[0].references["nested.target"].raw.null());
        assert!(scene.incomplete.is_empty());
    }
    #[test]
    fn removing_gameobject_removes_children_and_components() {
        let mut index = Index::default();
        let prefab = PREFAB.to_owned()
            + "--- !u!1 &4\nGameObject:\n  m_Name: Child\n--- !u!4 &5\nTransform:\n  m_GameObject: {fileID: 4}\n  m_Father: {fileID: 2}\n";
        asset(&mut index, "Enemy.prefab", "enemy", &prefab);
        asset(
            &mut index,
            "Level.unity",
            "scene",
            &placement(10, "enemy", "[]", "[{fileID: 1, guid: enemy, type: 3}]"),
        );
        build(&mut index).unwrap();
        assert!(
            index
                .composed("Level.unity")
                .unwrap()
                .objects
                .iter()
                .all(|o| !o.alive)
        );
    }
    #[test]
    fn duplicate_guids_do_not_resolve_arbitrarily() {
        let mut index = Index::default();
        asset(&mut index, "Enemy.prefab", "enemy", PREFAB);
        asset(&mut index, "Other.prefab", "other", PREFAB);
        index
            .guids
            .get_mut(&(".".into(), "enemy".into()))
            .unwrap()
            .push("Other.prefab".into());
        asset(
            &mut index,
            "Level.unity",
            "scene",
            &placement(10, "enemy", "[]", "[]"),
        );
        build(&mut index).unwrap();
        let scene = index.composed("Level.unity").unwrap();
        assert!(scene.objects.is_empty());
        assert!(!scene.incomplete.is_empty());
    }
}
