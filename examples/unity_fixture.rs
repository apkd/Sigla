//! Creates the controlled project used to record actual Unity compilation inputs.
use anyhow::{Context, Result, ensure};
use std::path::Path;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let root = Path::new(
        args.first()
            .context("Usage: unity_fixture DIRECTORY VERSION CONDUIT_PACKAGE")?,
    );
    let version: sigla::unity::UnityVersion =
        args.get(1).context("Missing Unity version")?.parse()?;
    let conduit = args.get(2).context("Missing Conduit package directory")?;
    ensure!(!root.exists(), "Fixture destination already exists");
    let files = [
        ("Assets/Runtime.cs", "public class Runtime {}\n"),
        (
            "Assets/Editor/EditorOnly.cs",
            "public class EditorOnly {}\n",
        ),
        ("Assets/Plugins/FirstPass.cs", "public class FirstPass {}\n"),
        (
            "Assets/Plugins/Editor/EditorFirstPass.cs",
            "public class EditorFirstPass {}\n",
        ),
        (
            "Assets/Custom/Custom.asmdef",
            r#"{"name":"Custom","rootNamespace":"NotAnImplicitNamespace"}"#,
        ),
        ("Assets/Custom/Custom.cs", "public class Custom {}\n"),
        (
            "Assets/Custom/Editor/StillCustom.cs",
            "public class StillCustom {}\n",
        ),
        (
            "Assets/Editor/ExportCompilation.cs",
            include_str!("../tests/unity-reference/ExportCompilation.cs"),
        ),
        (
            "ProjectSettings/ProjectSettings.asset",
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!129 &1\nPlayerSettings:\n  apiCompatibilityLevelPerPlatform:\n    Standalone: 6\n  scriptingBackend:\n    Standalone: 0\n  activeInputHandler: 0\n",
        ),
    ];
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, contents)?;
    }
    if args
        .get(3)
        .is_some_and(|a| matches!(a.as_str(), "advanced" | "compatibility"))
    {
        for (path, contents) in [
            (
                "Assets/Custom/Custom.asmdef.meta",
                "fileFormatVersion: 2\nguid: 72cc48b908e44813a8ddc6b5500d7048\n",
            ),
            ("Assets/NameRef/Ref.asmref", r#"{"reference":"Custom"}"#),
            (
                "Assets/NameRef/AssignedByName.cs",
                "public class AssignedByName {}",
            ),
            (
                "Assets/GuidRef/Ref.asmref",
                r#"{"reference":"GUID:72cc48b908e44813a8ddc6b5500d7048"}"#,
            ),
            (
                "Assets/GuidRef/AssignedByGuid.cs",
                "public class AssignedByGuid {}",
            ),
            (
                "Assets/Inactive/Inactive.asmdef",
                r#"{"name":"Inactive","defineConstraints":["NEVER_DEFINED"]}"#,
            ),
            ("Assets/Inactive/Inactive.cs", "public class Inactive {}"),
            (
                "Assets/EditorAssembly/EditorAssembly.asmdef",
                r#"{"name":"EditorAssembly","includePlatforms":["Editor"]}"#,
            ),
            (
                "Assets/EditorAssembly/OnlyEditor.cs",
                "public class OnlyEditor {}",
            ),
            (
                "Assets/PlayerAssembly/PlayerAssembly.asmdef",
                r#"{"name":"PlayerAssembly","includePlatforms":["LinuxStandalone64"]}"#,
            ),
            (
                "Assets/PlayerAssembly/OnlyPlayer.cs",
                "public class OnlyPlayer {}",
            ),
            (
                "Assets/Standalone/Standalone.asmdef",
                r#"{"name":"Standalone","autoReferenced":false,"noEngineReferences":true}"#,
            ),
            (
                "Assets/Standalone/Standalone.cs",
                "public class Standalone {}",
            ),
            (
                "Assets/Versioned/Versioned.asmdef",
                r#"{"name":"Versioned","references":["Custom"],"versionDefines":[{"name":"Unity","expression":"[2022.3,6001)","define":"VERSION_MATCH"}],"defineConstraints":["VERSION_MATCH","!NEVER_DEFINED || ALSO_NEVER"]}"#,
            ),
            (
                "Assets/Versioned/Versioned.cs",
                "public class Versioned : Custom {}",
            ),
            ("Assets/Versioned/csc.rsp", "-define:RESPONSE_LOCAL\n"),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, contents)?;
        }
    }
    std::fs::write(
        root.join("ProjectSettings/ProjectVersion.txt"),
        format!("m_EditorVersion: {version}\n"),
    )?;
    std::fs::create_dir_all(root.join("Packages"))?;
    std::fs::write(
        root.join("Packages/manifest.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"dependencies":{"dev.tryfinally.conduit":format!("file:{conduit}")}}),
        )?,
    )?;
    if args.get(3).is_some_and(|a| a == "compatibility") {
        compatibility(
            root,
            Path::new(
                args.get(4)
                    .context("Compatibility fixtures require a compiled plugin directory")?,
            ),
        )?;
    }
    std::fs::write(
        root.join("capture-request.json"),
        serde_json::to_vec(&serde_json::json!({"directory":root.join("captures"),"stage":0}))?,
    )?;
    println!(
        "Start this fixture with the Unity restart tool, then remove Conduit from its manifest. The exporter captures both profiles and presets after compilation settles."
    );
    Ok(())
}

