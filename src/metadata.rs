//! signature-only assembly extraction. no IL bodies or target code are loaded.
use crate::csharp::{
    syntax::{Accessor, GenericParameter, Header, Parameter, Variance, WrittenMember},
    types::{Constant, NamePart, PassingMode, WrittenType},
};
use crate::signature::*;
use anyhow::{Context, Result};
use dotscope::metadata::{cilassemblyview::CilAssemblyView, tables::*};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub semantic: Header,
    pub name: String,
    pub qualified: String,
    pub owner: String,
    pub namespace: String,
    pub kind: String,
    pub ty: String,
    pub parameters: Vec<String>,
    pub bases: Vec<String>,
    pub access: String,
    pub generic_count: u32,
    pub flags: u32,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AssemblyFacts {
    pub name: String,
    pub members: Vec<Member>,
    pub forwarders: Vec<(String, String)>,
    pub methods: usize,
    pub properties: usize,
    pub events: usize,
    pub interfaces: usize,
    pub method_implementations: usize,
    pub generic_parameters: usize,
}

struct Types {
    names: HashMap<u32, String>,
    specs: HashMap<u32, TypeSignature>,
    scopes: HashMap<u32, String>,
}
impl Types {
    fn written_token(&self, token: u32, depth: usize) -> WrittenType {
        if depth > 64 {
            return WrittenType::Unsupported(format!("recursive token {token:x}"));
        }
        if let Some(shape) = self.specs.get(&token) {
            return self.written(shape, depth + 1);
        }
        let Some(name) = self.names.get(&token) else {
            return WrittenType::Unsupported(format!("token {token:x}"));
        };
        let ty = WrittenType::Name {
            alias: Some("global".into()),
            parts: name
                .split('.')
                .map(|part| NamePart {
                    name: part.into(),
                    arguments: vec![],
                })
                .collect(),
        };
        match self.scopes.get(&token) {
            Some(assembly) => WrittenType::External {
                assembly: assembly.clone(),
                ty: Box::new(ty),
            },
            None => ty,
        }
    }
    fn written(&self, ty: &TypeSignature, depth: usize) -> WrittenType {
        if depth > 64 {
            return WrittenType::Unsupported("nested metadata signature".into());
        }
        match ty {
            TypeSignature::Primitive(name) => WrittenType::Name {
                alias: None,
                parts: vec![NamePart {
                    name: (*name).into(),
                    arguments: vec![],
                }],
            },
            TypeSignature::Named(token) => self.written_token(*token, depth + 1),
            TypeSignature::GenericType(ordinal) => WrittenType::MetadataParameter {
                method: false,
                ordinal: *ordinal,
            },
            TypeSignature::GenericMethod(ordinal) => WrittenType::MetadataParameter {
                method: true,
                ordinal: *ordinal,
            },
            TypeSignature::Pointer(ty) => {
                WrittenType::Pointer(Box::new(self.written(ty, depth + 1)))
            }
            TypeSignature::Vector(ty) => {
                WrittenType::Array(Box::new(self.written(ty, depth + 1)), 1)
            }
            TypeSignature::Array { element, rank, .. } => {
                WrittenType::Array(Box::new(self.written(element, depth + 1)), *rank)
            }
            TypeSignature::Generic(base, args) => {
                fn apply(ty: &mut WrittenType, args: &[WrittenType]) -> bool {
                    if let WrittenType::External { ty, .. } = ty {
                        return apply(ty, args);
                    }
                    let WrittenType::Name { parts, .. } = ty else {
                        return false;
                    };
                    let mut index = 0;
                    for part in parts {
                        if let Some((name, arity)) = part.name.rsplit_once('`') {
                            let Ok(arity) = arity.parse::<usize>() else {
                                return false;
                            };
                            let Some(arguments) = args.get(index..index + arity) else {
                                return false;
                            };
                            part.name = name.into();
                            part.arguments = arguments.to_vec();
                            index += arity;
                        }
                    }
                    index == args.len()
                }
                let mut base = self.written(base, depth + 1);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.written(arg, depth + 1))
                    .collect();
                if apply(&mut base, &args) {
                    base
                } else {
                    WrittenType::Unsupported("generic metadata owner arity".into())
                }
            }
            TypeSignature::ByRef(ty) => self.written(ty, depth + 1),
            // Required custom modifiers and function pointers need their own binding
            // rules. Retain uncertainty rather than erase a required distinction.
            TypeSignature::Modified {
                required: false,
                element,
                ..
            }
            | TypeSignature::Pinned(element) => self.written(element, depth + 1),
            _ => WrittenType::Unsupported(format!("{ty:?}")),
        }
    }
    fn header(&self, ty: Option<&TypeSignature>, params: &[TypeSignature], arity: u32) -> Header {
        Header {
            local: false,
            accessors: vec![],
            implementations: vec![],
            constant: None,
            declaration: 0,
            owner: None,
            ty: ty
                .map(|t| self.written(t, 0))
                .unwrap_or(WrittenType::Inferred),
            parameters: params
                .iter()
                .map(|ty| Parameter {
                    name: String::new(),
                    ty: self.written(ty, 0),
                    mode: if matches!(ty, TypeSignature::ByRef(_)) {
                        PassingMode::Ref
                    } else {
                        PassingMode::Value
                    },
                    default: None,
                    variadic: false,
                    receiver: false,
                })
                .collect(),
            generics: (0..arity)
                .map(|n| GenericParameter {
                    name: format!("T{n}"),
                    variance: Variance::Invariant,
                    constraints: vec![],
                    special_constraints: vec![],
                })
                .collect(),
            bases: vec![],
            explicit_interface: None,
        }
    }
    fn token(&self, token: u32, depth: usize) -> String {
        if depth > 32 {
            return "<recursive type>".into();
        }
        if let Some(name) = self.names.get(&token) {
            name.clone()
        } else if let Some(spec) = self.specs.get(&token) {
            self.shape(spec, depth + 1)
        } else {
            format!("<unresolved type {token:x}>")
        }
    }
    fn shape(&self, t: &TypeSignature, depth: usize) -> String {
        if depth > 32 {
            return "<recursive type>".into();
        }
        match t {
            TypeSignature::Primitive(name) => (*name).into(),
            TypeSignature::Named(token) => self.token(*token, depth + 1),
            TypeSignature::GenericType(n) => format!("!{n}"),
            TypeSignature::GenericMethod(n) => format!("!!{n}"),
            TypeSignature::ByRef(t) => format!("ref {}", self.shape(t, depth + 1)),
            TypeSignature::Pointer(t) => format!("{}*", self.shape(t, depth + 1)),
            TypeSignature::Vector(t) => format!("{}[]", self.shape(t, depth + 1)),
            TypeSignature::Array { element, rank, .. } => format!(
                "{}[{}]",
                self.shape(element, depth + 1),
                if *rank == 1 {
                    "*".into()
                } else {
                    ",".repeat((rank - 1) as usize)
                }
            ),
            TypeSignature::Generic(base, args) => format!(
                "{}<{}>",
                self.shape(base, depth + 1),
                args.iter()
                    .map(|a| self.shape(a, depth + 1))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            TypeSignature::Function(s) => format!(
                "delegate*{}<{}>",
                match s.flags & 0x0f {
                    1 => " unmanaged[Cdecl]",
                    2 => " unmanaged[Stdcall]",
                    3 => " unmanaged[Thiscall]",
                    4 => " unmanaged[Fastcall]",
                    9 => " unmanaged",
                    5 | 11 => " vararg",
                    _ => "",
                },
                s.params
                    .iter()
                    .chain(std::iter::once(&s.return_type))
                    .map(|p| self.shape(p, depth + 1))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            TypeSignature::Modified { element, .. } | TypeSignature::Pinned(element) => {
                self.shape(element, depth + 1)
            }
        }
    }
    fn parameter(&self, p: &TypeSignature) -> String {
        self.shape(p, 0)
    }
}
fn qualify(ns: &str, name: &str) -> String {
    if ns.is_empty() {
        name.into()
    } else {
        format!("{ns}.{name}")
    }
}
fn access(flags: u32) -> String {
    match flags & 7 {
        6 => "public",
        5 => "protected internal",
        4 => "protected",
        3 => "internal",
        2 => "private protected",
        _ => "private",
    }
    .into()
}

pub fn file_data(path: &Path) -> Result<crate::store::FileData> {
    let assembly = extract(path)?;
    let mut source = String::new();
    let mut facts = crate::model::Facts {
        csharp: Some(Default::default()),
        ..Default::default()
    };
    let owners: HashMap<_, _> = assembly
        .members
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.kind.as_str(), "class" | "struct" | "interface" | "enum"))
        .map(|(i, m)| (m.qualified.clone(), i as u32))
        .collect();
    facts.csharp.as_mut().unwrap().forwarders = assembly.forwarders;
    for mut m in assembly.members {
        if m.semantic.explicit_interface.is_some() {
            m.name = m.name.rsplit('.').next().unwrap_or(&m.name).into();
            m.qualified = qualify(&m.owner, &m.name);
        }
        m.semantic.declaration = facts.declarations.len() as u32;
        m.semantic.owner = owners.get(&m.owner).copied();
        facts.csharp.as_mut().unwrap().headers.push(m.semantic);
        if m.kind == "constructor" {
            m.name = m
                .owner
                .rsplit('.')
                .next()
                .unwrap_or(&m.owner)
                .split('`')
                .next()
                .unwrap_or(&m.owner)
                .into();
            m.qualified = qualify(&m.owner, &m.name);
        }
        let mut modifiers: Vec<String> = if matches!(
            m.kind.as_str(),
            "method" | "constructor" | "property" | "event"
        ) {
            [(0x10, "static"), (0x40, "virtual"), (0x400, "abstract")]
                .into_iter()
                .filter(|(flag, _)| m.flags & flag != 0)
                .map(|(_, name)| name.into())
                .collect()
        } else {
            Vec::new()
        };
        if m.flags & 0x40 != 0
            && m.flags & 0x100 == 0
            && matches!(m.kind.as_str(), "method" | "property" | "event")
        {
            modifiers.push("override".into());
        }
        if m.kind == "field" && m.flags & 0x10 != 0 {
            modifiers.push("static".into());
        }
        if m.kind == "class" {
            if m.flags & 0x80 != 0 {
                modifiers.push("abstract".into());
            }
            if m.flags & 0x100 != 0 {
                modifiers.push("sealed".into());
            }
        }
        let start = source.len();
        let prefix = if matches!(m.kind.as_str(), "class" | "struct" | "interface" | "enum") {
            format!("{} {} ", m.access, m.kind)
        } else if m.kind == "constructor" {
            format!("{} ", m.access)
        } else {
            format!("{} {} ", m.access, m.ty)
        };
        source.push_str(&prefix);
        let name_start = source.len();
        source.push_str(&m.name);
        let name_end = source.len();
        if matches!(m.kind.as_str(), "method" | "constructor") {
            if m.generic_count > 0 {
                source.push('<');
                source.push_str(
                    &(0..m.generic_count)
                        .map(|n| format!("T{n}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                source.push('>');
            }
            source.push('(');
            source.push_str(&m.parameters.join(", "));
            source.push(')');
        }
        if !m.bases.is_empty() {
            source.push_str(" : ");
            source.push_str(&m.bases.join(", "));
        }
        source.push(';');
        let end = source.len();
        source.push('\n');
        facts.declarations.push(crate::model::Declaration {
            name: m.name,
            qualified: m.qualified,
            kind: m.kind,
            namespace: m.namespace,
            owner: m.owner,
            name_span: name_start..name_end,
            span: start..end,
            header: start..end,
            scope: start..end,
            parameters: m.parameters,
            ty: m.ty,
            access: m.access,
            attributes: Vec::new(),
            modifiers,
            bases: m.bases,
        });
    }
    Ok(crate::store::FileData {
        source,
        facts,
        assembly: Some(assembly.name),
    })
}

pub fn extract(path: &Path) -> Result<AssemblyFacts> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "Cannot read assembly {}",
            crate::render::inline(&path.to_string_lossy())
        )
    })?;
    let view = CilAssemblyView::from_mem(bytes).with_context(|| {
        format!(
            "Cannot parse assembly {}",
            crate::render::inline(&path.to_string_lossy())
        )
    })?;
    let tables = view
        .tables()
        .ok_or_else(|| anyhow::anyhow!("Missing metadata tables"))?;
    let strings = view
        .strings()
        .ok_or_else(|| anyhow::anyhow!("Missing metadata strings"))?;
    let blobs = view
        .blobs()
        .ok_or_else(|| anyhow::anyhow!("Missing metadata blobs"))?;
    let mut result = AssemblyFacts {
        name: path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        ..AssemblyFacts::default()
    };
    if let Some(table) = tables.table::<AssemblyRaw>() {
        for row in table.iter() {
            result.name = strings.get(row?.name as usize)?.into();
        }
    }
    let mut types = Types {
        names: HashMap::new(),
        specs: HashMap::new(),
        scopes: HashMap::new(),
    };
    let mut assembly_scopes = HashMap::new();
    if let Some(table) = tables.table::<AssemblyRefRaw>() {
        for row in table.iter() {
            let row = row?;
            assembly_scopes.insert(
                row.token.value(),
                strings.get(row.name as usize)?.to_owned(),
            );
        }
    }
    let mut nested_names = Vec::new();
    if let Some(table) = tables.table::<TypeRefRaw>() {
        for row in table.iter() {
            let r = row?;
            if let Some(scope) = assembly_scopes.get(&r.resolution_scope.token.value()) {
                types.scopes.insert(r.token.value(), scope.clone());
            }
            types.names.insert(
                r.token.value(),
                qualify(
                    strings.get(r.type_namespace as usize)?,
                    strings.get(r.type_name as usize)?,
                ),
            );
            if r.resolution_scope.token.value() >> 24 == 1 {
                nested_names.push((
                    r.token.value(),
                    r.resolution_scope.token.value(),
                    strings.get(r.type_name as usize)?.to_owned(),
                ));
            }
        }
    }
    let defs = tables
        .table::<TypeDefRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    for r in &defs {
        types.scopes.insert(r.token.value(), result.name.clone());
        types.names.insert(
            r.token.value(),
            qualify(
                strings.get(r.type_namespace as usize)?,
                strings.get(r.type_name as usize)?,
            ),
        );
    }
    if let Some(table) = tables.table::<NestedClassRaw>() {
        let nested = table.iter().collect::<dotscope::Result<Vec<_>>>()?;
        for r in nested {
            let definition = defs
                .get(r.nested_class.saturating_sub(1) as usize)
                .ok_or_else(|| anyhow::anyhow!("Invalid nested type row"))?;
            nested_names.push((
                0x02000000 | r.nested_class,
                0x02000000 | r.enclosing_class,
                strings.get(definition.type_name as usize)?.to_owned(),
            ));
        }
    }
    for _ in 0..nested_names.len().min(32) {
        let mut changed = false;
        for (key, parent, name) in &nested_names {
            if let Some(scope) = types.scopes.get(parent).cloned() {
                types.scopes.insert(*key, scope);
            }
            if let Some(parent) = types.names.get(parent) {
                let full = qualify(parent, name);
                if types.names.get(key) != Some(&full) {
                    types.names.insert(*key, full);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    if let Some(table) = tables.table::<TypeSpecRaw>() {
        for row in table.iter() {
            let r = row?;
            let sig = parse_type_spec_signature(blobs.get(r.signature as usize)?)?;
            types.specs.insert(r.token.value(), sig);
        }
    }
    let mut interfaces: HashMap<u32, Vec<String>> = HashMap::new();
    let mut interface_types: HashMap<u32, Vec<WrittenType>> = HashMap::new();
    if let Some(table) = tables.table::<InterfaceImplRaw>() {
        for row in table.iter() {
            let r = row?;
            result.interfaces += 1;
            interface_types
                .entry(r.class)
                .or_default()
                .push(types.written_token(r.interface.token.value(), 0));
            interfaces
                .entry(r.class)
                .or_default()
                .push(types.token(r.interface.token.value(), 0));
        }
    }
    let implementation_rows = tables
        .table::<MethodImplRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    result.method_implementations = implementation_rows.len();
    let mut constraints: HashMap<u32, Vec<WrittenType>> = HashMap::new();
    if let Some(table) = tables.table::<GenericParamConstraintRaw>() {
        for row in table.iter() {
            let row = row?;
            constraints
                .entry(row.owner)
                .or_default()
                .push(types.written_token(row.constraint.token.value(), 0));
        }
    }
    let mut generics: HashMap<u32, Vec<(u32, GenericParameter)>> = HashMap::new();
    if let Some(table) = tables.table::<GenericParamRaw>() {
        for row in table.iter() {
            let row = row?;
            result.generic_parameters += 1;
            generics.entry(row.owner.token.value()).or_default().push((
                row.number,
                GenericParameter {
                    name: strings.get(row.name as usize)?.into(),
                    variance: match row.flags & 3 {
                        1 => Variance::Out,
                        2 => Variance::In,
                        _ => Variance::Invariant,
                    },
                    constraints: constraints.remove(&row.rid).unwrap_or_default(),
                    special_constraints: [(4, "class"), (8, "struct"), (16, "new()")]
                        .into_iter()
                        .filter(|(bit, _)| row.flags & bit != 0)
                        .map(|(_, name)| name.into())
                        .collect(),
                },
            ));
        }
    }
    for parameters in generics.values_mut() {
        parameters.sort_by_key(|(ordinal, _)| *ordinal);
    }
    let mut attributes: HashMap<u32, Vec<String>> = HashMap::new();
    let mut attribute_constructors = HashMap::new();
    let mut member_references = HashMap::new();
    let implementation_targets: std::collections::HashSet<_> = implementation_rows
        .iter()
        .map(|r| r.method_declaration.token.value())
        .collect();
    if let Some(table) = tables.table::<MemberRefRaw>() {
        for row in table.iter() {
            let row = row?;
            if strings.get(row.name as usize)? == ".ctor" {
                attribute_constructors
                    .insert(row.token.value(), types.token(row.class.token.value(), 0));
            }
            let signature = blobs.get(row.signature as usize)?;
            if implementation_targets.contains(&row.token.value())
                && signature.first().is_some_and(|prefix| prefix & 0x0f != 6)
            {
                let signature = parse_method_signature(signature)?;
                member_references.insert(
                    row.token.value(),
                    WrittenMember {
                        owner: types.written_token(row.class.token.value(), 0),
                        name: strings.get(row.name as usize)?.into(),
                        parameters: signature
                            .params
                            .iter()
                            .map(|p| types.written(p, 0))
                            .collect(),
                        generic_arity: signature.param_count_generic,
                    },
                );
            }
        }
    }
    if let Some(table) = tables.table::<CustomAttributeRaw>() {
        for row in table.iter() {
            let row = row?;
            if let Some(name) = attribute_constructors.get(&row.constructor.token.value())
                && matches!(
                    name.as_str(),
                    "System.Runtime.CompilerServices.ExtensionAttribute"
                        | "System.ParamArrayAttribute"
                )
            {
                attributes
                    .entry(row.parent.token.value())
                    .or_default()
                    .push(name.clone());
            }
        }
    }
    let parameters = tables
        .table::<ParamRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    let mut defaults = HashMap::new();
    let mut constants = HashMap::new();
    if let Some(table) = tables.table::<ConstantRaw>() {
        for row in table.iter() {
            let row = row?;
            let bytes = blobs.get(row.value as usize)?;
            let integer = match row.base {
                0x04 if bytes.len() == 1 => Some(i128::from(bytes[0] as i8)),
                0x05 if bytes.len() == 1 => Some(i128::from(bytes[0])),
                0x06 if bytes.len() == 2 => Some(i128::from(i16::from_le_bytes(bytes.try_into()?))),
                0x07 if bytes.len() == 2 => Some(i128::from(u16::from_le_bytes(bytes.try_into()?))),
                0x08 if bytes.len() == 4 => Some(i128::from(i32::from_le_bytes(bytes.try_into()?))),
                0x09 if bytes.len() == 4 => Some(i128::from(u32::from_le_bytes(bytes.try_into()?))),
                0x0a if bytes.len() == 8 => Some(i128::from(i64::from_le_bytes(bytes.try_into()?))),
                0x0b if bytes.len() == 8 => Some(i128::from(u64::from_le_bytes(bytes.try_into()?))),
                _ => None,
            };
            constants.insert(
                row.parent.token.value(),
                integer.map_or(Constant::Unsupported, Constant::Integer),
            );
            defaults.insert(
                row.parent.token.value(),
                format!("{}:{:x?}", row.base, blobs.get(row.value as usize)?),
            );
        }
    }
    let methods = tables
        .table::<MethodDefRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    let mut accessors: HashMap<u32, u32> = HashMap::new();
    let mut accessor_roles: HashMap<u32, Vec<Accessor>> = HashMap::new();
    for target in implementation_targets.iter().filter(|t| **t >> 24 == 6) {
        let rid = target & 0x00ff_ffff;
        if let (Some(method), Some(owner)) = (
            methods.get(rid.saturating_sub(1) as usize),
            defs.iter().rev().find(|d| d.method_list <= rid),
        ) {
            let signature = parse_method_signature(blobs.get(method.signature as usize)?)?;
            member_references.insert(
                *target,
                WrittenMember {
                    owner: types.written_token(owner.token.value(), 0),
                    name: strings.get(method.name as usize)?.into(),
                    parameters: signature
                        .params
                        .iter()
                        .map(|p| types.written(p, 0))
                        .collect(),
                    generic_arity: signature.param_count_generic,
                },
            );
        }
    }
    if let Some(table) = tables.table::<MethodSemanticsRaw>() {
        for row in table.iter() {
            let row = row?;
            let method = methods
                .get(row.method.saturating_sub(1) as usize)
                .ok_or_else(|| anyhow::anyhow!("Invalid accessor method row"))?;
            let flags = accessors.entry(row.association.token.value()).or_default();
            *flags = (*flags | method.flags) & !7 | (*flags & 7).max(method.flags & 7);
            accessor_roles
                .entry(row.association.token.value())
                .or_default()
                .push(Accessor {
                    role: match row.semantics {
                        1 => "set",
                        2 => "get",
                        8 => "add",
                        16 => "remove",
                        32 => "raise",
                        _ => "other",
                    }
                    .into(),
                    access: access(method.flags),
                    metadata_method: Some(method.token.value()),
                });
        }
    }
    let fields = tables
        .table::<FieldRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    result.members.reserve(
        defs.len()
            + methods.len()
            + fields.len()
            + tables
                .table::<PropertyRaw>()
                .map_or(0, |table| table.row_count as usize)
            + tables
                .table::<EventRaw>()
                .map_or(0, |table| table.row_count as usize),
    );
    for (i, d) in defs.iter().enumerate() {
        let name = strings.get(d.type_name as usize)?;
        if name == "<Module>" {
            continue;
        }
        let owner = types.names[&d.token.value()].clone();
        let namespace = strings.get(d.type_namespace as usize)?.to_owned();
        let mut bases = interfaces.remove(&d.rid).unwrap_or_default();
        if d.extends.row != 0 {
            bases.insert(0, types.token(d.extends.token.value(), 0));
        }
        let kind = if d.flags & 0x20 != 0 {
            "interface"
        } else if bases.first().is_some_and(|b| b == "System.Enum") {
            "enum"
        } else if bases.first().is_some_and(|b| b == "System.ValueType") {
            "struct"
        } else {
            "class"
        };
        result.members.push(Member {
            semantic: {
                let mut header = types.header(
                    None,
                    &[],
                    name.rsplit_once('`')
                        .and_then(|(_, n)| n.parse().ok())
                        .unwrap_or(0),
                );
                if let Some(parameters) = generics.remove(&d.token.value()) {
                    let own_arity = name
                        .rsplit_once('`')
                        .and_then(|(_, n)| n.parse::<usize>().ok())
                        .unwrap_or(0);
                    let inherited = parameters.len().saturating_sub(own_arity);
                    header.generics = parameters
                        .into_iter()
                        .skip(inherited)
                        .map(|(_, parameter)| parameter)
                        .collect();
                }
                header.bases = interface_types.remove(&d.rid).unwrap_or_default();
                if d.extends.row != 0 {
                    header
                        .bases
                        .insert(0, types.written_token(d.extends.token.value(), 0));
                }
                header
            },
            name: name.split('`').next().unwrap_or(name).into(),
            qualified: owner.clone(),
            owner: nested_names
                .iter()
                .find(|(key, _, _)| *key == d.token.value())
                .map(|(_, parent, _)| types.token(*parent, 0))
                .unwrap_or_default(),
            namespace: namespace.clone(),
            kind: kind.into(),
            ty: String::new(),
            parameters: Vec::new(),
            bases,
            access: if matches!(d.flags & 7, 1 | 2) {
                "public"
            } else {
                "internal"
            }
            .into(),
            generic_count: 0,
            flags: d.flags,
        });
        let end = defs
            .get(i + 1)
            .map_or(methods.len() as u32 + 1, |d| d.method_list);
        for method in rows(&methods, d.method_list, end)? {
            let name = strings.get(method.name as usize)?;
            let sig = parse_method_signature(blobs.get(method.signature as usize)?)
                .with_context(|| format!("Invalid method signature {owner}.{name}"))?;
            result.methods += 1;
            let mut semantic =
                types.header(Some(&sig.return_type), &sig.params, sig.param_count_generic);
            for implementation in implementation_rows
                .iter()
                .filter(|r| r.method_body.token.value() == method.token.value())
            {
                if let Some(target) =
                    member_references.get(&implementation.method_declaration.token.value())
                {
                    semantic.implementations.push(target.clone());
                }
            }
            if name.contains('.') {
                semantic.explicit_interface =
                    semantic.implementations.first().map(|i| i.owner.clone());
            }
            if let Some(parameters) = generics.remove(&method.token.value()) {
                semantic.generics = parameters
                    .into_iter()
                    .map(|(_, parameter)| parameter)
                    .collect();
            }
            let end = methods
                .get(method.rid as usize)
                .map_or(parameters.len() as u32 + 1, |next| next.param_list);
            for parameter in rows(&parameters, method.param_list, end)? {
                if parameter.sequence == 0 {
                    continue;
                }
                if let Some(target) = semantic.parameters.get_mut(parameter.sequence as usize - 1) {
                    target.name = strings.get(parameter.name as usize)?.into();
                    if target.mode == PassingMode::Ref {
                        target.mode = if parameter.flags & 2 != 0 {
                            PassingMode::Out
                        } else if parameter.flags & 1 != 0 {
                            PassingMode::In
                        } else {
                            PassingMode::Ref
                        };
                    }
                    target.default = defaults
                        .get(&parameter.token.value())
                        .cloned()
                        .or_else(|| (parameter.flags & 0x10 != 0).then(|| "optional".into()));
                    target.variadic =
                        attributes
                            .get(&parameter.token.value())
                            .is_some_and(|attrs| {
                                attrs.iter().any(|a| a == "System.ParamArrayAttribute")
                            });
                }
            }
            if attributes.get(&method.token.value()).is_some_and(|attrs| {
                attrs
                    .iter()
                    .any(|a| a == "System.Runtime.CompilerServices.ExtensionAttribute")
            }) && let Some(receiver) = semantic.parameters.first_mut()
            {
                receiver.receiver = true;
            }
            result.members.push(Member {
                semantic,
                name: name.into(),
                qualified: qualify(&owner, name),
                owner: owner.clone(),
                namespace: namespace.clone(),
                kind: if matches!(name, ".ctor" | ".cctor") {
                    "constructor"
                } else {
                    "method"
                }
                .into(),
                ty: types.parameter(&sig.return_type),
                parameters: sig.params.iter().map(|p| types.parameter(p)).collect(),
                bases: Vec::new(),
                access: access(method.flags),
                generic_count: sig.param_count_generic,
                flags: method.flags,
            });
        }
        let end = defs
            .get(i + 1)
            .map_or(fields.len() as u32 + 1, |d| d.field_list);
        for field in rows(&fields, d.field_list, end)? {
            let name = strings.get(field.name as usize)?;
            let sig = parse_field_signature(blobs.get(field.signature as usize)?)?;
            result.members.push(Member {
                semantic: {
                    let mut header = types.header(Some(&sig), &[], 0);
                    header.constant = constants.get(&field.token.value()).cloned();
                    header
                },
                name: name.into(),
                qualified: qualify(&owner, name),
                owner: owner.clone(),
                namespace: namespace.clone(),
                kind: "field".into(),
                ty: types.shape(&sig, 0),
                parameters: Vec::new(),
                bases: Vec::new(),
                access: access(field.flags),
                generic_count: 0,
                flags: field.flags,
            });
        }
    }
    let properties = tables
        .table::<PropertyRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    let maps = tables
        .table::<PropertyMapRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    for (i, map) in maps.iter().enumerate() {
        let owner = types.token(0x02000000 | map.parent, 0);
        let end = maps
            .get(i + 1)
            .map_or(properties.len() as u32 + 1, |m| m.property_list);
        for p in rows(&properties, map.property_list, end)? {
            let name = strings.get(p.name as usize)?;
            let sig = parse_property_signature(blobs.get(p.signature as usize)?)?;
            result.properties += 1;
            result.members.push(Member {
                semantic: {
                    let mut header = types.header(Some(&sig.return_type), &sig.params, 0);
                    header.accessors = accessor_roles.remove(&p.token.value()).unwrap_or_default();
                    header
                },
                name: name.into(),
                qualified: qualify(&owner, name),
                owner: owner.clone(),
                namespace: owner.rsplit_once('.').map_or("", |(p, _)| p).into(),
                kind: "property".into(),
                ty: types.shape(&sig.return_type, 0),
                parameters: sig.params.iter().map(|p| types.parameter(p)).collect(),
                bases: Vec::new(),
                access: access(*accessors.get(&p.token.value()).unwrap_or(&0)),
                generic_count: 0,
                flags: *accessors.get(&p.token.value()).unwrap_or(&0),
            });
        }
    }
    let events = tables
        .table::<EventRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    let maps = tables
        .table::<EventMapRaw>()
        .map(|t| t.iter().collect::<dotscope::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    for (i, map) in maps.iter().enumerate() {
        let owner = types.token(0x02000000 | map.parent, 0);
        let end = maps
            .get(i + 1)
            .map_or(events.len() as u32 + 1, |m| m.event_list);
        for event in rows(&events, map.event_list, end)? {
            let name = strings.get(event.name as usize)?;
            let flags = *accessors.get(&event.token.value()).unwrap_or(&0);
            result.events += 1;
            result.members.push(Member {
                semantic: {
                    let mut header = types.header(
                        Some(&TypeSignature::Named(event.event_type.token.value())),
                        &[],
                        0,
                    );
                    header.accessors = accessor_roles
                        .remove(&event.token.value())
                        .unwrap_or_default();
                    header
                },
                name: name.into(),
                qualified: qualify(&owner, name),
                owner: owner.clone(),
                namespace: owner.rsplit_once('.').map_or("", |(p, _)| p).into(),
                kind: "event".into(),
                ty: types.token(event.event_type.token.value(), 0),
                parameters: Vec::new(),
                bases: Vec::new(),
                access: access(flags),
                generic_count: 0,
                flags,
            });
        }
    }
    let mut assemblies = HashMap::new();
    if let Some(table) = tables.table::<AssemblyRefRaw>() {
        for row in table.iter() {
            let r = row?;
            assemblies.insert(r.rid, strings.get(r.name as usize)?.to_owned());
        }
    }
    if let Some(table) = tables.table::<ExportedTypeRaw>() {
        for row in table.iter() {
            let r = row?;
            if r.flags & 0x00200000 != 0 {
                result.forwarders.push((
                    qualify(
                        strings.get(r.namespace as usize)?,
                        strings.get(r.name as usize)?,
                    ),
                    assemblies
                        .get(&r.implementation.row)
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
        }
    }
    Ok(result)
}

fn rows<T>(table: &[T], start: u32, end: u32) -> Result<&[T]> {
    let start = start
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid metadata row range"))? as usize;
    let end = end
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid metadata row range"))? as usize;
    table
        .get(start..end)
        .ok_or_else(|| anyhow::anyhow!("Metadata row range exceeds table"))
}
