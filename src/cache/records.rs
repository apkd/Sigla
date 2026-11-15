//! Current-format cache records. Cache ownership handles incompatible layouts.
use anyhow::{Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub fn read<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<T> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    read_file(file, max_bytes)
}

pub(crate) fn read_file<T: DeserializeOwned>(file: File, max_bytes: u64) -> Result<T> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= max_bytes,
        "Invalid cache record"
    );
    crate::binary::decode_reader(BufReader::new(file), metadata.len().try_into()?, &())
}

pub fn stage<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<tempfile::NamedTempFile> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cache record has no parent"))?;
    stage_in(parent, ".record-", value)
}

pub(crate) fn stage_in<T: Serialize + ?Sized>(
    parent: &Path,
    prefix: &str,
    value: &T,
) -> Result<tempfile::NamedTempFile> {
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".bin")
        .tempfile_in(parent)?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        postcard::to_io(value, &mut writer)?;
        writer.flush()?;
    }
    file.as_file().sync_all()?;
    Ok(file)
}

pub fn write<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    stage(path, value)?.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_keeps_existing_readers_and_unpublished_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("record.bin");
        write(&path, &vec![1u64, 2]).unwrap();
        let reader = File::open(&path).unwrap();
        let next = stage(&path, &vec![3u64]).unwrap();
        assert_eq!(read::<Vec<u64>>(&path, 1024).unwrap(), [1, 2]);
        next.persist(&path).unwrap();
        assert_eq!(read_file::<Vec<u64>>(reader, 1024).unwrap(), [1, 2]);
        assert_eq!(read::<Vec<u64>>(&path, 1024).unwrap(), [3]);
    }

    #[test]
    fn rejects_links_and_oversized_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("record.bin");
        write(&path, &vec![42u64; 20]).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read::<Vec<u64>>(&link, 1024).is_err());
        assert!(read::<Vec<u64>>(&path, 1).is_err());
    }
}
