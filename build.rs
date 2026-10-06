#[path = "scripts/licenses.rs"]
mod licenses;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn cargo_notices(output: &Path) -> Result<Vec<licenses::Notice>> {
    #[derive(Deserialize)]
    struct Report {
        licenses: Vec<License>,
    }
    #[derive(Deserialize)]
    struct License {
        id: String,
        text: String,
        source_path: Option<String>,
        used_by: Vec<Usage>,
    }
    #[derive(Deserialize)]
    struct Usage {
        #[serde(rename = "crate")]
        krate: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        name: String,
        version: String,
        manifest_path: PathBuf,
    }

    let json = output.join("cargo-licenses.json");
    ensure!(
        Command::new(env::var_os("CARGO").context("Missing CARGO")?)
            .args([
                "about", "generate", "--locked", "--fail", "--format", "json", "--target"
            ])
            .arg(env::var("TARGET")?)
            .arg("-o")
            .arg(&json)
            .status()?
            .success(),
        "Cannot collect Cargo licenses; install cargo-about 0.9.2"
    );
    let list: Report = serde_json::from_slice(&fs::read(json)?)?;
    let mut notices = Vec::new();
    let mut packages = BTreeMap::new();
    for license in list.licenses {
        // Generic templates can omit required owners. Apache has no owner field.
        ensure!(
            license.source_path.is_some() || license.id == "Apache-2.0",
            "Missing original {} notice for {:?}",
            license.id,
            license
                .used_by
                .iter()
                .map(|usage| &usage.krate.name)
                .collect::<Vec<_>>()
        );
        for usage in license.used_by {
            if usage.krate.name == "sigla" {
                continue;
            }
            let root = usage
                .krate
                .manifest_path
                .parent()
                .context("Missing crate root")?;
            let name = format!("{} {}", usage.krate.name, usage.krate.version);
            packages.insert(root.to_owned(), name.clone());
            let source = license.source_path.as_deref().map(Path::new);
            let source_name = source
                .map(|path| {
                    path.strip_prefix(root)
                        .unwrap_or_else(|_| {
                            if path.is_absolute() {
                                Path::new(path.file_name().unwrap())
                            } else {
                                path
                            }
                        })
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| "Apache-2.0 license template".into());
            let original = source
                .map(|path| root.join(path))
                .filter(|path| path.is_file())
                .map(fs::read_to_string)
                .transpose()?;
            // Keep the original bytes when cargo-about selected a whole file.
            // Explicit source-header selections in about.toml remain selections.
            let text = original
                .filter(|text| text.split_whitespace().eq(license.text.split_whitespace()))
                .unwrap_or_else(|| license.text.clone());
            notices.push(licenses::Notice {
                name,
                source: source_name,
                text,
                license: license.id.clone(),
                note: None,
            });
        }
    }
    for (root, name) in packages {
        notices.extend(licenses::package_notices(&root, &name)?);
        if name.starts_with("aws-lc-rs ") {
            let inherited = licenses::isc_source_notices(&root, &name)?;
            ensure!(
                !inherited.is_empty(),
                "Missing inherited ISC notices for {name}"
            );
            notices.extend(inherited);
        }
        // This upstream release makes additional licensing statements here.
        // Preserve them without deciding how they interact with its LICENSE.
        if name.starts_with("dotscope ") {
            let readme = fs::read_to_string(root.join("README.md"))?;
            let policy = readme
                .split_once("### Responsible Use Policy\n")
                .context("Missing upstream use policy")?
                .1
                .split_once("\n## Acknowledgments")
                .context("Missing policy boundary")?
                .0;
            notices.push(licenses::Notice {
                name,
                source: "README.md (Responsible Use Policy excerpt)".into(),
                text: policy.trim().into(),
                license: String::new(),
                note: Some(
                    "The upstream README identifies Apache 2.0 licensing and also includes \
                    this policy. Its relationship to the supplied LICENSE, reproduced separately, \
                    is unresolved. This excerpt records an upstream statement; Sigla does not \
                    impose it as an additional condition. Reproducing it does not determine \
                    whether it is binding."
                        .into(),
                ),
            });
        }
    }
    Ok(notices)
}

#[derive(Deserialize)]
struct NativeSource {
    version: Option<String>,
    url: String,
    sha256: String,
}

fn archive_source(
    output: &Path,
    name: &str,
    source: &NativeSource,
    files: &[String],
) -> Result<PathBuf> {
    let archive = output.join(format!("{}.tar", source.sha256));
    let temporary = archive.with_extension("tmp");
    if !archive.exists() {
        ensure!(
            Command::new("curl")
                .args([
                    "-fsSL",
                    "--connect-timeout",
                    "15",
                    "--max-time",
                    "120",
                    "--retry",
                    "3"
                ])
                .arg(&source.url)
                .arg("-o")
                .arg(&temporary)
                .status()?
                .success(),
            "Cannot download {name}"
        );
    }
    let bytes = fs::read(if archive.exists() {
        &archive
    } else {
        &temporary
    })?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == source.sha256,
        "Checksum mismatch for {name}"
    );
    if !archive.exists() {
        fs::rename(temporary, &archive)?;
    }
    let root = output.join(format!("{name}-{}", source.sha256));
    fs::create_dir_all(&root)?;
    ensure!(
        Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(&root)
            .arg("--strip-components=1")
            .args(files)
            .status()?
            .success(),
        "Cannot extract {name}"
    );
    Ok(root)
}

