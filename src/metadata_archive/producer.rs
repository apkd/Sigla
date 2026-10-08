use super::*;
use crate::{
    acquisition,
    unity::{ReleaseBranch, UnityVersion},
};
use clap::Subcommand;
use serde_json::Value;
use std::io::Write;

#[derive(Subcommand)]
pub enum Command {
    /// Select current editor releases.
    Plan {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = REPOSITORY)]
        repo: String,
        #[arg(long)]
        force: bool,
    },
    /// Extract and analyze one selected editor.
    Editor {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        index: usize,
        #[arg(long)]
        output: PathBuf,
    },
    /// Resolve registry packages from the selected editors.
    PlanPackages {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Extract and analyze a shard of selected packages.
    Packages {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        shard: usize,
        #[arg(long)]
        shards: usize,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify a binary bundle and its analysis records.
    Verify { path: PathBuf },
    /// Publish the complete selection and delete obsolete release assets.
    Publish {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        packages: PathBuf,
        #[arg(long)]
        input: PathBuf,
        #[arg(long, default_value = REPOSITORY)]
        repo: String,
    },
}
#[derive(Archive, Serialize, Deserialize)]
pub(super) struct Plan {
    pub editors: Vec<Origin>,
    pub previous: Catalog,
}

impl Command {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Plan {
                output,
                repo,
                force,
            } => plan(&output, &repo, force),
            Self::Editor {
                plan,
                index,
                output,
            } => {
                let plan: Plan = binary::decode(payload(&read(&plan)?)?)?;
                let editor = plan
                    .editors
                    .get(index)
                    .context("Editor index outside plan")?;
                editor_bundle(editor.clone(), &output)?;
                Ok(())
            }
            Self::PlanPackages {
                base,
                input,
                output,
            } => {
                let base: Plan = binary::decode(payload(&read(&base)?)?)?;
                let current = artifacts(&input)?;
                let editors = base
                    .editors
                    .iter()
                    .map(|origin| {
                        find(&current, &base.previous.artifacts, origin)
                            .cloned()
                            .with_context(|| format!("Missing editor output {}", origin.version))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut plan = registry::plan(&editors)?;
                plan.previous = base.previous;
                let pending = plan
                    .requests
                    .iter()
                    .filter(|r| find(&[], &plan.previous.artifacts, &r.origin).is_none())
                    .count();
                let shards = pending.div_ceil(24).min(32);
                save(&output, &binary::encode(&plan)?)?;
                matrix(
                    (0..shards)
                        .map(|shard| serde_json::json!({"shard":shard,"shards":shards}))
                        .collect(),
                )
            }
            Self::Packages {
                plan,
                shard,
                shards,
                output,
            } => {
                ensure!(shards > 0 && shard < shards, "Invalid package shard");
                fs::create_dir_all(&output)?;
                let plan: registry::Plan = binary::decode(payload(&read(&plan)?)?)?;
                let mut unavailable = Vec::new();
                let pending = plan
                    .requests
                    .iter()
                    .filter(|r| find(&[], &plan.previous.artifacts, &r.origin).is_none());
                for (index, request) in pending.enumerate() {
                    if index % shards != shard {
                        continue;
                    }
                    let temp = tempfile::tempdir()?;
                    let archive = temp.path().join("package.tgz");
                    let response = acquisition::client()?
                        .get(acquisition::https(&request.origin.url)?)
                        .send()?;
                    if matches!(response.status().as_u16(), 401 | 403 | 404) {
                        unavailable.push(format!(
                            "{}@{}: HTTP {}",
                            request.origin.name,
                            request.origin.version,
                            response.status().as_u16()
                        ));
                        continue;
                    }
                    acquisition::transfer(
                        response,
                        &mut fs::File::create(&archive)?,
                        request.origin.integrity.as_deref(),
                    )?;
                    let mut builder = bundle::Builder::new(
                        request.origin.clone(),
                        registry::profiles(&request.editors)?,
                    )?;
                    read_package(&archive, &mut builder, Some(&request.origin))?;
                    builder.finish(&output)?;
                }
                save(
                    &output.join(format!("unavailable-{shard}.bin")),
                    &binary::encode(&unavailable)?,
                )
            }
            Self::Verify { path } => {
                bundle::verify(&path, None)?;
                Ok(())
            }
            Self::Publish {
                base,
                packages,
                input,
                repo,
            } => {
                let plan: Plan = binary::decode(payload(&read(&base)?)?)?;
                let packages: registry::Plan = binary::decode(payload(&read(&packages)?)?)?;
                release::publish(&repo, plan, packages, &input)
            }
        }
    }
}
fn matrix(include: Vec<Value>) -> Result<()> {
    let matrix = serde_json::json!({"include": include});
    if let Some(path) = std::env::var_os("GITHUB_OUTPUT") {
        let mut output = fs::OpenOptions::new().append(true).open(path)?;
        writeln!(output, "matrix={matrix}")?;
        writeln!(
            output,
            "has_work={}",
            !matrix["include"].as_array().unwrap().is_empty()
        )?;
    } else {
        println!("{matrix}");
    }
    Ok(())
}
fn plan(output: &Path, repo: &str, force: bool) -> Result<()> {
    let previous = if force {
        Catalog::default()
    } else {
        match client::catalog(repo) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::info!("Building a fresh metadata archive: {error:#}");
                Catalog::default()
            }
        }
    };
    let client = acquisition::client()?;
    let mut editors = BTreeMap::<ReleaseBranch, (UnityVersion, Origin)>::new();
    let mut offset = 0;
    loop {
        let page: Value = client
            .get("https://services.api.unity.com/unity/editor/release/v1/releases")
            .query(&[
                ("platform", "LINUX".to_owned()),
                ("architecture", "X86_64".into()),
                ("limit", "25".into()),
                ("offset", offset.to_string()),
            ])
            .send()?
            .error_for_status()?
            .json()?;
        let results = page["results"]
            .as_array()
            .context("Unity release page has no results")?;
        let total = page["total"]
            .as_u64()
            .context("Unity release page has no total")?;
        ensure!(total <= 100_000, "Unity release inventory exceeds bound");
        for release in results {
            let Some(text) = release["version"].as_str() else {
                continue;
            };
            let Ok(version) = text.parse::<UnityVersion>() else {
                continue;
            };
            if version.branch
                < (ReleaseBranch {
                    major: 2021,
                    minor: 3,
                })
            {
                continue;
            }
            let downloads = release["downloads"]
                .as_array()
                .context("Release has no downloads")?;
            let Some(download) = downloads.iter().find(|d| {
                d["platform"] == "LINUX" && d["architecture"] == "X86_64" && d["type"] == "TAR_XZ"
            }) else {
                continue;
            };
            if editors
                .get(&version.branch)
                .is_some_and(|(v, _)| *v >= version)
            {
                continue;
            }
            editors.insert(
                version.branch,
                (
                    version,
                    Origin {
                        kind: "editor".into(),
                        name: "unity".into(),
                        version: text.into(),
                        revision: release["shortRevision"]
                            .as_str()
                            .context("Release revision missing")?
                            .into(),
                        url: download["url"]
                            .as_str()
                            .context("Editor URL missing")?
                            .into(),
                        integrity: download["integrity"].as_str().map(str::to_owned),
                    },
                ),
            );
        }
        offset += results.len();
        if offset as u64 >= total {
            break;
        }
        ensure!(!results.is_empty(), "Unity pagination stopped early");
    }
    ensure!(!editors.is_empty(), "No supported editors published");
    let plan = Plan {
        editors: editors.into_values().map(|(_, o)| o).collect(),
        previous,
    };
    let pending = plan
        .editors
        .iter()
        .enumerate()
        .filter(|(_, o)| find(&[], &plan.previous.artifacts, o).is_none())
        .map(|(index, o)| serde_json::json!({"index":index,"version":o.version}))
        .collect();
    save(output, &binary::encode(&plan)?)?;
    matrix(pending)
}
pub(super) fn find<'a>(
    fresh: &'a [Artifact],
    previous: &'a [Artifact],
    origin: &Origin,
) -> Option<&'a Artifact> {
    fresh.iter().chain(previous).find(|a| {
        let saved = &a.manifest.origin;
        saved.kind == origin.kind
            && saved.name == origin.name
            && saved.version == origin.version
            && saved.revision == origin.revision
            && saved.integrity == origin.integrity
    })
}
pub(super) fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            result.extend(files(&entry.path())?);
        } else if entry.file_type()?.is_file() {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(result)
}
pub(super) fn artifacts(input: &Path) -> Result<Vec<Artifact>> {
    files(input)?
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".tar.bin"))
        .map(|p| binary::decode(payload(&read(&p)?)?))
        .collect()
}
fn editor_bundle(origin: Origin, output: &Path) -> Result<Artifact> {
    let temp = tempfile::tempdir()?;
    let extracted = temp.path().join("editor");
    let retain = |path: &Path| {
        let Ok(path) = path.strip_prefix("Editor/Data") else {
            return bundle::license(path);
        };
        (path.starts_with("Resources/PackageManager/BuiltInPackages") && bundle::retained(path))
            || (path.starts_with("Resources/PackageManager/Editor")
                && (path.extension().is_some_and(|e| e == "tgz")
                    || path.file_name().is_some_and(|n| n == "manifest.json")))
            || path == Path::new("Resources/modules.asset")
            || reference_group(path).is_some()
            || bundle::license(path)
    };
    acquisition::download_extract(
        &acquisition::client()?,
        &origin.url,
        &extracted,
        origin.integrity.as_deref(),
        retain,
    )?;
    let mut builder = bundle::Builder::new(
        origin.clone(),
        registry::profiles(std::slice::from_ref(&origin.version))?,
    )?;
    for path in files(&extracted)? {
        let relative = path.strip_prefix(&extracted)?;
        let Ok(data) = relative.strip_prefix("Editor/Data") else {
            builder.add(
                format!("licenses/{}", relative.display()),
                fs::read(&path)?,
                "license",
            )?;
            continue;
        };
        if let Ok(package) = data.strip_prefix("Resources/PackageManager/BuiltInPackages") {
            if package.components().count() == 2
                && package.file_name().is_some_and(|n| n == "package.json")
            {
                let package = registry::package(&serde_json::from_slice(&fs::read(&path)?)?)?;
                builder.manifest.package_names.push(package.name.clone());
                builder.manifest.packages.push(package);
            }
            add_input(
                &mut builder,
                &path,
                format!(
                    "Editor/Data/Resources/PackageManager/BuiltInPackages/{}",
                    package.display()
                ),
                "package",
            )?;
        } else if data.starts_with("Resources/PackageManager/Editor")
            && data.extension().is_some_and(|e| e == "tgz")
        {
            read_package(&path, &mut builder, None)?;
        } else if data == Path::new("Resources/PackageManager/Editor/manifest.json") {
            let value: Value = serde_json::from_slice(&fs::read(&path)?)?;
            fn names(value: &Value, out: &mut Vec<String>) {
                match value {
                    Value::Object(values) => {
                        for (name, value) in values {
                            if name.starts_with("com.") {
                                out.push(name.clone());
                            }
                            names(value, out);
                        }
                    }
                    Value::Array(values) => {
                        for value in values {
                            names(value, out);
                        }
                    }
                    _ => {}
                }
            }
            names(&value, &mut builder.manifest.package_names);
            if let Some(packages) = value["packages"].as_object() {
                for (name, package) in packages {
                    if let Some(version) = package["version"].as_str() {
                        builder
                            .manifest
                            .recommended
                            .insert(name.clone(), version.into());
                    }
                }
            }
        } else if data == Path::new("Resources/modules.asset") {
            builder.add(
                "Editor/Data/Resources/modules.asset".into(),
                fs::read(&path)?,
                "configuration",
            )?;
        } else if let Some(group) = reference_group(data) {
            add_input(
                &mut builder,
                &path,
                format!("Editor/Data/{}", data.display()),
                group,
            )?;
        } else if bundle::license(data) {
            builder.add(
                format!("licenses/{}", data.display()),
                fs::read(&path)?,
                "license",
            )?;
        }
    }
    ensure!(
        builder
            .manifest
            .entries
            .iter()
            .any(|e| e.path.ends_with("UnityEngine.CoreModule.sigla"))
            && builder
                .manifest
                .entries
                .iter()
                .any(|e| e.group == "standard")
            && builder
                .manifest
                .entries
                .iter()
                .any(|e| e.group == "framework")
            && builder
                .manifest
                .entries
                .iter()
                .any(|e| e.path == "Editor/Data/Resources/modules.asset")
            && !builder.manifest.packages.is_empty(),
        "Editor archive lacks required references or packages"
    );
    builder.finish(output)
}
fn add_input(
    builder: &mut bundle::Builder,
    physical: &Path,
    logical: String,
    group: &str,
) -> Result<()> {
    if physical
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
        && !crate::metadata::is_managed(physical)?
    {
        return Ok(());
    }
    builder.add(logical, read(physical)?, group)
}
fn read_package(
    archive: &Path,
    builder: &mut bundle::Builder,
    expected: Option<&Origin>,
) -> Result<()> {
    let temp = tempfile::tempdir()?;
    acquisition::extract(fs::File::open(archive)?, temp.path(), bundle::retained)?;
    let root = if temp.path().join("package/package.json").is_file() {
        temp.path().join("package")
    } else {
        temp.path().to_owned()
    };
    let info = registry::package(&serde_json::from_slice(&fs::read(
        root.join("package.json"),
    )?)?)?;
    ensure!(
        expected.is_none_or(|o| info.name == o.name && info.version == o.version),
        "Package archive identity mismatch"
    );
    for path in files(&root)? {
        let prefix = if builder.manifest.origin.kind == "editor" {
            "Editor/Data/Resources/PackageManager/BuiltInPackages"
        } else {
            "packages"
        };
        add_input(
            builder,
            &path,
            format!(
                "{prefix}/{}/{}",
                info.name,
                path.strip_prefix(&root)?.display()
            ),
            "package",
        )?;
    }
    builder.manifest.package_names.push(info.name.clone());
    builder.manifest.packages.push(info);
    Ok(())
}
fn reference_group(path: &Path) -> Option<&'static str> {
    if path.extension().is_none_or(|e| e != "dll") {
        return None;
    }
    let text = path.to_string_lossy();
    if path.starts_with("Managed") {
        Some("editor")
    } else if path.starts_with("NetStandard") {
        Some("standard")
    } else if path.starts_with("UnityReferenceAssemblies")
        || path.starts_with("MonoBleedingEdge/lib/mono/4.7.1-api")
        || path.starts_with("MonoBleedingEdge/lib/mono/4.8-api")
    {
        Some("framework")
    } else if path.starts_with("PlaybackEngines/LinuxStandaloneSupport") {
        if text.contains("/il2cpp/") && text.contains("/Managed/") {
            Some("player-il2cpp")
        } else if text.contains("/mono/") && text.contains("/Managed/") {
            Some("player-mono")
        } else if text.ends_with(".Extensions.dll") {
            Some("editor")
        } else {
            None
        }
    } else if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("Unity.IL2CPP"))
    {
        Some("tools")
    } else {
        None
    }
}
