use super::{app, object_handles, write};
use std::path::Path;

fn editor(root: &Path, version: &str) {
    write(
        root,
        "ProjectSettings/ProjectVersion.txt",
        &format!("m_EditorVersion: {version}\n"),
    );
}

// Independent behavior fixtures, not an enumeration of the production catalog.
const COMPONENTS: &[(i32, &str)] = &[
    (138, "UnityEngine.FixedJoint"),
    (144, "UnityEngine.CharacterJoint"),
    (145, "UnityEngine.SpringJoint"),
    (153, "UnityEngine.ConfigurableJoint"),
    (66, "UnityEngine.CompositeCollider2D"),
    (70, "UnityEngine.CapsuleCollider2D"),
    (893571522, "UnityEngine.CustomCollider2D"),
    (19719996, "UnityEngine.Tilemaps.TilemapCollider2D"),
    (249, "UnityEngine.AreaEffector2D"),
    (253, "UnityEngine.BuoyancyEffector2D"),
    (231, "UnityEngine.SpringJoint2D"),
    (254, "UnityEngine.RelativeJoint2D"),
    (255, "UnityEngine.FixedJoint2D"),
    (256, "UnityEngine.FrictionJoint2D"),
    (257, "UnityEngine.TargetJoint2D"),
    (164, "UnityEngine.AudioReverbFilter"),
    (165, "UnityEngine.AudioHighPassFilter"),
    (166, "UnityEngine.AudioChorusFilter"),
    (167, "UnityEngine.AudioReverbZone"),
    (168, "UnityEngine.AudioEchoFilter"),
    (169, "UnityEngine.AudioLowPassFilter"),
    (170, "UnityEngine.AudioDistortionFilter"),
    (81, "UnityEngine.AudioListener"),
    (82, "UnityEngine.AudioSource"),
    (156049354, "UnityEngine.Grid"),
    (1839735485, "UnityEngine.Tilemaps.Tilemap"),
    (483693784, "UnityEngine.Tilemaps.TilemapRenderer"),
    (895512359, "UnityEngine.Animations.AimConstraint"),
    (1183024399, "UnityEngine.Animations.LookAtConstraint"),
    (1773428102, "UnityEngine.Animations.ParentConstraint"),
    (1818360608, "UnityEngine.Animations.PositionConstraint"),
    (1818360609, "UnityEngine.Animations.RotationConstraint"),
    (1818360610, "UnityEngine.Animations.ScaleConstraint"),
    (2083052967, "UnityEngine.VFX.VisualEffect"),
    (73398921, "UnityEngine.VFX.VFXRenderer"),
    (320, "UnityEngine.Playables.PlayableDirector"),
];

#[tokio::test]
async fn broad_native_components_match_exact_types_and_real_managed_bases() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    editor(root.path(), "6000.0.1f1");
    let mut prefab = "--- !u!1 &1\nGameObject:\n  m_Name: Native\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n".to_owned();
    for (i, (class, name)) in COMPONENTS.iter().enumerate() {
        let short = name.rsplit('.').next().unwrap();
        prefab.push_str(&format!(
            "--- !u!{class} &{}\n{short}:\n  m_GameObject: {{fileID: 1}}\n",
            i + 100
        ));
    }
    write(root.path(), "Assets/Native.prefab", &prefab);
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    for (i, (_, name)) in COMPONENTS.iter().enumerate() {
        let result = app
            .search(
                codebase,
                &format!("instance:{name} type-match:exact path:Assets/Native.prefab"),
            )
            .await
            .unwrap();
        let handles = object_handles(&result);
        assert_eq!(handles.len(), 1, "{name}: {result}");
        assert!(handles[0].ends_with(&format!("#{}", i + 100)), "{result}");
        assert!(result.contains(name), "{result}");
        assert!(!result.contains("Coverage is incomplete"), "{result}");
    }
    for (base, count) in [
        ("UnityEngine.Component", COMPONENTS.len() + 1),
        ("UnityEngine.Joint", 4),
        ("UnityEngine.Collider2D", 4),
        ("UnityEngine.Effector2D", 2),
        ("UnityEngine.Joint2D", 5),
        ("UnityEngine.AnchoredJoint2D", 3),
        ("UnityEngine.AudioBehaviour", 2),
        ("UnityEngine.GridLayout", 2),
        ("UnityEngine.Renderer", 2),
    ] {
        let result = app
            .search(
                codebase,
                &format!("instance:{base} path:Assets/Native.prefab limit:100"),
            )
            .await
            .unwrap();
        assert_eq!(object_handles(&result).len(), count, "{base}: {result}");
        let exact = app
            .search(
                codebase,
                &format!("instance:{base} type-match:exact path:Assets/Native.prefab"),
            )
            .await
            .unwrap();
        assert!(object_handles(&exact).is_empty(), "{exact}");
    }
    let behaviours = app
        .search(codebase, "instance:UnityEngine.Behaviour limit:100")
        .await
        .unwrap();
    assert!(
        behaviours.contains("UnityEngine.CapsuleCollider2D"),
        "{behaviours}"
    );
    assert!(
        behaviours.contains("UnityEngine.AudioSource"),
        "{behaviours}"
    );
    assert!(
        !behaviours.contains("UnityEngine.ConfigurableJoint"),
        "{behaviours}"
    );
    for wrong in [
        "UnityEngine.Tilemap",
        "UnityEngine.AimConstraint",
        "UnityEngine.PlayableDirector",
        "UnityEngine.VisualEffect",
    ] {
        let result = app
            .search(codebase, &format!("instance:{wrong}"))
            .await
            .unwrap();
        assert!(object_handles(&result).is_empty(), "{result}");
    }
    app.shutdown().await;
}