fn build_licenses(output: &Path) -> Result<()> {
    let mut notices = cargo_notices(output)?;
    let sources: BTreeMap<String, NativeSource> =
        serde_json::from_slice(&fs::read("scripts/native-libraries.json")?)?;
    for library in ["libarchive", "zstd", "musl"] {
        let source = &sources[library];
        let root = archive_source(output, library, source, &[])?;
        let name = format!(
            "{library} {}",
            source
                .version
                .as_deref()
                .context("Missing native library version")?
        );
        notices.extend(licenses::native_notices(&root, library, &name)?);
    }
    let version = Command::new(env::var_os("RUSTC").context("Missing RUSTC")?)
        .arg("--version")
        .output()?;
    ensure!(version.status.success(), "Cannot identify Rust version");
    let version = String::from_utf8(version.stdout)?.trim().to_owned();
    let release = version
        .split_whitespace()
        .nth(1)
        .context("Missing Rust release number")?;
    // License documents are platform-independent; use one official archive on every host.
    let archive = format!("rustc-{release}-x86_64-unknown-linux-gnu");
    let url = format!("https://static.rust-lang.org/dist/{archive}.tar.xz");
    let checksum_path = output.join(format!("{archive}.sha256"));
    let checksum = if checksum_path.exists() {
        fs::read_to_string(&checksum_path)?
    } else {
        let checksum = Command::new("curl")
            .args([
                "-fsSL",
                "--connect-timeout",
                "15",
                "--max-time",
                "120",
                "--retry",
                "3",
            ])
            .arg(format!("{url}.sha256"))
            .output()?;
        ensure!(
            checksum.status.success(),
            "Cannot download checksum for {archive}"
        );
        let checksum = String::from_utf8(checksum.stdout)?;
        fs::write(&checksum_path, &checksum)?;
        checksum
    };
    let source = NativeSource {
        version: Some(release.into()),
        url,
        sha256: checksum
            .split_whitespace()
            .next()
            .context("Missing Rust archive checksum")?
            .into(),
    };
    let files = ["COPYRIGHT-library.html", "licenses/Unicode-3.0.txt"]
        .map(|file| format!("{archive}/rustc/share/doc/rust/{file}"));
    let docs =
        archive_source(output, "rust-licenses", &source, &files)?.join("rustc/share/doc/rust");
    let read_document = |name: &str| -> Result<String> {
        let path = docs.join(name);
        fs::read_to_string(&path)
            .with_context(|| format!("Cannot read Rust license document {}", path.display()))
    };
    let runtime = read_document("COPYRIGHT-library.html")?;
    let apache = regex::Regex::new(
        r"(?s)<summary><code>LICENSE-APACHE</code></summary>\s*<pre>(.*?)</pre>",
    )?
    .captures(&runtime)
    .context("Rust runtime Apache license was not found")?[1]
        .to_string();
    let apache = html_escape::decode_html_entities(&apache).into_owned();
    let target = env::var("TARGET")?;
    let runtime_name = format!("Rust standard library {release} ({target})");
    let target_libdir = Command::new(env::var_os("RUSTC").context("Missing RUSTC")?)
        .args(["--print", "target-libdir", "--target", &target])
        .output()?;
    ensure!(
        target_libdir.status.success(),
        "Cannot locate Rust target libraries"
    );
    let libraries: BTreeSet<_> = fs::read_dir(String::from_utf8(target_libdir.stdout)?.trim())?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|name| name.ends_with(".rlib"))
        .filter_map(|name| {
            name.strip_prefix("lib")?
                .split_once('-')
                .map(|(name, _)| name.to_owned())
        })
        .collect();
    let package = regex::Regex::new(r"^(.+?)-(\d.*)$")?;
    let documents =
        regex::Regex::new(r"(?s)<summary><code>(.*?)</code></summary>\s*<pre>(.*?)</pre>")?;
    for section in runtime.split("<h3>📦 ").skip(1) {
        let (label, section) = section
            .split_once("</h3>")
            .context("Incomplete Rust dependency entry")?;
        let package = package
            .captures(label)
            .context("Unrecognized Rust dependency entry")?;
        if !libraries.contains(&package[1].replace('-', "_")) {
            continue;
        }
        let files: Vec<_> = documents.captures_iter(section).collect();
        let selected = files
            .iter()
            .find(|file| file[1].to_ascii_lowercase().contains("mit"))
            .or_else(|| {
                files
                    .iter()
                    .find(|file| file[1].to_ascii_lowercase().contains("apache"))
            });
        for file in &files {
            let is_license = file[1].to_ascii_lowercase().starts_with("license");
            if is_license && selected.is_some_and(|selected| selected[1] != file[1]) {
                continue;
            }
            let license = if file[1].to_ascii_lowercase().contains("mit") {
                "MIT"
            } else if file[1].to_ascii_lowercase().contains("apache") {
                "Apache-2.0"
            } else {
                ""
            };
            notices.push(licenses::Notice {
                name: format!("Rust {release} / {label}"),
                source: format!("COPYRIGHT-library.html / {}", &file[1]),
                license: license.into(),
                text: html_escape::decode_html_entities(&file[2]).into_owned(),
                note: None,
            });
        }
    }
    notices.push(licenses::Notice {
        name: runtime_name.clone(),
        source: "Apache-2.0 option".into(),
        license: "Apache-2.0".into(),
        text: apache.clone(),
        note: None,
    });
    notices.push(licenses::Notice {
        name: format!("{runtime_name} / Unicode data"),
        source: "licenses/Unicode-3.0.txt".into(),
        text: read_document("licenses/Unicode-3.0.txt")?,
        license: "Unicode-3.0".into(),
        note: None,
    });
    let inventory = runtime
        .split_once("<h2 id=\"in-tree-files\">")
        .context("Missing Rust source inventory")?
        .1
        .split_once("<h2 id=\"out-of-tree-dependencies\">")
        .context("Missing Rust dependency inventory")?
        .0;
    let tags = regex::Regex::new("<[^>]+>")?;
    let inventory = tags.replace_all(inventory, "");
    let inventory = html_escape::decode_html_entities(&inventory);
    let inventory = inventory
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    notices.push(licenses::Notice {
        name: runtime_name,
        source: "COPYRIGHT-library.html (in-tree inventory excerpt, rendered as text; includes other targets)".into(),
        license: String::new(),
        text: inventory,
        note: None,
    });
    // Keep source records beside Cargo's build output for review and comparison.
    fs::write(
        output.join("license-sources.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({ "rustc": version, "target": target, "native_sources": serde_json::from_slice::<serde_json::Value>(&fs::read("scripts/native-libraries.json")?)?, "notices": notices }),
        )?,
    )?;
    fs::write(
        output.join("licenses.txt"),
        licenses::render(&notices, &apache, &fs::read_to_string("LICENSE")?),
    )?;
    Ok(())
}

fn main() {
    for input in [
        "Cargo.toml",
        "Cargo.lock",
        "LICENSE",
        "about.toml",
        "scripts/native-libraries.json",
        "scripts/licenses.rs",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    build_licenses(&out_dir)
        .unwrap_or_else(|error| panic!("Cannot generate bundled licenses: {error:#}"));

    println!("cargo:rerun-if-changed=managed/Program.cs");
    println!("cargo:rerun-if-changed=managed/TrackedFileSystem.cs");
    println!("cargo:rerun-if-changed=managed/PackageSnapshot.cs");
    println!("cargo:rerun-if-changed=managed/Sigla.Discovery.csproj");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("managed");
    let status = Command::new("dotnet")
        .args([
            "build",
            "managed/Sigla.Discovery.csproj",
            "--configuration",
            "Release",
            "--nologo",
            "--verbosity",
            "quiet",
            "--output",
        ])
        .arg(&output)
        .arg(format!(
            "-p:BaseIntermediateOutputPath={}/",
            output.join("obj").display()
        ))
        .status()
        .expect("Building Sigla's embedded MSBuild integration requires a .NET SDK");
    assert!(
        status.success(),
        "Cannot build embedded MSBuild integration"
    );
}
