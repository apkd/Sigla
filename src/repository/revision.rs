//! Resolve selectors against advertised refs and complete commit histories.
use super::{
    Endpoint, Repository,
    materialize::{self, Target},
    transport::{Fetch, Filter, Session},
};
use anyhow::{Context, Result, ensure};
use gix_hash::ObjectId;
use gix_protocol::handshake::Ref;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(super) struct Resolved {
    pub id: ObjectId,
    pub branch: Option<String>,
    pub pinned: bool,
    pub bytes: u64,
    pub requests: usize,
}

#[derive(Debug)]
pub(super) enum PrefixError {
    Missing(String),
    Ambiguous(String),
}
impl std::fmt::Display for PrefixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(prefix) => write!(f, "Commit prefix {prefix:?} was not found"),
            Self::Ambiguous(prefix) => {
                write!(f, "Commit prefix {prefix:?} is ambiguous; use a longer SHA")
            }
        }
    }
}
impl std::error::Error for PrefixError {}
impl Resolved {
    fn new(id: ObjectId) -> Self {
        Self {
            id,
            branch: None,
            pinned: false,
            bytes: 0,
            requests: 0,
        }
    }
}

pub(super) fn is_prefix(value: &str) -> bool {
    (7..64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn resolve(
    session: &mut Session,
    target: &Target,
    prefix: impl FnOnce(&mut Session, &str) -> Result<Resolved>,
) -> Result<Resolved> {
    if let Target::Branch(name) | Target::Tag(name) | Target::Named(name) = target {
        super::validate_branch(name)?;
    }
    if let Target::Commit(id) = target {
        return Ok(Resolved::new(ObjectId::from_hex(id.as_bytes())?));
    }
    let names = match target {
        Target::DefaultBranch => vec!["HEAD".into()],
        Target::Branch(s) => vec![format!("refs/heads/{s}")],
        Target::Tag(s) => vec![format!("refs/tags/{s}")],
        Target::Named(s) => vec![format!("refs/tags/{s}"), format!("refs/heads/{s}")],
        Target::Commit(_) => unreachable!(),
    };
    let refs = session.refs(&names)?;
    for name in &names {
        for reference in &refs {
            let (full_ref_name, object) = match reference {
                Ref::Direct {
                    full_ref_name,
                    object,
                }
                | Ref::Peeled {
                    full_ref_name,
                    object,
                    ..
                }
                | Ref::Symbolic {
                    full_ref_name,
                    object,
                    ..
                } => (full_ref_name, object),
                _ => continue,
            };
            if full_ref_name.as_slice() != name.as_bytes() {
                continue;
            }
            let mut resolved = Resolved::new(*object);
            let branch = match reference {
                Ref::Symbolic { target, .. } if name == "HEAD" => std::str::from_utf8(target)?,
                _ => name,
            };
            resolved.branch = branch.strip_prefix("refs/heads/").map(str::to_owned);
            return Ok(resolved);
        }
    }
    if let Target::Named(name) = target
        && is_prefix(name)
    {
        return prefix(session, name);
    }
    let name = match target {
        Target::DefaultBranch => "HEAD",
        Target::Branch(s) | Target::Tag(s) | Target::Named(s) => s,
        Target::Commit(_) => unreachable!(),
    };
    anyhow::bail!(target.unavailable(name))
}

pub(super) struct Prefix<'a> {
    pub repository: &'a Repository,
    pub private: bool,
    pub store: &'a Path,
    pub cancel: &'a AtomicBool,
    pub remaining: Option<u64>,
}

impl Prefix<'_> {
    pub fn resolve(
        &self,
        session: &mut Session,
        prefix: &str,
        connect: impl Fn() -> Result<Session>,
    ) -> Result<Resolved> {
        check(self.cancel)?;
        let prefix = prefix.to_ascii_lowercase();
        if self.repository.identity.endpoint == Endpoint::Github {
            let (id, bytes) = super::github::commit(
                self.repository,
                &prefix,
                self.private,
                self.cancel,
                self.remaining,
            )?;
            let mut resolved = Resolved::new(id);
            resolved.pinned = true;
            resolved.bytes = bytes;
            resolved.requests = 1;
            return Ok(resolved);
        }
        let refs = session.refs(&["refs/heads/".into(), "refs/tags/".into()])?;
        ensure!(
            refs.len() <= 100_000,
            "Repository reference inventory exceeds resolution limit"
        );
        let mut tips = BTreeSet::new();
        for reference in refs {
            let (name, object) = match reference {
                Ref::Direct {
                    full_ref_name,
                    object,
                }
                | Ref::Peeled {
                    full_ref_name,
                    object,
                    ..
                }
                | Ref::Symbolic {
                    full_ref_name,
                    object,
                    ..
                } => (full_ref_name, object),
                _ => continue,
            };
            if name.starts_with(b"refs/heads/") || name.starts_with(b"refs/tags/") {
                tips.insert(object);
            }
        }
        if tips.is_empty() {
            return Err(PrefixError::Missing(prefix).into());
        }
        // A separate shallow boundary is essential: a workspace's depth-one fetch
        // must never make a partial commit graph appear complete.
        let control = tempfile::tempdir()?;
        let mut bytes = 0;
        let mut requests = 0;
        let commits = match walk(self.store, control.path(), &tips, self.cancel)? {
            Some(commits) => commits,
            None => {
                let mut filter = Filter::Commits;
                loop {
                    check(self.cancel)?;
                    requests += 1;
                    let result = materialize::receive_filtered_measured(
                        session,
                        self.store,
                        control.path(),
                        tips.iter().copied().collect(),
                        &Fetch {
                            filter,
                            depth: None,
                            cancel: self.cancel,
                        },
                        self.remaining,
                    );
                    match result {
                        Ok(transfer) => {
                            bytes += transfer.bytes;
                            break;
                        }
                        Err(error)
                            if matches!(filter, Filter::Commits)
                                && error
                                    .downcast_ref::<super::transport::TreeFilterRejected>()
                                    .is_some() =>
                        {
                            *session = connect()?;
                            filter = Filter::Trees;
                        }
                        Err(error) => {
                            return Err(
                                error.context("Cannot acquire commit history for SHA resolution")
                            );
                        }
                    }
                }
                if !session.can_fetch_again() {
                    *session = connect()?;
                }
                let shallow = control.path().join("shallow");
                ensure!(
                    !shallow.try_exists()? || std::fs::read(shallow)?.is_empty(),
                    "Cannot resolve a commit prefix against incomplete upstream history"
                );
                walk(self.store, control.path(), &tips, self.cancel)?.context(
                    "Acquired commit history is incomplete; cannot resolve a commit prefix",
                )?
            }
        };
        let mut selected = None;
        for line in BufReader::new(commits).lines() {
            check(self.cancel)?;
            let line = line?;
            if line.starts_with(&prefix) {
                let id = ObjectId::from_hex(line.as_bytes())?;
                if selected.replace(id).is_some() {
                    return Err(PrefixError::Ambiguous(prefix).into());
                }
            }
        }
        let mut resolved = Resolved::new(selected.ok_or(PrefixError::Missing(prefix))?);
        resolved.pinned = true;
        resolved.bytes = bytes;
        resolved.requests = requests;
        Ok(resolved)
    }
}