#[tokio::test]
async fn asset_labels_gain_full_names_without_broadening_instance_queries() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    editor(root.path(), "2022.3.1f1");
    let mut asset = "--- !u!1 &1\nGameObject:\n  m_Name: Target\n".to_owned();
    let assets = [
        (28, "UnityEngine.Texture2D"),
        (117, "UnityEngine.Texture3D"),
        (84, "UnityEngine.RenderTexture"),
        (86, "UnityEngine.CustomRenderTexture"),
        (89, "UnityEngine.Cubemap"),
        (187, "UnityEngine.Texture2DArray"),
        (188, "UnityEngine.CubemapArray"),
        (74, "UnityEngine.AnimationClip"),
        (91, "UnityEditor.Animations.AnimatorController"),
        (206, "UnityEditor.Animations.BlendTree"),
        (1101, "UnityEditor.Animations.AnimatorStateTransition"),
        (1102, "UnityEditor.Animations.AnimatorState"),
        (1107, "UnityEditor.Animations.AnimatorStateMachine"),
        (1109, "UnityEditor.Animations.AnimatorTransition"),
    ];
    for (i, (class, name)) in assets.iter().enumerate() {
        asset.push_str(&format!(
            "--- !u!{class} &{}\n{}:\n  m_TestReference: {{fileID: 1}}\n",
            i + 100,
            name.rsplit('.').next().unwrap()
        ));
    }
    write(root.path(), "Assets/Native.asset", &asset);
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    let dependencies = app
        .search(codebase, "dependencies:Assets/Native.asset limit:100")
        .await
        .unwrap();
    for (_, name) in assets {
        assert!(dependencies.contains(name), "{name}: {dependencies}");
    }
    assert!(!dependencies.contains("type unresolved"), "{dependencies}");
    for ty in [
        "UnityEngine.Object",
        "UnityEngine.Motion",
        "UnityEngine.Texture",
        "UnityEditor.Animations.AnimatorController",
    ] {
        let result = app
            .search(codebase, &format!("instance:{ty}"))
            .await
            .unwrap();
        assert!(object_handles(&result).is_empty(), "{result}");
        assert!(!result.contains("Coverage is incomplete"), "{result}");
    }
    app.shutdown().await;
}

const MATERIAL: &str =
    "--- !u!134 &1\nPhysicMaterial:\n  m_Name: Material\n  m_TestReference: {fileID: 1}\n";

#[tokio::test]
async fn other_editor_versions_keep_known_components_with_explicit_partial_coverage() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    editor(root.path(), "6000.3.1f1");
    write(
        root.path(),
        "Assets/Camera.prefab",
        "--- !u!20 &1\nCamera:\n  m_Name: Camera\n",
    );
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    let result = app
        .search(codebase, "instance:UnityEngine.Component")
        .await
        .unwrap();
    let handles = object_handles(&result);
    assert_eq!(handles.len(), 1, "{result}");
    assert!(result.contains("unresolved native types"), "{result}");
    let viewed = app.view(codebase, handles[0], "exact").await.unwrap();
    assert!(viewed.contains("coverage is unverified"), "{viewed}");
    app.shutdown().await;
}

