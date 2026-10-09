//! Portable records shared by the store and metadata archive.
mod bounded;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    Ok(postcard::to_allocvec(value)?)
}

pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T> {
    Ok(decode_with_size(bytes)?.0)
}

pub fn decode_with_size<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<(T, usize)> {
    let mut decoder = postcard::Deserializer::from_bytes(bytes);
    let mut budget = bounded::Budget::new(bytes.len());
    let value =
        T::deserialize(bounded::Decoder::new(&mut decoder, &mut budget)).map_err(|error| {
            match budget.failure {
                Some(message) => anyhow::anyhow!(message),
                None => error.into(),
            }
        })?;
    ensure!(
        decoder.finalize()?.is_empty(),
        "Trailing binary record data"
    );
    Ok((
        value,
        budget.allocated().saturating_add(std::mem::size_of::<T>()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_trailing_truncated_and_unbounded_sequences() {
        let bytes = encode(&vec!["one", "two"]).unwrap();
        for end in 0..bytes.len() {
            assert!(decode::<Vec<String>>(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode::<Vec<String>>(&trailing).is_err());
        let count = encode(&u64::MAX).unwrap();
        assert!(decode::<Vec<String>>(&count).is_err());
        // Unit values consume no input bytes; work still consumes the allocation budget.
        assert!(decode::<Vec<()>>(&count).is_err());
    }
}
