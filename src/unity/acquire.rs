use super::{ReleaseBranch, UnityVersion, catalog::Editor};
use crate::{acquisition, repository::manager::write_json};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};

const FORMAT: u32 = 3;
#[derive(Clone, Deserialize, Serialize)]
struct Pin {
    version: String,
    revision: String,
    url: String,
    integrity: Option<String>,
}
#[derive(Deserialize, Serialize)]
struct Complete {
    format: u32,
    version: String,
    inventory: Vec<String>,
}
#[derive(Deserialize)]
struct Page {
    total: usize,
    results: Vec<Release>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    version: String,
    short_revision: String,
    downloads: Vec<Download>,
}
#[derive(Deserialize)]
struct Download {
    platform: String,
    architecture: String,
    url: String,
    integrity: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

fn resolve(branch: ReleaseBranch) -> Result<Pin> {
    let client = acquisition::client()?;
    let mut offset = 0;
    let mut best: Option<(UnityVersion, Pin)> = None;
    loop {
        let response = client
            .get("https://services.api.unity.com/unity/editor/release/v1/releases")
            .query(&[
                ("version", branch.to_string()),
                ("platform", "LINUX".into()),
                ("architecture", "X86_64".into()),
                ("limit", "25".into()),
                ("offset", offset.to_string()),
            ])
            .send()?
            .error_for_status()?;
        let page: Page = response.json()?;
        ensure!(
            page.total <= 100_000,
            "Unity release inventory is too large"
        );
        let count = page.results.len();
        for release in page.results {
            let Ok(version) = release.version.parse::<UnityVersion>() else {
                continue;
            };
            if version.branch != branch || !version.stable() {
                continue;
            }
            let Some(download) = release.downloads.into_iter().find(|d| {
                d.platform == "LINUX" && d.architecture == "X86_64" && d.kind == "TAR_XZ"
            }) else {
                continue;
            };
            if best.as_ref().is_none_or(|(current, _)| version > *current) {
                best = Some((
                    version,
                    Pin {
                        version: release.version,
                        revision: release.short_revision,
                        url: download.url,
                        integrity: download.integrity,
                    },
                ));
            }
        }
        offset += count;
        if offset >= page.total {
            break;
        }
        ensure!(count != 0, "Unity release pagination ended prematurely");
    }
    best.map(|(_, pin)| pin)
        .context("No stable Linux editor is available in the declared Unity release branch")
}

fn retained(path: &Path) -> bool {
    let Ok(path) = path.strip_prefix("Editor/Data") else {
        return false;
    };
    if path.starts_with("Resources/PackageManager/BuiltInPackages") {
        return acquisition::analysis_input(path);
    }
    if path == Path::new("Resources/modules.asset") {
        return true;
    }
    if path.extension().is_none_or(|e| e != "dll") {
        return false;
    }
    [
        "Managed",
        "NetStandard/ref",
        "NetStandard/shims",
        "NetStandard/compat",
        "NetStandard/Extensions",
        "UnityReferenceAssemblies",
        "MonoBleedingEdge/lib/mono/4.7.1-api",
        "MonoBleedingEdge/lib/mono/4.8-api",
    ]
    .iter()
    .any(|p| path.starts_with(p))
        || path.starts_with("PlaybackEngines/LinuxStandaloneSupport")
            && (path.to_string_lossy().contains("/Managed/")
                || path
                    .file_name()
                    .is_some_and(|n| n == "UnityEditor.LinuxStandalone.Extensions.dll"))
}

pub fn prefetch(cache: &Path, branch: ReleaseBranch) -> Result<PathBuf> {
    let root = cache.join("editors").join(branch.to_string());
    acquisition::shared(&root.to_string_lossy(), || {
        fs::create_dir_all(&root)?;
        let pin_file = root.join("selection.json");
        let pin: Pin = if pin_file.is_file() {
            serde_json::from_slice(&fs::read(&pin_file)?)?
        } else {
            let pin = resolve(branch)?;
            write_json(&pin_file, &pin)?;
            pin
        };
        let version: UnityVersion = pin.version.parse()?;
        ensure!(
            version.branch == branch && version.stable(),
            "Pinned editor selection is invalid"
        );
        let complete = root.join("complete.json");
        let content = root.join("contents");
        if complete.is_file() {
            let state: Complete = serde_json::from_slice(&fs::read(&complete)?)?;
            if state.format == FORMAT
                && state.version == pin.version
                && state.inventory.iter().all(|p| content.join(p).is_file())
            {
                return Ok(content.join("Editor/Data"));
            }
        }
        let staging = tempfile::Builder::new()
            .prefix("extract-")
            .tempdir_in(&root)?;
        let mut archive = tempfile::tempfile_in(&root)?;
        acquisition::download(
            &acquisition::client()?,
            &pin.url,
            &mut archive,
            pin.integrity.as_deref(),
        )?;
        archive.seek(SeekFrom::Start(0))?;
        let inventory =
            acquisition::extract(xz2::read::XzDecoder::new(archive), staging.path(), retained)?;
        validate(&staging.path().join("Editor/Data"), version)?;
        if content.exists() {
            fs::remove_dir_all(&content)?;
        }
        fs::rename(staging.path(), &content)?;
        write_json(
            &complete,
            &Complete {
                format: FORMAT,
                version: pin.version,
                inventory,
            },
        )?;
        Ok(content.join("Editor/Data"))
    })
}

pub fn inspect_archive(
    archive: &Path,
    destination: &Path,
    version: UnityVersion,
) -> Result<Vec<String>> {
    fs::create_dir_all(destination)?;
    let inventory = acquisition::extract(
        xz2::read::XzDecoder::new(File::open(archive)?),
        destination,
        retained,
    )?;
    validate(&destination.join("Editor/Data"), version)?;
    Ok(inventory)
}

fn validate(data: &Path, version: UnityVersion) -> Result<()> {
    ensure!(
        matches!(
            (version.branch.major, version.branch.minor),
            (2022, 3) | (6000, 3)
        ),
        "Unsupported Unity editor bundle branch {version}"
    );
    for path in [
        "Managed/UnityEngine/UnityEngine.CoreModule.dll",
        "Managed/UnityEditor.dll",
        "NetStandard/ref/2.1.0/netstandard.dll",
        "UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll",
        "Resources/PackageManager/BuiltInPackages",
    ] {
        ensure!(
            data.join(path).exists(),
            "Incomplete Unity editor bundle: missing {path}"
        );
    }
    super::catalog::validate_references(data, version)?;
    Ok(())
}

pub fn editor(project: &Path, cache: &Path) -> Result<Editor> {
    let text = fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?;
    let declared: UnityVersion = text
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .context("Unity project has no declared editor version")?
        .trim()
        .parse()?;
    let data = prefetch(cache, declared.branch)?;
    let pin: Pin = serde_json::from_slice(&fs::read(
        cache
            .join("editors")
            .join(declared.branch.to_string())
            .join("selection.json"),
    )?)?;
    Ok(Editor {
        declared,
        selected: pin.version.parse()?,
        data,
        declared_revision: text
            .lines()
            .find_map(|l| l.strip_prefix("m_EditorVersionWithRevision:"))
            .map(|s| s.trim().to_owned()),
        selected_revision: Some(pin.revision),
    })
}