fn compatibility(root: &Path, plugins: &Path) -> Result<()> {
    let write = |path: &str, contents: &str| -> Result<()> {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, contents)?;
        Ok(())
    };
    for (name, explicit, editor, player, constraint) in [
        ("Automatic", false, true, true, ""),
        ("Explicit", true, true, true, ""),
        ("Editor", false, true, false, ""),
        ("Player", false, false, true, ""),
        ("Disabled", false, true, true, "NEVER_DEFINED"),
    ] {
        let path = format!("Assets/ManagedPlugins/Fixture{name}.dll");
        std::fs::create_dir_all(root.join("Assets/ManagedPlugins"))?;
        std::fs::copy(
            plugins.join(name).join(format!("Fixture{name}.dll")),
            root.join(&path),
        )?;
        let guid = blake3::hash(name.as_bytes()).to_hex()[..32].to_owned();
        write(
            &format!("{path}.meta"),
            &format!(
                "fileFormatVersion: 2\nguid: {guid}\nPluginImporter:\n  serializedVersion: 2\n  isExplicitlyReferenced: {}\n  validateReferences: 1\n  defineConstraints: {}\n  platformData:\n  - first:\n      Any: \n    second:\n      enabled: 0\n      settings: {{}}\n  - first:\n      Editor: Editor\n    second:\n      enabled: {}\n      settings:\n        CPU: AnyCPU\n        OS: Linux\n  - first:\n      Standalone: Linux64\n    second:\n      enabled: {}\n      settings:\n        CPU: AnyCPU\n",
                u8::from(explicit),
                if constraint.is_empty() {
                    "[]".into()
                } else {
                    format!("[{constraint}]")
                },
                u8::from(editor),
                u8::from(player)
            ),
        )?;
    }
    write(
        "Assets/Explicit/Explicit.asmdef",
        r#"{"name":"ExplicitConsumer","overrideReferences":true,"precompiledReferences":["FixtureExplicit.dll"]}"#,
    )?;
    write(
        "Assets/Explicit/Explicit.cs",
        "public class ExplicitConsumer {}",
    )?;
    write(
        "Assets/NoPlugins/NoPlugins.asmdef",
        r#"{"name":"NoPlugins","overrideReferences":true}"#,
    )?;
    write("Assets/NoPlugins/NoPlugins.cs", "public class NoPlugins {}")?;
    for (name, location) in [("embedded", "Packages"), ("local", "FixturePackages")] {
        for variant in ["enabled", "unlisted"] {
            let package = format!("com.example.{name}.{variant}");
            let directory = format!("{location}/{package}");
            write(
                &format!("{directory}/package.json"),
                &format!(r#"{{"name":"{package}","version":"1.0.0"}}"#),
            )?;
            write(
                &format!("{directory}/Runtime/Runtime.asmdef"),
                &format!(r#"{{"name":"{name}.{variant}.Runtime"}}"#),
            )?;
            write(
                &format!("{directory}/Runtime/Runtime.cs"),
                &format!("namespace {name}.{variant} {{ public class Runtime {{}} }}"),
            )?;
            write(
                &format!("{directory}/Tests/Tests.asmdef"),
                &format!(
                    r#"{{"name":"{name}.{variant}.Tests","defineConstraints":["UNITY_INCLUDE_TESTS"],"autoReferenced":false}}"#
                ),
            )?;
            write(
                &format!("{directory}/Tests/Tests.cs"),
                &format!("namespace {name}.{variant} {{ public class Tests {{}} }}"),
            )?;
        }
    }
    // Tiny controlled assemblies isolate Unity's historical UI implicit-reference
    // rule without storing the entire UI package in comparison fixtures.
    write(
        "Packages/com.unity.ugui/package.json",
        r#"{"name":"com.unity.ugui","version":"1.0.0"}"#,
    )?;
    for (directory, name, platforms) in [
        ("Runtime", "UnityEngine.UI", "[]"),
        ("Editor", "UnityEditor.UI", r#"["Editor"]"#),
    ] {
        write(
            &format!("Packages/com.unity.ugui/{directory}/UI.asmdef"),
            &format!(r#"{{"name":"{name}","includePlatforms":{platforms}}}"#),
        )?;
        write(
            &format!("Packages/com.unity.ugui/{directory}/UI.cs"),
            &format!("namespace {name} {{ public class ControlledUI {{}} }}"),
        )?;
    }
    let path = root.join("Packages/manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    manifest["dependencies"]["com.unity.modules.physics"] = "1.0.0".into();
    for variant in ["enabled", "unlisted"] {
        manifest["dependencies"][format!("com.example.local.{variant}")] =
            format!("file:../FixturePackages/com.example.local.{variant}").into();
    }
    manifest["testables"] =
        serde_json::json!(["com.example.embedded.enabled", "com.example.local.enabled"]);
    std::fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}
