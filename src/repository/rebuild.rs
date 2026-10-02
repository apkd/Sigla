//! Network-free selected-object pool rebuilding in a bounded child process.
use super::cache::Retention;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::Path,
    process::Command,
    time::Duration,
};

const COMPLETE: &[u8] = b"sigla-selected-git-pool-1\n";
#[derive(Serialize, Deserialize)]
struct Job {
    source: std::path::PathBuf,
    target: std::path::PathBuf,
    roots: Retention,
}
fn git(store: &Path) -> Command {
    let mut c = Command::new("git");
    c.args([
        "--no-lazy-fetch",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
        "--git-dir",
    ])
    .arg(store)
    .env("GIT_NO_LAZY_FETCH", "1")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env_remove("GIT_DIR")
    .env_remove("GIT_WORK_TREE")
    .env_remove("GIT_OBJECT_DIRECTORY")
    .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
    .env_remove("GIT_COMMON_DIR")
    .env_remove("GIT_SHALLOW_FILE");
    c
}
fn run(c: &mut Command) -> Result<Vec<u8>> {
    crate::process::run(c, Duration::from_secs(120))
}
fn oid(id: &str) -> Result<()> {
    ensure!(
        matches!(id.len(), 40 | 64) && id.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid Git object ID"
    );
    Ok(())
}
fn kinds(store: &Path, ids: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
    let mut present = BTreeMap::new();
    let ids: Vec<_> = ids.iter().collect();
    for batch in ids.chunks(2048) {
        for id in batch {
            oid(id)?;
        }
        let input = batch
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let output = crate::process::capture(
            git(store).args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]),
            Duration::from_secs(120),
            Some(input),
            None,
        )?;
        ensure!(output.status.success(), "Cannot inspect retained objects");
        let text = String::from_utf8(output.stdout)?;
        let rows: Vec<_> = text.lines().collect();
        ensure!(rows.len() == batch.len(), "Truncated object check response");
        for (row, expected) in rows.into_iter().zip(batch) {
            let (returned, kind) = row
                .split_once(' ')
                .context("Invalid object check response")?;
            ensure!(
                returned == expected.as_str(),
                "Unexpected object check response"
            );
            if kind != "missing" {
                present.insert(returned.to_owned(), kind.to_owned());
            }
        }
    }
    Ok(present)
}
fn tree_ids(store: &Path, revision: &str) -> Result<BTreeSet<String>> {
    let root = String::from_utf8(run(
        git(store).args(["rev-parse", &format!("{revision}^{{tree}}")])
    )?)?
    .trim()
    .to_owned();
    oid(&root)?;
    let mut ids = BTreeSet::from([root]);
    let listing = tempfile::tempfile()?;
    let result = crate::process::capture(
        git(store).args(["ls-tree", "-r", "-t", "-z", revision]),
        Duration::from_secs(120),
        None,
        Some(listing.try_clone()?),
    )?;
    ensure!(
        result.status.success(),
        "Retained tree inventory is incomplete"
    );
    ensure!(
        listing.metadata()?.len() <= 256 * 1024 * 1024,
        "Retained tree inventory exceeds bound"
    );
    let mut listing = listing;
    listing.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(listing);
    let mut record = Vec::new();
    while reader.read_until(0, &mut record)? != 0 {
        ensure!(record.last() == Some(&0), "Truncated tree entry");
        let header = record
            .split(|b| *b == b'\t')
            .next()
            .context("Invalid tree entry")?;
        let text = std::str::from_utf8(header)?;
        let fields: Vec<_> = text.split(' ').collect();
        ensure!(fields.len() == 3, "Invalid retained tree entry");
        oid(fields[2])?;
        // A gitlink is not a commit retained from this repository.
        if fields[1] == "tree" {
            ids.insert(fields[2].to_owned());
        }
        record.clear();
    }
    Ok(ids)
}
pub fn completed(store: &Path) -> Result<bool> {
    let path = store.join("sigla-complete");
    match fs::symlink_metadata(&path) {
        Ok(m) => {
            ensure!(
                m.is_file() && !m.file_type().is_symlink() && m.len() == COMPLETE.len() as u64,
                "Invalid Git pool completion marker"
            );
            Ok(fs::read(path)? == COMPLETE && store.join("HEAD").is_file())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn execute(source: &Path, target: &Path, roots: &Retention, cache: &Path) -> Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("git-rebuild-job-")
        .tempdir_in(cache)?;
    let input = directory.path().join("request.json");
    let output = directory.path().join("result.json");
    fs::write(
        &input,
        serde_json::to_vec(&Job {
            source: source.to_owned(),
            target: target.to_owned(),
            roots: roots.clone(),
        })?,
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__git-rebuild")
        .arg(&input)
        .arg(&output)
        .current_dir(directory.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_SHALLOW_FILE");
    let result = crate::process::capture(&mut command, Duration::from_secs(300), None, None)?;
    ensure!(
        result.status.success(),
        "Git pool rebuild subprocess failed"
    );
    ensure!(
        fs::metadata(&output)?.len() <= 64 * 1024,
        "Oversized rebuild response"
    );
    let result: std::result::Result<(), String> = serde_json::from_slice(&fs::read(&output)?)?;
    result.map_err(anyhow::Error::msg)?;
    ensure!(
        completed(target)?,
        "Rebuild did not publish a completion marker"
    );
    Ok(())
}
pub fn worker(input: &Path, output: &Path) -> Result<()> {
    crate::process::inherit_process_group();
    ensure!(
        fs::metadata(input)?.len() <= 256 * 1024 * 1024,
        "Rebuild request exceeds bound"
    );
    let job: Job = serde_json::from_slice(&fs::read(input)?)?;
    let result = rebuild(&job.source, &job.target, &job.roots).map_err(|e| format!("{e:#}"));
    fs::write(output, serde_json::to_vec(&result)?)?;
    Ok(())
}

/// Does not replace the live store. Only cache::Pool publishes the validated result.
fn rebuild(source: &Path, target: &Path, roots: &Retention) -> Result<()> {
    ensure!(!target.try_exists()?, "Rebuild target must be new");
    let format = String::from_utf8(run(git(source).args(["rev-parse", "--show-object-format"]))?)?
        .trim()
        .to_owned();
    ensure!(
        matches!(format.as_str(), "sha1" | "sha256"),
        "Unsupported Git object format"
    );
    let present = kinds(source, &roots.revisions)?;
    let mut commits = BTreeSet::new();
    let mut objects = BTreeSet::new();
    for revision in &roots.revisions {
        let Some(kind) = present.get(revision) else {
            continue;
        };
        ensure!(kind == "commit", "Retained root is not a commit");
        commits.insert(revision.clone());
        objects.insert(revision.clone());
        // Existing commit + incomplete tree is NOT a safely skippable root.
        objects.extend(tree_ids(source, revision)?);
    }
    // Keep selected bytes even if their commit has not reached a cold-migrated pool.
    for (blob, kind) in kinds(source, &roots.selected)? {
        ensure!(kind == "blob", "Selected object is not a blob");
        objects.insert(blob);
    }
    run(Command::new("git")
        .args(["init", "--bare", "--quiet", "--template="])
        .arg(format!("--object-format={format}"))
        .arg(target))?;
    if !objects.is_empty() {
        let pack = target.join("incoming.pack");
        let pack_file = File::create(&pack)?;
        let input = objects
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        // Explicit objects, no --revs/--all: no ancestry or excluded blobs are traversed.
        let result = crate::process::capture(
            git(source).args(["pack-objects", "--stdout"]),
            Duration::from_secs(120),
            Some(input),
            Some(pack_file.try_clone()?),
        )?;
        ensure!(result.status.success(), "Cannot pack retained Git objects");
        pack_file.sync_all()?;
        let index = target.join("incoming.idx");
        let hash = String::from_utf8(run(git(target)
            .args(["index-pack", "--index-version=2", "-o"])
            .arg(&index)
            .arg(&pack))?)?
        .trim()
        .to_owned();
        oid(&hash)?;
        File::open(&index)?.sync_all()?;
        let destination = target.join("objects/pack").join(format!("pack-{hash}"));
        File::create(destination.with_extension("promisor"))?.sync_all()?;
        fs::rename(index, destination.with_extension("idx"))?;
        fs::rename(pack, destination.with_extension("pack"))?;
        File::open(target.join("objects/pack"))?.sync_all()?;
    }
    let shallow = commits
        .iter()
        .map(|id| format!("{id}\n"))
        .collect::<String>();
    let mut file = File::create(target.join("shallow"))?;
    file.write_all(shallow.as_bytes())?;
    file.sync_all()?;
    // Validation is deliberately exact and local; no lazy fetching can fill a hole.
    for revision in &commits {
        ensure!(
            tree_ids(source, revision)? == tree_ids(target, revision)?,
            "Lost retained tree inventory"
        );
    }
    let retained = kinds(target, &objects)?;
    ensure!(retained.len() == objects.len(), "Lost retained object");
    for revision in &commits {
        ensure!(
            retained.get(revision).map(String::as_str) == Some("commit"),
            "Lost retained commit"
        );
    }
    for name in ["HEAD", "config"] {
        File::open(target.join(name))?.sync_all()?;
    }
    File::open(target.join("objects"))?.sync_all()?;
    let mut marker = File::create(target.join("sigla-complete"))?;
    marker.write_all(COMPLETE)?;
    marker.sync_all()?;
    File::open(target)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "rebuild_tests.rs"]
mod tests;
