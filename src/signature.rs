//! bounded ECMA-335 II.23.2 signature decoding, independent of the PE reader.
use anyhow::{Result, bail, ensure};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSignature {
    Primitive(&'static str),
    Named(u32),
    GenericType(u32),
    GenericMethod(u32),
    Pointer(Box<Self>),
    ByRef(Box<Self>),
    Vector(Box<Self>),
    Array {
        element: Box<Self>,
        rank: u32,
        sizes: Vec<u32>,
        lower_bounds: Vec<i32>,
    },
    Generic(Box<Self>, Vec<Self>),
    Function(Box<SignatureMethod>),
    Modified {
        required: bool,
        modifier: u32,
        element: Box<Self>,
    },
    Pinned(Box<Self>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureMethod {
    pub flags: u8,
    pub param_count_generic: u32,
    pub return_type: TypeSignature,
    pub params: Vec<TypeSignature>,
    pub sentinel: Option<usize>,
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl Decoder<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| anyhow::anyhow!("Truncated CLI signature"))?;
        self.position += 1;
        Ok(b)
    }
    fn unsigned_with_width(&mut self) -> Result<(u32, u32)> {
        let b = self.byte()?;
        if b & 0x80 == 0 {
            Ok((u32::from(b), 7))
        } else if b & 0xc0 == 0x80 {
            Ok(((u32::from(b & 0x3f) << 8) | u32::from(self.byte()?), 14))
        } else if b & 0xe0 == 0xc0 {
            let n = (u32::from(b & 0x1f) << 24)
                | (u32::from(self.byte()?) << 16)
                | (u32::from(self.byte()?) << 8)
                | u32::from(self.byte()?);
            Ok((n, 29))
        } else {
            bail!("Invalid compressed CLI integer")
        }
    }
    fn unsigned(&mut self) -> Result<u32> {
        Ok(self.unsigned_with_width()?.0)
    }
    fn signed(&mut self) -> Result<i32> {
        let (n, width) = self.unsigned_with_width()?;
        Ok((n >> 1) as i32 - if n & 1 != 0 { 1i32 << (width - 1) } else { 0 })
    }
    fn token(&mut self) -> Result<u32> {
        let coded = self.unsigned()?;
        let table = match coded & 3 {
            0 => 0x02,
            1 => 0x01,
            2 => 0x1b,
            _ => bail!("Invalid signature type token"),
        };
        let row = coded >> 2;
        ensure!(row > 0 && row <= 0x00ff_ffff, "Invalid signature type row");
        Ok((table << 24) | row)
    }
    fn count(&mut self) -> Result<usize> {
        let n = self.unsigned()? as usize;
        ensure!(
            n <= self.bytes.len() - self.position,
            "Signature count exceeds remaining input"
        );
        Ok(n)
    }
    fn ty(&mut self, depth: usize) -> Result<TypeSignature> {
        ensure!(depth <= 64, "Signature nesting exceeds 64 levels");
        let code = self.byte()?;
        let primitive = match code {
            0x01 => Some("void"),
            0x02 => Some("bool"),
            0x03 => Some("char"),
            0x04 => Some("sbyte"),
            0x05 => Some("byte"),
            0x06 => Some("short"),
            0x07 => Some("ushort"),
            0x08 => Some("int"),
            0x09 => Some("uint"),
            0x0a => Some("long"),
            0x0b => Some("ulong"),
            0x0c => Some("float"),
            0x0d => Some("double"),
            0x0e => Some("string"),
            0x16 => Some("System.TypedReference"),
            0x18 => Some("nint"),
            0x19 => Some("nuint"),
            0x1c => Some("object"),
            _ => None,
        };
        if let Some(p) = primitive {
            return Ok(TypeSignature::Primitive(p));
        }
        Ok(match code {
            0x0f => TypeSignature::Pointer(Box::new(self.ty(depth + 1)?)),
            0x10 => TypeSignature::ByRef(Box::new(self.ty(depth + 1)?)),
            0x11 | 0x12 => TypeSignature::Named(self.token()?),
            0x13 => TypeSignature::GenericType(self.unsigned()?),
            0x14 => {
                let element = Box::new(self.ty(depth + 1)?);
                let rank = self.unsigned()?;
                ensure!(rank > 0 && rank <= 32, "Invalid CLI array rank");
                let count = self.count()?;
                ensure!(count <= rank as usize, "Array size count exceeds rank");
                let sizes = (0..count).map(|_| self.unsigned()).collect::<Result<_>>()?;
                let count = self.count()?;
                ensure!(
                    count <= rank as usize,
                    "Array lower-bound count exceeds rank"
                );
                let lower_bounds = (0..count).map(|_| self.signed()).collect::<Result<_>>()?;
                TypeSignature::Array {
                    element,
                    rank,
                    sizes,
                    lower_bounds,
                }
            }
            0x15 => {
                ensure!(
                    matches!(self.byte()?, 0x11 | 0x12),
                    "Generic instance needs a class or value type"
                );
                let base = Box::new(TypeSignature::Named(self.token()?));
                let count = self.count()?;
                let arguments = (0..count)
                    .map(|_| self.ty(depth + 1))
                    .collect::<Result<_>>()?;
                TypeSignature::Generic(base, arguments)
            }
            0x1b => TypeSignature::Function(Box::new(self.method(depth + 1)?)),
            0x1d => TypeSignature::Vector(Box::new(self.ty(depth + 1)?)),
            0x1e => TypeSignature::GenericMethod(self.unsigned()?),
            0x1f | 0x20 => {
                let modifier = self.token()?;
                let element = Box::new(self.ty(depth + 1)?);
                TypeSignature::Modified {
                    required: code == 0x1f,
                    modifier,
                    element,
                }
            }
            0x45 => TypeSignature::Pinned(Box::new(self.ty(depth + 1)?)),
            _ => bail!("Unsupported CLI element type 0x{code:02x}"),
        })
    }
    fn method(&mut self, depth: usize) -> Result<SignatureMethod> {
        ensure!(depth <= 64, "Signature nesting exceeds 64 levels");
        let flags = self.byte()?;
        ensure!(
            flags & 0x80 == 0 && matches!(flags & 0x0f, 0..=5 | 9 | 11),
            "Invalid method calling convention"
        );
        let generic = if flags & 0x10 != 0 {
            self.unsigned()?
        } else {
            0
        };
        ensure!(generic <= 65535, "Too many method generic parameters");
        let count = self.count()?;
        let return_type = self.ty(depth + 1)?;
        let mut params = Vec::with_capacity(count);
        let mut sentinel = None;
        for _ in 0..count {
            if self.bytes.get(self.position) == Some(&0x41) {
                ensure!(matches!(flags & 0x0f, 5 | 11), "Sentinel requires varargs");
                ensure!(sentinel.is_none(), "Duplicate signature sentinel");
                self.position += 1;
                sentinel = Some(params.len());
            }
            params.push(self.ty(depth + 1)?);
        }
        Ok(SignatureMethod {
            flags,
            param_count_generic: generic,
            return_type,
            params,
            sentinel,
        })
    }
    fn finish(&self) -> Result<()> {
        ensure!(
            self.position == self.bytes.len(),
            "Trailing bytes in CLI signature"
        );
        Ok(())
    }
}
pub fn parse_method_signature(bytes: &[u8]) -> Result<SignatureMethod> {
    let mut d = Decoder { bytes, position: 0 };
    let method = d.method(0)?;
    d.finish()?;
    Ok(method)
}
pub fn parse_field_signature(bytes: &[u8]) -> Result<TypeSignature> {
    let mut d = Decoder { bytes, position: 0 };
    ensure!(d.byte()? == 0x06, "Invalid field signature prefix");
    let ty = d.ty(0)?;
    d.finish()?;
    Ok(ty)
}
pub fn parse_type_spec_signature(bytes: &[u8]) -> Result<TypeSignature> {
    let mut d = Decoder { bytes, position: 0 };
    let ty = d.ty(0)?;
    d.finish()?;
    Ok(ty)
}
pub fn parse_property_signature(bytes: &[u8]) -> Result<SignatureMethod> {
    let mut d = Decoder { bytes, position: 0 };
    let flags = d.byte()?;
    ensure!(flags & 0xdf == 8, "Invalid property signature prefix");
    let count = d.count()?;
    let return_type = d.ty(0)?;
    let params = (0..count).map(|_| d.ty(0)).collect::<Result<_>>()?;
    d.finish()?;
    Ok(SignatureMethod {
        flags,
        param_count_generic: 0,
        return_type,
        params,
        sentinel: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn array_shape_does_not_consume_the_next_parameter() {
        let array = [0x14, 0x08, 2, 0, 2, 0, 0];
        let mut bytes = vec![0x20, 1];
        bytes.extend(array);
        bytes.extend(array);
        let s = parse_method_signature(&bytes).unwrap();
        assert_eq!(s.return_type, s.params[0]);
        assert!(matches!(
            s.return_type,
            TypeSignature::Array { rank: 2, .. }
        ));
    }
    #[test]
    fn generic_method_identity() {
        let s = parse_method_signature(&[0x30, 1, 1, 0x1e, 0, 0x1e, 0]).unwrap();
        assert_eq!(s.return_type, s.params[0]);
        assert!(matches!(s.return_type, TypeSignature::GenericMethod(0)));
    }
    #[test]
    fn malformed_input_is_an_error() {
        for bytes in [
            vec![],
            vec![0x20, 1],
            vec![0, 0, 1, 1],
            vec![0x20, 0xff],
            vec![0, 1, 0x01, 0x12, 3],
        ] {
            assert!(parse_method_signature(&bytes).is_err());
        }
    }
    #[test]
    fn signed_array_lower_bounds() {
        let ty = parse_type_spec_signature(&[0x14, 0x08, 1, 0, 1, 0x7f]).unwrap();
        assert!(matches!(ty,TypeSignature::Array{lower_bounds,..} if lower_bounds==vec![-1]));
    }
    #[test]
    fn nested_function_pointer_and_modifiers() {
        let bytes = [0x1b, 1, 1, 0x1f, 5, 0x08, 0x1d, 0x1e, 0];
        let ty = parse_type_spec_signature(&bytes).unwrap();
        let TypeSignature::Function(method) = ty else {
            panic!("Expected function pointer")
        };
        assert!(matches!(
            method.return_type,
            TypeSignature::Modified { required: true, .. }
        ));
        assert!(
            matches!(&method.params[0], TypeSignature::Vector(element) if matches!(**element,TypeSignature::GenericMethod(0)))
        );
        for end in 0..bytes.len() {
            assert!(parse_type_spec_signature(&bytes[..end]).is_err());
        }
    }
    #[test]
    fn nesting_and_vararg_boundaries() {
        let mut bytes = vec![0x1d; 65];
        bytes.push(0x08);
        assert!(parse_type_spec_signature(&bytes).is_err());
        let signature = parse_method_signature(&[5, 2, 1, 8, 0x41, 0x0e]).unwrap();
        assert_eq!(signature.sentinel, Some(1));
        assert!(parse_method_signature(&[0, 1, 1, 0x41, 8]).is_err());
    }
}
