//! Committed attribute policy, isolated from worktree and user overrides.
use super::{git, init, run};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

pub(super) const MEMORY: usize = super::MEMORY / 8;
const ENTRY_BYTES: usize = 80;

pub(super) struct Reader {
    control: tempfile::TempDir,
    objects: PathBuf,
    policy: Option<[u8; 32]>,
    paths: lru::LruCache<String, bool>,
    bytes: usize,
}

impl Reader {
    pub fn new(path: &Path, format: &str) -> Result<Self> {
        let control = tempfile::tempdir()?;
        init(control.path(), format)?;
        let objects = run(git(path).args(["rev-parse", "--git-path", "objects"]))?;
        let objects = PathBuf::from(std::str::from_utf8(&objects)?.trim()).canonicalize()?;
        Ok(Self {
            control,
            objects,
            policy: None,
            paths: lru::LruCache::unbounded(),
            bytes: 0,
        })
    }

    fn remember(&mut self, path: &str, allowed: bool) {
        if let Some(value) = self.paths.get_mut(path) {
            *value = allowed;
            return;
        }
        let path = path.to_owned();
        let bytes = path.capacity() + ENTRY_BYTES;
        if bytes > MEMORY {
            return;
        }
        while self.bytes > MEMORY - bytes {
            let (path, _) = self.paths.pop_lru().unwrap();
            self.bytes -= path.capacity() + ENTRY_BYTES;
        }
        self.bytes += bytes;
        self.paths.put(path, allowed);
    }

    pub fn select(
        &mut self,
        tree: &str,
        policy: [u8; 32],
        paths: &[String],
        value_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<BTreeSet<String>> {
        super::check(cancel)?;
        if self.policy != Some(policy) {
            self.paths.clear();
            self.bytes = 0;
            self.policy = Some(policy);
        }
        let mut allowed = BTreeSet::new();
        let mut missing = Vec::new();
        for path in paths {
            match self.paths.get(path) {
                Some(true) => {
                    allowed.insert(path.clone());
                }
                Some(false) => {}
                None => missing.push(path),
            }
        }
        let mut pending = missing.as_slice();
        while !pending.is_empty() {
            // Bound all three output records per path, including custom values.
            let mut bytes = 0usize;
            let mut count = 0;
            for path in pending {
                let cost = 3 * (path.len() + value_bytes + 9);
                if count != 0 && cost > MEMORY.saturating_sub(bytes) {
                    break;
                }
                bytes = bytes.saturating_add(cost);
                count += 1;
            }
            let (chunk, rest) = pending.split_at(count);
            pending = rest;
            let mut input = Vec::new();
            for path in chunk {
                input.extend_from_slice(path.as_bytes());
                input.push(0);
            }
            let output = crate::process::capture_cancellable(
                git(self.control.path())
                    .env("GIT_OBJECT_DIRECTORY", &self.objects)
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
                Some(cancel),
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
            for (path, fields) in chunk.iter().zip(fields.chunks_exact(9)) {
                let mut include = true;
                for fields in fields.chunks_exact(3) {
                    ensure!(
                        fields[0] == path.as_bytes(),
                        "Unexpected Git attribute path"
                    );
                    if fields[1] == b"binary" && fields[2] == b"set"
                        || fields[1] == b"diff" && fields[2] == b"unset"
                        || fields[1] == b"filter" && fields[2] == b"lfs"
                    {
                        include = false;
                    }
                }
                self.remember(path, include);
                if include {
                    allowed.insert((*path).clone());
                }
            }
        }
        Ok(allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_decisions_evict_within_their_reserved_budget() {
        let root = tempfile::tempdir().unwrap();
        init(root.path(), "sha1").unwrap();
        let mut reader = Reader::new(root.path(), "sha1").unwrap();
        let suffix = "x".repeat(MEMORY / 64);
        for index in 0..128 {
            reader.remember(&format!("{index}/{suffix}"), index % 2 == 0);
            assert!(reader.bytes <= MEMORY);
        }
        assert!(reader.paths.len() < 128);
        assert_eq!(
            reader.bytes,
            reader
                .paths
                .iter()
                .map(|(path, _)| path.capacity() + ENTRY_BYTES)
                .sum::<usize>()
        );
        let path = format!("127/{suffix}");
        let before = reader.bytes;
        reader.remember(&path, true);
        assert_eq!(reader.paths.get(&path), Some(&true));
        assert_eq!(reader.bytes, before);
    }
}
