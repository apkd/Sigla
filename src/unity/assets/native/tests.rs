use super::*;

fn catalog(version: &str) -> &'static Snapshot {
    snapshot(Some(version.parse().unwrap())).unwrap()
}

fn fallback(version: &str, class: i32) -> Arc<ScriptType> {
    let resolved = resolve(catalog(version), &Metadata::default(), class);
    assert!(resolved.issue.is_none(), "{:?}", resolved.issue);
    resolved.ty.unwrap()
}

fn matching(catalog: &Snapshot) -> Metadata {
    Metadata {
        assemblies: catalog.types.values().map(|d| d.assembly.clone()).collect(),
        types: catalog
            .types
            .iter()
            .map(|(n, d)| (n.clone(), Some(d.clone())))
            .collect(),
    }
}

#[test]
fn catalogs_are_sorted_unambiguous_and_have_complete_managed_class_chains() {
    let bindings: BTreeMap<String, String> =
        serde_json::from_str(include_str!("bindings.json")).unwrap();
    for catalog in snapshots() {
        assert!(!catalog.sources.is_empty());
        assert!(catalog.classes.windows(2).all(|w| w[0].0 < w[1].0));
        for (id, native, name) in &catalog.classes {
            let resolved = resolve(catalog, &Metadata::default(), *id);
            if let Some(name) = name {
                assert_ne!(*id, 114);
                assert_eq!(&bindings[native], name);
                assert!(resolved.issue.is_none(), "{id}: {:?}", resolved.issue);
                let ty = resolved.ty.unwrap();
                assert_eq!(&ty.name, name);
                assert_eq!(ty.ancestry.last().map(String::as_str), Some(OBJECT));
                assert_eq!(ty.assembly, catalog.types[name].assembly);
            } else {
                assert!(resolved.ty.is_none(), "{id}");
                assert!(resolved.issue.is_some(), "{id}");
            }
        }
    }
}

#[test]
fn immediate_bases_and_namespaces_are_not_native_inheritance_or_prefix_guesses() {
    for version in ["2022.3.1f1", "6000.0.1f1"] {
        for (id, name, base) in [
            (74, "UnityEngine.AnimationClip", "UnityEngine.Motion"),
            (
                81,
                "UnityEngine.AudioListener",
                "UnityEngine.AudioBehaviour",
            ),
            (82, "UnityEngine.AudioSource", "UnityEngine.AudioBehaviour"),
            (53, "UnityEngine.Collider2D", "UnityEngine.Behaviour"),
            (
                247,
                "UnityEngine.ConstantForce2D",
                "UnityEngine.PhysicsUpdateBehaviour2D",
            ),
            (
                255,
                "UnityEngine.FixedJoint2D",
                "UnityEngine.AnchoredJoint2D",
            ),
            (
                86,
                "UnityEngine.CustomRenderTexture",
                "UnityEngine.RenderTexture",
            ),
            (
                91,
                "UnityEditor.Animations.AnimatorController",
                "UnityEngine.RuntimeAnimatorController",
            ),
            (
                206,
                "UnityEditor.Animations.BlendTree",
                "UnityEngine.Motion",
            ),
            (
                1101,
                "UnityEditor.Animations.AnimatorStateTransition",
                "UnityEditor.Animations.AnimatorTransitionBase",
            ),
            (
                320,
                "UnityEngine.Playables.PlayableDirector",
                "UnityEngine.Behaviour",
            ),
            (
                1839735485,
                "UnityEngine.Tilemaps.Tilemap",
                "UnityEngine.GridLayout",
            ),
            (
                895512359,
                "UnityEngine.Animations.AimConstraint",
                "UnityEngine.Behaviour",
            ),
            (
                2058629509,
                "UnityEngine.VFX.VisualEffectAsset",
                "UnityEngine.VFX.VisualEffectObject",
            ),
        ] {
            let ty = fallback(version, id);
            assert_eq!(ty.name, name);
            assert_eq!(ty.ancestry[1], base, "{version}: {name}");
        }
        let filter = fallback(version, 164);
        assert!(
            !filter
                .ancestry
                .iter()
                .any(|n| n == "UnityEngine.AudioBehaviour")
        );
        assert!(
            !filter
                .ancestry
                .iter()
                .any(|n| n == "UnityEngine.AudioFilter")
        );
    }
}

