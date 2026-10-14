//! Object-only history access. No workspace, checkout, content-store import or LFS path.
mod planner;

use super::{
    Repository,
    cache::ObjectLease,
    materialize,
    transport::{Fetch, Filter, Session},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MEMORY: usize = 64 * 1024 * 1024;
const OBJECT_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Acquire), "history cancelled");
    Ok(())
}

pub(super) fn git(store: &Path) -> Command {
    let mut command = materialize::git(store);
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    command
}
fn run(command: &mut Command) -> Result<Vec<u8>> {
    crate::process::run(command, Duration::from_secs(120))
}
pub(super) fn init(path: &Path, format: &str) -> Result<()> {
    super::revision::init(path, format)
}
fn id(value: &str) -> Result<gix_hash::ObjectId> {
    ensure!(
        matches!(value.len(), 40 | 64),
        "Expected full Git object ID"
    );
    Ok(gix_hash::ObjectId::from_hex(value.as_bytes())?)
}

#[derive(Clone, Debug)]
pub(crate) struct Commit {
    pub id: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub date: String,
    pub subject: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub id: String,
    pub mode: u32,
}
pub(crate) type Tree = BTreeMap<String, Entry>;

struct Object {
    kind: String,
    bytes: Vec<u8>,
}
struct Pipe {
    output: ChildStdout,
    cancel: Arc<AtomicBool>,
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        if bytes.is_empty() {
            return Ok(0);
        }
        let start = Instant::now();
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return Err(std::io::Error::other("history cancelled"));
            }
            if start.elapsed() >= Duration::from_secs(120) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Git object reader stalled",
                ));
            }
            let mut descriptor = libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
            if ready > 0 {
                return self.output.read(bytes);
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}
struct Batch {
    _child: crate::process::Child,
    input: ChildStdin,
    output: BufReader<Pipe>,
}
impl Batch {
    fn new(store: &Path, cancel: Arc<AtomicBool>) -> Result<Self> {
        let mut child = crate::process::spawn(
            git(store)
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )?;
        Ok(Self {
            input: child.stdin.take().context("Missing Git input")?,
            output: BufReader::new(Pipe {
                output: child.stdout.take().context("Missing Git output")?,
                cancel,
            }),
            _child: child,
        })
    }
    fn read(&mut self, oid: &str, cancel: &AtomicBool) -> Result<Option<Object>> {
        check(cancel)?;
        id(oid)?;
        writeln!(self.input, "{oid}")?;
        self.input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        ensure!(
            header.len() <= 256 && header.ends_with('\n'),
            "Invalid Git batch header"
        );
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(
            fields.first() == Some(&oid),
            "Unexpected Git object response"
        );
        if fields.get(1) == Some(&"missing") {
            return Ok(None);
        }
        ensure!(fields.len() == 3, "Invalid Git object header");
        let size: usize = fields[2].parse()?;
        ensure!(
            size <= OBJECT_LIMIT,
            "object {oid} exceeds history reader memory bound"
        );
        let mut bytes = vec![0; size];
        // Bound cancellation latency while decoding large objects.
        for chunk in bytes.chunks_mut(64 * 1024) {
            check(cancel)?;
            self.output.read_exact(chunk)?;
        }
        let mut newline = [0];
        self.output.read_exact(&mut newline)?;
        ensure!(newline == [b'\n'], "Truncated Git object response");
        Ok(Some(Object {
            kind: fields[1].into(),
            bytes,
        }))
    }
}

struct Remote {
    repository: Repository,
    private: bool,
    unlimited: bool,
    session: Option<Session>,
    transferred: u64,
    requests: usize,
    commit_filter: Filter,
    #[cfg(test)]
    local: Option<PathBuf>,
}
impl Remote {
    fn connect(&self) -> Result<Session> {
        #[cfg(test)]
        if let Some(path) = &self.local {
            return Session::local(path);
        }
        Session::connect(&self.repository, None, self.private)
    }

    fn session(&mut self) -> Result<&mut Session> {
        if self.session.is_none() {
            self.session = Some(self.connect()?);
        }
        Ok(self.session.as_mut().unwrap())
    }

    fn resolve(
        &mut self,
        target: &materialize::Target,
        lease: &ObjectLease,
        cancel: &AtomicBool,
    ) -> Result<String> {
        check(cancel)?;
        self.session()?;
        let mut session = self.session.take().unwrap();
        let result = super::revision::resolve(&mut session, target, |session, prefix| {
            let mut resolved = None;
            lease.acquire(|pool, _| {
                let value = super::revision::Prefix {
                    repository: &self.repository,
                    private: self.private,
                    store: pool,
                    cancel,
                    remaining: (!self.unlimited)
                        .then(|| materialize::TRANSFER_LIMIT.saturating_sub(self.transferred)),
                }
                .resolve(session, prefix, || self.connect())?;
                let bytes = value.bytes;
                resolved = Some(value);
                Ok(bytes)
            })?;
            Ok(resolved.unwrap())
        });
        if result.is_ok() {
            self.session = Some(session);
        }
        let resolved = result?;
        self.transferred += resolved.bytes;
        self.requests += resolved.requests;
        let id = resolved.id.to_string();
        Ok(id)
    }
}

