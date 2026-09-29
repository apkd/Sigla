//! Streaming libarchive driver. It never creates filesystem entries.
use anyhow::{Context, Result, ensure};
use libarchive3_sys::ffi;
use std::{ffi::CStr, fs::File, os::fd::AsRawFd, path::PathBuf};

pub struct Archive {
    handle: *mut ffi::Struct_archive,
    _file: File,
}

pub struct Entry {
    pub path: PathBuf,
    pub regular: bool,
    pub link: bool,
}

impl Archive {
    pub fn open(file: File) -> Result<Self> {
        // SAFETY: The handle is owned by this driver and freed in Drop. The file
        // remains open until after archive_read_free and is never read elsewhere.
        unsafe {
            let handle = ffi::archive_read_new();
            ensure!(!handle.is_null(), "Cannot allocate archive reader");
            let archive = Self {
                handle,
                _file: file,
            };
            archive.check(ffi::archive_read_support_filter_all(handle))?;
            archive.check(ffi::archive_read_support_format_tar(handle))?;
            archive.check(ffi::archive_read_open_fd(
                handle,
                archive._file.as_raw_fd(),
                64 * 1024,
            ))?;
            Ok(archive)
        }
    }

    fn check(&self, status: i32) -> Result<()> {
        if status < 0 {
            // SAFETY: libarchive owns the error string for the lifetime of the handle.
            let error = unsafe {
                let error = ffi::archive_error_string(self.handle);
                if error.is_null() {
                    "Archive decoding failed".into()
                } else {
                    CStr::from_ptr(error).to_string_lossy().into_owned()
                }
            };
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<Entry>> {
        // SAFETY: Entry pointers belong to the live reader. Copy all metadata
        // before advancing; none of these pointers escape this method.
        unsafe {
            let mut entry = std::ptr::null_mut();
            let status = ffi::archive_read_next_header(self.handle, &mut entry);
            self.check(status)?;
            if status == ffi::ARCHIVE_EOF {
                return Ok(None);
            }
            ensure!(!entry.is_null(), "Archive entry is missing");
            let name = ffi::archive_entry_pathname(entry);
            ensure!(!name.is_null(), "Archive path is missing");
            Ok(Some(Entry {
                path: CStr::from_ptr(name)
                    .to_str()
                    .context("Archive path is not UTF-8")?
                    .into(),
                regular: ffi::archive_entry_filetype(entry) == libc::S_IFREG,
                link: !ffi::archive_entry_symlink(entry).is_null()
                    || !ffi::archive_entry_hardlink(entry).is_null(),
            }))
        }
    }

    pub fn copy(&mut self, output: &mut impl std::io::Write) -> Result<()> {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            // SAFETY: The output buffer is valid and writable for its supplied length.
            let count = unsafe {
                ffi::archive_read_data(self.handle, buffer.as_mut_ptr().cast(), buffer.len())
            };
            if count < 0 {
                self.check(count as i32)?;
            }
            if count == 0 {
                return Ok(());
            }
            output.write_all(&buffer[..count as usize])?;
        }
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        // SAFETY: This is the only owner; free precedes dropping the backing file.
        unsafe {
            ffi::archive_read_free(self.handle);
        }
    }
}
