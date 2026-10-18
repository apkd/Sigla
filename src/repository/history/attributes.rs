//! Committed attribute policy, isolated from worktree and user overrides.
use super::{Entry, git, init, run};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

pub(super) const MEMORY: usize = super::MEMORY / 8;
const OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const INDEX_BYTES: usize = 8 * 1024 * 1024;
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

    fn run(&self, args: &[&str], input: Option<Vec<u8>>, cancel: &AtomicBool) -> Result<Vec<u8>> {
        super::check(cancel)?;
        let output = crate::process::capture_cancellable(
            git(self.control.path())
                .env("GIT_OBJECT_DIRECTORY", &self.objects)
                .env("GIT_ATTR_NOSYSTEM", "1")
                .args(["-c", "core.attributesFile=/dev/null"])
                .args(args),
            Duration::from_secs(120),
            input,
            None,
            Some(cancel),
        )?;
        ensure!(
            output.status.success(),
            "Cannot read committed source attributes"
        );
        Ok(output.stdout)
    }

    fn prepare(
        &mut self,
        policy: [u8; 32],
        attributes: &[(&str, &Entry)],
        cancel: &AtomicBool,
    ) -> Result<()> {
        if self.policy == Some(policy) {
            return Ok(());
        }
        // A failed rebuild must not leave the previous policy claiming this index.
        self.policy = None;
        self.run(&["read-tree", "--empty"], None, cancel)?;
        let mut input = Vec::new();
        for (path, entry) in attributes {
            let bytes = path.len() + entry.id.len() + 14;
            if !input.is_empty() && bytes > INDEX_BYTES.saturating_sub(input.len()) {
                self.run(
                    &["update-index", "-z", "--index-info"],
                    Some(std::mem::take(&mut input)),
                    cancel,
                )?;
            }
            write!(input, "{:o} blob {}\t{path}\0", entry.mode, entry.id)?;
        }
        if !input.is_empty() {
            self.run(&["update-index", "-z", "--index-info"], Some(input), cancel)?;
        }
        self.paths.clear();
        self.bytes = 0;
        self.policy = Some(policy);
        Ok(())
    }

    pub fn exclusions(
        &mut self,
        policy: [u8; 32],
        attributes: &[(&str, &Entry)],
        paths: &[&str],
        value_bytes: usize,
        cancel: &AtomicBool,
    ) -> Result<BTreeSet<String>> {
        super::check(cancel)?;
        self.prepare(policy, attributes, cancel)?;
        let mut excluded = BTreeSet::new();
        let mut missing = Vec::new();
        for &path in paths {
            match self.paths.get(path) {
                Some(false) => {
                    excluded.insert(path.to_owned());
                }
                Some(true) => {}
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
                if count != 0 && cost > OUTPUT_BYTES.saturating_sub(bytes) {
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
            let output = self.run(
                &[
                    "check-attr",
                    "--cached",
                    "-z",
                    "--stdin",
                    "binary",
                    "diff",
                    "filter",
                ],
                Some(input),
                cancel,
            )?;
            let mut fields: Vec<_> = output.split(|&b| b == 0).collect();
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
                if !include {
                    excluded.insert((*path).to_owned());
                }
            }
        }
        Ok(excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn blob(reader: &Reader, contents: &str, cancel: &AtomicBool) -> Arc<str> {
        String::from_utf8(
            reader
                .run(
                    &["hash-object", "-w", "--stdin"],
                    Some(contents.as_bytes().to_vec()),
                    cancel,
                )
                .unwrap(),
        )
        .unwrap()
        .trim()
        .into()
    }

    fn tree(reader: &Reader, entries: &super::super::Tree, cancel: &AtomicBool) -> String {
        reader.run(&["read-tree", "--empty"], None, cancel).unwrap();
        let mut input = Vec::new();
        for (path, entry) in entries {
            let kind = if entry.mode == 0o160000 {
                "commit"
            } else {
                "blob"
            };
            write!(input, "{:o} {kind} {}\t{path}\0", entry.mode, entry.id).unwrap();
        }
        reader
            .run(&["update-index", "-z", "--index-info"], Some(input), cancel)
            .unwrap();
        String::from_utf8(reader.run(&["write-tree"], None, cancel).unwrap())
            .unwrap()
            .trim()
            .into()
    }

    #[test]
    fn indexed_attributes_match_source_trees_across_policy_changes() {
        for format in ["sha1", "sha256"] {
            let root = tempfile::tempdir().unwrap();
            init(root.path(), format).unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let writer = Reader::new(root.path(), format).unwrap();
            let mut entries = super::super::Tree::new();
            for (path, contents, mode) in [
                (
                    ".gitattributes",
                    "[attr]large filter=lfs -diff\n*.cs diff=custom-driver\n*.asset large\n\"quoted é file.cs\" filter=lfs\n",
                    0o100644,
                ),
                (
                    "nested/.gitattributes",
                    "*.cs -diff\nkeep.cs diff\n*.asset -filter diff\n",
                    0o100755,
                ),
                (
                    "nested/deeper/.gitattributes",
                    "*.cs diff=another-driver\n",
                    0o100644,
                ),
                (
                    "directory/.gitattributes/.gitattributes",
                    "*.cs binary\n",
                    0o100644,
                ),
                ("space é line/.gitattributes", "*.cs binary\n", 0o100644),
                ("linked/.gitattributes", "../policy", 0o120000),
                ("policy", "*.cs binary\n", 0o100644),
            ] {
                entries.insert(
                    path.into(),
                    Entry {
                        id: blob(&writer, contents, &cancel),
                        mode,
                    },
                );
            }
            // Gitlinks need not have their objects in the containing repository.
            entries.insert(
                "module/.gitattributes".into(),
                Entry {
                    id: "1".repeat(if format == "sha1" { 40 } else { 64 }).into(),
                    mode: 0o160000,
                },
            );
            let paths = [
                "root.cs",
                "macro.asset",
                "nested/source.cs",
                "nested/keep.cs",
                "nested/deeper/source.cs",
                "nested/macro.asset",
                "nested/missing.cs",
                "renamed/source.cs",
                "linked/source.cs",
                "module/source.cs",
                "directory/source.cs",
                "directory/.gitattributes/child.cs",
                "space é line/source.cs",
                "quoted é file.cs",
                "missing.cs",
            ];
            let source = blob(&writer, "class Source {}\n", &cancel);
            for path in paths.iter().filter(|p| !p.ends_with("missing.cs")) {
                entries.insert(
                    (*path).into(),
                    Entry {
                        id: source.clone(),
                        mode: 0o100644,
                    },
                );
            }
            let original = tree(&writer, &entries, &cancel);
            let attributes = entries.remove("nested/.gitattributes").unwrap();
            entries.insert("renamed/.gitattributes".into(), attributes);
            let renamed = tree(&writer, &entries, &cancel);
            entries.remove("renamed/.gitattributes");
            let deleted = tree(&writer, &entries, &cancel);
            let mut input = Vec::new();
            for path in paths {
                input.extend_from_slice(path.as_bytes());
                input.push(0);
            }
            let mut store = super::super::Store::local(root.path(), cancel.clone()).unwrap();
            for revision in [&original, &renamed, &deleted, &original] {
                let inventory = store.tree(revision).unwrap();
                let mut expected: BTreeSet<String> = [
                    "macro.asset",
                    "directory/.gitattributes/child.cs",
                    "space é line/source.cs",
                    "quoted é file.cs",
                ]
                .map(str::to_owned)
                .into();
                if revision == &original {
                    expected.extend(["nested/source.cs", "nested/missing.cs"].map(str::to_owned));
                } else {
                    expected.insert("nested/macro.asset".into());
                }
                if revision == &renamed {
                    expected.insert("renamed/source.cs".into());
                }
                // Repeat to exercise the path-decision cache as well as the index.
                for _ in 0..2 {
                    assert_eq!(
                        store
                            .source_exclusions(revision, &inventory, &paths)
                            .unwrap(),
                        expected
                    );
                }
                let from_tree = writer
                    .run(
                        &[
                            "check-attr",
                            "--source",
                            revision,
                            "-z",
                            "--stdin",
                            "binary",
                            "diff",
                            "filter",
                        ],
                        Some(input.clone()),
                        &cancel,
                    )
                    .unwrap();
                let from_index = store
                    .attributes
                    .as_ref()
                    .unwrap()
                    .run(
                        &[
                            "check-attr",
                            "--cached",
                            "-z",
                            "--stdin",
                            "binary",
                            "diff",
                            "filter",
                        ],
                        Some(input.clone()),
                        &cancel,
                    )
                    .unwrap();
                assert_eq!(from_index, from_tree, "{format} {revision}");
            }
        }
    }

    #[test]
    fn indexed_attributes_keep_newlines_in_nul_delimited_paths() {
        // History currently rejects control characters before reaching Reader.
        // The Git index and attribute transport must still preserve its framing.
        for format in ["sha1", "sha256"] {
            let root = tempfile::tempdir().unwrap();
            init(root.path(), format).unwrap();
            let cancel = AtomicBool::new(false);
            let mut reader = Reader::new(root.path(), format).unwrap();
            let entry = Entry {
                id: blob(&reader, "*.cs binary\n", &cancel),
                mode: 0o100644,
            };
            let attributes = [("space é\nline/.gitattributes", &entry)];
            let entries = [(attributes[0].0.into(), entry.clone())].into();
            let revision = tree(&reader, &entries, &cancel);
            let paths = ["space é\nline/source\nname.cs", "ordinary.cs"];
            let input = paths
                .iter()
                .flat_map(|p| p.bytes().chain([0]))
                .collect::<Vec<_>>();
            let from_tree = reader
                .run(
                    &[
                        "check-attr",
                        "--source",
                        &revision,
                        "-z",
                        "--stdin",
                        "binary",
                        "diff",
                        "filter",
                    ],
                    Some(input.clone()),
                    &cancel,
                )
                .unwrap();
            assert_eq!(
                reader
                    .exclusions(
                        *blake3::hash(revision.as_bytes()).as_bytes(),
                        &attributes,
                        &paths,
                        32,
                        &cancel
                    )
                    .unwrap(),
                BTreeSet::from([paths[0].to_owned()]),
            );
            let from_index = reader
                .run(
                    &[
                        "check-attr",
                        "--cached",
                        "-z",
                        "--stdin",
                        "binary",
                        "diff",
                        "filter",
                    ],
                    Some(input),
                    &cancel,
                )
                .unwrap();
            assert_eq!(from_index, from_tree);
        }
    }

    #[test]
    fn failed_index_rebuild_restores_the_requested_policy_on_retry() {
        let root = tempfile::tempdir().unwrap();
        init(root.path(), "sha1").unwrap();
        let cancel = AtomicBool::new(false);
        let mut reader = Reader::new(root.path(), "sha1").unwrap();
        let valid = Entry {
            id: blob(&reader, "*.rs binary\n", &cancel),
            mode: 0o100644,
        };
        let invalid = Entry {
            id: "invalid-object-id".into(),
            mode: 0o100644,
        };
        let policy = *blake3::hash(b"original").as_bytes();
        let changed = *blake3::hash(b"changed").as_bytes();
        reader
            .exclusions(
                policy,
                &[(".gitattributes", &valid)],
                &["first.rs"],
                32,
                &cancel,
            )
            .unwrap();
        assert!(
            reader
                .exclusions(
                    changed,
                    &[(".gitattributes", &invalid)],
                    &["first.rs"],
                    32,
                    &cancel
                )
                .is_err()
        );
        assert_eq!(
            reader
                .exclusions(
                    policy,
                    &[(".gitattributes", &valid)],
                    &["next.rs"],
                    32,
                    &cancel
                )
                .unwrap(),
            BTreeSet::from(["next.rs".into()]),
        );
    }

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