pub(crate) struct Store {
    path: PathBuf,
    cache_scope: [u8; 32],
    lease: Option<ObjectLease>,
    remote: Option<Remote>,
    batch: Option<Batch>,
    objects: lru::LruCache<String, Arc<Object>>,
    bytes: usize,
    resolved: BTreeMap<String, String>,
    planner: planner::Planner,
    pub cancel: Arc<AtomicBool>,
    pub complete: BTreeSet<String>,
    pub boundaries: BTreeSet<String>,
    pub reads: usize,
}
impl Store {
    pub fn local(path: &Path, cancel: Arc<AtomicBool>) -> Result<Self> {
        let output = run(Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--absolute-git-dir"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_SHALLOW_FILE")
            .env_remove("GIT_CONFIG_COUNT"))?;
        let path = PathBuf::from(std::str::from_utf8(&output)?.trim()).canonicalize()?;
        Ok(Self::new(path, None, None, cancel))
    }
    pub fn remote(
        repository: Repository,
        rules: &[super::Rule],
        cache: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        check(&cancel)?;
        let private = super::authorize(rules, &repository)?;
        let unlimited = rules.iter().any(|r| r.exact_match(&repository));
        let remote = Remote {
            repository,
            private,
            unlimited,
            session: None,
            transferred: 0,
            requests: 0,
            commit_filter: Filter::Commits,
            #[cfg(test)]
            local: None,
        };
        Self::from_remote(remote, cache, cancel)
    }

    fn from_remote(mut remote: Remote, cache: &Path, cancel: Arc<AtomicBool>) -> Result<Self> {
        // Access is checked on every request, including pinned warm-cache reads.
        let selector = remote.repository.selector.clone();
        let target = selector
            .as_deref()
            .map(materialize::Target::selector)
            .transpose()?
            .unwrap_or(materialize::Target::DefaultBranch);
        let format = remote.session()?.object_format()?;
        let lease = super::cache::object_lease(cache, &remote.repository.identity, format)?;
        let revision = remote.resolve(&target, &lease, &cancel)?;
        let mut store = Self::new(lease.path().to_owned(), Some(lease), Some(remote), cancel);
        store
            .resolved
            .insert(selector.unwrap_or_else(|| "HEAD".into()), revision);
        Ok(store)
    }
    fn new(
        path: PathBuf,
        lease: Option<ObjectLease>,
        remote: Option<Remote>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let identity = lease
            .as_ref()
            .map(ObjectLease::owner)
            .unwrap_or_else(|| path.clone());
        let mut scope = blake3::Hasher::new();
        scope.update(if lease.is_some() {
            b"remote\0"
        } else {
            b"local\0"
        });
        scope.update(identity.as_os_str().as_encoded_bytes());
        Self {
            path,
            cache_scope: *scope.finalize().as_bytes(),
            lease,
            remote,
            cancel,
            batch: None,
            objects: lru::LruCache::new(NonZeroUsize::new(4096).unwrap()),
            bytes: 0,
            resolved: BTreeMap::new(),
            planner: planner::Planner::default(),
            complete: BTreeSet::new(),
            boundaries: BTreeSet::new(),
            reads: 0,
        }
    }
    pub fn owner(&self) -> Option<PathBuf> {
        self.lease.as_ref().map(ObjectLease::owner)
    }
    pub fn cache_scope(&self) -> &[u8; 32] {
        &self.cache_scope
    }
    pub fn finish(&self) -> Result<()> {
        if let Some(lease) = &self.lease {
            lease.publish(&self.complete, &self.boundaries)?;
        }
        if let Some(remote) = &self.remote {
            tracing::debug!(
                reads = self.reads,
                requests = remote.requests,
                bytes = remote.transferred,
                "History object acquisition"
            );
        }
        Ok(())
    }
    pub fn resolve(&mut self, selector: Option<&str>) -> Result<String> {
        check(&self.cancel)?;
        let selector = selector.unwrap_or("HEAD");
        if let Some(id) = self.resolved.get(selector) {
            return Ok(id.clone());
        }
        let target = if selector == "HEAD" {
            materialize::Target::DefaultBranch
        } else {
            materialize::Target::selector(selector)?
        };
        let resolved = if let Some(remote) = &mut self.remote {
            let resolved = remote.resolve(
                &target,
                self.lease
                    .as_ref()
                    .context("Missing repository reader lease")?,
                &self.cancel,
            )?;
            self.batch = None;
            resolved
        } else {
            let names = match target {
                materialize::Target::DefaultBranch => vec!["HEAD".into()],
                materialize::Target::Branch(ref s) => vec![format!("refs/heads/{s}")],
                materialize::Target::Tag(ref s) => vec![format!("refs/tags/{s}")],
                materialize::Target::Named(ref s) => {
                    vec![format!("refs/tags/{s}"), format!("refs/heads/{s}")]
                }
                materialize::Target::Commit(ref s) => vec![s.clone()],
            };
            let mut found = None;
            for name in names {
                let output = crate::process::capture_cancellable(
                    git(&self.path).args([
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{name}^{{commit}}"),
                    ]),
                    Duration::from_secs(120),
                    None,
                    None,
                    Some(&self.cancel),
                )?;
                if output.status.success() {
                    found = Some(std::str::from_utf8(&output.stdout)?.trim().to_owned());
                    break;
                }
            }
            match found {
                Some(id) => id,
                None if (7..64).contains(&selector.len())
                    && selector.bytes().all(|c| c.is_ascii_hexdigit()) =>
                {
                    super::revision::local_prefix(&self.path, selector, &self.cancel)?.to_string()
                }
                None => bail!("Revision {selector:?} is unavailable"),
            }
        };
        let resolved = id(&resolved)?.to_string();
        self.resolved.insert(selector.into(), resolved.clone());
        Ok(resolved)
    }
    pub fn commit_window(&mut self, commits: Option<usize>) {
        self.planner.commits = commits;
    }
    fn acquisition_kind(&self, kind: &str) -> planner::Kind {
        match kind {
            "commit"
                if self
                    .remote
                    .as_ref()
                    .is_some_and(|remote| matches!(remote.commit_filter, Filter::Trees)) =>
            {
                planner::Kind::CommitTrees
            }
            "commit" => planner::Kind::Commits,
            "tree" => planner::Kind::Trees,
            "blob" => planner::Kind::Blobs,
            _ => unreachable!("Unsupported history object kind"),
        }
    }
    fn remaining(&self) -> Option<u64> {
        self.remote
            .as_ref()
            .filter(|r| !r.unlimited)
            .map(|r| materialize::TRANSFER_LIMIT.saturating_sub(r.transferred))
    }
    fn acquire(&mut self, ids: &[String], kind: &str) -> Result<()> {
        check(&self.cancel)?;
        ensure!(!ids.is_empty(), "Cannot acquire an empty history batch");
        let wants = ids.iter().map(|s| id(s)).collect::<Result<Vec<_>>>()?;
        loop {
            check(&self.cancel)?;
            let class = self.acquisition_kind(kind);
            let remaining = self.remaining();
            let depth = (kind == "commit").then(|| self.planner.depth(class, ids.len(), remaining));
            let remote = self
                .remote
                .as_mut()
                .context("required objects are absent from the local Git object database")?;
            let lease = self
                .lease
                .as_ref()
                .context("Missing repository reader lease")?;
            let options = Fetch {
                filter: if kind == "commit" {
                    remote.commit_filter
                } else {
                    Filter::Trees
                },
                depth,
                cancel: &self.cancel,
            };
            let start = Instant::now();
            let mut transfer = None;
            let session = remote.session()?;
            let result = lease.acquire(|pool, view| {
                let measured = materialize::receive_filtered_measured(
                    session,
                    pool,
                    view,
                    wants.clone(),
                    &options,
                    remaining,
                )?;
                let bytes = measured.bytes;
                transfer = Some(measured);
                Ok(bytes)
            });
            let reusable = session.can_fetch_again() && result.is_ok();
            if !reusable {
                remote.session = None;
            }
            remote.requests += 1;
            match result {
                Ok(bytes) => {
                    remote.transferred += bytes;
                    let elapsed = start.elapsed();
                    let streaming = transfer.and_then(|t| t.streaming());
                    self.planner.observe(
                        class,
                        ids.len() * depth.unwrap_or(1) as usize,
                        bytes,
                        elapsed,
                        streaming,
                    );
                    tracing::trace!(
                        kind,
                        wants = ids.len(),
                        ?depth,
                        bytes,
                        elapsed_ms = elapsed.as_millis(),
                        streaming_bytes = streaming.map(|s| s.0),
                        streaming_us = streaming.map(|s| s.1.as_micros()),
                        "History filtered fetch"
                    );
                    break;
                }
                Err(error)
                    if matches!(options.filter, Filter::Commits)
                        && error
                            .downcast_ref::<super::transport::TreeFilterRejected>()
                            .is_some() =>
                {
                    // Retry only with blob filtering. Re-plan depth using the
                    // separate, initially unpriced commit-with-trees cost class.
                    remote.commit_filter = Filter::Trees;
                }
                Err(error) => return Err(error),
            }
        }
        self.batch = None; // Refresh pack inventory, including negative lookup caches.
        Ok(())
    }
    /// Bounded, object-header-only inventory. This cannot trigger promisor fetching.
    fn missing(&mut self, ids: &[String], kind: &str) -> Result<Vec<String>> {
        check(&self.cancel)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = BTreeSet::new();
        let mut inspect = Vec::new();
        for oid in ids {
            if !seen.insert(oid.as_str()) {
                continue;
            }
            id(oid)?;
            if let Some(object) = self.objects.get(oid.as_str()) {
                ensure!(object.kind == kind, "Unexpected history object kind");
            } else {
                inspect.push(oid);
            }
        }
        let ids = inspect;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let input = ids
            .iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .into_bytes();
        let output = crate::process::capture_cancellable(
            git(&self.path).args([
                "cat-file",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
            ]),
            Duration::from_secs(120),
            Some(input),
            None,
            Some(&self.cancel),
        )?;
        ensure!(
            output.status.success(),
            "Cannot inspect required history objects"
        );
        let rows: Vec<_> = std::str::from_utf8(&output.stdout)?.lines().collect();
        ensure!(
            rows.len() == ids.len(),
            "Incomplete history object inventory"
        );
        let class = self.acquisition_kind(kind);
        let mut missing = Vec::new();
        for (line, expected) in rows.iter().zip(ids) {
            let fields: Vec<_> = line.split_whitespace().collect();
            ensure!(
                fields.first() == Some(&expected.as_str()),
                "Unexpected history object inventory"
            );
            if fields.get(1) == Some(&"missing") && fields.len() == 2 {
                missing.push(expected.clone());
            } else {
                ensure!(
                    fields.len() == 3 && fields[1] == kind,
                    "Unexpected history object kind"
                );
                let size: usize = fields[2].parse()?;
                ensure!(
                    size <= OBJECT_LIMIT,
                    "object {expected} exceeds history reader memory bound"
                );
                self.planner.object(class, size);
            }
        }
        Ok(missing)
    }
    pub fn prefetch(&mut self, ids: &[String], kind: &str) -> Result<()> {
        // Inventory the whole selected set so sparse holes in otherwise local
        // windows can share a request. No source is decoded by these checks.
        let mut pending = Vec::new();
        for chunk in ids.chunks(256) {
            pending.extend(self.missing(chunk, kind)?);
        }
        // Local inventory windows bound subprocess input/output, not completion.
        for chunk in pending.chunks(256) {
            let mut missing = self.missing(chunk, kind)?;
            while !missing.is_empty() {
                let count = self.planner.count(
                    self.acquisition_kind(kind),
                    missing.len(),
                    self.remaining(),
                );
                self.acquire(&missing[..count], kind)?;
                // A tree/ancestry closure can satisfy later wants too. Observe
                // sizes and refresh coverage before choosing another batch.
                let next = self.missing(&missing, kind)?;
                ensure!(
                    !next.iter().any(|oid| missing[..count].contains(oid)),
                    "required {kind} objects remain missing after filtered acquisition"
                );
                missing = next;
            }
        }
        Ok(())
    }
    fn object(&mut self, oid: &str, kind: &str) -> Result<Arc<Object>> {
        check(&self.cancel)?;
        if let Some(object) = self.objects.get(oid) {
            ensure!(object.kind == kind, "Unexpected Git object kind");
            return Ok(object.clone());
        }
        if self.batch.is_none() {
            self.batch = Some(Batch::new(&self.path, self.cancel.clone())?);
        }
        let mut object = self.batch.as_mut().unwrap().read(oid, &self.cancel)?;
        if object.is_none() {
            self.batch = None;
            self.acquire(&[oid.into()], kind)
                .with_context(|| format!("missing {kind} {oid}"))?;
            self.batch = Some(Batch::new(&self.path, self.cancel.clone())?);
            object = self.batch.as_mut().unwrap().read(oid, &self.cancel)?;
        }
        let object = Arc::new(object.with_context(|| format!("missing {kind} {oid}"))?);
        ensure!(object.kind == kind, "Object {oid} is not a {kind}");
        self.planner
            .object(self.acquisition_kind(kind), object.bytes.len());
        if let Some(lease) = &self.lease {
            lease.retain(oid);
        }
        while self.bytes + object.bytes.len() > MEMORY
            || self.objects.len() == self.objects.cap().get()
        {
            let Some((_, removed)) = self.objects.pop_lru() else {
                break;
            };
            self.bytes -= removed.bytes.len();
        }
        self.bytes += object.bytes.len();
        self.objects.put(oid.into(), object.clone());
        self.reads += 1;
        Ok(object)
    }
    pub fn blob(&mut self, oid: &str) -> Result<Vec<u8>> {
        Ok(self.object(oid, "blob")?.bytes.clone())
    }
    pub fn commit(&mut self, oid: &str) -> Result<Commit> {
        let object = self
            .object(oid, "commit")
            .with_context(|| format!("missing ancestor {oid}"))?;
        let source = String::from_utf8_lossy(&object.bytes);
        let (headers, message) = source.split_once("\n\n").context("Invalid Git commit")?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut date = "unknown-date".to_owned();
        for line in headers.lines() {
            if let Some(oid) = line.strip_prefix("tree ") {
                id(oid)?;
                tree = Some(oid.into());
            }
            if let Some(oid) = line.strip_prefix("parent ") {
                id(oid)?;
                parents.push(oid.into());
            }
            if let Some(value) = line.strip_prefix("committer ") {
                let fields: Vec<_> = value.rsplitn(3, ' ').collect();
                if fields.len() >= 2
                    && let Ok(seconds) = fields[1].parse::<i64>()
                {
                    date = calendar(seconds, fields[0]);
                }
            }
        }
        Ok(Commit {
            id: oid.into(),
            tree: tree.context("Commit has no tree")?,
            parents,
            date,
            subject: message
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        })
    }
    pub fn tree(&mut self, root: &str) -> Result<Tree> {
        let mut result = Tree::new();
        let mut bytes_used = 0usize;
        let mut pending = vec![(String::new(), root.to_owned())];
        while let Some((prefix, oid)) = pending.pop() {
            check(&self.cancel)?;
            let object = self.object(&oid, "tree")?;
            let mut bytes = object.bytes.as_slice();
            while !bytes.is_empty() {
                let space = bytes
                    .iter()
                    .position(|&c| c == b' ')
                    .context("Invalid Git tree mode")?;
                let nul = bytes
                    .iter()
                    .position(|&c| c == 0)
                    .context("Invalid Git tree path")?;
                ensure!(
                    space < nul && bytes.len() >= nul + 1 + oid.len() / 2,
                    "Truncated Git tree"
                );
                let mode = u32::from_str_radix(std::str::from_utf8(&bytes[..space])?, 8)?;
                let name = std::str::from_utf8(&bytes[space + 1..nul])?;
                super::selection::validate_path(name)?;
                ensure!(!name.contains('/'), "Invalid Git tree component");
                let path = format!("{prefix}{name}");
                bytes_used = bytes_used.saturating_add(path.len() + 160);
                ensure!(
                    bytes_used <= 64 * 1024 * 1024,
                    "history tree inventory exceeds request memory bound"
                );
                let mut hash = String::with_capacity(oid.len());
                for byte in &bytes[nul + 1..nul + 1 + oid.len() / 2] {
                    use std::fmt::Write;
                    write!(hash, "{byte:02x}").unwrap();
                }
                bytes = &bytes[nul + 1 + oid.len() / 2..];
                if mode == 0o40000 {
                    pending.push((format!("{path}/"), hash.clone()));
                }
                result.insert(path, Entry { id: hash, mode });
            }
        }
        Ok(result)
    }
    pub fn source_paths(
        &mut self,
        tree: &str,
        entries: &Tree,
        paths: &[String],
    ) -> Result<BTreeSet<String>> {
        if paths.is_empty() {
            return Ok(BTreeSet::new());
        }
        let attributes: Vec<_> = entries
            .iter()
            .filter(|(path, entry)| {
                (path.as_str() == ".gitattributes" || path.ends_with("/.gitattributes"))
                    && matches!(entry.mode, 0o100644 | 0o100755)
            })
            .map(|(_, e)| e.id.clone())
            .collect();
        if attributes.is_empty() {
            return Ok(paths.iter().cloned().collect());
        }
        self.prefetch(&attributes, "blob")?;
        // Reading also pins these attribute objects for the declared source policy.
        for oid in &attributes {
            self.object(oid, "blob")?;
        }
        let control = tempfile::tempdir()?;
        init(
            control.path(),
            if tree.len() == 64 { "sha256" } else { "sha1" },
        )?;
        let objects = run(git(&self.path).args(["rev-parse", "--git-path", "objects"]))?;
        let objects = PathBuf::from(std::str::from_utf8(&objects)?.trim()).canonicalize()?;
        let mut allowed: BTreeSet<_> = paths.iter().cloned().collect();
        for chunk in paths.chunks(256) {
            let mut input = Vec::new();
            for path in chunk {
                input.extend_from_slice(path.as_bytes());
                input.push(0);
            }
            let output = crate::process::capture_cancellable(
                git(control.path())
                    .env("GIT_OBJECT_DIRECTORY", &objects)
                    .env("GIT_ATTR_NOSYSTEM", "1")
                    .args([
                        "-c",
                        "core.attributesFile=/dev/null",
                        "check-attr",
                        "--source",
                        tree,
                        "-z",
                        "--stdin",
                        "binary",
                        "diff",
                        "filter",
                    ]),
                Duration::from_secs(120),
                Some(input),
                None,
                Some(&self.cancel),
            )?;
            ensure!(
                output.status.success(),
                "Cannot read committed source attributes"
            );
            let mut fields: Vec<_> = output.stdout.split(|&b| b == 0).collect();
            ensure!(
                fields.last().is_some_and(|s| s.is_empty()),
                "Invalid Git attribute output"
            );
            fields.pop();
            ensure!(
                fields.len() == chunk.len() * 9,
                "Incomplete Git attribute output"
            );
            for fields in fields.chunks_exact(3) {
                let path = std::str::from_utf8(fields[0])?;
                ensure!(
                    chunk.iter().any(|p| p == path),
                    "Unexpected Git attribute path"
                );
                if fields[1] == b"binary" && fields[2] == b"set"
                    || fields[1] == b"diff" && fields[2] == b"unset"
                    || fields[1] == b"filter" && fields[2] == b"lfs"
                {
                    allowed.remove(path);
                }
            }
        }
        Ok(allowed)
    }
    pub fn empty_tree(&self) -> Result<String> {
        let output = crate::process::capture_cancellable(
            git(&self.path).args(["hash-object", "-t", "tree", "--stdin"]),
            Duration::from_secs(120),
            Some(Vec::new()),
            None,
            Some(&self.cancel),
        )?;
        ensure!(output.status.success(), "Cannot identify empty Git tree");
        Ok(std::str::from_utf8(&output.stdout)?.trim().into())
    }
}

