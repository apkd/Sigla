//! Shared identity for immutable files and authenticated archive entries.
pub(crate) fn hasher(mode: u32) -> blake3::Hasher {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sigla-file-1\0");
    hash.update(&mode.to_le_bytes());
    hash
}
