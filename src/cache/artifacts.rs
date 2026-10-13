//! Immutable derived files shared by retained workspace views. Hard links are
//! their ownership records; collection runs under the service activity gate.
use anyhow::{Result, ensure};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub struct Store {
    root: PathBuf,
}
impl Store {
    pub fn open(cache: &Path) -> Result<Self> {
        let root = cache.join("asset-records");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    fn path(&self, name: &str) -> Result<PathBuf> {
        ensure!(
            name.is_ascii()
                && name.len() > 64
                && name[..64].bytes().all(|b| b.is_ascii_hexdigit())
                && matches!(&name[64..], ".summary.zst" | ".text.zst"),
            "Invalid asset record name"
        );
        Ok(self.root.join(&name[..2]).join(name))
    }
    pub fn restore(&self, local: &Path) -> Result<()> {
        if local.is_file() {
            return Ok(());
        }
        let source = self.path(local.file_name().unwrap().to_str().unwrap())?;
        match fs::hard_link(source, local) {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
    pub fn publish(&self, local: &Path) -> Result<()> {
        let target = self.path(local.file_name().unwrap().to_str().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        // Replace a corrupt or incomplete older record atomically. Existing
        // readers keep their own inode until their workspace is refreshed.
        let stage = tempfile::tempdir_in(target.parent().unwrap())?;
        let link = stage.path().join("record");
        fs::hard_link(local, &link)?;
        fs::rename(link, target)?;
        Ok(())
    }
    pub fn collect(&self, pressure: bool) -> Result<u64> {
        let mut bytes = 0;
        for shard in fs::read_dir(&self.root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                if metadata.is_file()
                    && metadata.nlink() == 1
                    && (pressure
                        || metadata
                            .modified()?
                            .elapsed()
                            .is_ok_and(|age| age >= crate::config::DEFAULT_REPO_TTL))
                {
                    fs::remove_file(entry.path())?;
                    bytes += metadata.blocks() * 512;
                }
            }
        }
        Ok(bytes)
    }
}
