//! Portable, checked records shared by the store and metadata archive.
use anyhow::Result;
use rkyv::{
    Archive, Deserialize, Serialize,
    api::high::{HighDeserializer, HighSerializer, HighValidator},
    bytecheck::CheckBytes,
    rancor::Error,
    ser::allocator::ArenaHandle,
    util::AlignedVec,
};

pub fn encode<T>(value: &T) -> Result<Vec<u8>>
where
    T: for<'a> Serialize<HighSerializer<AlignedVec, ArenaHandle<'a>, Error>>,
{
    Ok(rkyv::to_bytes::<Error>(value)?.to_vec())
}

pub fn view<T: Archive>(bytes: &[u8]) -> Result<&T::Archived>
where
    T::Archived: for<'a> CheckBytes<HighValidator<'a, Error>>,
{
    let mut validator = rkyv::validation::Validator::new(
        rkyv::validation::archive::ArchiveValidator::with_max_depth(
            bytes,
            std::num::NonZeroUsize::new(128),
        ),
        rkyv::validation::shared::SharedValidator::new(),
    );
    Ok(rkyv::api::access_with_context::<T::Archived, _, Error>(
        bytes,
        &mut validator,
    )?)
}

pub fn decode<T: Archive>(bytes: &[u8]) -> Result<T>
where
    T::Archived:
        for<'a> CheckBytes<HighValidator<'a, Error>> + Deserialize<T, HighDeserializer<Error>>,
{
    Ok(rkyv::deserialize::<T, Error>(view::<T>(bytes)?)?)
}
