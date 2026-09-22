//! Pack observed Unity exports and editor layout metadata for offline comparisons.
use anyhow::{Result, ensure};
use std::{collections::BTreeSet, fs, path::Path};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 3,
        "Usage: unity_reference CAPTURES EDITOR_DATA OUTPUT.tar.zst"
    );
    let captures = Path::new(&args[0]);
    let editor = Path::new(&args[1]);
    let mut archive = tar::Builder::new(zstd::Encoder::new(fs::File::create(&args[2])?, 19)?);
    for scope in ["", "advanced", "compatibility"] {
        for name in [
            "editor-standard.json",
            "editor-framework.json",
            "player-standard.json",
            "player-framework.json",
        ] {
            let relative = Path::new(scope).join(name);
            append(
                &mut archive,
                &relative,
                &fs::read(captures.join(&relative))?,
            )?;
        }
    }
    // Record actual directory inventories, including DLLs that Unity does not
    // reference. Empty placeholders suffice: discovery never executes these DLLs.
    let mut inventory = BTreeSet::new();
    for group in [
        "Managed/UnityEngine",
        "NetStandard",
        "UnityReferenceAssemblies/unity-4.8-api",
        "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
    ] {
        collect(editor, &editor.join(group), &mut inventory)?;
    }
    for path in [
        "Managed/UnityEditor.Graphs.dll",
        "PlaybackEngines/LinuxStandaloneSupport/UnityEditor.LinuxStandalone.Extensions.dll",
    ] {
        ensure!(
            editor.join(path).is_file(),
            "Missing editor reference {path}"
        );
        inventory.insert(path.into());
    }
    append(
        &mut archive,
        Path::new("editor-inventory.json"),
        &serde_json::to_vec(&inventory)?,
    )?;
    append(
        &mut archive,
        Path::new("modules.asset"),
        &fs::read(editor.join("Resources/modules.asset"))?,
    )?;
    for entry in fs::read_dir(editor.join("Resources/PackageManager/BuiltInPackages"))? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with("com.unity.modules.")
        {
            append(
                &mut archive,
                &Path::new("builtins")
                    .join(entry.file_name())
                    .join("package.json"),
                &fs::read(entry.path().join("package.json"))?,
            )?;
        }
    }
    archive.into_inner()?.finish()?;
    Ok(())
}

fn collect(root: &Path, path: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect(root, &entry.path(), files)?;
        } else if entry.path().extension().is_some_and(|e| e == "dll") {
            files.insert(entry.path().strip_prefix(root)?.to_str().unwrap().into());
        }
    }
    Ok(())
}

fn append<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    path: &Path,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o600);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes)?;
    Ok(())
}
