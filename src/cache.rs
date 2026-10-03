//! Managed disk storage. Owners retain views; jobs lease them until publication ends.
pub mod blobs;
pub mod compaction;
pub mod owners;
pub mod packages;
pub mod policy;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
