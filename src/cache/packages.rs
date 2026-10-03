//! Writable package views over immutable files. Overlayfs owns merge semantics.
use super::blobs::Store;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

const GENERATION: &str = ".sigla-generation";
#[derive(Serialize, Deserialize)]
struct Publication {
    stage: String,
    generation: String,
}

pub struct View {
    store: Arc<Store>,
    root: PathBuf,
    _lock: File,
}

impl View {
    pub fn local(cache: &Path, directory: &Path) -> Result<Self> {
        let root = cache.join("local-packages").join(
            blake3::hash(directory.as_os_str().as_bytes())
                .to_hex()
                .as_str(),
        );
        let view = Self::open(cache, &root)?;
        let marker = root.join("legacy-imported");
        if !marker.exists() {
            let legacy = cache.join("dotnet/packages");
            if legacy.is_dir() {
                view.store.import_tree(&legacy)?;
                view.store.link_tree(&legacy, &view.path())?;
            }
            fs::write(marker, b"1")?;
        }
        Ok(view)
    }
    pub fn open(cache: &Path, root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("packages.lock"))?;
        lock.lock()?;
        let view = Self {
            store: Store::open(cache)?,
            root,
            _lock: lock,
        };
        view.recover()?;
        fs::create_dir_all(view.path())?;
        if !view.path().join(GENERATION).is_file() {
            view.store.import_tree(&view.path())?;
            fs::write(view.path().join(GENERATION), b"initial")?;
        }
        fs::create_dir_all(view.upper())?;
        fs::create_dir_all(view.work())?;
        Ok(view)
    }
    pub fn path(&self) -> PathBuf {
        self.root.join("packages")
    }
    fn upper(&self) -> PathBuf {
        self.root.join("package-writes")
    }
    fn work(&self) -> PathBuf {
        self.root.join("package-work")
    }

    pub fn mount(&self, command: &mut Command, destination: &Path) {
        command
            .arg("--overlay-src")
            .arg(self.path())
            .arg("--overlay")
            .arg(self.upper())
            .arg(self.work())
            .arg(destination);
    }

    fn clear_writes(&self) -> Result<()> {
        for path in [self.upper(), self.work()] {
            let work = path.join("work");
            if work.is_dir() {
                fs::set_permissions(work, fs::Permissions::from_mode(0o700))?;
            }
            if path.exists() {
                fs::remove_dir_all(&path)?;
            }
            fs::create_dir_all(path)?;
        }
        Ok(())
    }

    fn recover(&self) -> Result<()> {
        let journal = self.root.join("package-publish.json");
        let bytes = match fs::read(&journal) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.discard_stages();
            }
            Err(error) => return Err(error.into()),
        };
        let publication: Publication = serde_json::from_slice(&bytes)?;
        ensure!(
            publication.stage.starts_with("package-snapshot-") && !publication.stage.contains('/'),
            "Invalid package publication"
        );
        if fs::read(self.path().join(GENERATION)).ok().as_deref()
            == Some(publication.generation.as_bytes())
        {
            self.clear_writes()?;
        }
        let stage = self.root.join(publication.stage);
        if stage.exists() {
            fs::remove_dir_all(stage)?;
        }
        fs::remove_file(journal)?;
        self.discard_stages()?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn discard_stages(&self) -> Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("package-snapshot-")
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    /// Called after all sandbox descendants exit, before interpreting their output paths.
    pub fn seal(&self, host: &Path, integration: &Path) -> Result<()> {
        if fs::read_dir(self.upper())?.next().is_none() {
            return Ok(());
        }
        validate_upper(&self.upper())?;
        let stage = tempfile::Builder::new()
            .prefix("package-snapshot-")
            .tempdir_in(&self.root)?;
        let contents = stage.path().join("contents");
        fs::create_dir(&contents)?;
        let mut command = Command::new("bwrap");
        command
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--cap-drop",
                "ALL",
                "--ro-bind",
                "/",
                "/",
                "--tmpfs",
                "/tmp",
                "--proc",
                "/proc",
            ])
            .arg("--bind")
            .arg(&self.root)
            .arg(&self.root)
            .arg("--overlay-src")
            .arg(self.path())
            .arg("--overlay-src")
            .arg(self.upper())
            .args(["--ro-overlay", "/tmp/package-view", "--"])
            .arg(host)
            .arg(integration.join("Sigla.Discovery.dll"))
            .arg("--seal-packages")
            .arg(self.path())
            .arg(self.upper())
            .arg(&contents);
        crate::process::run(&mut command, Duration::from_secs(300))?;
        self.store.import_tree(&contents)?;
        let generation = stage
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fs::write(contents.join(GENERATION), &generation)?;
        File::open(contents.join(GENERATION))?.sync_all()?;
        File::open(&contents)?.sync_all()?;
        crate::repository::manager::write_json(
            &self.root.join("package-publish.json"),
            &Publication {
                stage: generation.clone(),
                generation,
            },
        )?;
        File::open(&self.root)?.sync_all()?;
        exchange(&contents, &self.path())?;
        File::open(stage.path())?.sync_all()?;
        File::open(&self.root)?.sync_all()?;
        // The journal owns both sides until cleanup completes, including after a crash.
        let _ = stage.keep();
        self.recover()
    }
}

fn validate_upper(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            validate_upper(&path)?;
        } else {
            // Overlayfs represents a removed lower file with a 0:0 device.
            ensure!(
                metadata.is_file() || metadata.file_type().is_char_device() && metadata.rdev() == 0,
                "Package view contains a link or special file: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn exchange(a: &Path, b: &Path) -> Result<()> {
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    ensure!(
        unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                a.as_ptr(),
                libc::AT_FDCWD,
                b.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        } == 0,
        "Cannot publish package view: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_preserves_writes_before_exchange_and_cleans_them_after() {
        for installed in [false, true] {
            let cache = tempfile::tempdir().unwrap();
            let root = cache.path().join("owner");
            let view = View::open(cache.path(), &root).unwrap();
            fs::write(view.upper().join("written"), b"pending").unwrap();
            let stage = root.join("package-snapshot-recovery");
            fs::create_dir_all(stage.join("contents")).unwrap();
            fs::write(stage.join("contents").join(GENERATION), b"next").unwrap();
            fs::write(stage.join("contents/published"), b"complete").unwrap();
            crate::repository::manager::write_json(
                &root.join("package-publish.json"),
                &Publication {
                    stage: "package-snapshot-recovery".into(),
                    generation: "next".into(),
                },
            )
            .unwrap();
            if installed {
                exchange(&stage.join("contents"), &view.path()).unwrap();
            }
            drop(view);
            let recovered = View::open(cache.path(), &root).unwrap();
            assert_eq!(recovered.upper().join("written").exists(), !installed);
            assert_eq!(recovered.path().join("published").exists(), installed);
            assert!(!stage.exists());
            assert!(!root.join("package-publish.json").exists());
        }
    }

    #[test]
    fn links_and_pipes_are_rejected_before_snapshotting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("entry");
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(validate_upper(root.path()).is_err());
        fs::remove_file(&path).unwrap();
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(validate_upper(root.path()).is_err());
    }
}