#[test]
fn release_selection_does_not_extrapolate_renames_or_base_changes() {
    assert_eq!(
        fallback("2022.3.60f1", 134).name,
        "UnityEngine.PhysicMaterial"
    );
    assert_eq!(
        fallback("6000.0.1f1", 134).name,
        "UnityEngine.PhysicsMaterial"
    );
    let old = fallback("2022.3.1f1", 83);
    let new = fallback("6000.0.1f1", 83);
    assert_eq!(old.ancestry[1], OBJECT);
    assert_eq!(new.ancestry[1], "UnityEngine.Audio.AudioResource");
    assert!(
        resolve(catalog("2022.3.1f1"), &Metadata::default(), 355983997)
            .ty
            .is_none()
    );
    assert!(snapshot(None).is_none());
    for version in [
        "2021.3.1f1",
        "2023.1.1f1",
        "6000.1.1f1",
        "7000.0.1f1",
        "6000.0.1b1",
    ] {
        assert!(
            snapshot(Some(version.parse().unwrap())).is_none(),
            "{version}"
        );
    }
    // Unity 6 can contain the obsolete managed spelling as well; its presence
    // does not change the native ID's audited identity.
    let catalog = catalog("6000.0.1f1");
    let mut metadata = matching(catalog);
    metadata.types.insert(
        "UnityEngine.PhysicMaterial".into(),
        Some(Definition {
            assembly: "UnityEngine.PhysicsModule".into(),
            base: Some(OBJECT.into()),
        }),
    );
    assert_eq!(
        resolve(catalog, &metadata, 134).ty.unwrap().name,
        "UnityEngine.PhysicsMaterial"
    );
}

#[test]
fn matching_metadata_replaces_the_chain_and_missing_modules_use_the_snapshot() {
    let catalog = catalog("6000.0.1f1");
    let mut metadata = matching(catalog);
    metadata
        .types
        .get_mut("UnityEngine.Camera")
        .unwrap()
        .as_mut()
        .unwrap()
        .base = Some("UnityEngine.EditorSpecificBase".into());
    metadata.types.insert(
        "UnityEngine.EditorSpecificBase".into(),
        Some(Definition {
            assembly: "UnityEngine.CoreModule".into(),
            base: Some("UnityEngine.Component".into()),
        }),
    );
    let resolved = resolve(catalog, &metadata, 20);
    assert!(resolved.issue.is_none());
    let ty = resolved.ty.unwrap();
    assert_eq!(ty.ancestry[1], "UnityEngine.EditorSpecificBase");
    assert!(!ty.ancestry.iter().any(|n| n == "UnityEngine.Behaviour"));
    assert!(resolve(catalog, &Metadata::default(), 20).issue.is_none());
    metadata.types.remove("UnityEngine.Camera");
    assert!(resolve(catalog, &metadata, 20).ty.is_none());
}

#[test]
fn compatibility_keeps_shared_identities_without_choosing_version_specific_bases() {
    let shared = compatibility();
    let camera = resolve(shared, &Metadata::default(), 20).ty.unwrap();
    assert!(
        camera
            .ancestry
            .iter()
            .any(|name| name == "UnityEngine.Component")
    );
    assert!(resolve(shared, &Metadata::default(), 134).ty.is_none());
    let audio = resolve(shared, &Metadata::default(), 83);
    assert!(audio.issue.is_some());
    assert_eq!(audio.ty.unwrap().ancestry, ["UnityEngine.AudioClip"]);
    let audio = resolve(shared, &matching(catalog("6000.0.1f1")), 83);
    assert!(audio.issue.is_none());
    assert!(
        audio
            .ty
            .unwrap()
            .ancestry
            .iter()
            .any(|name| name == "UnityEngine.Audio.AudioResource")
    );
}

