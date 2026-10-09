//! Written types retain lookup context; semantic types carry definition identity.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DefinitionId {
    /// Compilation or immutable assembly revision.
    pub context: String,
    /// Logical declaration key, shared by partial sites.
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParameterId {
    pub owner: DefinitionId,
    pub ordinal: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamePart {
    pub name: String,
    pub arguments: Vec<WrittenType>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WrittenType {
    /// ECMA parameter slots are resolved against the declaring type/method,
    /// including the containing type's parameters for nested metadata types.
    MetadataParameter {
        method: bool,
        ordinal: u32,
    },
    External {
        assembly: String,
        ty: Box<Self>,
    },
    Name {
        alias: Option<String>,
        parts: Vec<NamePart>,
    },
    Parameter(ParameterId),
    Array(Box<Self>, u32),
    Pointer(Box<Self>),
    Nullable(Box<Self>),
    Tuple(Vec<(Self, Option<String>)>),
    Dynamic,
    Inferred,
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    Primitive(Primitive),
    Named {
        definition: DefinitionId,
        containing: Option<Box<Self>>,
        arguments: Vec<Self>,
    },
    Parameter(ParameterId),
    Array(Box<Self>, u32),
    Pointer(Box<Self>),
    Nullable(Box<Self>),
    Tuple(Vec<(Self, Option<String>)>),
    Dynamic,
    Null,
    Unresolved {
        written: WrittenType,
        context: DefinitionId,
    },
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Primitive {
    Void,
    Bool,
    Char,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    NativeInt,
    NativeUInt,
    F32,
    F64,
    Decimal,
    String,
    Object,
}

impl Primitive {
    pub fn metadata_name(self) -> &'static str {
        match self {
            Self::Void => "Void",
            Self::Bool => "Boolean",
            Self::Char => "Char",
            Self::I8 => "SByte",
            Self::U8 => "Byte",
            Self::I16 => "Int16",
            Self::U16 => "UInt16",
            Self::I32 => "Int32",
            Self::U32 => "UInt32",
            Self::I64 => "Int64",
            Self::U64 => "UInt64",
            Self::NativeInt => "IntPtr",
            Self::NativeUInt => "UIntPtr",
            Self::F32 => "Single",
            Self::F64 => "Double",
            Self::Decimal => "Decimal",
            Self::String => "String",
            Self::Object => "Object",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "void" | "System.Void" => Self::Void,
            "bool" | "System.Boolean" => Self::Bool,
            "char" | "System.Char" => Self::Char,
            "sbyte" | "System.SByte" => Self::I8,
            "byte" | "System.Byte" => Self::U8,
            "short" | "System.Int16" => Self::I16,
            "ushort" | "System.UInt16" => Self::U16,
            "int" | "System.Int32" => Self::I32,
            "uint" | "System.UInt32" => Self::U32,
            "long" | "System.Int64" => Self::I64,
            "ulong" | "System.UInt64" => Self::U64,
            "nint" | "System.IntPtr" => Self::NativeInt,
            "nuint" | "System.UIntPtr" => Self::NativeUInt,
            "float" | "System.Single" => Self::F32,
            "double" | "System.Double" => Self::F64,
            "decimal" | "System.Decimal" => Self::Decimal,
            "string" | "System.String" => Self::String,
            "object" | "System.Object" => Self::Object,
            _ => return None,
        })
    }
    pub fn widens_to(self, other: Self) -> bool {
        use Primitive::*;
        matches!(
            (self, other),
            (I8, I16 | I32 | I64 | NativeInt | F32 | F64 | Decimal)
                | (
                    U8,
                    I16 | U16
                        | I32
                        | U32
                        | I64
                        | U64
                        | NativeInt
                        | NativeUInt
                        | F32
                        | F64
                        | Decimal
                )
                | (I16, I32 | I64 | NativeInt | F32 | F64 | Decimal)
                | (
                    U16 | Char,
                    I32 | U32 | I64 | U64 | NativeInt | NativeUInt | F32 | F64 | Decimal
                )
                | (Char, U16)
                | (I32, I64 | NativeInt | F32 | F64 | Decimal)
                | (U32, I64 | U64 | NativeUInt | F32 | F64 | Decimal)
                | (I64 | U64 | NativeInt | NativeUInt, F32 | F64 | Decimal)
                | (NativeInt, I64)
                | (NativeUInt, U64)
                | (F32, F64)
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PassingMode {
    #[default]
    Value,
    Ref,
    Out,
    In,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Constant {
    Integer(i128),
    Unsupported,
}

pub fn integer_literal(written: &str) -> Option<i128> {
    let written = written.replace('_', "");
    let digits = written.trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()
    } else if let Some(binary) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        i128::from_str_radix(binary, 2).ok()
    } else {
        digits.parse().ok()
    }
}

/// Substitution is simultaneous: supplied arguments are not substituted again.
/// The work bound also limits growth when a parameter occurs repeatedly.
pub fn substitute(
    ty: &Type,
    arguments: &[(ParameterId, Type)],
    budget: &mut usize,
) -> Option<Type> {
    fn visit(
        ty: &Type,
        args: &[(ParameterId, Type)],
        budget: &mut usize,
        depth: usize,
    ) -> Option<Type> {
        if depth > 64 {
            return None;
        }
        *budget = budget.checked_sub(1)?;
        Some(match ty {
            Type::Parameter(id) => {
                if let Some((_, value)) = args.iter().find(|(parameter, _)| parameter == id) {
                    return visit(value, &[], budget, depth + 1);
                }
                ty.clone()
            }
            Type::Named {
                definition,
                containing,
                arguments,
            } => Type::Named {
                definition: definition.clone(),
                containing: match containing {
                    Some(t) => Some(Box::new(visit(t, args, budget, depth + 1)?)),
                    None => None,
                },
                arguments: arguments
                    .iter()
                    .map(|t| visit(t, args, budget, depth + 1))
                    .collect::<Option<_>>()?,
            },
            Type::Array(t, rank) => {
                Type::Array(Box::new(visit(t, args, budget, depth + 1)?), *rank)
            }
            Type::Pointer(t) => Type::Pointer(Box::new(visit(t, args, budget, depth + 1)?)),
            Type::Nullable(t) => Type::Nullable(Box::new(visit(t, args, budget, depth + 1)?)),
            Type::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|(t, n)| Some((visit(t, args, budget, depth + 1)?, n.clone())))
                    .collect::<Option<_>>()?,
            ),
            _ => ty.clone(),
        })
    }
    visit(ty, arguments, budget, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(key: &str) -> DefinitionId {
        DefinitionId {
            context: "test".into(),
            key: key.into(),
        }
    }

    #[test]
    fn substitution_preserves_generic_owners_and_containing_types() {
        let outer = ParameterId {
            owner: id("Outer`1"),
            ordinal: 0,
        };
        let method = ParameterId {
            owner: id("Outer`1.Method``1"),
            ordinal: 0,
        };
        let input = Type::Named {
            definition: id("Outer`1.Inner`1"),
            containing: Some(Box::new(Type::Parameter(outer.clone()))),
            arguments: vec![Type::Array(Box::new(Type::Parameter(method.clone())), 2)],
        };
        let output = substitute(&input, &[(outer, Type::Null)], &mut 100).unwrap();
        let Type::Named {
            containing,
            arguments,
            ..
        } = output
        else {
            panic!()
        };
        assert_eq!(containing, Some(Box::new(Type::Null)));
        assert_eq!(
            arguments,
            vec![Type::Array(Box::new(Type::Parameter(method)), 2)]
        );
    }

    #[test]
    fn substitution_is_simultaneous_and_bounded() {
        let parameter = ParameterId {
            owner: id("C`1"),
            ordinal: 0,
        };
        let argument = Type::Array(Box::new(Type::Parameter(parameter.clone())), 1);
        let substitutions = [(parameter.clone(), argument.clone())];
        assert_eq!(
            substitute(&Type::Parameter(parameter.clone()), &substitutions, &mut 10),
            Some(argument)
        );
        assert_eq!(
            substitute(&Type::Parameter(parameter), &substitutions, &mut 1),
            None
        );
    }
}
