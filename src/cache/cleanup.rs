//! Delete private trees after their writers and readers have released them.
use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// Incremental removal of a private tree. Open directory cursors avoid rescanning
/// large directories between maintenance batches. Errors retain the current entry.
pub(crate) struct Removal {
    stack: Vec<(PathBuf, std::iter::Peekable<fs::ReadDir>)>,
}

impl Removal {
    pub fn new(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let stack = if metadata.is_dir() {
            vec![(path.to_owned(), Self::directory(path, metadata)?.peekable())]
        } else {
            fs::remove_file(path)?;
            Vec::new()
        };
        Ok(Self { stack })
    }

    fn directory(path: &Path, metadata: fs::Metadata) -> io::Result<fs::ReadDir> {
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
        }
        fs::read_dir(path)
    }

    pub fn step(&mut self, limit: usize) -> io::Result<bool> {
        for _ in 0..limit {
            let Some((path, entries)) = self.stack.last_mut() else {
                return Ok(true);
            };
            if let Some(entry) = entries.peek() {
                let entry = entry
                    .as_ref()
                    .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
                let child = entry.path();
                if entry.file_type()?.is_dir() {
                    let next = Self::directory(&child, fs::symlink_metadata(&child)?)?;
                    entries.next();
                    self.stack.push((child, next.peekable()));
                } else {
                    fs::remove_file(child)?;
                    entries.next();
                }
            } else {
                fs::remove_dir(path)?;
                self.stack.pop();
            }
        }
        Ok(self.stack.is_empty())
    }
}

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
