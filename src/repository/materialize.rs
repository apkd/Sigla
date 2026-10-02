//! Private Git stores and explicit, raw, batched content extraction.
use super::{
    Repository,
    selection::{Selection, validate_path},
    transport::Session,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    DefaultBranch,
    Branch(String),
    Tag(String),
    Named(String),
    Commit(String),
}

impl Target {
    fn unavailable(&self, name: &str) -> String {
        let kind = match self {
            Self::Branch(_) => "branch",
            Self::Tag(_) => "tag",
            _ => "branch or tag",
        };
        let mut message = format!("Requested upstream {kind} {name:?} is unavailable.");
        if matches!(self, Self::Named(_))
            && (4..64).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_hexdigit())
        {
            message.push_str(" Abbreviated commit IDs are not supported; use the full commit ID.");
        }
        if name.ends_with('"') {
            message.push_str(" The selector ends with a double quote, possibly encoded as %22; remove it when unintended.");
        }
        message
    }

    pub fn selector(value: &str) -> Result<Self> {
        super::validate_branch(value)?;
        if let Some(name) = value.strip_prefix("refs/heads/") {
            super::validate_branch(name)?;
            Ok(Self::Branch(name.into()))
        } else if let Some(name) = value.strip_prefix("refs/tags/") {
            super::validate_branch(name)?;
            Ok(Self::Tag(name.into()))
        } else if matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self::Commit(value.to_ascii_lowercase()))
        } else {
            ensure!(
                !value.starts_with("refs/"),
                "Only refs/heads/ and refs/tags/ selectors are supported"
            );
            Ok(Self::Named(value.into()))
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub repository: String,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default)]
    pub preferred_transport: Option<String>,
    pub target: Target,
    pub store: PathBuf,
    pub staging: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub required: Vec<String>,
    pub additional: Vec<String>,
    pub previous: BTreeMap<String, String>,
    pub subdirectory: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prepared {
    #[serde(default)]
    pub transfer_bytes: u64,
    #[serde(default)]
    pub unavailable: BTreeSet<String>,
    #[serde(default)]
    pub transport: Option<String>,
    pub branch: Option<String>,
    pub revision: String,
    pub selected: BTreeMap<String, String>,
    pub tracked: BTreeMap<String, String>,
    pub directories: BTreeSet<String>,
}

fn git(store: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-lazy-fetch",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .arg("--git-dir")
        .arg(store)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_SHALLOW_FILE");
    command
}

fn run(command: &mut Command) -> Result<Vec<u8>> {
    crate::process::run(command, Duration::from_secs(120))
}

fn parse_id(value: &str) -> Result<gix_hash::ObjectId> {
    gix_hash::ObjectId::from_hex(value.as_bytes()).context("Invalid Git object identity")
}

fn receive(
    session: &mut Session,
    store: &Path,
    objects: Vec<gix_hash::ObjectId>,
    depth_one: bool,
) -> Result<u64> {
    let scratch = tempfile::tempdir_in(store.join("objects/pack"))?;
    let pack = scratch.path().join("incoming.pack");
    session.pack(objects, store, depth_one, &mut File::create(&pack)?)?;
    let bytes = fs::metadata(&pack)?.len();
    let index = scratch.path().join("incoming.idx");
    let result = run(git(store)
        .args(["index-pack", "--index-version=2", "-o"])
        .arg(&index)
        .arg(&pack))?;
    let id = String::from_utf8(result)?.trim().to_owned();
    parse_id(&id)?;
    let destination = store.join("objects/pack").join(format!("pack-{id}"));
    // The marker precedes publication so local inspection never attempts to repair omitted blobs.
    File::create(destination.with_extension("promisor"))?;
    fs::rename(index, destination.with_extension("idx"))?;
    fs::rename(pack, destination.with_extension("pack"))?;
    Ok(bytes)
}