#[test]
fn partial_and_ambiguous_metadata_never_acquire_unobserved_fallback_bases() {
    let catalog = catalog("6000.0.1f1");
    let mut metadata = matching(catalog);
    metadata.types.remove("UnityEngine.Component");
    let resolved = resolve(catalog, &metadata, 20);
    assert!(
        resolved
            .issue
            .as_deref()
            .unwrap()
            .contains("metadata unavailable")
    );
    assert_eq!(
        resolved.ty.unwrap().ancestry,
        [
            "UnityEngine.Camera",
            "UnityEngine.Behaviour",
            "UnityEngine.Component",
        ]
    );
    metadata.types.insert("UnityEngine.Behaviour".into(), None);
    let resolved = resolve(catalog, &metadata, 20);
    assert!(resolved.issue.as_deref().unwrap().contains("Ambiguous"));
    assert_eq!(
        resolved.ty.unwrap().ancestry,
        ["UnityEngine.Camera", "UnityEngine.Behaviour"]
    );
    metadata.types.insert("UnityEngine.Camera".into(), None);
    assert!(resolve(catalog, &metadata, 20).ty.is_none());
}

#[test]
fn cycles_non_objects_unknown_ids_and_scripted_ids_are_conservative() {
    let catalog = catalog("6000.0.1f1");
    let mut metadata = matching(catalog);
    metadata
        .types
        .get_mut("UnityEngine.Behaviour")
        .unwrap()
        .as_mut()
        .unwrap()
        .base = Some("UnityEngine.Camera".into());
    assert!(
        resolve(catalog, &metadata, 20)
            .issue
            .as_deref()
            .unwrap()
            .contains("Cyclic")
    );
    metadata
        .types
        .get_mut("UnityEngine.Camera")
        .unwrap()
        .as_mut()
        .unwrap()
        .base = Some("System.Object".into());
    assert!(resolve(catalog, &metadata, 20).ty.is_none());
    for id in [
        114,
        181,
        285090594,
        937362698,
        1660057539,
        9876543,
        i32::MAX,
        -1,
    ] {
        let resolved = resolve(catalog, &metadata, id);
        assert!(resolved.ty.is_none(), "{id}");
        assert!(resolved.issue.is_some(), "{id}");
    }
}

#[test]
fn only_this_roots_exact_declared_and_selected_editor_references_are_eligible() {
    use crate::model::{MetadataReference, Project, SourceRoot};
    let root = Path::new("/workspace/One");
    let version: UnityVersion = "6000.0.1f1".parse().unwrap();
    let reference = |path: &str, provenance: &str| MetadataReference {
        path: path.into(),
        aliases: Vec::new(),
        provenance: provenance.into(),
    };
    let project = Project {
        identity: "game".into(),
        origin: None,
        name: "game".into(),
        defines: Vec::new(),
        references: Vec::new(),
        edition: String::new(),
        assemblies: vec![
            reference("matching.dll", "Unity editor 6000.0.1f1"),
            reference("plugin.dll", "Unity plugin"),
            reference("older.dll", "Unity editor 2022.3.1f1"),
        ],
        compiler_options: [
            ("UnityVersion".into(), version.to_string()),
            ("DeclaredUnityVersion".into(), version.to_string()),
        ]
        .into_iter()
        .collect(),
        source_roots: vec![SourceRoot {
            physical: root.join("Assets"),
            logical: "Assets".into(),
        }],
    };
    let mut manifest = Manifest {
        projects: vec![project.clone()],
        ..Default::default()
    };
    assert_eq!(
        references(root, version, &manifest),
        [PathBuf::from("matching.dll")].into_iter().collect()
    );
    for key in ["UnityVersion", "DeclaredUnityVersion"] {
        manifest.projects[0] = project.clone();
        manifest.projects[0]
            .compiler_options
            .insert(key.into(), "6000.0.2f1".into());
        assert!(references(root, version, &manifest).is_empty());
    }
    manifest.projects[0] = project;
    assert!(references(Path::new("/workspace/Two"), version, &manifest).is_empty());
}

