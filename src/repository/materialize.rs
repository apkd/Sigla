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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    DefaultBranch,
    Branch(String),
    Commit(String),
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub repository: String,
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
        .env("GIT_NO_LAZY_FETCH", "1");
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
) -> Result<()> {
    let scratch = tempfile::tempdir_in(store.join("objects/pack"))?;
    let pack = scratch.path().join("incoming.pack");
    session.pack(objects, store, depth_one, &mut File::create(&pack)?)?;
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
    Ok(())
}

pub fn prepare(request: &Request) -> Result<Prepared> {
    prepare_with(request, Session::connect)
}

fn prepare_with(
    request: &Request,
    connect: impl Fn(&Repository) -> Result<Session>,
) -> Result<Prepared> {
    let repository =
        Repository::parse(&request.repository)?.context("Expected a repository identifier")?;
    ensure!(
        repository.branch.is_none(),
        "Internal transport must not contain a fragment"
    );
    let mut session = connect(&repository)?;
    let (branch, revision) = match &request.target {
        Target::Commit(id) => (None, parse_id(id)?),
        Target::Branch(branch) => {
            super::validate_branch(branch)?;
            let name = format!("refs/heads/{branch}");
            let refs = session.refs(std::slice::from_ref(&name))?;
            let id = refs
                .into_iter()
                .find_map(|r| match r {
                    gix_protocol::handshake::Ref::Direct {
                        full_ref_name,
                        object,
                    } if full_ref_name.as_slice() == name.as_bytes() => Some(object),
                    _ => None,
                })
                .context("Requested upstream branch is unavailable")?;
            (Some(branch.clone()), id)
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
    receive(&mut session, &request.store, vec![revision], true)?;
    drop(session);
    let listing = tempfile::tempfile()?;
    let outcome = crate::process::capture(
        git(&request.store)
            .args(["ls-tree", "-r", "-t", "-z"])
            .arg(revision.to_string()),
        Duration::from_secs(120),
        None,
        Some(listing.try_clone()?),
    )?;
    ensure!(
        outcome.status.success(),
        "Cannot enumerate fetched repository tree"
    );
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
            receive(&mut session, &request.store, missing, false)?;
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
            repository: "https://example.invalid/team/repo".into(),
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
