//! GitHub inventories contain blob sizes without requiring blob downloads.
use super::{Endpoint, Repository, selection::validate_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{BufWriter, Read, Write},
    path::Path,
    time::Duration,
};

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    truncated: bool,
}
#[derive(Deserialize)]
struct Entry {
    path: String,
    sha: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
}
#[derive(Deserialize, Serialize)]
struct Inventory {
    version: u32,
    sizes: BTreeMap<String, u64>,
}

pub fn sizes(
    repository: &Repository,
    revision: &str,
    store: &Path,
    private: bool,
) -> Result<Option<BTreeMap<String, u64>>> {
    if repository.identity.endpoint != Endpoint::Github {
        return Ok(None);
    }
    let cache = store.join(format!("github-tree-{revision}.json"));
    if let Ok(bytes) = std::fs::read(&cache)
        && let Ok(saved) = serde_json::from_slice::<Inventory>(&bytes)
        && saved.version == 1
    {
        return Ok(Some(saved.sizes));
    }
    let repo = repository.identity.components.join("/");
    let (client, token) = client(repository, private)?;
    let fetch = |sha: &str, recursive: bool| -> Result<Tree> {
        let mut request = client
            .get(format!(
                "https://api.github.com/repos/{repo}/git/trees/{sha}"
            ))
            .header("Accept", "application/vnd.github+json");
        if recursive {
            request = request.query(&[("recursive", "1")]);
        }
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|_| anyhow::anyhow!("GitHub file inventory request failed"))?;
        ensure!(
            response.status().is_success(),
            "GitHub file inventory unavailable (HTTP {}); check API credentials or rate limits",
            response.status()
        );
        serde_json::from_reader(response.take(16 * 1024 * 1024))
            .context("Invalid GitHub file inventory")
    };
    let sizes = inventory(revision, fetch)?;
    let saved = Inventory { version: 1, sizes };
    let mut file = tempfile::NamedTempFile::new_in(store)?;
    let mut writer = BufWriter::new(&mut file);
    serde_json::to_writer(&mut writer, &saved)?;
    writer.flush()?;
    drop(writer);
    file.persist(cache)?;
    Ok(Some(saved.sizes))
}

fn client(
    repository: &Repository,
    private: bool,
) -> Result<(reqwest::blocking::Client, Option<String>)> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("sigla")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut token = None;
    if private {
        let repo = repository.identity.components.join("/");
        let result = crate::process::capture(
            super::credentials::noninteractive(
                std::process::Command::new("git").args(["credential", "fill"]),
            ),
            Duration::from_secs(30),
            Some(format!("url=https://github.com/{repo}.git\n\n").into_bytes()),
            None,
        )?;
        if result.status.success() {
            token = String::from_utf8(result.stdout)?
                .lines()
                .find_map(|l| l.strip_prefix("password=").map(str::to_owned));
        }
    }
    Ok((client, token))
}