/// Return false for absent commit/tree data; never confuse that with missing blobs.
/// Reuse the temporary listing across a failed local check and a filtered fetch.
fn cached_tree_inventory(store: &Path, revision: &str, listing: &File) -> Result<bool> {
    use std::io::{Seek, SeekFrom};
    let kind = crate::process::capture(
        git(store).args(["cat-file", "-t", revision]),
        Duration::from_secs(120),
        None,
        None,
    )?;
    if !kind.status.success() {
        return Ok(false);
    }
    ensure!(
        kind.stdout == b"commit\n",
        "Selected revision does not point to a commit"
    );
    let mut output = listing.try_clone()?;
    output.set_len(0)?;
    output.seek(SeekFrom::Start(0))?;
    let result = crate::process::capture(
        git(store).args(["ls-tree", "-r", "-t", "-z", revision]),
        Duration::from_secs(120),
        None,
        Some(output),
    )?;
    ensure!(
        listing.metadata()?.len() <= 256 * 1024 * 1024,
        "Repository inventory exceeds size limit"
    );
    Ok(result.status.success())
}

pub fn prepare(request: &Request) -> Result<Prepared> {
    prepare_with(request, |repository| {
        Session::connect(
            repository,
            request.preferred_transport.as_deref(),
            request.allow_private,
        )
    })
}