// Gregorian civil date from the committer's local day; timestamps never order history.
fn calendar(seconds: i64, zone: &str) -> String {
    let offset = if zone.len() == 5
        && matches!(zone.as_bytes()[0], b'+' | b'-')
        && zone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        let hours: i64 = zone[1..3].parse().unwrap_or(0);
        let minutes: i64 = zone[3..5].parse().unwrap_or(0);
        (hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 }
    } else {
        0
    };
    let z = seconds.saturating_add(offset).div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    format!("{:04}-{month:02}-{day:02}", year + i64::from(month <= 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn command(path: &Path, args: &[&str]) -> String {
        String::from_utf8(
            run(Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1"))
            .unwrap(),
        )
        .unwrap()
        .trim()
        .to_owned()
    }
    struct Fixture {
        directory: tempfile::TempDir,
        cache: tempfile::TempDir,
        repository: Repository,
        tip: String,
        first: String,
        excluded: String,
        lfs: String,
    }
    impl Fixture {
        fn new(format: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path();
            command(
                path,
                &[
                    "init",
                    "--quiet",
                    "--template=",
                    &format!("--object-format={format}"),
                ],
            );
            for (key, value) in [
                ("user.name", "History"),
                ("user.email", "history@example.invalid"),
                ("commit.gpgsign", "false"),
                ("uploadpack.allowFilter", "true"),
                ("uploadpack.allowAnySHA1InWant", "true"),
            ] {
                command(path, &["config", key, value]);
            }
            fs::write(
                path.join(".gitattributes"),
                "excluded.cs binary\nlarge.rs filter=lfs\n",
            )
            .unwrap();
            fs::write(
                path.join("selected.rs"),
                "pub fn selected(value: i32) -> i32 { value * 27 }\n",
            )
            .unwrap();
            fs::write(path.join("excluded.cs"), vec![0; 64 * 1024]).unwrap();
            fs::write(path.join("large.rs"), "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 1000000000\n").unwrap();
            command(path, &["add", "."]);
            command(path, &["commit", "-qm", "first"]);
            let first = command(path, &["rev-parse", "HEAD"]);
            let excluded = command(path, &["rev-parse", "HEAD:excluded.cs"]);
            let lfs = command(path, &["rev-parse", "HEAD:large.rs"]);
            fs::write(
                path.join("selected.rs"),
                "pub fn selected(value: i32) -> i32 { value * 31 }\n",
            )
            .unwrap();
            command(path, &["add", "."]);
            command(path, &["commit", "-qm", "second"]);
            let tip = command(path, &["rev-parse", "HEAD"]);
            Self {
                directory,
                cache: tempfile::tempdir().unwrap(),
                tip,
                first,
                excluded,
                lfs,
                repository: Repository::parse("https://example.invalid/history/fixture")
                    .unwrap()
                    .unwrap(),
            }
        }
        fn store(&self) -> Store {
            let lease = super::super::cache::object_lease(
                self.cache.path(),
                &self.repository.identity,
                if self.tip.len() == 64 {
                    "sha256"
                } else {
                    "sha1"
                },
            )
            .unwrap();
            let remote = self.remote();
            let mut store = Store::new(
                lease.path().to_owned(),
                Some(lease),
                Some(remote),
                Arc::new(AtomicBool::new(false)),
            );
            store.resolved.insert("HEAD".into(), self.tip.clone());
            store
        }
        fn remote(&self) -> Remote {
            Remote {
                repository: self.repository.clone(),
                private: false,
                unlimited: false,
                session: None,
                transferred: 0,
                requests: 0,
                commit_filter: Filter::Commits,
                local: Some(self.directory.path().to_owned()),
            }
        }
        fn current(&self) -> PathBuf {
            self.cache
                .path()
                .join("git")
                .join(self.repository.identity.storage_key())
                .join("current")
        }
        fn inventory(&self) -> String {
            String::from_utf8(
                run(git(&self.current()).args([
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objectname) %(objecttype)",
                ]))
                .unwrap(),
            )
            .unwrap()
        }
    }
    #[test]
    fn cold_prefixes_and_range_endpoints_resolve_for_both_object_formats() {
        for format in ["sha1", "sha256"] {
            let fixture = Fixture::new(format);
            assert!(!fixture.current().exists());
            let mut remote = fixture.remote();
            remote.repository.selector = Some(fixture.tip[..8].to_ascii_uppercase());
            let mut store = Store::from_remote(
                remote,
                fixture.cache.path(),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            assert_eq!(store.resolve(Some(&fixture.tip[..8])).unwrap(), fixture.tip);
            assert_eq!(
                store.resolve(Some(&fixture.first[..8])).unwrap(),
                fixture.first
            );
            let objects = fixture.inventory();
            assert!(
                !objects.contains(" blob") && !objects.contains(" tree"),
                "{objects}"
            );
            let query = format!("diff {}..{}", &fixture.first[..8], &fixture.tip[..8]);
            let diff = crate::history::run(store, None, &query).unwrap();
            assert!(
                diff.contains("value * 27") && diff.contains("value * 31"),
                "{diff}"
            );
            assert!(!fixture.cache.path().join("repositories").exists());
        }
    }

    #[test]
    fn prefix_resolution_crosses_cached_shallow_boundaries() {
        let fixture = Fixture::new("sha1");
        let mut store = fixture.store();
        store.commit_window(Some(1));
        store.commit(&fixture.tip).unwrap();
        assert!(!fixture.inventory().contains(&fixture.first));
        assert_eq!(
            store.resolve(Some(&fixture.first[..8])).unwrap(),
            fixture.first
        );
        assert!(!fixture.inventory().contains(" blob"));
        let mut warm = fixture.store();
        assert_eq!(
            warm.resolve(Some(&fixture.first[..8])).unwrap(),
            fixture.first
        );
        assert_eq!(warm.remote.as_ref().unwrap().transferred, 0);
    }

    #[test]
    fn partial_caches_cannot_hide_an_ambiguous_commit_on_another_tag() {
        let fixture = Fixture::new("sha1");
        let tree = command(fixture.directory.path(), &["rev-parse", "HEAD^{tree}"]);
        let body = |nonce| {
            format!(
                "tree {tree}\nauthor A <a@a> 0 +0000\ncommitter A <a@a> 0 +0000\n\ncollision {nonce}\n"
            )
        };
        let mut seen = std::collections::HashMap::new();
        let (first, second) = (0..1_000_000)
            .find_map(|nonce| {
                let data = body(nonce);
                let mut hash = gix_hash::hasher(gix_hash::Kind::Sha1);
                hash.update(format!("commit {}\0", data.len()).as_bytes());
                hash.update(data.as_bytes());
                let id = hash.try_finalize().unwrap().to_string();
                seen.insert(id[..7].to_owned(), nonce)
                    .map(|old| (old, nonce))
            })
            .expect("Fixture must contain two colliding commit prefixes");
        let write = |nonce| {
            let output = crate::process::capture(
                Command::new("git")
                    .arg("-C")
                    .arg(fixture.directory.path())
                    .args(["hash-object", "-t", "commit", "-w", "--stdin"]),
                Duration::from_secs(10),
                Some(body(nonce).into_bytes()),
                None,
            )
            .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let first = write(first);
        let second = write(second);
        command(
            fixture.directory.path(),
            &["update-ref", "refs/heads/collision", &first],
        );
        command(
            fixture.directory.path(),
            &["update-ref", "refs/tags/collision", &second],
        );
        let mut store = fixture.store();
        store.commit_window(Some(1));
        store.commit(&first).unwrap();
        assert!(!fixture.inventory().contains(&second));
        let ambiguous = store.resolve(Some(&first[..7])).unwrap_err();
        assert!(matches!(
            ambiguous.downcast_ref::<super::super::revision::PrefixError>(),
            Some(super::super::revision::PrefixError::Ambiguous(_))
        ));
        assert!(fixture.inventory().contains(&second));
        let length = first
            .bytes()
            .zip(second.bytes())
            .position(|(a, b)| a != b)
            .unwrap()
            + 1;
        assert_eq!(store.resolve(Some(&first[..length])).unwrap(), first);
        assert_eq!(store.resolve(Some(&second[..length])).unwrap(), second);
    }

    #[test]
    fn prefix_acquisition_preserves_filters_cancellation_and_transfer_limits() {
        let fixture = Fixture::new("sha1");
        command(
            fixture.directory.path(),
            &["config", "uploadpackfilter.tree.allow", "false"],
        );
        let mut store = fixture.store();
        assert_eq!(
            store.resolve(Some(&fixture.first[..8])).unwrap(),
            fixture.first
        );
        assert!(!fixture.inventory().contains(" blob"));
        let fixture = Fixture::new("sha1");
        command(
            fixture.directory.path(),
            &["config", "uploadpack.allowFilter", "false"],
        );
        let mut store = fixture.store();
        assert!(store.resolve(Some(&fixture.first[..8])).is_err());
        assert!(fixture.inventory().is_empty());
        let fixture = Fixture::new("sha1");
        let mut store = fixture.store();
        store.remote.as_mut().unwrap().transferred = materialize::TRANSFER_LIMIT;
        assert!(store.resolve(Some(&fixture.first[..8])).is_err());
        assert!(fixture.inventory().is_empty());
        store.cancel.store(true, Ordering::Release);
        assert!(store.resolve(Some(&fixture.first[..8])).is_err());
        assert!(fixture.inventory().is_empty());
    }

    #[test]
    fn remote_log_fetches_metadata_only_then_acquires_selected_source_explicitly() {
        for format in ["sha1", "sha256"] {
            let fixture = Fixture::new(format);
            let metadata = crate::history::run(fixture.store(), None, "log").unwrap();
            assert!(
                metadata.contains(&fixture.first) && metadata.contains(&fixture.tip),
                "{metadata}"
            );
            assert!(metadata.ends_with("end\n"), "{metadata}");
            let objects = fixture.inventory();
            assert!(
                !objects.contains(" blob") && !objects.contains(" tree"),
                "{objects}"
            );
            let source =
                crate::history::run(fixture.store(), None, "log function:selected").unwrap();
            assert!(
                source.contains(&fixture.first)
                    && source.contains(&fixture.tip)
                    && source.ends_with("end\n"),
                "{source}"
            );
            let objects = fixture.inventory();
            assert!(
                objects.contains(" blob") && objects.contains(" tree"),
                "{objects}"
            );
            assert!(
                !objects.contains(&fixture.excluded) && !objects.contains(&fixture.lfs),
                "{objects}"
            );
            assert!(!fixture.cache.path().join("repositories").exists());
            assert!(!fixture.cache.path().join("analysis").exists());
            // Persisted verified objects can replay the request offline. A production
            // remote request still performs its independent authorization check.
            let mut offline = fixture.store();
            offline.remote = None;
            assert_eq!(
                crate::history::run(offline, None, "log function:selected").unwrap(),
                source
            );
        }
    }
    #[test]
    fn refreshed_inventory_deduplicates_wants_and_discovers_recursive_coverage() {
        let fixture = Fixture::new("sha1");
        let path = fixture.directory.path();
        fs::create_dir(path.join("nested")).unwrap();
        fs::write(path.join("nested/source.rs"), "pub fn nested() {}\n").unwrap();
        command(path, &["add", "."]);
        command(path, &["commit", "-qm", "nested"]);
        let root = command(path, &["rev-parse", "HEAD^{tree}"]);
        let nested = command(path, &["rev-parse", "HEAD:nested"]);
        let mut store = fixture.store();
        // Force a single-root probe through observed size, not a chosen batch constant.
        store.planner.object(planner::Kind::Trees, OBJECT_LIMIT);
        let wants = [root.clone(), nested, root.clone()];
        store.prefetch(&wants, "tree").unwrap();
        let requests = store.remote.as_ref().unwrap().requests;
        assert_eq!(
            requests, 1,
            "The root pack already contains the nested tree"
        );
        assert!(store.tree(&root).unwrap().contains_key("nested/source.rs"));
        store.prefetch(&wants, "tree").unwrap();
        assert_eq!(store.remote.as_ref().unwrap().requests, requests);
        assert!(!fixture.inventory().contains(" blob"));
    }

    #[test]
    fn selected_wants_complete_across_batches_with_warm_and_cancelled_replays() {
        let fixture = Fixture::new("sha1");
        let path = fixture.directory.path();
        let mut wants = Vec::new();
        for index in 0..17 {
            let name = format!("part{index}.rs");
            fs::write(path.join(&name), format!("pub fn part{index}() {{}}\n")).unwrap();
            command(path, &["add", &name]);
            wants.push(command(path, &["rev-parse", &format!(":{name}")]));
        }
        command(path, &["commit", "-qm", "parts"]);
        let mut store = fixture.store();
        store.planner.object(planner::Kind::Blobs, OBJECT_LIMIT);
        store.prefetch(&wants, "blob").unwrap();
        let requests = store.remote.as_ref().unwrap().requests;
        assert!(requests > 1);
        for (index, oid) in wants.iter().enumerate() {
            assert_eq!(
                store.blob(oid).unwrap(),
                format!("pub fn part{index}() {{}}\n").as_bytes()
            );
        }
        let expected: BTreeSet<_> = wants.iter().cloned().collect();
        let actual: BTreeSet<_> = fixture
            .inventory()
            .lines()
            .map(|line| line.split_once(' ').unwrap())
            .map(|(id, kind)| {
                assert_eq!(kind, "blob");
                id.to_owned()
            })
            .collect();
        assert_eq!(actual, expected);
        store.remote.as_mut().unwrap().transferred = materialize::TRANSFER_LIMIT;
        store.prefetch(&wants, "blob").unwrap(); // All local, even with no budget left.
        assert_eq!(store.remote.as_ref().unwrap().requests, requests);
        store.cancel.store(true, Ordering::Release);
        assert!(
            store
                .prefetch(std::slice::from_ref(&fixture.excluded), "blob")
                .is_err()
        );
        assert_eq!(store.remote.as_ref().unwrap().requests, requests);
    }

    #[test]
    fn adaptive_remote_pages_and_sparse_source_logs_match_local_history() {
        for format in ["sha1", "sha256"] {
            let mut fixture = Fixture::new(format);
            let path = fixture.directory.path();
            for index in 0..18 {
                command(
                    path,
                    &[
                        "commit",
                        "--allow-empty",
                        "-qm",
                        &format!("unchanged {index}"),
                    ],
                );
            }
            fixture.tip = command(path, &["rev-parse", "HEAD"]);
            let local = || Store::local(path, Arc::new(AtomicBool::new(false))).unwrap();
            let page = "log limit:3";
            let cold = crate::history::run(fixture.store(), None, page).unwrap();
            assert_eq!(cold, crate::history::run(local(), None, page).unwrap());
            assert!(!cold.ends_with("end\n"));
            let inventory = fixture.inventory();
            assert!(!inventory.contains(" blob") && !inventory.contains(" tree"));
            assert!(
                inventory.lines().count() < 20,
                "A page must not speculatively fill history"
            );
            for query in ["log file:selected.rs limit:100", "log limit:100"] {
                let cold = crate::history::run(fixture.store(), None, query).unwrap();
                assert!(cold.ends_with("end\n"), "{cold}");
                assert_eq!(cold, crate::history::run(local(), None, query).unwrap());
                let mut warm = fixture.store();
                warm.remote.as_mut().unwrap().transferred = materialize::TRANSFER_LIMIT;
                assert_eq!(cold, crate::history::run(warm, None, query).unwrap());
            }
            let inventory = fixture.inventory();
            assert!(!inventory.contains(&fixture.excluded) && !inventory.contains(&fixture.lfs));
        }
    }

    #[test]
    fn real_rust_split_resolves_before_hydrating_unrelated_source() {
        for method in ["force", "new"] {
            let mut fixture = Fixture::new("sha1");
            let path = fixture.directory.path();
            let unrelated = command(path, &["rev-parse", "HEAD:selected.rs"]);
            fs::create_dir_all(path.join("library/core/src/cell")).unwrap();
            fs::write(
                path.join("library/core/src/lazy.rs"),
                include_str!("../../tests/history/fixtures/rust-lazy-before.rs"),
            )
            .unwrap();
            command(path, &["add", "."]);
            command(path, &["commit", "-qm", "before real split"]);
            fixture.first = command(path, &["rev-parse", "HEAD"]);
            for (name, source) in [
                (
                    "lazy.rs",
                    include_str!("../../tests/history/fixtures/rust-lazy-stub.rs"),
                ),
                (
                    "cell/lazy.rs",
                    include_str!("../../tests/history/fixtures/rust-lazy-after.rs"),
                ),
                (
                    "cell/once.rs",
                    include_str!("../../tests/history/fixtures/rust-once-after.rs"),
                ),
            ] {
                fs::write(path.join("library/core/src").join(name), source).unwrap();
            }
            command(path, &["add", "."]);
            command(path, &["commit", "-qm", "real split"]);
            fixture.tip = command(path, &["rev-parse", "HEAD"]);
            let query = format!(
                "show {} method:{method} path:library/core/src/cell/lazy.rs",
                fixture.tip
            );
            let shown = crate::history::run(fixture.store(), None, &query).unwrap();
            assert!(shown.contains("a/library/core/src/lazy.rs"), "{shown}");
            assert!(
                !fixture.inventory().contains(&unrelated),
                "{method}: {shown}"
            );
        }
    }

    #[test]
    fn pinned_pages_reconstruct_after_history_eviction_and_branch_movement() {
        for format in ["sha1", "sha256"] {
            let fixture = Fixture::new(format);
            let queries = [
                "log limit:1".to_owned(),
                "log skip:1 limit:1".to_owned(),
                format!("show {} file:selected.rs", fixture.tip),
            ];
            let expected: Vec<_> = queries
                .iter()
                .map(|query| {
                    crate::history::run(fixture.store(), Some(&fixture.tip), query).unwrap()
                })
                .collect();
            command(
                fixture.directory.path(),
                &["commit", "--allow-empty", "-qm", "branch advanced"],
            );
            let owner = fixture.current().parent().unwrap().join("history.json");
            for (query, expected) in queries.iter().zip(expected) {
                assert!(super::super::cache::evict_history(&owner).unwrap());
                super::super::cache::reclaim(fixture.cache.path()).unwrap();
                assert!(!fixture.current().exists());
                let actual =
                    crate::history::run(fixture.store(), Some(&fixture.tip), query).unwrap();
                assert_eq!(actual, expected);
                let inventory = fixture.inventory();
                assert!(!inventory.contains(&fixture.excluded));
                assert!(!inventory.contains(&fixture.lfs));
            }
        }
    }

    #[test]
    fn exhausted_budget_reports_a_gap_instead_of_a_completed_history() {
        let fixture = Fixture::new("sha1");
        let mut store = fixture.store();
        store.commit_window(Some(1));
        store.commit(&fixture.tip).unwrap();
        store.remote.as_mut().unwrap().transferred = materialize::TRANSFER_LIMIT;
        let text = crate::history::run(store, None, "log").unwrap();
        assert!(
            text.contains("incomplete:") && !text.ends_with("end\n"),
            "{text}"
        );
        let coverage: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.current().parent().unwrap().join("history.json")).unwrap(),
        )
        .unwrap();
        assert!(
            coverage["boundaries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == &fixture.tip)
        );
        assert!(
            !coverage["complete"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == &fixture.tip)
        );
        assert!(!fixture.inventory().contains(&fixture.first));
    }

    #[test]
    fn comparison_batches_only_selected_changed_source() {
        let mut fixture = Fixture::new("sha1");
        let path = fixture.directory.path();
        fs::write(path.join("stable.rs"), "pub fn stable() {}\n").unwrap();
        command(path, &["add", "."]);
        command(path, &["commit", "-qm", "stable source"]);
        fixture.first = command(path, &["rev-parse", "HEAD"]);
        fs::write(path.join("selected.rs"), "pub fn selected() {}\n").unwrap();
        command(path, &["add", "."]);
        command(path, &["commit", "-qm", "changed source"]);
        fixture.tip = command(path, &["rev-parse", "HEAD"]);
        let query = format!("diff {}..{}", fixture.first, fixture.tip);
        let text = crate::history::run(fixture.store(), None, &query).unwrap();
        let local =
            Store::local(fixture.directory.path(), Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(text, crate::history::run(local, None, &query).unwrap());
        let expected: BTreeSet<_> = [
            format!("{}:selected.rs", fixture.first),
            format!("{}:selected.rs", fixture.tip),
            format!("{}:.gitattributes", fixture.tip),
        ]
        .iter()
        .map(|name| command(fixture.directory.path(), &["rev-parse", name]))
        .collect();
        let actual: BTreeSet<_> = fixture
            .inventory()
            .lines()
            .filter_map(|line| line.strip_suffix(" blob").map(str::to_owned))
            .collect();
        assert_eq!(actual, expected);
        let mut warm = fixture.store();
        warm.remote.as_mut().unwrap().transferred = materialize::TRANSFER_LIMIT;
        assert_eq!(text, crate::history::run(warm, None, &query).unwrap());
    }

    #[test]
    fn tree_filter_rejection_falls_back_to_blob_filter_without_source_transfer() {
        let fixture = Fixture::new("sha1");
        command(
            fixture.directory.path(),
            &["config", "uploadpackfilter.tree.allow", "false"],
        );
        let text = crate::history::run(fixture.store(), None, "log").unwrap();
        assert!(
            text.contains(&fixture.first) && text.ends_with("end\n"),
            "{text}"
        );
        assert!(!fixture.inventory().contains(" blob"));
    }
    #[test]
    fn missing_filter_capability_never_publishes_an_unfiltered_pack() {
        let fixture = Fixture::new("sha1");
        command(
            fixture.directory.path(),
            &["config", "uploadpack.allowFilter", "false"],
        );
        let result = crate::history::run(fixture.store(), None, "log");
        assert!(result.is_err());
        assert!(fixture.inventory().is_empty());
    }
    #[test]
    fn denied_repository_does_not_create_an_owner_or_contact_transport() {
        let cache = tempfile::tempdir().unwrap();
        let repository = Repository::parse("https://example.invalid/private/repo")
            .unwrap()
            .unwrap();
        let result = Store::remote(
            repository,
            &[],
            cache.path(),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(result.err().unwrap().to_string().contains("not authorized"));
        assert!(!cache.path().join("git").exists());
    }
    #[test]
    fn cancelled_persistent_reader_returns_and_reaps_the_child() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut child =
            crate::process::spawn(Command::new("sleep").arg("30").stdout(Stdio::piped())).unwrap();
        let pid = child.id() as i32;
        let mut pipe = Pipe {
            output: child.stdout.take().unwrap(),
            cancel: cancel.clone(),
        };
        cancel.store(true, Ordering::Release);
        assert!(pipe.read(&mut [0]).is_err());
        drop(child);
        // waitpid(ECHILD) proves Drop reaped the process, not just signalled it.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