#[test]
fn project_version_requires_one_well_formed_bounded_editor_version() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ProjectVersion.txt");
    std::fs::write(
        &path,
        "m_EditorVersion: 6000.0.1f1\nm_EditorVersionWithRevision: 6000.0.1f1 (abc)\n",
    )
    .unwrap();
    assert_eq!(version(&path).unwrap().to_string(), "6000.0.1f1");
    std::fs::write(&path, "\u{feff}m_EditorVersion: 2022.3.1f1\r\n").unwrap();
    assert_eq!(version(&path).unwrap().to_string(), "2022.3.1f1");
    for text in [
        "m_EditorVersion: latest\n".to_owned(),
        "m_EditorVersion: 6000.0.1f1\nm_EditorVersion: 2022.3.1f1\n".to_owned(),
        format!("m_EditorVersion: 6000.0.1f1\n{}", "x".repeat(64 * 1024)),
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(version(&path).is_none());
    }
}

#[test]
fn published_metadata_records_supply_ancestry_without_unrelated_assemblies() {
    use crate::{
        model::Language,
        store::FileData,
        workspace::{FileEntry, Stamp},
    };
    let root = tempfile::tempdir().unwrap();
    let store =
        Store::open_workspace(&root.path().join("analysis"), [1; 32], root.path(), None).unwrap();
    let mut manifest = Manifest::default();
    for (file, source) in [
        (
            "editor",
            "namespace UnityEngine { public class Camera : UnityEngine.CustomBase {} public class CustomBase : UnityEngine.Component {} public class Component : UnityEngine.Object {} }",
        ),
        (
            "unrelated",
            "namespace UnityEngine { public class Camera : System.Object {} }",
        ),
    ] {
        let path = root.path().join(format!("{file}.dll"));
        std::fs::write(&path, source).unwrap();
        let id = *blake3::hash(source.as_bytes()).as_bytes();
        store
            .install(
                file,
                &id,
                id,
                || Ok(()),
                || {
                    Ok(FileData {
                        source: source.into(),
                        facts: crate::extract::extract(source, Language::CSharp, &[], "").unwrap(),
                        assembly: Some("UnityEngine.CoreModule".into()),
                    })
                },
            )
            .unwrap();
        manifest.files.insert(
            file.into(),
            FileEntry {
                stamp: Stamp::read(&path).unwrap(),
                path,
                display: file.into(),
                language: Language::CSharp,
                memberships: Vec::new(),
                modules: Vec::new(),
                metadata: true,
            },
        );
    }
    store.save_manifest(&manifest).unwrap();
    let paths = BTreeSet::from([root.path().join("editor.dll")]);
    let tx = store.query_read().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let loaded = metadata(
        &paths,
        vec!["UnityEngine.Camera".into()],
        &manifest,
        &store,
        &tx,
        &cancel,
    )
    .unwrap();
    let result = resolve(catalog("6000.0.1f1"), &loaded, 20);
    assert!(result.issue.is_none(), "{:?}", result.issue);
    assert_eq!(
        result.ty.unwrap().ancestry,
        [
            "UnityEngine.Camera",
            "UnityEngine.CustomBase",
            "UnityEngine.Component",
            "UnityEngine.Object"
        ]
    );
    manifest.files.get_mut("editor").unwrap().metadata = false;
    assert!(
        metadata(
            &paths,
            vec!["UnityEngine.Camera".into()],
            &manifest,
            &store,
            &tx,
            &cancel
        )
        .unwrap()
        .types
        .is_empty()
    );
}