fn prepare_with(
    request: &Request,
    connect: impl Fn(&Repository) -> Result<Session>,
) -> Result<Prepared> {
    let repository =
        Repository::parse(&request.repository)?.context("Expected a repository identifier")?;
    ensure!(
        repository.selector.is_none(),
        "Internal transport must not contain a fragment"
    );
    let mut session = connect(&repository)?;
    let (branch, revision) = match &request.target {
        Target::Commit(id) => (None, parse_id(id)?),
        Target::Branch(name) | Target::Tag(name) | Target::Named(name) => {
            super::validate_branch(name)?;
            let names = match &request.target {
                Target::Branch(_) => vec![format!("refs/heads/{name}")],
                Target::Tag(_) => vec![format!("refs/tags/{name}")],
                // Match Git's short-name precedence: tags before branches.
                _ => vec![format!("refs/tags/{name}"), format!("refs/heads/{name}")],
            };
            let refs = session.refs(&names)?;
            let (resolved, id) = names
                .iter()
                .find_map(|name| {
                    refs.iter().find_map(|r| match r {
                        gix_protocol::handshake::Ref::Direct {
                            full_ref_name,
                            object,
                        }
                        | gix_protocol::handshake::Ref::Peeled {
                            full_ref_name,
                            object,
                            ..
                        }
                        | gix_protocol::handshake::Ref::Symbolic {
                            full_ref_name,
                            object,
                            ..
                        } if full_ref_name.as_slice() == name.as_bytes() => Some((name, *object)),
                        _ => None,
                    })
                })
                .with_context(|| request.target.unavailable(name))?;
            (resolved.strip_prefix("refs/heads/").map(str::to_owned), id)
        }
        Target::DefaultBranch => {
            let refs = session.refs(&["HEAD".into()])?;
            let (name, id) = refs
                .into_iter()
                .find_map(|r| match r {
                    gix_protocol::handshake::Ref::Symbolic {
                        full_ref_name,
                        target,
                        object,
                        ..
                    } if full_ref_name.as_slice() == b"HEAD" => Some((target, object)),
                    _ => None,
                })
                .context("Repository does not advertise a default branch")?;
            let name = std::str::from_utf8(&name)?
                .strip_prefix("refs/heads/")
                .context("Default reference is not a branch")?
                .to_owned();
            super::validate_branch(&name)?;
            return Ok(Prepared {
                transfer_bytes: 0,
                unavailable: BTreeSet::new(),
                transport: session.endpoint.clone(),
                branch: Some(name),
                revision: id.to_string(),
                selected: BTreeMap::new(),
                tracked: BTreeMap::new(),
                directories: BTreeSet::new(),
            });
        }
    };
    if !request.store.join("HEAD").is_file() {
        fs::create_dir_all(&request.store)?;
        run(Command::new("git")
            .args(["init", "--bare", "--template=", "--quiet"])
            .arg(if revision.to_string().len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            })
            .arg(&request.store))?;
    }
    let listing = tempfile::tempfile()?;
    let mut transfer_bytes = 0;
    if !cached_tree_inventory(&request.store, &revision.to_string(), &listing)? {
        transfer_bytes += receive(&mut session, &request.store, vec![revision], true).context(
            "Cannot fetch selected revision; the server may disallow fetching unadvertised commits",
        )?;
        ensure!(
            cached_tree_inventory(&request.store, &revision.to_string(), &listing)?,
            "Cannot enumerate fetched repository tree"
        );
    }
    let transport = session.endpoint.clone();
    drop(session);
    use std::io::{Seek, SeekFrom};
    let mut listing = listing;
    listing.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(listing);
    let mut record = Vec::new();
    let mut tracked = BTreeMap::new();
    let mut directories = BTreeSet::new();
    while reader.read_until(0, &mut record)? != 0 {
        let text = std::str::from_utf8(record.strip_suffix(&[0]).context("Invalid tree record")?)?;
        let (header, path) = text.split_once('\t').context("Invalid tree record")?;
        validate_path(path)?;
        let fields: Vec<_> = header.split(' ').collect();
        ensure!(fields.len() == 3, "Invalid tree record");
        parse_id(fields[2])?;
        if fields[0] == "040000" && Path::new(path).file_name().is_some_and(|n| n == "Assets") {
            directories.insert(path.to_owned());
        }
        if matches!(fields[0], "100644" | "100755") && fields[1] == "blob" {
            tracked.insert(path.to_owned(), fields[2].to_owned());
        }
        record.clear();
    }
    let selection = Selection::new(&request.include, &request.exclude)?;
    let unity_roots: Vec<_> = directories
        .iter()
        .filter_map(|assets| {
            let root = Path::new(assets).parent().unwrap();
            tracked
                .contains_key(root.join("ProjectSettings/ProjectVersion.txt").to_str()?)
                .then(|| root.to_owned())
        })
        .collect();
    let mut selected: BTreeMap<_, _> = tracked
        .iter()
        .filter(|(path, _)| {
            selection.selected_in(path, &unity_roots)
                && request
                    .subdirectory
                    .as_ref()
                    .is_none_or(|prefix| Path::new(path).starts_with(prefix))
        })
        .map(|(a, b)| (a.clone(), b.clone()))
        .collect();
    for path in &request.required {
        selection.require(path)?;
        selected.insert(
            path.clone(),
            tracked
                .get(path)
                .with_context(|| format!("Required input is absent from repository: {path}"))?
                .clone(),
        );
    }
    for path in &request.additional {
        if let Some(id) = tracked.get(path) {
            selection.require(path)?;
            selected.insert(path.clone(), id.clone());
        }
    }
    // Detect excluded mandatory Unity metadata even when the remaining source tree could be indexed.
    for assets in &directories {
        let root = Path::new(assets).parent().unwrap();
        let version = root
            .join("ProjectSettings/ProjectVersion.txt")
            .to_string_lossy()
            .into_owned();
        if tracked.contains_key(&version) {
            for relative in [
                "ProjectSettings/ProjectVersion.txt",
                "ProjectSettings/ProjectSettings.asset",
                "Packages/manifest.json",
                "Packages/packages-lock.json",
            ] {
                selection.require(&root.join(relative).to_string_lossy())?;
            }
        }
    }
    let changed: BTreeMap<_, _> = selected
        .iter()
        .filter(|(path, id)| request.previous.get(*path) != Some(*id))
        .map(|(p, i)| (p.clone(), i.clone()))
        .collect();
    let ids: BTreeSet<_> = changed.values().cloned().collect();
    for chunk in ids.iter().collect::<Vec<_>>().chunks(2048) {
        let input = chunk
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let status = crate::process::capture(
            git(&request.store)
                .arg("cat-file")
                .arg("--batch-check=%(objectname) %(objecttype)"),
            Duration::from_secs(120),
            Some(input),
            None,
        )?;
        ensure!(
            status.status.success(),
            "Cannot inspect selected Git objects"
        );
        let missing = String::from_utf8(status.stdout)?
            .lines()
            .filter_map(|line| line.strip_suffix(" missing"))
            .map(parse_id)
            .collect::<Result<Vec<_>>>()?;
        if !missing.is_empty() {
            let mut session = connect(&repository)?;
            transfer_bytes += receive(&mut session, &request.store, missing, false)?;
        }
    }
    fs::create_dir_all(&request.staging)?;
    for chunk in changed.iter().collect::<Vec<_>>().chunks(2048) {
        let contents = tempfile::tempfile()?;
        let input = chunk
            .iter()
            .map(|(_, id)| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let result = crate::process::capture(
            git(&request.store).args(["cat-file", "--batch"]),
            Duration::from_secs(120),
            Some(input),
            Some(contents.try_clone()?),
        )?;
        ensure!(result.status.success(), "Cannot read selected Git contents");
        let mut contents = contents;
        contents.seek(SeekFrom::Start(0))?;
        let mut contents = BufReader::new(contents);
        for (path, id) in chunk {
            let mut header = String::new();
            contents.read_line(&mut header)?;
            let fields: Vec<_> = header.split_whitespace().collect();
            ensure!(
                fields.len() == 3 && fields[0] == id.as_str() && fields[1] == "blob",
                "Selected Git object is unavailable"
            );
            let size: u64 = fields[2].parse()?;
            let target = request.staging.join(path);
            fs::create_dir_all(target.parent().unwrap())?;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            ensure!(
                std::io::copy(&mut contents.by_ref().take(size), &mut file)? == size,
                "Truncated Git blob"
            );
            let mut delimiter = [0];
            contents.read_exact(&mut delimiter)?;
            ensure!(delimiter == [b'\n'], "Invalid Git blob delimiter");
        }
    }
    Ok(Prepared {
        transfer_bytes,
        unavailable: BTreeSet::new(),
        transport,
        branch,
        revision: revision.to_string(),
        selected,
        tracked,
        directories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(path: &Path, filter: bool) {
        run(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main", "--template="])
            .arg(path))
        .unwrap();
        fs::write(path.join("Code.cs"), "class Searchable {}\n").unwrap();
        fs::write(path.join("asset.bin"), vec![123; 1024 * 1024]).unwrap();
        run(Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["add", "Code.cs", "asset.bin"]))
        .unwrap();
        run(Command::new("git").arg("-C").arg(path).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]))
        .unwrap();
        if filter {
            run(Command::new("git").arg("-C").arg(path).args([
                "config",
                "uploadpack.allowFilter",
                "true",
            ]))
            .unwrap();
        }
        run(Command::new("git").arg("-C").arg(path).args([
            "config",
            "uploadpack.allowAnySHA1InWant",
            "true",
        ]))
        .unwrap();
    }

    fn request(root: &Path) -> Request {
        Request {
            allow_private: false,
            repository: "https://example.invalid/team/repo".into(),
            preferred_transport: None,
            target: Target::Branch("main".into()),
            store: root.join("store"),
            staging: root.join("stage"),
            include: vec![],
            exclude: vec![],
            required: vec![],
            additional: vec![],
            previous: BTreeMap::new(),
            subdirectory: None,
        }
    }

    #[test]
    fn selectors_materialize_the_selected_commit() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let execute = |args: &[&str]| {
            run(Command::new("git")
                .arg("-C")
                .arg(&upstream_path)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                ])
                .args(args))
            .unwrap()
        };
        let old = String::from_utf8(execute(&["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        execute(&["tag", "release"]);
        execute(&["tag", "-a", "annotated", "-m", "release"]);
        execute(&["tag", "-a", "nested", "annotated", "-m", "nested release"]);
        fs::write(upstream_path.join("Code.cs"), "class Updated {}\n").unwrap();
        execute(&["commit", "--quiet", "-am", "update"]);
        execute(&["branch", "release"]);
        execute(&["branch", "face1234"]);
        execute(&["branch", "quoted\""]);
        let current = String::from_utf8(execute(&["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        for (index, (selector, expected, revision)) in [
            ("release", "class Searchable {}\n", old.as_str()),
            ("refs/tags/release", "class Searchable {}\n", old.as_str()),
            ("annotated", "class Searchable {}\n", old.as_str()),
            ("nested", "class Searchable {}\n", old.as_str()),
            (old.as_str(), "class Searchable {}\n", old.as_str()),
            ("refs/heads/release", "class Updated {}\n", current.as_str()),
            ("main", "class Updated {}\n", current.as_str()),
            ("face1234", "class Updated {}\n", current.as_str()),
            ("quoted\"", "class Updated {}\n", current.as_str()),
        ]
        .into_iter()
        .enumerate()
        {
            let mut request = request(&root.path().join(index.to_string()));
            request.target = Target::selector(selector).unwrap();
            let prepared = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
            assert_eq!(prepared.revision, revision);
            assert_eq!(
                fs::read_to_string(request.staging.join("Code.cs")).unwrap(),
                expected
            );
        }
        let tree = String::from_utf8(execute(&["rev-parse", "HEAD^{tree}"])).unwrap();
        execute(&["tag", "tree", tree.trim()]);
        for selector in ["missing", "tree"] {
            let mut request = request(&root.path().join(selector));
            request.target = Target::selector(selector).unwrap();
            assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
            assert!(!request.staging.exists());
        }
        for selector in ["deadbeef", "missing\"", "ordinary"] {
            let mut request = request(&root.path().join("unavailable"));
            request.target = Target::selector(selector).unwrap();
            let error = prepare_with(&request, |_| Session::local(&upstream_path))
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("{selector:?}")), "{error}");
            assert_eq!(error.contains("Abbreviated"), selector == "deadbeef");
            assert_eq!(error.contains("double quote"), selector.ends_with('"'));
        }
    }

    #[test]
    fn partial_acquisition_never_stores_excluded_blobs() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let request = request(root.path());
        let prepared = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(
            fs::read_to_string(request.staging.join("Code.cs")).unwrap(),
            "class Searchable {}\n"
        );
        assert!(!request.staging.join("asset.bin").exists());
        let asset = &prepared.tracked["asset.bin"];
        let status = crate::process::capture(
            git(&request.store).args(["cat-file", "-e", asset]),
            Duration::from_secs(10),
            None,
            None,
        )
        .unwrap();
        assert!(
            !status.status.success(),
            "excluded contents must not reach the object database"
        );
        assert!(request.store.join("shallow").is_file());
    }

    #[test]
    fn shared_objects_avoid_transfers_across_selectors() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        run(Command::new("git")
            .arg("-C")
            .arg(&upstream_path)
            .args(["tag", "release"]))
        .unwrap();
        let mut request = request(root.path());
        let first = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert!(first.transfer_bytes > 0);
        for (index, selector) in [
            Target::Tag("release".into()),
            Target::Commit(first.revision.clone()),
        ]
        .into_iter()
        .enumerate()
        {
            request.target = selector;
            request.staging = root.path().join(format!("stage-{index}"));
            let next = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
            assert_eq!(next.transfer_bytes, 0);
            assert_eq!(next.selected, first.selected);
            assert_eq!(
                fs::read(request.staging.join("Code.cs")).unwrap(),
                fs::read(root.path().join("stage/Code.cs")).unwrap()
            );
        }
    }

    #[test]
    fn unsupported_filtering_aborts_before_pack_transfer() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, false);
        let request = request(root.path());
        assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
        assert_eq!(
            fs::read_dir(request.store.join("objects/pack"))
                .unwrap()
                .count(),
            0
        );
        assert!(!request.staging.exists());
    }
}
