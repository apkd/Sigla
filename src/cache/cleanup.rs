//! Delete private trees after their writers and readers have released them.
use std::{fs, io, os::unix::fs::PermissionsExt, path::Path};

pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match remove_existing(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn remove_existing(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => restricted(path),
        result => result,
    }
}

// OverlayFS and build tools can leave inaccessible directories. The caller owns
// this tree exclusively; never change shared file permissions or follow links.
pub(super) fn restricted(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in fs::read_dir(path)? {
        restricted(&entry?.path())?;
    }
    fs::remove_dir(path)
}
