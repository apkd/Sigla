use sigla::{discovery::Policy, service::App};
use std::{path::Path, sync::Arc};
fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}
#[tokio::test]
async fn script_instances_inheritance_views_and_refresh() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Game.csproj",
        "<Project><ItemGroup><Compile Include=\"Assets/*.cs\" /></ItemGroup></Project>",
    );
    write(
        root.path(),
        "ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.0.1f1\n",
    );
    write(
        root.path(),
        "Assets/Engine.cs",
        "namespace UnityEngine { public class Object {} public class Component : Object {} public class MonoBehaviour : Component {} public class ScriptableObject : Object {} }",
    );
    write(
        root.path(),
        "Assets/Base.cs",
        "public class Base : UnityEngine.MonoBehaviour {}",
    );
    write(
        root.path(),
        "Assets/Health.cs",
        "public class Health : Base {}",
    );
    write(
        root.path(),
        "Assets/Health.cs.meta",
        "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );
    let prefab = format!(
        "--- !u!1 &1\nGameObject:\n  m_Name: Enemy\n--- !u!114 &2\nMonoBehaviour:\n  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, type: 3}}\n  weapon: {{fileID: 21, guid: cccccccccccccccccccccccccccccccc, type: 2}}\n  stats:\n    label: HitPoints\n    payload: '{}'\n",
        "x".repeat(10_000)
    );
    write(root.path(), "Assets/Enemy.prefab", &prefab);
    write(
        root.path(),
        "Assets/Enemy.prefab.meta",
        "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
    );
    for (name, guid) in [
        ("Sword", "cccccccccccccccccccccccccccccccc"),
        ("Bow", "dddddddddddddddddddddddddddddddd"),
    ] {
        write(
            root.path(),
            &format!("Assets/{name}.mat"),
            &format!("--- !u!21 &21\nMaterial:\n  m_Name: {name}\n"),
        );
        write(
            root.path(),
            &format!("Assets/{name}.mat.meta"),
            &format!("guid: {guid}\n"),
        );
    }
    write(
        root.path(),
        "Assets/Level.unity",
        "--- !u!1001 &10\nPrefabInstance:\n  m_SourcePrefab: {fileID: 100100000, guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb, type: 3}\n  m_Modification:\n    m_Modifications:\n    - target: {fileID: 2, guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb, type: 3}\n      propertyPath: weapon\n      value: \n      objectReference: {fileID: 21, guid: dddddddddddddddddddddddddddddddd, type: 2}\n",
    );
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap(),
    );
    let project = root
        .path()
        .join("Game.csproj")
        .to_string_lossy()
        .into_owned();
    let result = app.search(&project, "instance:Base").await.unwrap();
    assert!(result.contains("Health"), "{result}");
    assert!(result.contains("Level.unity"), "{result}");
    assert!(
        !app.search(&project, "instance:Base type-match:exact")
            .await
            .unwrap()
            .contains("unity@")
    );
    let handle = result
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("unity@")
                .map(|s| format!("unity@{s}"))
        })
        .unwrap();
    let view = app.view(&project, &handle, "exact").await.unwrap();
    assert!(view.contains("HitPoints"), "{view}");
    assert!(view.contains("omitted"), "{view}");
    assert!(view.len() < prefab.len());
    let bow = app
        .search(
            &project,
            "references:Assets/Bow.mat path:Assets/Level.unity",
        )
        .await
        .unwrap();
    assert!(bow.contains("Health"), "{bow}");
    let sword = app
        .search(
            &project,
            "references:Assets/Sword.mat path:Assets/Level.unity",
        )
        .await
        .unwrap();
    assert!(!sword.contains("Health"), "{sword}");
    assert!(
        app.search(&project, "references:Assets/Enemy.prefab")
            .await
            .unwrap()
            .contains("Level.unity")
    );
    let text = app.search(&project, "text:HitPoints").await.unwrap();
    assert!(text.contains("Enemy.prefab"), "{text}");
    write(
        root.path(),
        "Assets/Enemy.prefab",
        &prefab.replace("HitPoints", "ChangedLabel"),
    );
    let changed = app.search(&project, "text:ChangedLabel").await.unwrap();
    assert!(changed.contains("Enemy.prefab"), "{changed}");
    assert!(
        app.browse(&project, "Assets")
            .await
            .unwrap()
            .contains("Enemy.prefab")
    );
    app.shutdown().await;
}