/// Read one immutable source object without waiting for bulk Git acquisition.
/// Unsupported responses fall back to the ordinary repository materializer.
pub(super) fn blob(repository: &Repository, oid: &str, private: bool) -> Result<Option<Vec<u8>>> {
    if repository.identity.endpoint != Endpoint::Github {
        return Ok(None);
    }
    let id = gix_hash::ObjectId::from_hex(oid.as_bytes())?;
    let (client, token) = client(repository, private)?;
    let repo = repository.identity.components.join("/");
    let mut request = client
        .get(format!(
            "https://api.github.com/repos/{repo}/git/blobs/{id}"
        ))
        .header("Accept", "application/vnd.github.raw+json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|_| anyhow::anyhow!("GitHub source request failed"))?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > super::materialize::TRANSFER_LIMIT)
    {
        return Ok(None);
    }
    let limit = response
        .content_length()
        .unwrap_or(super::materialize::TRANSFER_LIMIT);
    read_blob(id, response, limit)
}

fn read_blob(id: gix_hash::ObjectId, input: impl Read, limit: u64) -> Result<Option<Vec<u8>>> {
    let _admission = crate::memory::admit_file(limit, false);
    let mut bytes = Vec::new();
    input.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    let mut hash = gix_hash::hasher(id.kind());
    hash.update(format!("blob {}\0", bytes.len()).as_bytes());
    hash.update(&bytes);
    ensure!(
        hash.try_finalize()? == id,
        "GitHub source object identity mismatch"
    );
    if bytes.starts_with(b"version https://git-lfs.github.com/spec/") {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn inventory(
    revision: &str,
    mut fetch: impl FnMut(&str, bool) -> Result<Tree>,
) -> Result<BTreeMap<String, u64>> {
    let first = fetch(revision, true)?;
    let mut sizes = BTreeMap::new();
    if !first.truncated {
        for entry in first.tree {
            insert(&mut sizes, entry)?;
        }
    } else {
        let mut pending = vec![(String::new(), revision.to_owned())];
        let mut count = 0;
        while let Some((prefix, sha)) = pending.pop() {
            count += 1;
            ensure!(count <= 100_000, "GitHub tree traversal limit exceeded");
            let tree = fetch(&sha, false)?;
            ensure!(
                !tree.truncated,
                "GitHub returned an incomplete directory inventory"
            );
            for mut entry in tree.tree {
                entry.path = format!("{prefix}{}", entry.path);
                if entry.kind == "tree" {
                    pending.push((format!("{}/", entry.path), entry.sha));
                } else {
                    insert(&mut sizes, entry)?;
                }
            }
        }
    }
    Ok(sizes)
}

fn insert(sizes: &mut BTreeMap<String, u64>, entry: Entry) -> Result<()> {
    validate_path(&entry.path)?;
    if entry.kind == "blob" {
        sizes.insert(
            entry.path,
            entry.size.context("GitHub omitted a file size")?,
        );
    }
    ensure!(
        sizes.len() <= 1_000_000,
        "GitHub inventory exceeds file limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object_id(bytes: &[u8]) -> gix_hash::ObjectId {
        let mut hash = gix_hash::hasher(gix_hash::Kind::Sha1);
        hash.update(format!("blob {}\0", bytes.len()).as_bytes());
        hash.update(bytes);
        hash.try_finalize().unwrap()
    }

    #[test]
    fn source_blobs_verify_identity_and_defer_lfs_hydration() {
        let source = b"class Example {}\n";
        let id = object_id(source);
        assert_eq!(
            read_blob(id, source.as_slice(), 1024).unwrap().unwrap(),
            source
        );
        assert!(read_blob(id, b"class Different {}".as_slice(), 1024).is_err());
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 12\n";
        assert!(
            read_blob(object_id(pointer), pointer.as_slice(), 1024)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn truncated_listing_is_replaced_by_complete_subtree_walk() {
        let result = inventory("commit", |sha, recursive| {
            let json = if recursive {
                serde_json::json!({"truncated":true,"tree":[{"path":"partial","type":"blob","sha":"p","size":99}]})
            } else if sha == "commit" {
                serde_json::json!({"truncated":false,"tree":[{"path":"Assets","type":"tree","sha":"subtree"}]})
            } else {
                serde_json::json!({"truncated":false,"tree":[{"path":"Thing.prefab","type":"blob","sha":"blob","size":123}]})
            };
            Ok(serde_json::from_value(json)?)
        }).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("Assets/Thing.prefab"));
        assert!(!result.contains_key("partial"));
    }
    #[test]
    fn incomplete_subtrees_and_missing_sizes_fail_instead_of_selecting_blindly() {
        assert!(
            inventory("commit", |_, _| Ok(Tree {
                tree: vec![],
                truncated: true
            }))
            .is_err()
        );
        assert!(
            inventory("commit", |_, _| Ok(Tree {
                truncated: false,
                tree: vec![Entry {
                    path: "file.asset".into(),
                    sha: "blob".into(),
                    kind: "blob".into(),
                    size: None
                }]
            }))
            .is_err()
        );
    }
}
