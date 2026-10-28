//! Decode owned records without retaining the complete serialized input.
use super::bounded;
use crate::cancellation::Cancellation;
use anyhow::{Result, ensure};
use postcard::de_flavors::Flavor;
use serde::de::DeserializeOwned;
use std::{cell::Cell, io::Read, ops::Range, rc::Rc};

struct Reader<'a, R> {
    input: R,
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    limit: usize,
    // Share the absolute input cursor with the allocation budget.
    read: Rc<Cell<usize>>,
    field: Vec<u8>,
    cancel: &'a dyn Cancellation,
    error: Option<anyhow::Error>,
}
impl<R: Read> Reader<'_, R> {
    #[cold]
    fn fill(&mut self) -> Result<()> {
        debug_assert_eq!(self.read.get(), self.end);
        self.cancel.check()?;
        let count = loop {
            match self.input.read(&mut self.buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    self.cancel.check()?;
                }
                result => break result?,
            }
        };
        ensure!(count != 0, "Truncated binary record");
        self.start = self.end;
        self.end = self
            .end
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("Binary input size limit exceeded"))?;
        Ok(())
    }

    fn fail(&mut self, error: anyhow::Error) -> postcard::Error {
        self.error = Some(error);
        postcard::Error::DeserializeUnexpectedEnd
    }

    fn read_field(&mut self, length: usize) -> Result<Option<Range<usize>>> {
        let read = self.read.get();
        ensure!(
            length <= self.limit - read,
            "Binary input size limit exceeded"
        );
        self.field.clear();
        if length <= self.end - read {
            let start = read - self.start;
            self.read.set(read + length);
            return Ok(Some(start..start + length));
        }
        while self.field.len() < length {
            let read = self.read.get();
            if read == self.end {
                self.fill()?;
                continue;
            }
            let count = (self.end - read).min(length - self.field.len());
            // Grow from bytes actually read, never from an untrusted length.
            self.field.try_reserve(count)?;
            let start = read - self.start;
            self.field
                .extend_from_slice(&self.buffer[start..start + count]);
            self.read.set(read + count);
        }
        Ok(None)
    }

    fn finish(mut self) -> Result<()> {
        self.cancel.check()?;
        ensure!(self.read.get() == self.end, "Trailing binary record data");
        // Reading through EOF also validates a compressed reader's checksum.
        let mut trailing = [0];
        let count = loop {
            match self.input.read(&mut trailing) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    self.cancel.check()?;
                }
                result => break result?,
            }
        };
        ensure!(count == 0, "Trailing binary record data");
        Ok(())
    }
}
impl<'de, R: Read + 'de> Flavor<'de> for Reader<'de, R> {
    type Remainder = Self;
    type Source = R;

    #[inline]
    fn pop(&mut self) -> postcard::Result<u8> {
        let read = self.read.get();
        if read == self.limit {
            return Err(self.fail(anyhow::anyhow!("Binary input size limit exceeded")));
        }
        if read == self.end {
            self.fill().map_err(|error| self.fail(error))?;
        }
        let byte = self.buffer[read - self.start];
        self.read.set(read + 1);
        Ok(byte)
    }

    fn try_take_n(&mut self, _length: usize) -> postcard::Result<&'de [u8]> {
        // The streaming API accepts owned records. Borrowed values need a slice.
        Err(postcard::Error::WontImplement)
    }

    fn try_take_n_temp<'a>(&'a mut self, length: usize) -> postcard::Result<&'a [u8]>
    where
        'de: 'a,
    {
        let range = self.read_field(length).map_err(|error| self.fail(error))?;
        Ok(match range {
            Some(range) => &self.buffer[range],
            None => &self.field,
        })
    }

    fn finalize(self) -> postcard::Result<Self::Remainder> {
        Ok(self)
    }
}

