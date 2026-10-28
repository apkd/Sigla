//! Carry depth and allocation limits through Serde's nested visitors.
use serde::de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use std::{cell::Cell, fmt, rc::Rc};

pub(super) struct Budget {
    remaining: usize,
    initial: usize,
    streamed: Option<Rc<Cell<usize>>>,
    pub failure: Option<&'static str>,
}

impl Budget {
    pub fn new(bytes: usize) -> Self {
        let initial = bytes
            .saturating_mul(1024)
            .clamp(64 * 1024, 1024 * 1024 * 1024);
        Self {
            // A one-byte enum variant can decode to a large inline Rust enum.
            remaining: initial,
            initial,
            streamed: None,
            failure: None,
        }
    }
    pub fn streamed(bytes: Rc<Cell<usize>>) -> Self {
        Self {
            streamed: Some(bytes),
            ..Self::new(0)
        }
    }
    pub fn allocated(&self) -> usize {
        self.initial - self.remaining
    }
    fn charge<E: de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        if let Some(read) = &self.streamed {
            // Whole indexes can exceed the per-object ceiling. Limit their
            // expansion by consumed input, including work on input-free values.
            let allowance = read.get().saturating_mul(1024).max(64 * 1024);
            self.remaining = self.remaining.saturating_add(allowance - self.initial);
            self.initial = allowance;
        }
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
            self.failure = Some("Binary record allocation limit exceeded");
            E::custom("Binary record allocation limit exceeded")
        })?;
        if self.streamed.is_some() {
            // Past input must not fund unbounded work on later input-free values.
            // Clip after charging so a large field can pay from its actual bytes.
            self.remaining = self.remaining.min(64 * 1024);
        }
        Ok(())
    }
}
pub(super) struct Decoder<'a, D> {
    inner: D,
    budget: &'a mut Budget,
    depth: usize,
}
impl<'a, D> Decoder<'a, D> {
    pub fn new(inner: D, budget: &'a mut Budget) -> Self {
        Self {
            inner,
            budget,
            depth: 0,
        }
    }
}
struct Checked<'a, V> {
    inner: V,
    budget: &'a mut Budget,
    depth: usize,
}
macro_rules! deserialize {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {$ (
        fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value, Self::Error> {
            // Source lowering allows 66 type nodes. A generic node includes several
            // Serde containers (variant, fields, parts and arguments).
            if self.depth >= 512 {
                self.budget.failure = Some("Binary record nesting limit exceeded");
                return Err(de::Error::custom("Binary record nesting limit exceeded"));
            }
            self.inner.$method($($arg,)* Checked { inner: visitor, budget: self.budget, depth: self.depth + 1 })
        }
    )*};
}
impl<'de, D: de::Deserializer<'de>> de::Deserializer<'de> for Decoder<'_, D> {
    type Error = D::Error;
    deserialize! {
        deserialize_any(); deserialize_bool(); deserialize_i8(); deserialize_i16();
        deserialize_i32(); deserialize_i64(); deserialize_i128(); deserialize_u8();
        deserialize_u16(); deserialize_u32(); deserialize_u64(); deserialize_u128();
        deserialize_f32(); deserialize_f64(); deserialize_char(); deserialize_str();
        deserialize_string(); deserialize_bytes(); deserialize_byte_buf(); deserialize_option();
        deserialize_unit(); deserialize_unit_struct(name: &'static str);
        deserialize_newtype_struct(name: &'static str); deserialize_seq();
        deserialize_tuple(len: usize); deserialize_tuple_struct(name: &'static str, len: usize);
        deserialize_map(); deserialize_struct(name: &'static str, fields: &'static [&'static str]);
        deserialize_enum(name: &'static str, variants: &'static [&'static str]);
        deserialize_identifier(); deserialize_ignored_any();
    }
    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}
macro_rules! scalar {
    ($($method:ident($ty:ty);)*) => {$ (
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> { self.inner.$method(value) }
    )*};
}
impl<'de, V: Visitor<'de>> Visitor<'de> for Checked<'_, V> {
    type Value = V::Value;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.inner.expecting(formatter)
    }
    scalar! {
        visit_bool(bool); visit_i8(i8); visit_i16(i16); visit_i32(i32); visit_i64(i64);
        visit_i128(i128); visit_u8(u8); visit_u16(u16); visit_u32(u32); visit_u64(u64);
        visit_u128(u128); visit_f32(f32); visit_f64(f64); visit_char(char);
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        self.budget.charge::<E>(value.len())?;
        self.inner.visit_str(value)
    }
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        self.budget.charge::<E>(value.len())?;
        self.inner.visit_borrowed_str(value)
    }
    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Self::Value, E> {
        self.budget.charge::<E>(value.len())?;
        self.inner.visit_bytes(value)
    }
    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        self.budget.charge::<E>(value.len())?;
        self.inner.visit_borrowed_bytes(value)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_none()
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner.visit_unit()
    }
    fn visit_some<D: de::Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner.visit_some(Decoder {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn visit_newtype_struct<D: de::Deserializer<'de>>(
        self,
        inner: D,
    ) -> Result<Self::Value, D::Error> {
        self.inner.visit_newtype_struct(Decoder {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_seq(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_map(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn visit_enum<A: EnumAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner.visit_enum(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
}
impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Checked<'_, S> {
    type Value = S::Value;
    fn deserialize<D: de::Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.budget
            .charge::<D::Error>(std::mem::size_of::<S::Value>().max(1).saturating_mul(2))?;
        self.inner.deserialize(Decoder {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Checked<'_, A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        inner: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.inner.next_element_seed(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    // Never preallocate from an untrusted element count.
    fn size_hint(&self) -> Option<usize> {
        None
    }
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for Checked<'_, A> {
    type Error = A::Error;
    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        inner: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.budget.charge::<Self::Error>(64)?;
        self.inner.next_key_seed(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        inner: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.next_value_seed(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn size_hint(&self) -> Option<usize> {
        None
    }
}
impl<'a, 'de, A: EnumAccess<'de>> EnumAccess<'de> for Checked<'a, A> {
    type Error = A::Error;
    type Variant = Checked<'a, A::Variant>;
    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        inner: S,
    ) -> Result<(S::Value, Self::Variant), Self::Error> {
        let (value, variant) = self.inner.variant_seed(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })?;
        Ok((
            value,
            Checked {
                inner: variant,
                budget: self.budget,
                depth: self.depth,
            },
        ))
    }
}
impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Checked<'_, A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        self.inner.unit_variant()
    }
    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        inner: S,
    ) -> Result<S::Value, Self::Error> {
        self.inner.newtype_variant_seed(Checked {
            inner,
            budget: self.budget,
            depth: self.depth,
        })
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, inner: V) -> Result<V::Value, Self::Error> {
        self.inner.tuple_variant(
            len,
            Checked {
                inner,
                budget: self.budget,
                depth: self.depth,
            },
        )
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        inner: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner.struct_variant(
            fields,
            Checked {
                inner,
                budget: self.budget,
                depth: self.depth,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::binary::{decode_reader, encode};
    use serde::Deserialize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn earlier_fields_cannot_fund_later_input_free_work() {
        static VISITS: AtomicUsize = AtomicUsize::new(0);
        struct CountedUnit;
        impl<'de> Deserialize<'de> for CountedUnit {
            fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
                <()>::deserialize(decoder)?;
                VISITS.fetch_add(1, Ordering::Relaxed);
                Ok(Self)
            }
        }
        let work: Vec<_> = [128, 4096]
            .into_iter()
            .map(|length| {
                let mut bytes = encode(&"x".repeat(length)).unwrap();
                bytes.extend(encode(&u64::MAX).unwrap());
                VISITS.store(0, Ordering::Relaxed);
                assert!(
                    decode_reader::<(String, Vec<CountedUnit>)>(bytes.as_slice(), bytes.len(), &())
                        .is_err()
                );
                VISITS.load(Ordering::Relaxed)
            })
            .collect();
        assert_eq!(work[0], work[1]);
        assert!(work[0] > 0);
    }

    #[test]
    fn consuming_input_replenishes_stream_budget_after_large_field() {
        let value = ("x".repeat(8193), vec![(7_u8, ()); 70_000]);
        let bytes = encode(&value).unwrap();
        let decoded: (String, Vec<(u8, ())>) =
            decode_reader(bytes.as_slice(), bytes.len(), &()).unwrap();
        assert_eq!(decoded, value);
    }
}
