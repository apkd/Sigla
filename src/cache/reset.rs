//! Reset incompatible cache data while holding exclusive cache ownership.
use anyhow::{Context, Result};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const MARKER: &str = "cache-fingerprint";
pub(crate) const RETIRED: &str = ".retired-cache-";
const COMPATIBLE: &[u8] = concat!(
    "SIGLA-CACHE-2\n",
    env!("SIGLA_CACHE_LAYOUT_FINGERPRINT"),
    "\n"
)
.as_bytes();
const FINGERPRINT: &[u8] = concat!(
    "SIGLA-CACHE-2\n",
    env!("SIGLA_CACHE_LAYOUT_FINGERPRINT"),
    "\n",
    env!("SIGLA_CACHE_FINGERPRINT")
)
.as_bytes();

/// Retire incompatible workspace state, retaining inputs when the storage layout matches.
/// Returns true when any cache state was invalidated.
/// The caller must hold exclusive cache ownership, or own a private cache.
pub fn prepare(cache: &Path) -> Result<bool> {
    with_space_recovery(cache, || {
        prepare_with(cache, |from, to| fs::rename(from, to))
    })
}

/// Retry startup allocation after reclaiming obsolete data on a full filesystem.
/// The caller must hold exclusive ownership; successful startup keeps cleanup lazy.
pub(crate) fn with_space_recovery<T>(
    cache: &Path,
    mut initialize: impl FnMut() -> Result<T>,
) -> Result<T> {
    let error = match initialize() {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if !error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| matches!(error.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)))
    }) {
        return Err(error);
    }
    let incompatible = !current(cache)?;
    let mut reclaimed = false;
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        if entry.file_name() != "ownership.lock"
            && (incompatible || entry.file_name().to_string_lossy().starts_with(RETIRED))
        {
            super::cleanup::remove(&entry.path())?;
            reclaimed = true;
        }
    }
    if !reclaimed {
        return Err(error);
    }
    File::open(cache)?.sync_all()?;
    tracing::warn!(%error, "Reclaimed obsolete cache data to retry startup");
    initialize()
}

/// Cache ownership protects these detached trees. They stay on the cache mount
/// and remain part of normal allocation accounting until physical reclamation.
pub(crate) fn cleanup_step(
    cache: &Path,
    pending: &mut Option<super::cleanup::Removal>,
) -> Result<bool> {
    if pending.is_none() {
        for entry in fs::read_dir(cache)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(RETIRED) {
                *pending = Some(super::cleanup::Removal::new(&entry.path())?);
                break;
            }
        }
    }
    let Some(removal) = pending else {
        return Ok(true);
    };
    let done = match removal.step(1024) {
        Ok(done) => done,
        Err(error) => {
            *pending = None;
            return Err(error.into());
        }
    };
    if done {
        *pending = None;
        File::open(cache)?.sync_all()?;
    }
    Ok(false)
}

/// Checks the cache marker without changing cache contents.
pub fn current(cache: &Path) -> Result<bool> {
    Ok(marker(cache)?.is_some_and(|bytes| bytes == FINGERPRINT))
}

fn marker(cache: &Path) -> Result<Option<Vec<u8>>> {
    let marker = cache.join(MARKER);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != FINGERPRINT.len() as u64 {
        return Ok(None);
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(marker)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != FINGERPRINT.len() as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(FINGERPRINT.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() == FINGERPRINT.len()
        && bytes[COMPATIBLE.len()..].iter().all(u8::is_ascii_hexdigit))
    .then_some(bytes))
}

