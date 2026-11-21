//! Decode opaque byte buffers in bulk while retaining their sequence wire format.
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use std::{collections::BTreeMap, fmt};

pub(crate) fn byte_vec<'de, D: Deserializer<'de>>(decoder: D) -> Result<Vec<u8>, D::Error> {
    struct Buffer;
    impl<'de> Visitor<'de> for Buffer {
        type Value = Vec<u8>;
        fn expecting(&self, output: &mut fmt::Formatter) -> fmt::Result {
            output.write_str("a byte buffer")
        }
        fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
            Ok(bytes.to_vec())
        }
        fn visit_byte_buf<E: serde::de::Error>(self, bytes: Vec<u8>) -> Result<Self::Value, E> {
            Ok(bytes)
        }
    }
    decoder.deserialize_byte_buf(Buffer)
}

#[derive(Deserialize)]
#[serde(transparent)]
struct Buffer(#[serde(deserialize_with = "byte_vec")] Vec<u8>);

pub(crate) fn byte_map<'de, D: Deserializer<'de>>(
    decoder: D,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, D::Error> {
    struct Records;
    impl<'de> Visitor<'de> for Records {
        type Value = BTreeMap<Vec<u8>, Vec<u8>>;
        fn expecting(&self, output: &mut fmt::Formatter) -> fmt::Result {
            output.write_str("byte keys and values")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
            let mut records = BTreeMap::new();
            while let Some((Buffer(key), Buffer(value))) = entries.next_entry()? {
                records.insert(key, value);
            }
            Ok(records)
        }
    }
    decoder.deserialize_map(Records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Records {
        #[serde(deserialize_with = "byte_vec")]
        names: Vec<u8>,
        #[serde(deserialize_with = "byte_map")]
        records: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    #[test]
    fn bulk_buffers_preserve_wire_data_and_allocation_accounting() {
        let expected = Records {
            names: (0..=255).collect(),
            records: BTreeMap::from([
                (vec![], vec![]),
                (vec![1], vec![42; 8193]),
                (vec![2, 3], vec![7; 255]),
            ]),
        };
        let bytes = crate::binary::encode(&expected).unwrap();
        let (actual, allocated) = crate::binary::decode_with_size::<Records>(&bytes).unwrap();
        let (legacy, previous) =
            crate::binary::decode_with_size::<(Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>)>(&bytes)
                .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(legacy, (expected.names.clone(), expected.records.clone()));
        assert_eq!(allocated, previous);
        assert_eq!(crate::binary::encode(&legacy).unwrap(), bytes);
        assert_eq!(
            crate::binary::decode_reader::<Records>(bytes.as_slice(), bytes.len(), &()).unwrap(),
            expected
        );
        for end in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(crate::binary::decode::<Records>(&bytes[..end]).is_err());
        }
    }
}
