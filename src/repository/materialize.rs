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
    pub(super) fn unavailable(&self, name: &str) -> String {
        let kind = match self {
            Self::Branch(_) => "branch",
            Self::Tag(_) => "tag",
            _ => "branch or tag",
        };
        let mut message = format!("Requested upstream {kind} {name:?} is unavailable.");
        if matches!(self, Self::Named(_))
            && name.len() < 7
            && name.bytes().all(|b| b.is_ascii_hexdigit())
        {
            message.push_str(
                " Commit abbreviations require at least 7 hexadecimal characters; use a branch, tag, or full SHA.",
            );
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum Contents {
    #[default]
    All,
    Inventory,
    Only(BTreeSet<String>),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub contents: Contents,
    #[serde(default)]
    pub transfer_used: u64,
    #[serde(default)]
    pub unlimited_transfer: bool,
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
    pub omitted: BTreeMap<String, String>,
    #[serde(default)]
    pub transfer_bytes: u64,
    #[serde(default)]
    pub unavailable: BTreeSet<String>,
    #[serde(default)]
    pub transport: Option<String>,
    /// Fixed semantic target when an abbreviated commit selector resolves.
    #[serde(default)]
    pub resolved_target: Option<Target>,
    pub branch: Option<String>,
    pub revision: String,
    pub selected: BTreeMap<String, String>,
    pub tracked: BTreeMap<String, String>,
    pub directories: BTreeSet<String>,
}

/// Provenance accompanies inventories reused by a later acquisition job.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Pinned {
    pub repository: super::Identity,
    pub policy: String,
    pub subdirectory: Option<String>,
    pub prepared: Prepared,
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Metrics {
    pub inventory_reused: bool,
    pub extracted_files: u64,
    pub extracted_bytes: u64,
    pub reused_files: u64,
    pub reused_bytes: u64,
}

pub(super) fn git(store: &Path) -> Command {
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
    remaining: Option<u64>,
) -> Result<u64> {
    receive_filtered(
        session,
        store,
        store,
        objects,
        &super::transport::Fetch {
            filter: super::transport::Filter::Trees,
            depth: depth_one.then_some(1),
            haves: &[],
            cancel: &std::sync::atomic::AtomicBool::new(false),
        },
        remaining,
    )
}

pub(super) fn receive_filtered(
    session: &mut Session,
    store: &Path,
    control: &Path,
    objects: Vec<gix_hash::ObjectId>,
    options: &super::transport::Fetch<'_>,
    remaining: Option<u64>,
) -> Result<u64> {
    receive_filtered_measured(session, store, control, objects, options, remaining)
        .map(|transfer| transfer.bytes)
}

pub(super) fn receive_filtered_measured(
    session: &mut Session,
    store: &Path,
    control: &Path,
    objects: Vec<gix_hash::ObjectId>,
    options: &super::transport::Fetch<'_>,
    remaining: Option<u64>,
) -> Result<Transfer> {
    let scratch = tempfile::tempdir_in(store.join("objects/pack"))?;
    let pack = scratch.path().join("incoming.pack");
    let mut output = LimitedWriter {
        output: File::create(&pack)?,
        remaining,
        exceeded: false,
    };
    let mut transfer = Transfer::default();
    let received = session.pack_filtered(
        objects,
        control,
        options,
        &mut MeasuredWriter {
            output: &mut output,
            transfer: &mut transfer,
        },
    );
    ensure!(
        !output.exceeded,
        "Repository transfer limit exceeded; use an exact repository allow entry to remove the cap"
    );
    received?;
    output.output.sync_all()?;
    let index = scratch.path().join("incoming.idx");
    let (pack, id) = if options.haves.is_empty() {
        let result = run(git(store)
            .args(["index-pack", "--index-version=2", "-o"])
            .arg(&index)
            .arg(&pack))?;
        (pack, String::from_utf8(result)?.trim().to_owned())
    } else {
        // Negotiated packs may use bases retained only by this reader's lease.
        // Expanding thin packs requires stdin; ordinary packs index in place.
        let indexed = scratch.path().join("indexed.pack");
        let result = crate::process::capture_file(
            git(control)
                .args([
                    "index-pack",
                    "--stdin",
                    "--fix-thin",
                    "--index-version=2",
                    "-o",
                ])
                .arg(&index)
                .arg(&indexed),
            Duration::from_secs(120),
            File::open(&pack)?,
            Some(options.cancel),
        )?;
        ensure!(
            result.status.success(),
            "Cannot index repository pack: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let result = String::from_utf8(result.stdout)?;
        let id = result
            .trim()
            .strip_prefix("pack\t")
            .context("Invalid Git pack receipt")?;
        (indexed, id.to_owned())
    };
    parse_id(&id)?;
    let destination = store.join("objects/pack").join(format!("pack-{id}"));
    // The marker precedes publication so local inspection never attempts to repair omitted blobs.
    File::open(&index)?.sync_all()?;
    File::create(destination.with_extension("promisor"))?.sync_all()?;
    fs::rename(index, destination.with_extension("idx"))?;
    File::open(&pack)?.sync_all()?;
    fs::rename(pack, destination.with_extension("pack"))?;
    File::open(store.join("objects/pack"))?.sync_all()?;
    Ok(transfer)
}

/// Timing of successful pack writes, without assuming that a one-buffer pack
/// reveals throughput. Require at least a first-buffer's worth of subsequent
/// bytes so a short tail does not turn write latency into a bandwidth estimate.
#[derive(Default)]
pub(super) struct Transfer {
    pub bytes: u64,
    first: Option<(std::time::Instant, u64)>,
    last: Option<std::time::Instant>,
}
impl Transfer {
    pub fn streaming(&self) -> Option<(u64, Duration)> {
        let (first, first_bytes) = self.first?;
        let elapsed = self.last?.duration_since(first);
        let bytes = self.bytes.saturating_sub(first_bytes);
        (bytes >= first_bytes && !elapsed.is_zero()).then_some((bytes, elapsed))
    }
}
struct MeasuredWriter<'a, W> {
    output: &'a mut W,
    transfer: &'a mut Transfer,
}
impl<W: std::io::Write> std::io::Write for MeasuredWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let at = std::time::Instant::now();
        let count = self.output.write(bytes)?;
        if count > 0 {
            self.transfer.first.get_or_insert((at, count as u64));
            self.transfer.last = Some(at);
            self.transfer.bytes += count as u64;
        }
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

pub const TRANSFER_LIMIT: u64 = 256 * 1024 * 1024;

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn streaming_budget_stops_before_publishing_excess_bytes() {
        let mut output = LimitedWriter {
            output: Vec::new(),
            remaining: Some(5),
            exceeded: false,
        };
        output.write_all(b"abc").unwrap();
        assert!(output.write_all(b"def").is_err());
        assert!(output.exceeded);
        assert_eq!(output.output, b"abc");
    }
    #[test]
    fn transfer_timing_excludes_the_first_buffer_and_ignores_single_writes() {
        let at = std::time::Instant::now();
        let mut transfer = Transfer {
            bytes: 10,
            first: Some((at, 10)),
            last: Some(at),
        };
        assert!(transfer.streaming().is_none());
        transfer.bytes += 1;
        transfer.last = Some(at + Duration::from_secs(1));
        assert!(transfer.streaming().is_none());
        transfer.bytes += 19;
        transfer.last = Some(at + Duration::from_secs(2));
        assert_eq!(transfer.streaming(), Some((20, Duration::from_secs(2))));
        let mut output = LimitedWriter {
            output: Vec::new(),
            remaining: Some(3),
            exceeded: false,
        };
        let mut transfer = Transfer::default();
        let mut writer = MeasuredWriter {
            output: &mut output,
            transfer: &mut transfer,
        };
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(transfer.bytes, 3);
        assert!(transfer.streaming().is_none());
        assert!(output.exceeded);
    }
    #[test]
    fn exact_permissions_are_distinct_from_wildcard_permissions() {
        let repo = super::super::Repository::parse("https://github.com/owner/repo")
            .unwrap()
            .unwrap();
        assert!(
            super::super::Rule::parse("git@github.com:owner/repo.git")
                .unwrap()
                .exact_match(&repo)
        );
        assert!(
            !super::super::Rule::parse("https://github.com/owner/*")
                .unwrap()
                .exact_match(&repo)
        );
        assert!(
            !super::super::Rule::parse("https://github.com/other/repo")
                .unwrap()
                .exact_match(&repo)
        );
    }
}

struct LimitedWriter<W> {
    output: W,
    remaining: Option<u64>,
    exceeded: bool,
}
impl<W: std::io::Write> std::io::Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.remaining.is_some_and(|n| n < bytes.len() as u64) {
            self.exceeded = true;
            return Err(std::io::Error::other("Repository transfer limit exceeded"));
        }
        let n = self.output.write(bytes)?;
        if let Some(left) = &mut self.remaining {
            *left -= n as u64;
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
impl Request {
    pub fn remaining(&self, used: u64) -> Option<u64> {
        (!self.unlimited_transfer).then(|| TRANSFER_LIMIT.saturating_sub(used))
    }
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

pub(super) fn prepare(
    request: &Request,
    pinned: Option<Pinned>,
    cache: &Path,
    metrics: &mut Metrics,
) -> Result<Prepared> {
    prepare_inventory(
        request,
        pinned,
        cache,
        |repository| {
            Session::connect(
                repository,
                request.preferred_transport.as_deref(),
                request.allow_private,
            )
        },
        metrics,
    )
}

#[cfg(test)]
fn prepare_with(
    request: &Request,
    connect: impl Fn(&Repository) -> Result<Session>,
) -> Result<Prepared> {
    prepare_inventory(
        request,
        None,
        request.staging.parent().unwrap(),
        connect,
        &mut Metrics::default(),
    )
}

fn prepare_inventory(
    request: &Request,
    pinned: Option<Pinned>,
    cache: &Path,
    connect: impl Fn(&Repository) -> Result<Session>,
    metrics: &mut Metrics,
) -> Result<Prepared> {
    if let Some(pinned) = pinned {
        let repository =
            Repository::parse(&request.repository)?.context("Expected a repository identifier")?;
        ensure!(
            repository.selector.is_none()
                && repository.identity == pinned.repository
                && Selection::new(&request.include, &request.exclude)?.identity == pinned.policy
                && request.subdirectory == pinned.subdirectory
                && request.target == Target::Commit(pinned.prepared.revision.clone())
                && request.transfer_used >= pinned.prepared.transfer_bytes,
            "Pinned repository inventory does not match acquisition"
        );
        if request.store.join("HEAD").is_file() {
            metrics.inventory_reused = true;
            let mut prepared = pinned.prepared;
            prepared.transfer_bytes = request.transfer_used;
            return materialize_inventory(request, &repository, prepared, cache, connect, metrics);
        }
        let mut prepared = prepare_fresh(request, cache, connect, metrics)?;
        prepared.branch = pinned.prepared.branch;
        prepared.resolved_target = pinned.prepared.resolved_target;
        return Ok(prepared);
    }
    prepare_fresh(request, cache, connect, metrics)
}

fn prepare_fresh(
    request: &Request,
    cache: &Path,
    connect: impl Fn(&Repository) -> Result<Session>,
    metrics: &mut Metrics,
) -> Result<Prepared> {
    let repository =
        Repository::parse(&request.repository)?.context("Expected a repository identifier")?;
    ensure!(
        repository.selector.is_none(),
        "Internal transport must not contain a fragment"
    );
    let mut session = connect(&repository)?;
    let resolved = super::revision::resolve(&mut session, &request.target, |session, prefix| {
        super::revision::init(&request.store, session.object_format()?)?;
        super::revision::Prefix {
            repository: &repository,
            store: &request.store,
            cancel: &std::sync::atomic::AtomicBool::new(false),
            remaining: request.remaining(request.transfer_used),
        }
        .resolve(session, prefix, || connect(&repository))
    })?;
    let branch = resolved.branch;
    let revision = resolved.id;
    let resolved_target = resolved
        .pinned
        .then(|| Target::Commit(revision.to_string()));
    if matches!(request.target, Target::DefaultBranch) {
        let name = branch.context("Repository does not advertise a default branch")?;
        super::validate_branch(&name)?;
        return Ok(Prepared {
            omitted: BTreeMap::new(),
            transfer_bytes: 0,
            unavailable: BTreeSet::new(),
            transport: session.endpoint.clone(),
            resolved_target: None,
            branch: Some(name),
            revision: revision.to_string(),
            selected: BTreeMap::new(),
            tracked: BTreeMap::new(),
            directories: BTreeSet::new(),
        });
    }
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
    let mut transfer_bytes = request.transfer_used + resolved.bytes;
    if !cached_tree_inventory(&request.store, &revision.to_string(), &listing)? {
        transfer_bytes += receive(
            &mut session,
            &request.store,
            vec![revision],
            true,
            request.remaining(transfer_bytes),
        )
        .context(
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
    materialize_inventory(
        request,
        &repository,
        Prepared {
            omitted: BTreeMap::new(),
            transfer_bytes,
            unavailable: BTreeSet::new(),
            transport,
            resolved_target,
            branch,
            revision: revision.to_string(),
            selected: BTreeMap::new(),
            tracked,
            directories,
        },
        cache,
        connect,
        metrics,
    )
}

fn materialize_inventory(
    request: &Request,
    repository: &Repository,
    inventory: Prepared,
    cache: &Path,
    connect: impl Fn(&Repository) -> Result<Session>,
    metrics: &mut Metrics,
) -> Result<Prepared> {
    use std::io::{Seek, SeekFrom};
    let Prepared {
        mut transfer_bytes,
        transport,
        resolved_target,
        branch,
        revision,
        tracked,
        directories,
        ..
    } = inventory;
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
    if let Contents::Only(paths) = &request.contents {
        ensure!(
            paths.iter().all(|path| selected.contains_key(path)),
            "Requested file was not selected for acquisition"
        );
        selected.retain(|path, _| paths.contains(path));
    }
    if matches!(request.contents, Contents::Inventory) {
        return Ok(Prepared {
            omitted: BTreeMap::new(),
            transfer_bytes,
            unavailable: BTreeSet::new(),
            transport,
            resolved_target,
            branch,
            revision: revision.to_string(),
            selected,
            tracked,
            directories,
        });
    }
    let sizes = if selected.keys().any(|path| {
        super::selection::asset(Path::new(path)) || super::selection::visual_graph(Path::new(path))
    }) {
        super::github::sizes(repository, &revision, &request.store)?
    } else {
        None
    };
    if let Some(sizes) = &sizes {
        ensure!(
            selected.keys().all(|p| sizes.contains_key(p)),
            "GitHub inventory does not cover selected Git files"
        );
    }
    let mut omitted = BTreeMap::new();
    selected.retain(|path, _| {
        if (super::selection::asset(Path::new(path))
            || super::selection::visual_graph(Path::new(path)))
            && sizes
                .as_ref()
                .and_then(|s| s.get(path))
                .is_some_and(|s| *s > super::selection::ASSET_LIMIT)
        {
            omitted.insert(
                path.clone(),
                format!(
                    "Exceeds remote asset size limit ({} bytes)",
                    sizes.as_ref().unwrap()[path]
                ),
            );
            false
        } else {
            true
        }
    });
    let mut changed: BTreeMap<_, _> = selected
        .iter()
        .filter(|(path, id)| request.previous.get(*path) != Some(*id))
        .map(|(p, i)| (p.clone(), i.clone()))
        .collect();
    super::files::reuse(request, cache, &mut changed, &mut omitted, metrics)?;
    let ids: BTreeSet<_> = changed.values().cloned().collect();
    let mut transfer_session = None;
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
            if transfer_session.is_none() {
                transfer_session = Some(connect(repository)?);
            }
            let session = transfer_session.as_mut().unwrap();
            transfer_bytes += receive(
                session,
                &request.store,
                missing,
                false,
                request.remaining(transfer_bytes),
            )?;
            if !session.can_fetch_again() {
                transfer_session = None;
            }
        }
    }
    drop(transfer_session);
    // Inspect decompressed lengths before extracting payloads into staging files.
    let mut blob_sizes = BTreeMap::new();
    for chunk in ids.iter().collect::<Vec<_>>().chunks(2048) {
        let input = chunk
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let output = crate::process::capture(
            git(&request.store)
                .arg("cat-file")
                .arg("--batch-check=%(objectname) %(objecttype) %(objectsize)"),
            Duration::from_secs(120),
            Some(input),
            None,
        )?;
        ensure!(
            output.status.success(),
            "Cannot inspect selected blob sizes"
        );
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            ensure!(
                fields.len() == 3 && fields[1] == "blob",
                "Selected Git blob unavailable"
            );
            blob_sizes.insert(fields[0].to_owned(), fields[2].parse::<u64>()?);
        }
    }
    changed.retain(|path, id| {
        if (super::selection::asset(Path::new(path))
            || super::selection::visual_graph(Path::new(path)))
            && blob_sizes[id] > super::selection::ASSET_LIMIT
        {
            omitted.insert(
                path.clone(),
                format!("Exceeds remote asset size limit ({} bytes)", blob_sizes[id]),
            );
            false
        } else {
            true
        }
    });
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
            if super::selection::asset(Path::new(path)) && size > super::selection::ASSET_LIMIT {
                std::io::copy(&mut contents.by_ref().take(size + 1), &mut std::io::sink())?;
                omitted.insert(
                    path.to_string(),
                    format!("Exceeds remote asset size limit ({size} bytes)"),
                );
                continue;
            }
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
            metrics.extracted_files += 1;
            metrics.extracted_bytes += size;
        }
    }
    selected.retain(|p, _| !omitted.contains_key(p));
    Ok(Prepared {
        omitted,
        transfer_bytes,
        unavailable: BTreeSet::new(),
        transport,
        resolved_target,
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

    fn pinned(request: &Request, prepared: Prepared) -> Pinned {
        Pinned {
            repository: Repository::parse(&request.repository)
                .unwrap()
                .unwrap()
                .identity,
            policy: Selection::new(&request.include, &request.exclude)
                .unwrap()
                .identity,
            subdirectory: request.subdirectory.clone(),
            prepared,
        }
    }

    #[test]
    fn pinned_inventory_reuses_blobs_without_resolving_or_reading_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let mut request = request(root.path());
        request.contents = Contents::Inventory;
        let inventory = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        let pinned = pinned(&request, inventory.clone());
        request.target = Target::Commit(inventory.revision.clone());
        request.transfer_used = inventory.transfer_bytes;
        request.contents = Contents::All;
        request.required.push("asset.bin".into());
        // Only the blobs exist here: reuse cannot inspect a commit or its trees.
        request.store = bare_store(root.path(), "sha1");
        for path in ["Code.cs", "asset.bin"] {
            assert_eq!(
                write_object(
                    &request.store,
                    "blob",
                    &fs::read(upstream_path.join(path)).unwrap()
                ),
                inventory.tracked[path]
            );
        }
        let mut metrics = Metrics::default();
        let complete = prepare_inventory(
            &request,
            Some(pinned.clone()),
            root.path(),
            |_| panic!("cached blobs must not connect"),
            &mut metrics,
        )
        .unwrap();
        assert!(metrics.inventory_reused);
        assert_eq!(complete.tracked, inventory.tracked);
        assert_eq!(complete.branch, inventory.branch);
        assert_eq!(complete.transfer_bytes, inventory.transfer_bytes);
        assert_eq!(metrics.extracted_files as usize, complete.selected.len());
        for path in complete.selected.keys() {
            assert_eq!(
                fs::read(request.staging.join(path)).unwrap(),
                fs::read(upstream_path.join(path)).unwrap()
            );
        }
        // A later pass keeps previous files and adds all newly required paths.
        request.previous = complete.selected.clone();
        request.additional = vec!["asset.bin".into()];
        request.required.clear();
        request.staging = root.path().join("unchanged");
        let mut metrics = Metrics::default();
        let unchanged = prepare_inventory(
            &request,
            Some(pinned),
            root.path(),
            |_| panic!("unchanged inputs must not connect"),
            &mut metrics,
        )
        .unwrap();
        assert_eq!(unchanged.selected, complete.selected);
        assert_eq!(metrics.extracted_files, 0);
        assert_eq!(unchanged.transfer_bytes, complete.transfer_bytes);
    }

    #[test]
    fn pinned_inventory_validates_provenance_and_restores_an_absent_git_store() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let mut request = request(root.path());
        request.contents = Contents::Inventory;
        let inventory = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        let original = pinned(&request, inventory.clone());
        request.target = Target::Commit(inventory.revision.clone());
        request.transfer_used = inventory.transfer_bytes;
        request.contents = Contents::All;
        for invalid in [
            Request {
                repository: "https://github.com/other/repository".into(),
                ..request.clone()
            },
            Request {
                target: Target::Branch("main".into()),
                ..request.clone()
            },
            Request {
                include: vec!["extra.bin".into()],
                ..request.clone()
            },
            Request {
                subdirectory: Some("Packages".into()),
                ..request.clone()
            },
            Request {
                transfer_used: 0,
                ..request.clone()
            },
        ] {
            assert!(
                prepare_inventory(
                    &invalid,
                    Some(original.clone()),
                    root.path(),
                    |_| panic!("invalid provenance must not connect"),
                    &mut Metrics::default()
                )
                .is_err()
            );
        }
        request.store = root.path().join("restored");
        let mut metrics = Metrics::default();
        let complete = prepare_inventory(
            &request,
            Some(original),
            root.path(),
            |_| Session::local(&upstream_path),
            &mut metrics,
        )
        .unwrap();
        assert!(!metrics.inventory_reused);
        assert_eq!(complete.branch, inventory.branch);
        assert_eq!(complete.revision, inventory.revision);
        assert!(complete.transfer_bytes > inventory.transfer_bytes);
        assert_eq!(
            fs::read(request.staging.join("Code.cs")).unwrap(),
            fs::read(upstream_path.join("Code.cs")).unwrap()
        );
    }

    fn write_object(store: &Path, kind: &str, contents: &[u8]) -> String {
        let output = crate::process::capture(
            git(store).args(["hash-object", "-t", kind, "-w", "--stdin"]),
            Duration::from_secs(10),
            Some(contents.to_vec()),
            None,
        )
        .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit_object(store: &Path, nonce: u32, width: usize) -> String {
        write_object(
            store,
            "commit",
            format!(
                "tree {}\nauthor A <a@a> 0 +0000\ncommitter A <a@a> 0 +0000\n\ncommit {nonce}\n",
                "0".repeat(width)
            )
            .as_bytes(),
        )
    }

    fn bare_store(root: &Path, format: &str) -> PathBuf {
        let store = root.join(format!("objects-{format}"));
        run(Command::new("git")
            .args(["init", "--bare", "--quiet", "--template="])
            .arg(format!("--object-format={format}"))
            .arg(&store))
        .unwrap();
        store
    }

    fn upstream(path: &Path, filter: bool) {
        upstream_format(path, filter, "sha1");
    }

    fn upstream_format(path: &Path, filter: bool, format: &str) {
        run(Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main", "--template="])
            .arg(format!("--object-format={format}"))
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
            "Create fixture sources\n\nCreate source and asset snapshots for repository tests.",
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
            contents: Contents::All,
            transfer_used: 0,
            unlimited_transfer: false,
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
    fn immutable_files_are_reused_across_selectors_and_renamed_paths() {
        use std::os::unix::fs::MetadataExt;
        for format in ["sha1", "sha256"] {
            let root = tempfile::tempdir().unwrap();
            let upstream_path = root.path().join("upstream");
            upstream_format(&upstream_path, true, format);
            let mut request = request(root.path());
            let first = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
            let files = crate::cache::blobs::Store::open(root.path()).unwrap();
            files.import_owned_tree(&request.staging).unwrap();
            super::super::files::record(root.path(), root.path(), &first, &request.staging)
                .unwrap();
            let source = request.staging.join("Code.cs");
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
                .unwrap();
            };
            execute(&["switch", "--quiet", "-c", "renamed"]);
            execute(&["mv", "Code.cs", "Renamed.cs"]);
            execute(&[
                "commit",
                "--quiet",
                "-m",
                "Rename fixture input\n\nMove the source to another path for reuse coverage.",
            ]);
            // Start without compressed Git objects; only the weak aliases survive.
            request.store = root.path().join("fresh-git");
            request.staging = root.path().join("renamed");
            request.target = Target::Branch("renamed".into());
            let mut metrics = Metrics::default();
            let next = prepare_inventory(
                &request,
                None,
                root.path(),
                |_| Session::local(&upstream_path),
                &mut metrics,
            )
            .unwrap();
            assert_eq!(metrics.extracted_files, 0);
            assert_eq!(metrics.reused_files as usize, next.selected.len());
            assert_eq!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(request.staging.join("Renamed.cs"))
                    .unwrap()
                    .ino()
            );
            assert_eq!(
                files
                    .import_owned_tree(&request.staging)
                    .unwrap()
                    .hashed_bytes,
                0
            );
            let blob = next.selected["Renamed.cs"].clone();
            assert!(
                !crate::process::capture(
                    git(&request.store).args(["cat-file", "-e", &blob]),
                    Duration::from_secs(10),
                    None,
                    None
                )
                .unwrap()
                .status
                .success()
            );

            // Aliases are weak: no selected view means the canonical file is collectible.
            fs::remove_file(&source).unwrap();
            fs::remove_file(request.staging.join("Renamed.cs")).unwrap();
            assert!(files.collect_with_pressure(true).unwrap() > 0);
            request.staging = root.path().join("after-collection");
            let mut metrics = Metrics::default();
            prepare_inventory(
                &request,
                None,
                root.path(),
                |_| Session::local(&upstream_path),
                &mut metrics,
            )
            .unwrap();
            assert_eq!(metrics.reused_files, 0);
            assert!(metrics.extracted_files > 0);
            assert_eq!(
                fs::read(request.staging.join("Renamed.cs")).unwrap(),
                fs::read(upstream_path.join("Renamed.cs")).unwrap()
            );
            super::super::files::prune(root.path(), root.path()).unwrap();
            let aliases: serde_json::Value =
                serde_json::from_slice(&fs::read(root.path().join("files.json")).unwrap()).unwrap();
            assert!(aliases["entries"].as_object().unwrap().is_empty());

            fs::write(root.path().join("files.json"), "incomplete publication").unwrap();
            request.staging = root.path().join("after-corruption");
            prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
            assert_eq!(
                fs::read(request.staging.join("Renamed.cs")).unwrap(),
                fs::read(upstream_path.join("Renamed.cs")).unwrap()
            );

            fs::write(upstream_path.join("Renamed.cs"), "class Changed {}\n").unwrap();
            execute(&[
                "commit",
                "--quiet",
                "-am",
                "Change fixture input\n\nReplace the source contents for acquisition coverage.",
            ]);
            request.staging = root.path().join("changed");
            let mut metrics = Metrics::default();
            prepare_inventory(
                &request,
                None,
                root.path(),
                |_| Session::local(&upstream_path),
                &mut metrics,
            )
            .unwrap();
            assert!(metrics.extracted_files > 0);
            assert_eq!(
                fs::read_to_string(request.staging.join("Renamed.cs")).unwrap(),
                "class Changed {}\n"
            );
        }
    }

    #[test]
    fn one_session_receives_multiple_blob_packs() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let mut request = request(root.path());
        request.include.push("asset.bin".into());
        request.contents = Contents::Inventory;
        let inventory = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        let mut session = Session::local(&upstream_path).unwrap();
        for (path, id) in &inventory.selected {
            receive(
                &mut session,
                &request.store,
                vec![parse_id(id).unwrap()],
                false,
                None,
            )
            .unwrap();
            assert!(session.can_fetch_again());
            assert_eq!(
                run(git(&request.store).args(["cat-file", "blob", id])).unwrap(),
                fs::read(upstream_path.join(path)).unwrap()
            );
        }
    }

    #[test]
    fn inventory_defers_blobs_and_targeted_acquisition_fetches_only_requested_files() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let mut request = request(root.path());
        request.include.push("asset.bin".into());
        request.contents = Contents::Inventory;
        let inventory = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert!(inventory.selected.contains_key("Code.cs"));
        assert!(inventory.selected.contains_key("asset.bin"));
        assert!(!request.staging.exists());
        let available = |id: &str| {
            crate::process::capture(
                git(&request.store).args(["cat-file", "-e", id]),
                Duration::from_secs(10),
                None,
                None,
            )
            .unwrap()
            .status
            .success()
        };
        for id in inventory.selected.values() {
            assert!(!available(id));
        }

        request.target = Target::Commit(inventory.revision.clone());
        request.contents = Contents::Only(BTreeSet::from(["Code.cs".into()]));
        let file = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(file.revision, inventory.revision);
        assert_eq!(
            fs::read(request.staging.join("Code.cs")).unwrap(),
            fs::read(upstream_path.join("Code.cs")).unwrap()
        );
        assert!(!request.staging.join("asset.bin").exists());
        assert!(!available(&inventory.selected["asset.bin"]));
        assert_eq!(file.tracked, inventory.tracked);

        request.contents = Contents::All;
        request.staging = root.path().join("complete");
        let complete = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(complete.selected, inventory.selected);
        assert_eq!(
            fs::read(request.staging.join("asset.bin")).unwrap(),
            fs::read(upstream_path.join("asset.bin")).unwrap()
        );
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
        execute(&[
            "commit",
            "--quiet",
            "-am",
            "Update fixture source\n\nReplace the source snapshot for selector tests.",
        ]);
        execute(&["branch", "release"]);
        execute(&["branch", "face1234"]);
        execute(&["branch", "abc123"]);
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
            ("abc123", "class Updated {}\n", current.as_str()),
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
        for selector in ["deadbeef", "abc12", "missing\"", "ordinary"] {
            let mut request = request(&root.path().join("unavailable"));
            request.target = Target::selector(selector).unwrap();
            assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
            assert!(!request.staging.exists());
        }
    }

    #[test]
    fn cold_abbreviations_resolve_only_after_named_refs_miss() {
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
        fs::write(upstream_path.join("Code.cs"), "class Updated {}\n").unwrap();
        execute(&[
            "commit",
            "--quiet",
            "-am",
            "Update fixture source\n\nReplace the source snapshot for selector tests.",
        ]);
        let current = String::from_utf8(execute(&["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        let prefix = &current[..7];
        let mut request = request(root.path());
        request.staging = root.path().join("stage-too-short");
        request.target = Target::selector(&prefix[..prefix.len() - 1]).unwrap();
        assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
        assert!(!request.staging.exists());

        request.staging = root.path().join("stage-prefix");
        request.target = Target::selector(&prefix.to_ascii_uppercase()).unwrap();
        let abbreviated = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(abbreviated.revision, current);
        assert!(matches!(
            abbreviated.resolved_target,
            Some(Target::Commit(ref id)) if id == &current
        ));
        assert!(abbreviated.transfer_bytes > 0);
        assert_eq!(
            fs::read_to_string(request.staging.join("Code.cs")).unwrap(),
            "class Updated {}\n"
        );

        request.staging = root.path().join("stage-warm-prefix");
        let warm = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(warm.revision, current);
        assert_eq!(warm.transfer_bytes, 0);

        execute(&["branch", prefix, &current]);
        execute(&["tag", prefix, &old]);
        request.staging = root.path().join("stage-named");
        request.target = Target::selector(prefix).unwrap();
        let named = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(named.revision, old);
        assert!(named.resolved_target.is_none());

        request.staging = root.path().join("stage-branch");
        request.target = Target::selector(&format!("refs/heads/{prefix}")).unwrap();
        let branch = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert_eq!(branch.revision, current);

        execute(&["branch", "-D", prefix]);
        request.staging = root.path().join("stage-explicit-missing");
        assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());

        execute(&["tag", "-d", prefix]);
        request.target = Target::selector(&format!("refs/tags/{prefix}")).unwrap();
        request.staging = root.path().join("stage-explicit-tag-missing");
        assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
    }

    #[test]
    fn cached_prefixes_are_commit_only_unique_and_object_format_aware() {
        let cached_commit = |store: &Path, prefix: &str| {
            super::super::revision::local_prefix(
                store,
                prefix,
                &std::sync::atomic::AtomicBool::new(false),
            )
        };
        let root = tempfile::tempdir().unwrap();
        let sha1 = bare_store(root.path(), "sha1");

        // These deterministic objects share the same seven hexadecimal digits.
        let commit = commit_object(&sha1, 5879, 40);
        let blob = write_object(&sha1, "blob", b"blob 9994\n");
        assert_eq!(&commit[..7], &blob[..7]);
        assert_eq!(
            cached_commit(&sha1, &commit[..7].to_ascii_uppercase())
                .unwrap()
                .to_string(),
            commit
        );

        let first = commit_object(&sha1, 6785, 40);
        let second = commit_object(&sha1, 18252, 40);
        assert_eq!(&first[..7], &second[..7]);
        let ambiguous = cached_commit(&sha1, &first[..7]).unwrap_err().to_string();
        assert!(!ambiguous.contains(&first));
        assert!(!ambiguous.contains(&second));
        assert!(cached_commit(&sha1, "fffffff").is_err());

        let sha256 = bare_store(root.path(), "sha256");
        let id = commit_object(&sha256, 1, 64);
        assert_eq!(id.len(), 64);
        assert_eq!(
            cached_commit(&sha256, &id[..7].to_ascii_uppercase())
                .unwrap()
                .to_string(),
            id
        );
    }

    #[test]
    fn generic_hosts_omit_oversized_assets_before_extracting_sources() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let oversized = ["Large.asset", "Large.shadergraph"];
        File::create(upstream_path.join(oversized[0]))
            .unwrap()
            .set_len(super::super::selection::ASSET_LIMIT + 1)
            .unwrap();
        fs::hard_link(
            upstream_path.join(oversized[0]),
            upstream_path.join(oversized[1]),
        )
        .unwrap();
        fs::write(upstream_path.join("Small.asset"), "small asset").unwrap();
        run(Command::new("git")
            .arg("-C")
            .arg(&upstream_path)
            .args(["add", "."]))
        .unwrap();
        run(Command::new("git").arg("-C").arg(&upstream_path).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Add assets of different sizes\n\nExercise asset limits without a host API.",
        ]))
        .unwrap();
        let mut request = request(root.path());
        request.include.push("*.shadergraph".into());
        let prepared = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        for path in oversized {
            assert!(prepared.omitted.contains_key(path), "{path}");
            assert!(!prepared.selected.contains_key(path));
            assert!(!request.staging.join(path).exists());
        }
        for path in ["Code.cs", "Small.asset"] {
            assert_eq!(
                fs::read(request.staging.join(path)).unwrap(),
                fs::read(upstream_path.join(path)).unwrap()
            );
        }
    }

    #[test]
    fn github_size_inventory_prevents_oversized_blob_downloads() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        fs::write(upstream_path.join("Large.asset"), "large asset fixture").unwrap();
        run(Command::new("git")
            .arg("-C")
            .arg(&upstream_path)
            .args(["add", "Large.asset"]))
        .unwrap();
        run(Command::new("git").arg("-C").arg(&upstream_path).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Add fixture asset\n\nCover deferred acquisition of oversized source objects.",
        ]))
        .unwrap();
        let revision = String::from_utf8(
            run(Command::new("git")
                .arg("-C")
                .arg(&upstream_path)
                .args(["rev-parse", "HEAD"]))
            .unwrap(),
        )
        .unwrap()
        .trim()
        .to_owned();
        let mut request = request(root.path());
        request.repository = "https://github.com/fixture/repo".into();
        fs::create_dir_all(&request.store).unwrap();
        // The inventory supplies the size, so the fixture needs no giant payload.
        fs::write(
            request.store.join(format!("github-tree-{revision}.json")),
            serde_json::to_vec(&serde_json::json!({"version":1,"sizes":{
                "Code.cs": fs::metadata(upstream_path.join("Code.cs")).unwrap().len(),
                "Large.asset": super::super::selection::ASSET_LIMIT + 1
            }}))
            .unwrap(),
        )
        .unwrap();
        request.contents = Contents::Inventory;
        let inventory = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert!(inventory.selected.contains_key("Large.asset"));
        assert!(inventory.omitted.is_empty());
        assert!(!request.staging.join("Code.cs").exists());
        request.contents = Contents::All;
        let prepared = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert!(request.staging.join("Code.cs").is_file());
        assert!(!request.staging.join("Large.asset").exists());
        assert!(prepared.omitted.contains_key("Large.asset"));
        let blob = &prepared.tracked["Large.asset"];
        assert!(
            !crate::process::capture(
                git(&request.store).args(["cat-file", "-e", blob]),
                Duration::from_secs(10),
                None,
                None
            )
            .unwrap()
            .status
            .success()
        );
    }

    #[test]
    fn followup_acquisition_keeps_the_existing_transfer_budget() {
        let root = tempfile::tempdir().unwrap();
        let upstream_path = root.path().join("upstream");
        upstream(&upstream_path, true);
        let mut request = request(root.path());
        request.transfer_used = TRANSFER_LIMIT;
        assert!(prepare_with(&request, |_| Session::local(&upstream_path)).is_err());
        assert!(!request.staging.exists());
        request.unlimited_transfer = true;
        let prepared = prepare_with(&request, |_| Session::local(&upstream_path)).unwrap();
        assert!(request.staging.join("Code.cs").is_file());
        assert!(prepared.transfer_bytes > request.transfer_used);
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