fn prepare_with(
    cache: &Path,
    mut retire_entry: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<bool> {
    let previous = marker(cache)?;
    if previous.as_deref() == Some(FINGERPRINT) {
        return Ok(false);
    }
    let compatible = previous.is_some_and(|bytes| bytes.starts_with(COMPATIBLE));
    let marker = cache.join(MARKER);
    // Retire an invalid marker first, including a directory or symlink. Even a
    // corrupt marker must not introduce recursive deletion before listening.
    let mut entries = fs::read_dir(cache)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name() != MARKER);
    let mut retired = None;
    for entry in entries {
        if entry.file_name() == "ownership.lock"
            || entry.file_name().to_string_lossy().starts_with(RETIRED)
            || compatible && entry.file_name() != MARKER
        {
            continue;
        }
        let directory = match &retired {
            Some(directory) => directory,
            None => retired.insert(
                tempfile::Builder::new()
                    .prefix(RETIRED)
                    .tempdir_in(cache)?
                    .keep(),
            ),
        };
        retire_entry(&entry.path(), &directory.join(entry.file_name()))
            .with_context(|| format!("Cannot retire cache entry {}", entry.path().display()))?;
    }
    if let Some(retired) = retired {
        File::open(retired)?.sync_all()?;
    }
    File::open(cache)?.sync_all()?;
    if compatible {
        // Recovery may replace the database. Finish it before opening any store.
        crate::repository::state_store::invalidate_indexed(cache)?;
        crate::repository::state_store::recover(cache)?;
        super::compaction::recover(cache)?;
        crate::store::retire_workspaces(&cache.join("analysis"))?;
        tracing::info!("Retired workspace graphs; retained compatible cache inputs and analysis");
    }
    let mut staged = tempfile::NamedTempFile::new_in(cache)?;
    staged.write_all(FINGERPRINT)?;
    staged.as_file().sync_all()?;
    staged.persist(marker)?;
    File::open(cache)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn reclaim(cache: &Path) {
        let mut pending = None;
        while !cleanup_step(cache, &mut pending).unwrap() {}
    }

    fn graph_change(cache: &Path) {
        let mut marker = FINGERPRINT.to_vec();
        let last = marker.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        fs::write(cache.join(MARKER), marker).unwrap();
    }

    #[test]
    fn compatible_reset_retires_graphs_but_reuses_inputs_and_analysis() {
        use crate::store::{Database, FileData, payload};
        for interrupted_compaction in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path();
            prepare(cache).unwrap();
            let inputs = [
                "git/repository/pack",
                "lfs/object",
                "blobs/object",
                "unity-metadata/editor/section",
                "unity-metadata-packs/pack/source",
                "unity-packages/package/source",
                "local-jobs/project/nuget/cache",
                "repositories/selector/generated/packages/cache",
                "repositories/selector/source/Code.cs",
            ];
            for path in inputs {
                let path = cache.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"retained input").unwrap();
            }
            let analysis = cache.join("analysis");
            let source = "pub struct Kept;";
            let language = crate::model::Language::Rust;
            let id = payload::source_id(source, language, &[], "2021").unwrap();
            let old_id = {
                let database = Database::open(&analysis).unwrap();
                let scope = database.workspace([1; 32], cache, None).unwrap();
                scope
                    .install(
                        "Code.rs",
                        vec![1],
                        id,
                        1,
                        || Ok(()),
                        || {
                            payload::encode(FileData {
                                source: source.into(),
                                assembly: None,
                                facts: crate::extract::extract(source, language, &[], "2021")?,
                            })
                        },
                    )
                    .unwrap();
                scope
                    .finish_refresh(b"obsolete graph", &["Code.rs".into()].into(), 1)
                    .unwrap();
                database.checkpoint().unwrap();
                scope.id
            };
            if interrupted_compaction {
                fs::rename(
                    analysis.join("data.mdb"),
                    analysis.join("data.mdb.previous"),
                )
                .unwrap();
                fs::write(analysis.join("data.mdb"), b"interrupted copy").unwrap();
            }
            graph_change(cache);
            assert!(prepare(cache).unwrap());
            assert!(current(cache).unwrap());
            for path in inputs {
                assert_eq!(fs::read(cache.join(path)).unwrap(), b"retained input");
            }
            let database = Database::open(&analysis).unwrap();
            assert!(database.existing([1; 32]).unwrap().is_none());
            assert!(
                database
                    .namespace([1; 32], old_id)
                    .unwrap()
                    .unwrap()
                    .is_retiring()
                    .unwrap()
            );
            let fresh = database.workspace([1; 32], cache, None).unwrap();
            assert_ne!(fresh.id, old_id);
            let installed = fresh
                .install(
                    "Code.rs",
                    vec![1],
                    id,
                    2,
                    || Ok(()),
                    || panic!("Compatible analysis must be reused"),
                )
                .unwrap();
            assert!(installed.reused);
            reclaim(cache);
            assert!(analysis.join("data.mdb").is_file());
            assert!(!prepare(cache).unwrap());
        }
    }

    #[test]
    fn interrupted_graph_reset_leaves_no_current_marker_and_retries_conservatively() {
        let root = tempfile::tempdir().unwrap();
        prepare(root.path()).unwrap();
        fs::write(root.path().join("input"), b"raw input").unwrap();
        // A failed database migration must leave the marker retired.
        fs::create_dir(root.path().join("analysis")).unwrap();
        fs::write(root.path().join("analysis/data.mdb"), b"invalid database").unwrap();
        graph_change(root.path());
        assert!(prepare(root.path()).is_err());
        assert!(!current(root.path()).unwrap());
        assert!(root.path().join("input").is_file());
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("input").exists());
        assert!(current(root.path()).unwrap());
    }

    #[test]
    fn layout_changes_and_legacy_markers_require_a_full_reset() {
        let mut different_layout = FINGERPRINT.to_vec();
        let byte = &mut different_layout[b"SIGLA-CACHE-2\n".len()];
        *byte = if *byte == b'0' { b'1' } else { b'0' };
        for marker in [different_layout, vec![b'a'; 64], COMPATIBLE.to_vec()] {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join(MARKER), marker).unwrap();
            fs::write(root.path().join("input"), b"old layout").unwrap();
            assert!(prepare(root.path()).unwrap());
            assert!(!root.path().join("input").exists());
            assert!(current(root.path()).unwrap());
        }
    }

    #[test]
    fn space_recovery_reclaims_only_obsolete_data_and_preserves_ownership() {
        for compatible in [false, true] {
            for code in [libc::ENOSPC, libc::EDQUOT, libc::EACCES] {
                let cache = tempfile::tempdir().unwrap();
                let lock = cache.path().join("ownership.lock");
                fs::write(&lock, b"owner").unwrap();
                let inode = fs::metadata(&lock).unwrap().ino();
                fs::create_dir(cache.path().join("old")).unwrap();
                fs::write(cache.path().join("old/value"), b"obsolete").unwrap();
                if compatible {
                    prepare(cache.path()).unwrap();
                    fs::write(cache.path().join("live"), b"current").unwrap();
                }
                let mut attempts = 0;
                let result = with_space_recovery(cache.path(), || {
                    attempts += 1;
                    if attempts == 1 {
                        return Err(std::io::Error::from_raw_os_error(code))
                            .context("Startup failed");
                    }
                    assert!(!cache.path().join("old").exists());
                    assert!(!fs::read_dir(cache.path()).unwrap().any(|entry| {
                        entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with(RETIRED)
                    }));
                    prepare(cache.path())
                });
                if code == libc::EACCES {
                    assert!(result.is_err());
                    assert_eq!(attempts, 1);
                } else {
                    assert!(result.is_ok());
                    assert_eq!(attempts, 2);
                    assert!(current(cache.path()).unwrap());
                }
                assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
                if compatible {
                    assert_eq!(fs::read(cache.path().join("live")).unwrap(), b"current");
                }
            }
        }
    }

    #[test]
    fn retirement_precedes_deletion_and_cleanup_preserves_the_new_namespace() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("old")).unwrap();
        fs::write(root.path().join("old/value"), vec![1; 8192]).unwrap();
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("old").exists());
        let retired = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| entry.file_name().to_string_lossy().starts_with(RETIRED))
            .unwrap()
            .path();
        assert_eq!(fs::read(retired.join("old/value")).unwrap(), vec![1; 8192]);
        fs::write(root.path().join("current"), "new").unwrap();
        assert!(!prepare(root.path()).unwrap());
        reclaim(root.path());
        assert!(!retired.exists());
        assert_eq!(fs::read(root.path().join("current")).unwrap(), b"new");
        assert!(current(root.path()).unwrap());
    }

    #[test]
    fn corrupt_marker_directory_is_retired_without_deleting_its_contents() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(MARKER)).unwrap();
        fs::write(root.path().join(MARKER).join("old"), b"retired").unwrap();
        assert!(prepare(root.path()).unwrap());
        assert!(current(root.path()).unwrap());
        let retired = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(RETIRED)
            })
            .unwrap();
        assert_eq!(
            fs::read(retired.join(MARKER).join("old")).unwrap(),
            b"retired"
        );
        reclaim(root.path());
        assert!(current(root.path()).unwrap());
    }

    #[test]
    fn missing_or_changed_fingerprint_clears_cache_but_preserves_ownership() {
        for old in [None, Some(b"old".as_slice())] {
            let root = tempfile::tempdir().unwrap();
            let lock = root.path().join("ownership.lock");
            fs::write(&lock, b"owner").unwrap();
            let identity = fs::metadata(&lock).unwrap().ino();
            fs::create_dir(root.path().join("repository")).unwrap();
            fs::write(root.path().join("repository/old.json"), b"old").unwrap();
            if let Some(old) = old {
                fs::write(root.path().join(MARKER), old).unwrap();
            }
            assert!(prepare(root.path()).unwrap());
            assert!(!root.path().join("repository").exists());
            assert_eq!(fs::read(root.path().join(MARKER)).unwrap(), FINGERPRINT);
            assert_eq!(fs::metadata(lock).unwrap().ino(), identity);
            fs::write(root.path().join("current"), b"keep").unwrap();
            assert!(!prepare(root.path()).unwrap());
            assert_eq!(fs::read(root.path().join("current")).unwrap(), b"keep");
        }
    }

    #[test]
    fn interrupted_cleanup_retries_and_never_touches_symlink_target() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"outside").unwrap();
        fs::write(root.path().join("ownership.lock"), b"owner").unwrap();
        fs::write(root.path().join(MARKER), b"old").unwrap();
        fs::write(root.path().join("fail"), b"old").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("alias")).unwrap();
        let result = prepare_with(root.path(), |path, destination| {
            if path.file_name().is_some_and(|name| name == "fail") {
                Err(std::io::Error::other("injected deletion failure"))
            } else {
                fs::rename(path, destination)
            }
        });
        assert!(result.is_err());
        assert!(!root.path().join(MARKER).exists());
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("alias").exists());
        reclaim(root.path());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"outside");
    }

    #[test]
    fn reset_removes_restricted_directories_without_changing_external_files() {
        check_restricted_cleanup(false);
        // CI also runs as root, where the standard deletion can bypass mode 000.
        check_restricted_cleanup(true);
    }

    fn check_restricted_cleanup(explicit_fallback: bool) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let retained = outside.path().join("retained");
        fs::write(&retained, b"shared").unwrap();
        fs::set_permissions(&retained, fs::Permissions::from_mode(0o444)).unwrap();
        let work = root
            .path()
            .join("repositories/old/generated/overlay-work/work");
        fs::create_dir_all(&work).unwrap();
        fs::hard_link(&retained, work.join("shared")).unwrap();
        std::os::unix::fs::symlink(outside.path(), work.join("outside")).unwrap();
        fs::set_permissions(&work, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(work.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap();
        if explicit_fallback {
            super::super::cleanup::restricted(&root.path().join("repositories")).unwrap();
        }
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("repositories").exists());
        reclaim(root.path());
        assert_eq!(fs::read(&retained).unwrap(), b"shared");
        assert_eq!(
            fs::metadata(&retained).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert!(current(root.path()).unwrap());
    }

    #[test]
    fn fingerprint_symlink_is_not_trusted_or_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("fingerprint");
        fs::write(&target, FINGERPRINT).unwrap();
        fs::write(root.path().join("stale"), b"old").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join(MARKER)).unwrap();
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("stale").exists());
        assert_eq!(fs::read(&target).unwrap(), FINGERPRINT);
        assert!(
            fs::symlink_metadata(root.path().join(MARKER))
                .unwrap()
                .is_file()
        );
    }
}
