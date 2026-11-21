//! Managed disk storage. Owners retain views; jobs lease them until publication ends.
pub mod artifacts;
pub mod blobs;
pub(crate) mod cleanup;
pub mod compaction;
pub(crate) mod file_id;
pub mod inspect;
pub mod owners;
pub mod packages;
pub mod policy;
pub mod records;
pub mod reset;
pub mod views;

/// Swap a completed private directory with an existing managed view atomically.
pub(crate) fn exchange(a: &std::path::Path, b: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    anyhow::ensure!(
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
        "Cannot publish managed view: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

pub(crate) struct Ownership(pub std::fs::File);
impl Drop for Ownership {
    fn drop(&mut self) {
        // A concurrently spawned child can retain a duplicate until it execs.
        let _ = self.0.unlock();
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