pub(crate) fn decode_reader<T: DeserializeOwned>(
    input: impl Read,
    limit: usize,
    cancel: &dyn Cancellation,
) -> Result<T> {
    cancel.check()?;
    let read = Rc::new(Cell::new(0));
    let reader = Reader {
        input,
        buffer: vec![0; 8192].into_boxed_slice(),
        start: 0,
        end: 0,
        limit,
        read: read.clone(),
        field: Vec::new(),
        cancel,
        error: None,
    };
    let mut decoder = postcard::Deserializer::from_flavor(reader);
    let mut budget = bounded::Budget::streamed(read);
    let result = T::deserialize(bounded::Decoder::new(&mut decoder, &mut budget));
    cancel.check()?;
    let mut reader = decoder.finalize()?;
    let value = result.map_err(|error| {
        reader.error.take().unwrap_or_else(|| {
            budget
                .failure
                .map_or_else(|| error.into(), anyhow::Error::msg)
        })
    })?;
    reader.finish()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        path: PathBuf,
        names: BTreeMap<String, Arc<str>>,
        values: Vec<Option<(u64, String)>>,
    }

    #[test]
    fn streams_owned_fields_larger_than_the_read_buffer() {
        let value = Record {
            path: "Assets/Some file.prefab".into(),
            names: BTreeMap::from([("name".into(), "value".into())]),
            values: vec![None, Some((42, "large field 🦀".repeat(4096)))],
        };
        let bytes = super::super::encode(&value).unwrap();
        let decoded: Record = decode_reader(bytes.as_slice(), bytes.len(), &()).unwrap();
        assert_eq!(decoded, value);
        assert!(decode_reader::<Record>(bytes.as_slice(), bytes.len() - 1, &()).is_err());
    }

    #[test]
    fn interleaves_fields_and_scalars_across_short_reads() {
        struct Chunks<'a> {
            input: &'a [u8],
            size: usize,
        }
        impl Read for Chunks<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(self.size);
                self.input.read(&mut output[..count])
            }
        }
        let values: Vec<_> = [0, 1, 31, 8191, 8192, 8193, 128, 0]
            .into_iter()
            .map(|length| ("a".repeat(length), length as u64, "🦀".to_owned()))
            .collect();
        let bytes = super::super::encode(&values).unwrap();
        for size in [1, 17, 8192] {
            let input = Chunks {
                input: &bytes,
                size,
            };
            let decoded: Vec<(String, u64, String)> =
                decode_reader(input, bytes.len(), &()).unwrap();
            assert_eq!(decoded, values);
        }
    }

    #[test]
    fn retries_interrupted_reads_during_fields_and_at_eof() {
        struct Interrupting<'a> {
            input: &'a [u8],
            calls: usize,
        }
        impl Read for Interrupting<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                if self.calls % 2 == 1 {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let count = output.len().min(31);
                self.input.read(&mut output[..count])
            }
        }
        let value = ("long field 🦀".repeat(1024), 42_u64);
        let bytes = super::super::encode(&value).unwrap();
        let mut input = Interrupting {
            input: &bytes,
            calls: 0,
        };
        let decoded: (String, u64) = decode_reader(&mut input, bytes.len(), &()).unwrap();
        assert_eq!(decoded, value);
        assert!(input.input.is_empty());
        assert!(input.calls > 4);
    }

    #[test]
    fn rejects_truncated_trailing_and_excessive_records() {
        let bytes = super::super::encode(&vec!["one", "two"]).unwrap();
        for end in 0..bytes.len() {
            assert!(decode_reader::<Vec<String>>(&bytes[..end], bytes.len(), &()).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_reader::<Vec<String>>(trailing.as_slice(), bytes.len(), &()).is_err());
        let length = super::super::encode(&u64::MAX).unwrap();
        assert!(decode_reader::<String>(length.as_slice(), 1024, &()).is_err());
        // A large input allowance must not permit work without reading bytes.
        assert!(decode_reader::<Vec<()>>(length.as_slice(), 512 * 1024 * 1024, &()).is_err());
    }

    #[test]
    fn verifies_compressed_stream_completion_and_cancellation() {
        let bytes = super::super::encode(&vec!["one", "two"]).unwrap();
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        std::io::Write::write_all(&mut encoder, &bytes).unwrap();
        let encoded = encoder.finish().unwrap();
        let read = |input: &[u8]| {
            let decoder = zstd::stream::Decoder::new(input).unwrap();
            decode_reader::<Vec<String>>(decoder, bytes.len(), &())
        };
        assert_eq!(read(&encoded).unwrap(), ["one", "two"]);
        for missing in 1..=4 {
            assert!(read(&encoded[..encoded.len() - missing]).is_err());
        }
        let mut corrupt = encoded;
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(read(&corrupt).is_err());
        let cancelled = tokio_util::sync::CancellationToken::new();
        cancelled.cancel();
        assert!(
            decode_reader::<Vec<String>>(bytes.as_slice(), bytes.len(), &cancelled)
                .unwrap_err()
                .is::<crate::diagnostics::Cancelled>()
        );
    }

    #[test]
    fn cancellation_stops_reading_a_large_field() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct CancelOnRead<'a> {
            remaining: &'a [u8],
            cancel: &'a AtomicBool,
        }
        impl Read for CancelOnRead<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let count = self.remaining.read(bytes)?;
                self.cancel.store(true, Ordering::Release);
                Ok(count)
            }
        }
        let bytes = super::super::encode(&"large field".repeat(4096)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut input = CancelOnRead {
            remaining: &bytes,
            cancel: &cancel,
        };
        let error = decode_reader::<String>(&mut input, bytes.len(), &cancel).unwrap_err();
        assert!(error.is::<crate::diagnostics::Cancelled>());
        assert!(!input.remaining.is_empty());

        // A malformed sequence can fail without refilling the buffer again.
        let bytes = super::super::encode(&u64::MAX).unwrap();
        cancel.store(false, Ordering::Release);
        let input = CancelOnRead {
            remaining: &bytes,
            cancel: &cancel,
        };
        let error = decode_reader::<Vec<()>>(input, bytes.len(), &cancel).unwrap_err();
        assert!(error.is::<crate::diagnostics::Cancelled>());
    }
}
