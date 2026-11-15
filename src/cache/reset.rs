//! Reset incompatible cache data while holding exclusive cache ownership.
use anyhow::Result;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const MARKER: &str = "cache-fingerprint";
const FINGERPRINT: &[u8] = env!("SIGLA_CACHE_FINGERPRINT").as_bytes();

/// Returns true after clearing an incompatible or unmarked cache.
/// The caller must hold exclusive cache ownership, or own a private cache.
pub fn prepare(cache: &Path) -> Result<bool> {
    prepare_with(cache, remove)
}

/// Checks the cache marker without changing cache contents.
pub fn current(cache: &Path) -> Result<bool> {
    let marker = cache.join(MARKER);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() != FINGERPRINT.len() as u64 {
        return Ok(false);
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(marker)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != FINGERPRINT.len() as u64 {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    file.take(FINGERPRINT.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes == FINGERPRINT)
}

fn remove(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn prepare_with(
    cache: &Path,
    mut remove_entry: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<bool> {
    let marker = cache.join(MARKER);
    if current(cache)? {
        return Ok(false);
    }
    // An incomplete reset cannot leave a current marker behind.
    match fs::symlink_metadata(&marker) {
        Ok(_) => remove_entry(&marker)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        if entry.file_name() != "ownership.lock" {
            remove_entry(&entry.path())?;
        }
    }
    File::open(cache)?.sync_all()?;
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
    use std::os::unix::fs::MetadataExt;

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
        let result = prepare_with(root.path(), |path| {
            if path.file_name().is_some_and(|name| name == "fail") {
                Err(std::io::Error::other("injected deletion failure"))
            } else {
                remove(path)
            }
        });
        assert!(result.is_err());
        assert!(!root.path().join(MARKER).exists());
        assert!(prepare(root.path()).unwrap());
        assert!(!root.path().join("alias").exists());
        assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"outside");
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