#[tokio::test]
async fn scriptable_objects_stay_in_their_project_and_follow_asset_moves_and_deletions() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (project, name) in [("One", "Weapon"), ("Two", "Potion")] {
        let directory = root.path().join(project);
        write(
            &directory,
            "Game.csproj",
            "<Project><ItemGroup><Compile Include=\"Assets/*.cs\" /></ItemGroup></Project>",
        );
        write(
            &directory,
            "ProjectSettings/ProjectVersion.txt",
            "m_EditorVersion: 6000.0.1f1\n",
        );
        write(
            &directory,
            "Assets/Engine.cs",
            "namespace UnityEngine { public class Object {} public class ScriptableObject : Object {} }",
        );
        write(
            &directory,
            &format!("Assets/{name}.cs"),
            &format!("public class {name} : UnityEngine.ScriptableObject {{}}"),
        );
        // Reusing GUIDs is valid across separate Unity projects.
        write(
            &directory,
            &format!("Assets/{name}.cs.meta"),
            "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );
        write(
            &directory,
            "Assets/Data.asset",
            &format!(
                "--- !u!114 &1\nMonoBehaviour:\n  m_Name: {name}Data\n  m_Script: {{fileID: 11500000, guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, type: 3}}\n"
            ),
        );
        write(
            &directory,
            "Assets/Data.asset.meta",
            "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        );
        write(
            &directory,
            "Assets/Holder.asset",
            "--- !u!114 &2\nMonoBehaviour:\n  m_Name: Holder\n  target: {fileID: 1, guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb, type: 2}\n",
        );
    }
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap(),
    );
    let codebase = root.path().to_str().unwrap();
    let all = app
        .search(codebase, "instance:UnityEngine.ScriptableObject")
        .await
        .unwrap();
    for project in ["One", "Two"] {
        assert!(
            all.contains(&format!("{project}/Assets/Data.asset")),
            "{all}"
        );
    }
    let weapon = app.search(codebase, "instance:Weapon").await.unwrap();
    assert!(weapon.contains("One/Assets/Data.asset"), "{weapon}");
    assert!(!weapon.contains("Two/Assets"), "{weapon}");
    let scoped = app
        .search(
            codebase,
            "instance:UnityEngine.ScriptableObject unity-project:Two",
        )
        .await
        .unwrap();
    assert!(scoped.contains("Potion"), "{scoped}");
    assert!(!scoped.contains("One/Assets"), "{scoped}");
    let references = app
        .search(codebase, "references:One/Assets/Data.asset")
        .await
        .unwrap();
    assert!(
        references.contains("One/Assets/Holder.asset"),
        "{references}"
    );
    assert!(!references.contains("Two/Assets"), "{references}");

    for suffix in ["", ".meta"] {
        std::fs::rename(
            root.path().join(format!("One/Assets/Data.asset{suffix}")),
            root.path().join(format!("One/Assets/Moved.asset{suffix}")),
        )
        .unwrap();
    }
    let moved = app.search(codebase, "instance:Weapon").await.unwrap();
    assert!(moved.contains("One/Assets/Moved.asset"), "{moved}");
    assert!(!moved.contains("One/Assets/Data.asset"), "{moved}");
    assert!(
        app.search(codebase, "references:One/Assets/Moved.asset")
            .await
            .unwrap()
            .contains("Holder.asset")
    );
    std::fs::remove_file(root.path().join("One/Assets/Moved.asset")).unwrap();
    std::fs::remove_file(root.path().join("One/Assets/Moved.asset.meta")).unwrap();
    let deleted = app.search(codebase, "instance:Weapon").await.unwrap();
    assert!(!deleted.contains("unity@"), "{deleted}");
    assert!(
        app.search(codebase, "instance:Potion")
            .await
            .unwrap()
            .contains("Two/Assets/Data.asset")
    );
    app.shutdown().await;
}
