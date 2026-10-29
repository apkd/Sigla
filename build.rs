#[path = "scripts/cargo_licenses.rs"]
mod cargo_licenses;
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

    let list: Report = serde_json::from_slice(&cargo_licenses::collect(output)?)?;
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
    fn sources(path: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    println!("cargo:rerun-if-changed=src");
    let mut files: Vec<PathBuf> = [
        "Cargo.toml",
        "Cargo.lock",
        "src/unity/assets/native/2022.3.json",
        "src/unity/assets/native/6000.0.json",
        "vendor/tree-sitter-c-sharp/Cargo.toml",
        "vendor/tree-sitter-c-sharp/tree-sitter.json",
        "vendor/tree-sitter-c-sharp/src/grammar.json",
        "vendor/tree-sitter-c-sharp/src/scanner.c",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    sources(Path::new("src"), &mut files);
    sources(
        Path::new("vendor/tree-sitter-c-sharp/bindings/rust"),
        &mut files,
    );
    files.sort();
    let mut analysis = Sha256::new();
    let mut workspace = Sha256::new();
    let mut assets = Sha256::new();
    let mut asset_records = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        let name = file.to_string_lossy();
        let mut bytes = fs::read(&file).unwrap();
        // Release numbers and build profiles do not change extracted facts.
        if name == "Cargo.lock" {
            let text = String::from_utf8(bytes).unwrap();
            bytes = text
                .split("[[package]]")
                .filter(|block| !block.starts_with("\nname = \"sigla\"\n"))
                .collect::<Vec<_>>()
                .join("[[package]]")
                .into_bytes();
        } else if name == "Cargo.toml" {
            let text = String::from_utf8(bytes).unwrap();
            let mut include = false;
            bytes = text
                .lines()
                .filter(|line| {
                    if line.starts_with('[') {
                        include = line.contains("dependencies") || *line == "[features]";
                    }
                    include
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into_bytes();
        }
        let facts = name.starts_with("Cargo.")
            || name.starts_with("src/binary/")
            || name.starts_with("vendor/tree-sitter-c-sharp/")
            || name.starts_with("src/extract/")
            || name.starts_with("src/native/") && !name.ends_with("jobs.rs")
            || matches!(
                name.as_ref(),
                "src/binary.rs"
                    | "src/model.rs"
                    | "src/metadata.rs"
                    | "src/query.rs"
                    | "src/minify.rs"
                    | "src/csharp/syntax.rs"
                    | "src/csharp/lower.rs"
                    | "src/csharp/types.rs"
                    | "src/store/data.rs"
                    | "src/store/format.rs"
                    | "src/store/payload.rs"
            );
        let graph = facts
            || name.starts_with("src/discovery")
            || name.starts_with("src/unity/") && !name.starts_with("src/unity/assets/")
            || name.starts_with("src/metadata_archive")
            || matches!(
                name.as_ref(),
                "src/workspace.rs"
                    | "src/msbuild.rs"
                    | "src/selection.rs"
                    | "src/documents.rs"
                    | "src/cache/packages.rs"
                    | "src/repository/selection.rs"
            );
        let objects = facts || name.starts_with("src/csharp/") || name.starts_with("src/unity/");
        let records = name.starts_with("Cargo.")
            || matches!(
                name.as_ref(),
                "src/unity/assets/parse.rs"
                    | "src/unity/assets/yaml.rs"
                    | "src/unity/assets/records.rs"
            );
        for (hash, include) in [
            (&mut analysis, facts),
            (&mut workspace, graph),
            (&mut assets, objects),
            (&mut asset_records, records),
        ] {
            if include {
                hash.update(name.as_bytes());
                hash.update([0]);
                hash.update(&bytes);
            }
        }
    }
    for file in [
        "managed/Program.cs",
        "managed/TrackedFileSystem.cs",
        "managed/PackageSnapshot.cs",
        "managed/Sigla.Discovery.csproj",
    ] {
        workspace.update(fs::read(file).unwrap());
    }
    // Each pair certifies equivalent output for one exact source snapshot.
    // Unlisted source hashes keep their own identity and invalidate old caches.
    // 2026-10-13: scope-stack extraction preserves Facts and FileSyntax bytes;
    // prefix-scan oracle tests also cover recovery, top-level code and directives.
    let analysis_aliases: &[(&str, &str)] = &[
        // Whole-file history validation leaves stored extraction unchanged.
        (
            "ae37a2d983a2cc170033623fc6f124731e1f735b0841a80579790ac8c3a441d4",
            "91b922abc95d4695d28d85e606778da6a68880c799c219ad4029b9406da5720e",
        ),
        // Typed decoding slices and closure copies preserve extracted records.
        (
            "68a677db02c7f51ee2630749b7c9047c8d29df04cf118f32331b6cd30526b15d",
            "91b922abc95d4695d28d85e606778da6a68880c799c219ad4029b9406da5720e",
        ),
        // Shared stream cursors and bounded work retain valid extracted records.
        (
            "b7d333ec38a4b11155238613efb0d2c67cecae2f2a4401883b980eca13c677cd",
            "91b922abc95d4695d28d85e606778da6a68880c799c219ad4029b9406da5720e",
        ),
        // Stream decoding and fingerprint coverage leave extracted records unchanged.
        (
            "e0bf102146b0b5c5f0f48fe43ccd8f5f2b5ede0cb49d08a95b215a6c14523af3",
            "91b922abc95d4695d28d85e606778da6a68880c799c219ad4029b9406da5720e",
        ),
        // Admission adds an existing executor dependency without changing records.
        (
            "10c610797834103c1b9d2b7377b57b5d8913e9f05e15e35deea5981fca8727eb",
            "3e651c01b11bb7f42ac56fd730d4129e648006878e5b70c16e5b55b514281f28",
        ),
        (
            "e96765a745ee254ff3a52072cfef53d3ae37d4418b1060000c473f5ac2a1c214",
            "629b05383367eea33b4dc3d889476d1322538c00db095b6028d7581ec525b2e0",
        ),
    ];
    // Planning reuses stamps only within a pass and retains fresh input fences.
    // Batched environment reads preserve the hash; archive changes retain valid
    // inputs and only alter publication or reject conflicting bundle paths.
    // Transport packs retain the same logical artifacts, analysis and discovery
    // inputs. Release placement and acquisition retries do not change the graph.
    // Removing an unused search-result renderer does not change discovery.
    let workspace_aliases: &[(&str, &str)] = &[
        // Whole-file history validation does not participate in discovery.
        (
            "75a8e2548bf5c30cc2b034e20078e1c38f1968922f9695bef151f9ac7214b95b",
            "70c5cc4ac529782cfe8fd9dc559070610499a687247dfe5cc1fc460d59679ca7",
        ),
        // Decoder and record validation cleanup leaves discovery unchanged.
        (
            "255e16b919e7729aa63b0d63e1d548df185f1f638b3ea697a26aa5eede5d9243",
            "70c5cc4ac529782cfe8fd9dc559070610499a687247dfe5cc1fc460d59679ca7",
        ),
        // Decoder safeguards retain the completed discovery graph.
        (
            "8706be81a0fe1c76713a24513b1f689911db713f90d97cc967417e7a4a7b7a54",
            "a75d85f257ecf40eadc848a217961e1687abaebbc976ceaba0fa945ab515757e",
        ),
        // Rust module admission changes scheduling, not discovered graphs.
        (
            "983e4bb01e38b324e2bd510f818134043945885b72f2494da79d0ce8eb63c6ac",
            "a75d85f257ecf40eadc848a217961e1687abaebbc976ceaba0fa945ab515757e",
        ),
        // Batching missing inputs changes retry count, not completed graphs.
        (
            "ba72b80198d72dd262e80ec34c9fdfdd37b3d514f302344a6a19111ea2fe87f4",
            "5c379c23f178ba88af622b3fafa87083358ff8bd9b08827b6c5377849295bec7",
        ),
        // Separate acquisition limits preserve Git package inputs and graphs.
        (
            "db0f9ad05ec046aae589a229652f4104ce9b51ae4d4a5ac02275820df4f092ac",
            "5c379c23f178ba88af622b3fafa87083358ff8bd9b08827b6c5377849295bec7",
        ),
        // Monitoring wraps metadata acquisition without changing its results.
        (
            "1eb8eea5c9250a83c1af0fafe95af48877fc8c78b9f9c7b14217249483853c96",
            "41b545ea1eefd8fe8f203afb2cffb377ec9727fa9303d97e8e5f215836bd629a",
        ),
        (
            "c817019d8f730eef42bb3f4ba0f3e544d0aef8f294914a96695dc89638822525",
            "441f5755af01b491e09beba20b3271ee63d640c3360af7722fc0110b4504e83a",
        ),
        (
            "3d502949e13e2973211eccd01d18c70bc072c0723e14bd371cd6afd751b24210",
            "441f5755af01b491e09beba20b3271ee63d640c3360af7722fc0110b4504e83a",
        ),
        (
            "f68ff5a894c70cd60a7bc188b48e92e99cb043d6126bb2a2248f6b2a7a1667a3",
            "41b545ea1eefd8fe8f203afb2cffb377ec9727fa9303d97e8e5f215836bd629a",
        ),
    ];
    // The first alias covers equivalent C# extraction. The second covers
    // query and memory changes: shared Arcs serialize as the same values,
    // and loading discards unused destination paths from old null references.
    let asset_aliases: &[(&str, &str)] = &[
        // Whole-file history validation leaves Unity extraction unchanged.
        (
            "703eec02f425de99745600157fad3f87125c6550620a149e49610d368cf5a55e",
            "983b0744503abab6eafe5c340af7b2bfbb9145041aeb9d822f00789598d4064a",
        ),
        // Decoder and record validation cleanup preserves asset data.
        (
            "ebcb5bb77cc53c8747b6f792fc3b2b74e3dab29541114a3c6939e4900700e895",
            "983b0744503abab6eafe5c340af7b2bfbb9145041aeb9d822f00789598d4064a",
        ),
        // Cancellation, cache ownership and timings leave asset records unchanged.
        (
            "a7b3926522dd62496e60de4af1d4575dbeaa76b9fe5ac8fef467ebf58f90ba14",
            "869730ab76207835787d55eae648d6a036e28062862726a67c18489c83f1b346",
        ),
        // Borrowed snapshot fields retain the owned format. Streaming reads and
        // ticket pins preserve completed indexes; composition round trips agree.
        (
            "ea070f539d3d34bea3d1b3d95023417b863aa63969b20b6bef17a2571f091e6a",
            "869730ab76207835787d55eae648d6a036e28062862726a67c18489c83f1b346",
        ),
        (
            "1b03b0f8d0bb24005b1f0a05be3b608f82606ec512db83aaba97ec4cd8fa130b",
            "145165ef7429e83ad739ef5d2fa1a944cd40f0f5ab500a66e444414329d5d777",
        ),
        (
            "4e7023452dc7ffc227db9038c975cbe4dae139896728112ea470aedf3924b4b2",
            "145165ef7429e83ad739ef5d2fa1a944cd40f0f5ab500a66e444414329d5d777",
        ),
        // Job telemetry and ticket metadata preserve asset records and publication.
        (
            "26a28624ba2bece70d061176d2d39dc606f13e622f677d9b30781ba4d079910f",
            "5ad4f718fe48de00884cf4f3dbac59e18027bae0fbc56653e0e1d42c723b42e3",
        ),
        (
            "21daf083db5802634df74c925bd21eae66fa116ae7308aa56092014bc6732de6",
            "2bd548c115a34ba087131a20b944888147f6a6847f8334c13d29dabfe3595787",
        ),
        (
            "333e9021e9eb5d33885b7d170ef38a10789a8bc119098451070885295013b1b3",
            "5ad4f718fe48de00884cf4f3dbac59e18027bae0fbc56653e0e1d42c723b42e3",
        ),
    ];
    // Content reads skip an unused summary decode; record writers and parsers
    // are unchanged, so existing records retain their identities.
    let asset_record_aliases: &[(&str, &str)] = &[
        // Cancellable content reads retain existing parser and record output.
        (
            "7144951c56df962a843143a217233821922003ca9b4b32bf095e5fddc70954a6",
            "e80e976471351cff9d66e10a7caf210cc8d87dfae64acb26493f128d189619d3",
        ),
        // Admission and streaming decode do not change record writers or parsing.
        (
            "9452a3c8a7a1c3cc5daf2e7d5e8a7694f06074199f0097edffcc0de0561ccb0a",
            "e80e976471351cff9d66e10a7caf210cc8d87dfae64acb26493f128d189619d3",
        ),
        (
            "a7d4ff626cdd6e550e6fe9f841794fc702e8224b08b1a630e9c743d908e57125",
            "7ea7722245f71bfaba49fbbbd82565b4addec86c63b81d19d10c2f9db7cacb76",
        ),
        (
            "2a314d4f98408707b51bcc64bb3fd0bb86c1dd8315db5a79a9768b799c589f82",
            "7ea7722245f71bfaba49fbbbd82565b4addec86c63b81d19d10c2f9db7cacb76",
        ),
    ];
    for (domain, hash, aliases) in [
        ("ANALYSIS", analysis, analysis_aliases),
        ("WORKSPACE", workspace, workspace_aliases),
        ("ASSET", assets, asset_aliases),
        ("ASSET_RECORD", asset_records, asset_record_aliases),
    ] {
        let source = format!("{:x}", hash.finalize());
        let version = aliases
            .iter()
            .find_map(|(raw, canonical)| (*raw == source.as_str()).then_some(*canonical))
            .unwrap_or(source.as_str());
        println!("cargo:rustc-env=SIGLA_{domain}_SOURCE_FINGERPRINT={source}");
        println!("cargo:rustc-env=SIGLA_{domain}_FINGERPRINT={version}");
    }
    for input in [
        "Cargo.toml",
        "Cargo.lock",
        "LICENSE",
        "about.toml",
        "scripts/native-libraries.json",
        "scripts/licenses.rs",
        "scripts/cargo_licenses.rs",
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