#[tokio::test]
async fn each_unity_root_uses_its_own_version_and_version_only_changes_refresh_types() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (name, version) in [("Old", "2022.3.1f1"), ("New", "6000.0.1f1")] {
        let project = root.path().join(name);
        editor(&project, version);
        write(&project, "Assets/Material.asset", MATERIAL);
    }
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    let query = "dependencies:Old/Assets/Material.asset";
    let old = app.search(codebase, query).await.unwrap();
    assert!(old.contains("UnityEngine.PhysicMaterial"), "{old}");
    assert!(!old.contains("UnityEngine.PhysicsMaterial"), "{old}");
    let new = app
        .search(codebase, "dependencies:New/Assets/Material.asset")
        .await
        .unwrap();
    // Native identity is from the ID catalog, not a trusted YAML root name.
    assert!(new.contains("UnityEngine.PhysicsMaterial"), "{new}");
    assert!(!new.contains("UnityEngine.PhysicMaterial"), "{new}");
    editor(&root.path().join("Old"), "6000.0.1f1");
    let changed = app.search(codebase, query).await.unwrap();
    assert!(changed.contains("UnityEngine.PhysicsMaterial"), "{changed}");
    assert!(!changed.contains("UnityEngine.PhysicMaterial"), "{changed}");
    editor(&root.path().join("Old"), "7000.0.1f1");
    let unsupported = app.search(codebase, query).await.unwrap();
    assert!(unsupported.contains("type unresolved"), "{unsupported}");
    assert!(
        unsupported.contains("unresolved native types"),
        "{unsupported}"
    );
    assert!(
        !unsupported.contains("UnityEngine.PhysicsMaterial"),
        "{unsupported}"
    );
    app.shutdown().await;
}

#[tokio::test]
async fn names_in_yaml_or_source_do_not_supply_native_ids_or_override_script_resolution() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    editor(root.path(), "6000.0.1f1");
    write(
        root.path(),
        "Game.csproj",
        "<Project><ItemGroup><Compile Include=\"Assets/*.cs\" /></ItemGroup></Project>",
    );
    write(
        root.path(),
        "Assets/Engine.cs",
        "namespace UnityEngine { public class Object {} public class Component : Object {} public class Camera : Object {} public class AudioFilter : Component {} public class SceneRoots : Component {} }",
    );
    write(
        root.path(),
        "Assets/Unknown.prefab",
        r#"--- !u!1 &1
GameObject:
  m_Name: Test
--- !u!20 &2
Camera:
  m_GameObject: {fileID: 1}
--- !u!9876543 &3
Camera:
  m_GameObject: {fileID: 1}
--- !u!181 &4
AudioFilter:
  m_GameObject: {fileID: 1}
--- !u!1660057539 &5
SceneRoots:
  m_GameObject: {fileID: 1}
--- !u!114 &6
MonoBehaviour:
  m_GameObject: {fileID: 1}
  m_Script: {fileID: 0}
"#,
    );
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    let components = app
        .search(codebase, "instance:UnityEngine.Component")
        .await
        .unwrap();
    let handles = object_handles(&components);
    assert_eq!(handles.len(), 1, "{components}");
    assert!(handles[0].ends_with("#2"), "{components}");
    assert!(
        components.contains("unresolved native types"),
        "{components}"
    );
    assert!(
        components.contains("unresolved script types"),
        "{components}"
    );
    for ty in [
        "UnityEngine.MonoBehaviour",
        "UnityEngine.AudioFilter",
        "UnityEngine.SceneRoots",
    ] {
        let result = app
            .search(codebase, &format!("instance:{ty}"))
            .await
            .unwrap();
        assert!(object_handles(&result).is_empty(), "{result}");
    }
    app.shutdown().await;
}

#[tokio::test]
async fn removed_unknown_native_objects_do_not_taint_the_live_composition() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    editor(root.path(), "6000.0.1f1");
    write(
        root.path(),
        "Assets/Source.prefab",
        "--- !u!1 &1\nGameObject:\n  m_Name: Root\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n--- !u!9876543 &3\nUnknown:\n  m_GameObject: {fileID: 1}\n",
    );
    write(
        root.path(),
        "Assets/Source.prefab.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    write(
        root.path(),
        "Assets/Scene.unity",
        r#"--- !u!1001 &10
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, type: 3}
  m_Modification:
    m_RemovedComponents:
    - {fileID: 3, guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, type: 3}
"#,
    );
    let app = app(root.path(), cache.path());
    let codebase = root.path().to_str().unwrap();
    let scene = app
        .search(
            codebase,
            "instance:UnityEngine.Component path:Assets/Scene.unity",
        )
        .await
        .unwrap();
    assert_eq!(object_handles(&scene).len(), 1, "{scene}");
    assert!(!scene.contains("Coverage is incomplete"), "{scene}");
    let source = app
        .search(
            codebase,
            "instance:UnityEngine.Component path:Assets/Source.prefab",
        )
        .await
        .unwrap();
    assert!(source.contains("unresolved native types"), "{source}");
    app.shutdown().await;
}