fn walk(
    store: &Path,
    control: &Path,
    tips: &BTreeSet<ObjectId>,
    cancel: &AtomicBool,
) -> Result<Option<File>> {
    let mut listing = tempfile::tempfile()?;
    let input = tips
        .iter()
        .map(|id| format!("{id}\n"))
        .collect::<String>()
        .into_bytes();
    let result = crate::process::capture_cancellable(
        materialize::git(store)
            .args(["rev-list", "--stdin"])
            .env("GIT_SHALLOW_FILE", control.join("shallow"))
            .env("GIT_NO_REPLACE_OBJECTS", "1"),
        Duration::from_secs(120),
        Some(input),
        Some(listing.try_clone()?),
        Some(cancel),
    )?;
    ensure!(
        listing.metadata()?.len() <= 256 * 1024 * 1024,
        "Commit history exceeds resolution limit"
    );
    if !result.status.success() {
        return Ok(None);
    }
    listing.seek(SeekFrom::Start(0))?;
    Ok(Some(listing))
}

fn check(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "Revision resolution cancelled"
    );
    Ok(())
}

pub(super) fn init(path: &Path, format: &str) -> Result<()> {
    ensure!(
        matches!(format, "sha1" | "sha256"),
        "Unsupported Git object format"
    );
    if !path.join("HEAD").is_file() {
        std::fs::create_dir_all(path)?;
        crate::process::run(
            std::process::Command::new("git")
                .args(["init", "--bare", "--quiet", "--template="])
                .arg(format!("--object-format={format}"))
                .arg(path),
            Duration::from_secs(120),
        )?;
    }
    Ok(())
}

/// A local repository defines the available object set; remote caches never use this path.
pub(super) fn local_prefix(store: &Path, selector: &str, cancel: &AtomicBool) -> Result<ObjectId> {
    check(cancel)?;
    let prefix = selector.to_ascii_lowercase();
    let candidates = crate::process::capture_cancellable(
        materialize::git(store)
            .arg("rev-parse")
            .arg(format!("--disambiguate={prefix}")),
        Duration::from_secs(120),
        None,
        None,
        Some(cancel),
    )?;
    ensure!(
        candidates.status.success(),
        "Cannot inspect local commit identities"
    );
    let objects = crate::process::capture_cancellable(
        materialize::git(store).args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]),
        Duration::from_secs(120),
        Some(candidates.stdout),
        None,
        Some(cancel),
    )?;
    ensure!(objects.status.success(), "Cannot inspect local Git objects");
    let mut selected = None;
    for line in std::str::from_utf8(&objects.stdout)?.lines() {
        let (id, kind) = line
            .split_once(' ')
            .context("Invalid local Git object record")?;
        if kind != "commit" {
            continue;
        }
        ensure!(
            id.starts_with(&prefix),
            "Git disambiguation returned an unrelated object"
        );
        if selected
            .replace(ObjectId::from_hex(id.as_bytes())?)
            .is_some()
        {
            return Err(PrefixError::Ambiguous(selector.into()).into());
        }
    }
    selected.ok_or_else(|| PrefixError::Missing(selector.into()).into())
}
