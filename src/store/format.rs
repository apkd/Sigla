//! Shared declaration names and descriptions. Storage IDs never leave this layer.
#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
use crate::binary::encode;
use crate::{
    csharp::{syntax::*, types::*},
    model::Declaration,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    sync::Arc,
};

const ROWS: u8 = 18;
const STRINGS: u8 = 19;
const TYPES: u8 = 20;
const SIGNATURES: u8 = 21;
const DIRECTORY: u8 = 22;
const PAGE_BYTES: usize = 16 * 1024;
// Leave room for the lowering limit (66 nodes) and assembly qualification wrappers.
const TYPE_DEPTH: usize = 128;

fn vector_bytes<T>(values: &Vec<T>, heap: impl Fn(&T) -> usize) -> usize {
    values.capacity() * std::mem::size_of::<T>() + values.iter().map(heap).sum::<usize>()
}
fn strings_bytes(values: &Vec<String>) -> usize {
    vector_bytes(values, String::capacity)
}
fn type_bytes(ty: &WrittenType) -> usize {
    match ty {
        WrittenType::External { assembly, ty } => {
            assembly.capacity() + std::mem::size_of::<WrittenType>() + type_bytes(ty)
        }
        WrittenType::Name { alias, parts } => {
            alias.as_ref().map_or(0, String::capacity)
                + vector_bytes(parts, |p| {
                    p.name.capacity() + vector_bytes(&p.arguments, type_bytes)
                })
        }
        WrittenType::Parameter(p) => p.owner.context.capacity() + p.owner.key.capacity(),
        WrittenType::Array(ty, _) | WrittenType::Pointer(ty) | WrittenType::Nullable(ty) => {
            std::mem::size_of::<WrittenType>() + type_bytes(ty)
        }
        WrittenType::Tuple(parts) => vector_bytes(parts, |(ty, name)| {
            type_bytes(ty) + name.as_ref().map_or(0, String::capacity)
        }),
        WrittenType::Unsupported(text) => text.capacity(),
        _ => 0,
    }
}
pub(crate) fn declaration_bytes(file: &DeclarationFile) -> usize {
    std::mem::size_of::<DeclarationFile>()
        + vector_bytes(&file.declarations, |d| {
            [
                &d.name,
                &d.qualified,
                &d.kind,
                &d.namespace,
                &d.owner,
                &d.ty,
                &d.access,
            ]
            .into_iter()
            .map(String::capacity)
            .sum::<usize>()
                + [&d.parameters, &d.attributes, &d.modifiers, &d.bases]
                    .into_iter()
                    .map(strings_bytes)
                    .sum::<usize>()
        })
        + vector_bytes(&file.headers, |h| {
            type_bytes(&h.ty)
                + vector_bytes(&h.parameters, |p| {
                    p.name.capacity()
                        + type_bytes(&p.ty)
                        + p.default.as_ref().map_or(0, String::capacity)
                })
                + vector_bytes(&h.generics, |g| {
                    g.name.capacity()
                        + vector_bytes(&g.constraints, type_bytes)
                        + strings_bytes(&g.special_constraints)
                })
                + vector_bytes(&h.bases, type_bytes)
                + h.explicit_interface.as_ref().map_or(0, type_bytes)
                + vector_bytes(&h.implementations, |m| {
                    type_bytes(&m.owner)
                        + m.name.capacity()
                        + vector_bytes(&m.parameters, type_bytes)
                })
                + vector_bytes(&h.accessors, |a| a.role.capacity() + a.access.capacity())
        })
        + vector_bytes(&file.imports, |i| {
            type_bytes(&i.ty)
                + match &i.kind {
                    ImportKind::Alias(name) => name.capacity(),
                    _ => 0,
                }
        })
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Directory {
    pub count: u32,
    pub csharp: bool,
    pub source: bool,
    pages: [Vec<u32>; 4],
}

#[derive(Serialize, Deserialize)]
struct Page<'a> {
    offsets: Vec<u32>,
    #[serde(borrow)]
    data: &'a [u8],
}

pub(crate) struct PageIndex {
    offsets: Vec<u32>,
    data_start: usize,
}
struct ReadPage<'a> {
    index: Arc<PageIndex>,
    data: &'a [u8],
}

fn cache_key(identity: &super::ObjectId, tag: u8, index: Option<u32>) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(identity);
    hash.update(&[tag]);
    if let Some(index) = index {
        hash.update(&index.to_be_bytes());
    }
    *hash.finalize().as_bytes()
}

fn key(tag: u8, page: u32) -> Vec<u8> {
    let mut key = vec![tag];
    key.extend(page.to_be_bytes());
    key
}

fn pages<T: Serialize>(
    tag: u8,
    items: &[T],
    records: &mut BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Vec<u32>> {
    let mut starts = Vec::new();
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for (id, item) in items.iter().enumerate() {
        let bytes = encode(item)?;
        if !data.is_empty() && data.len() + bytes.len() + offsets.len() * 4 > PAGE_BYTES {
            offsets.push(data.len().try_into()?);
            records.insert(
                key(tag, (starts.len() - 1).try_into()?),
                encode(&Page {
                    offsets: std::mem::take(&mut offsets),
                    data: &data,
                })?,
            );
            data.clear();
        }
        if offsets.is_empty() {
            starts.push(id.try_into()?);
        }
        offsets.push(data.len().try_into()?);
        data.extend(bytes);
    }
    if !offsets.is_empty() {
        offsets.push(data.len().try_into()?);
        records.insert(
            key(tag, (starts.len() - 1).try_into()?),
            encode(&Page {
                offsets,
                data: &data,
            })?,
        );
    }
    Ok(starts)
}

#[derive(Serialize, Deserialize)]
struct Names {
    declarations: Vec<u32>,
    occurrences: Vec<u32>,
    global_imports: bool,
}

pub(crate) fn names<'a>(
    bytes: &[u8],
    read: impl Fn(&[u8]) -> Result<Option<&'a [u8]>>,
) -> Result<super::shared::Names> {
    let names: Names = crate::binary::decode(bytes)?;
    let mut reader = Reader::new(read)?;
    Ok(super::shared::Names {
        declarations: reader.strings(&names.declarations)?.into_iter().collect(),
        occurrences: reader.strings(&names.occurrences)?.into_iter().collect(),
        global_imports: names.global_imports,
    })
}

pub(crate) fn write(
    declarations: &[Declaration],
    headers: Option<&[Header]>,
    source: bool,
    names: &super::shared::Names,
    records: &mut BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut pool = Pool::new();
    let mut rows = Vec::with_capacity(declarations.len());
    for (index, declaration) in declarations.iter().enumerate() {
        let plain = Header {
            local: false,
            declaration: index.try_into()?,
            owner: None,
            ty: WrittenType::Inferred,
            parameters: Vec::new(),
            generics: Vec::new(),
            bases: Vec::new(),
            explicit_interface: None,
            implementations: Vec::new(),
            constant: None,
            accessors: Vec::new(),
        };
        let header = match headers {
            Some(headers) => headers.get(index).context("Missing declaration header")?,
            None => &plain,
        };
        rows.push(pool.row(declaration, header)?);
    }
    let names = Names {
        declarations: names
            .declarations
            .iter()
            .map(|name| pool.string(name))
            .collect(),
        occurrences: names
            .occurrences
            .iter()
            .map(|name| pool.string(name))
            .collect(),
        global_imports: names.global_imports,
    };
    let directory = Directory {
        count: rows.len().try_into()?,
        csharp: headers.is_some(),
        source,
        pages: [
            pages(ROWS, &rows, records)?,
            pages(STRINGS, &pool.strings, records)?,
            pages(TYPES, &pool.types, records)?,
            pages(SIGNATURES, &pool.signatures, records)?,
        ],
    };
    records.insert(vec![DIRECTORY], encode(&directory)?);
    encode(&names)
}

pub(crate) struct Reader<'a, F> {
    read: F,
    pub directory: Arc<Directory>,
    strings: HashMap<u32, String>,
    pages: std::cell::RefCell<HashMap<(u8, usize), ReadPage<'a>>>,
    remaining: usize,
    identity: Option<super::ObjectId>,
}
impl<'a, F: Fn(&[u8]) -> Result<Option<&'a [u8]>>> Reader<'a, F> {
    pub fn new(read: F) -> Result<Self> {
        Self::open(read, None)
    }
    pub fn for_analysis(read: F, identity: super::ObjectId) -> Result<Self> {
        Self::open(read, Some(identity))
    }
    fn open(read: F, identity: Option<super::ObjectId>) -> Result<Self> {
        let key = identity.map(|identity| cache_key(&identity, DIRECTORY, None));
        let cached = key.and_then(|key| match super::DECODED.lock().unwrap().get(&key) {
            Some(super::Decoded::Directory(value)) => Some(value),
            _ => None,
        });
        let directory = match cached {
            Some(directory) => directory,
            None => {
                let directory: Directory = crate::binary::decode(
                    read(&[DIRECTORY])?.context("Missing declaration directory")?,
                )?;
                ensure!(
                    directory
                        .pages
                        .iter()
                        .all(|p| p.is_empty() || (p[0] == 0 && p.windows(2).all(|w| w[0] < w[1]))),
                    "Invalid page directory"
                );
                let size = std::mem::size_of::<Directory>()
                    + directory
                        .pages
                        .iter()
                        .map(|page| vector_bytes(page, |_| 0))
                        .sum::<usize>();
                let directory = Arc::new(directory);
                if let Some(key) = key {
                    super::DECODED.lock().unwrap().put(
                        key,
                        super::Decoded::Directory(directory.clone()),
                        size,
                    );
                }
                directory
            }
        };
        Ok(Self {
            read,
            directory,
            strings: HashMap::new(),
            pages: Default::default(),
            remaining: 1024 * 1024 * 1024,
            identity,
        })
    }
    pub fn validate_pages(&self) -> Result<()> {
        let mut counts = [0u32; 4];
        for tag in ROWS..=SIGNATURES {
            let starts = &self.directory.pages[(tag - ROWS) as usize];
            for (index, &first) in starts.iter().enumerate() {
                let bytes = (self.read)(&key(tag, index.try_into()?))?
                    .context("Missing shared record page")?;
                let page: Page = crate::binary::decode(bytes)?;
                ensure!(
                    page.offsets.len() >= 2
                        && page.offsets[0] == 0
                        && page.offsets.last().copied() == Some(page.data.len().try_into()?)
                        && page.offsets.windows(2).all(|pair| pair[0] < pair[1]),
                    "Invalid shared record offsets"
                );
                let end = first
                    .checked_add((page.offsets.len() - 1).try_into()?)
                    .context("Shared record count overflow")?;
                counts[(tag - ROWS) as usize] = end;
                if let Some(&next) = starts.get(index + 1) {
                    ensure!(next == end, "Incomplete shared record directory");
                } else if tag == ROWS {
                    ensure!(
                        end == self.directory.count,
                        "Incomplete declaration directory"
                    );
                }
            }
        }
        ensure!(
            self.directory.count == 0 || !self.directory.pages[0].is_empty(),
            "Missing declaration pages"
        );
        let text = |id: u32| -> Result<()> {
            ensure!(id < counts[1], "Invalid shared name ID");
            Ok(())
        };
        let ty = |id: u32| -> Result<()> {
            ensure!(id < counts[2], "Invalid shared type ID");
            Ok(())
        };
        for id in 0..counts[1] {
            self.item::<String>(STRINGS, id)?;
        }
        let mut depths = Vec::new();
        for id in 0..counts[2] {
            let node: TypeNode = self.item(TYPES, id)?;
            let mut depth = 0;
            type_references(&node, text, |child| {
                ensure!(child < id, "Invalid shared type reference");
                depth = depth.max(depths[child as usize]);
                Ok(())
            })?;
            ensure!(depth < TYPE_DEPTH, "Shared type nesting limit exceeded");
            depths.push(depth + 1);
        }
        for id in 0..counts[3] {
            let signature: Signature = self.item(SIGNATURES, id)?;
            ty(signature.ty)?;
            for parameter in &signature.parameters {
                text(parameter.name)?;
                ty(parameter.ty)?;
                if let Some(id) = parameter.default {
                    text(id)?;
                }
            }
            for generic in &signature.generics {
                text(generic.name)?;
                for &id in &generic.constraints {
                    ty(id)?;
                }
                for &id in &generic.special {
                    text(id)?;
                }
            }
            for id in signature.bases {
                ty(id)?;
            }
            if let Some(id) = signature.explicit {
                ty(id)?;
            }
            for member in signature.implementations {
                ty(member.owner)?;
                text(member.name)?;
                for id in member.parameters {
                    ty(id)?;
                }
            }
            for accessor in signature.accessors {
                text(accessor.role)?;
                text(accessor.access)?;
            }
        }
        Ok(())
    }
    pub fn contains_key(&self, key: &[u8]) -> bool {
        if key == [DIRECTORY] {
            return true;
        }
        if key.len() != 5 || !(ROWS..=SIGNATURES).contains(&key[0]) {
            return false;
        }
        let page = u32::from_be_bytes(key[1..].try_into().unwrap()) as usize;
        page < self.directory.pages[(key[0] - ROWS) as usize].len()
    }
    fn item<T: Record>(&self, tag: u8, id: u32) -> Result<T> {
        let cache_key = self
            .identity
            .map(|identity| cache_key(&identity, tag, Some(id)));
        if let Some(key) = cache_key
            && let Some(super::Decoded::Shared(value)) = super::DECODED.lock().unwrap().get(&key)
            && let Some(value) = T::get(&value)
        {
            return Ok(value.clone());
        }
        let starts = &self.directory.pages[(tag - ROWS) as usize];
        let page = starts
            .partition_point(|&first| first <= id)
            .checked_sub(1)
            .context("Invalid shared record ID")?;
        let mut pages = self.pages.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = pages.entry((tag, page)) {
            entry.insert(self.page(tag, page.try_into()?)?);
        }
        let page_data = &pages[&(tag, page)];
        let slot = (id - starts[page]) as usize;
        let start = *page_data
            .index
            .offsets
            .get(slot)
            .context("Invalid shared record ID")? as usize;
        let end = *page_data
            .index
            .offsets
            .get(slot + 1)
            .context("Invalid shared record ID")? as usize;
        let value: T = crate::binary::decode(
            page_data
                .data
                .get(start..end)
                .context("Invalid shared record offsets")?,
        )?;
        if let Some(key) = cache_key {
            let size = value.bytes() + std::mem::size_of::<Shared>();
            super::DECODED.lock().unwrap().put(
                key,
                super::Decoded::Shared(std::sync::Arc::new(value.clone().shared())),
                size,
            );
        }
        Ok(value)
    }
    fn page(&self, tag: u8, number: u32) -> Result<ReadPage<'a>> {
        let bytes = (self.read)(&key(tag, number))?.context("Missing shared record page")?;
        let key = self
            .identity
            .map(|identity| cache_key(&identity, tag | 128, Some(number)));
        let cached = key.and_then(|key| match super::DECODED.lock().unwrap().get(&key) {
            Some(super::Decoded::Page(value)) => Some(value),
            _ => None,
        });
        let index = match cached {
            Some(index) => index,
            None => {
                let page: Page = crate::binary::decode(bytes)?;
                let index = Arc::new(PageIndex {
                    offsets: page.offsets,
                    data_start: bytes.len() - page.data.len(),
                });
                let size = std::mem::size_of::<PageIndex>() + vector_bytes(&index.offsets, |_| 0);
                if let Some(key) = key {
                    super::DECODED.lock().unwrap().put(
                        key,
                        super::Decoded::Page(index.clone()),
                        size,
                    );
                }
                index
            }
        };
        Ok(ReadPage {
            data: bytes
                .get(index.data_start..)
                .context("Invalid shared page data")?,
            index,
        })
    }
    fn string(&mut self, id: u32) -> Result<String> {
        if !self.strings.contains_key(&id) {
            self.strings.insert(id, self.item(STRINGS, id)?);
        }
        let value = &self.strings[&id];
        self.remaining = self
            .remaining
            .checked_sub(value.len())
            .context("Shared record expansion limit exceeded")?;
        Ok(value.clone())
    }
    fn strings(&mut self, ids: &[u32]) -> Result<Vec<String>> {
        self.reserve::<String>(ids.len())?;
        ids.iter().map(|&id| self.string(id)).collect()
    }
    fn reserve<T>(&mut self, count: usize) -> Result<()> {
        // Include spare capacity when a growing output vector doubles.
        let bytes = count
            .checked_mul(std::mem::size_of::<T>())
            .and_then(|bytes| bytes.checked_mul(2))
            .context("Shared record expansion size overflow")?;
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .context("Shared record expansion limit exceeded")?;
        Ok(())
    }
    fn ty(&mut self, id: u32, depth: usize) -> Result<WrittenType> {
        ensure!(depth < TYPE_DEPTH, "Shared type nesting limit exceeded");
        self.reserve::<WrittenType>(1)?;
        // Children precede their parents. This also rejects cycles without a traversal stack.
        let node: TypeNode = self.item(TYPES, id)?;
        match &node {
            TypeNode::Name { parts, .. } => self.reserve::<NamePart>(parts.len())?,
            TypeNode::Tuple(parts) => self.reserve::<Option<String>>(parts.len())?,
            _ => {}
        }
        let child = |this: &mut Self, child: u32| -> Result<WrittenType> {
            ensure!(child < id, "Invalid shared type reference");
            this.ty(child, depth + 1)
        };
        Ok(match node {
            TypeNode::MetadataParameter { method, ordinal } => {
                WrittenType::MetadataParameter { method, ordinal }
            }
            TypeNode::External { assembly, ty } => WrittenType::External {
                assembly: self.string(assembly)?,
                ty: Box::new(child(self, ty)?),
            },
            TypeNode::Name { alias, parts } => WrittenType::Name {
                alias: alias.map(|id| self.string(id)).transpose()?,
                parts: parts
                    .into_iter()
                    .map(|(name, arguments)| {
                        Ok(NamePart {
                            name: self.string(name)?,
                            arguments: arguments
                                .into_iter()
                                .map(|id| child(self, id))
                                .collect::<Result<_>>()?,
                        })
                    })
                    .collect::<Result<_>>()?,
            },
            TypeNode::Parameter {
                context,
                key,
                ordinal,
            } => WrittenType::Parameter(ParameterId {
                owner: DefinitionId {
                    context: self.string(context)?,
                    key: self.string(key)?,
                },
                ordinal,
            }),
            TypeNode::Array(ty, rank) => WrittenType::Array(Box::new(child(self, ty)?), rank),
            TypeNode::Pointer(ty) => WrittenType::Pointer(Box::new(child(self, ty)?)),
            TypeNode::Nullable(ty) => WrittenType::Nullable(Box::new(child(self, ty)?)),
            TypeNode::Tuple(parts) => WrittenType::Tuple(
                parts
                    .into_iter()
                    .map(|(ty, name)| {
                        Ok((
                            child(self, ty)?,
                            name.map(|id| self.string(id)).transpose()?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            TypeNode::Dynamic => WrittenType::Dynamic,
            TypeNode::Inferred => WrittenType::Inferred,
            TypeNode::Unsupported(text) => WrittenType::Unsupported(self.string(text)?),
        })
    }
    fn expand_declaration(&mut self, row: &Row) -> Result<Declaration> {
        self.reserve::<Declaration>(1)?;
        let range = |i: usize| -> Result<Range<usize>> {
            ensure!(row.spans[i] <= row.spans[i + 1], "Invalid declaration span");
            Ok(row.spans[i] as usize..row.spans[i + 1] as usize)
        };
        Ok(Declaration {
            name: self.string(row.text[0])?,
            qualified: self.string(row.text[1])?,
            kind: self.string(row.text[2])?,
            namespace: self.string(row.text[3])?,
            owner: self.string(row.text[4])?,
            ty: self.string(row.text[5])?,
            access: self.string(row.text[6])?,
            name_span: range(0)?,
            span: range(2)?,
            header: range(4)?,
            scope: range(6)?,
            parameters: self.strings(&row.parameters)?,
            attributes: self.strings(&row.attributes)?,
            modifiers: self.strings(&row.modifiers)?,
            bases: self.strings(&row.bases)?,
        })
    }
    pub fn declarations(&mut self) -> Result<Vec<Declaration>> {
        let mut declarations = Vec::new();
        for id in 0..self.directory.count {
            let row: Row = self.item(ROWS, id)?;
            ensure!(row.declaration == id, "Invalid declaration ordinal");
            declarations.push(self.expand_declaration(&row)?);
        }
        Ok(declarations)
    }
    pub fn declaration(&mut self, id: u32) -> Result<(Declaration, Header)> {
        ensure!(id < self.directory.count, "Invalid declaration ID");
        let row: Row = self.item(ROWS, id)?;
        ensure!(
            row.declaration == id && row.owner.is_none_or(|owner| owner < self.directory.count),
            "Invalid declaration owner or ordinal"
        );
        let signature: Signature = self.item(SIGNATURES, row.signature)?;
        self.reserve::<Header>(1)?;
        self.reserve::<Parameter>(signature.parameters.len())?;
        self.reserve::<GenericParameter>(signature.generics.len())?;
        self.reserve::<WrittenMember>(signature.implementations.len())?;
        self.reserve::<Accessor>(signature.accessors.len())?;
        let declaration = self.expand_declaration(&row)?;
        let header = Header {
            local: row.local,
            declaration: row.declaration,
            owner: row.owner,
            ty: self.ty(signature.ty, 0)?,
            parameters: signature
                .parameters
                .into_iter()
                .map(|p| {
                    Ok(Parameter {
                        name: self.string(p.name)?,
                        ty: self.ty(p.ty, 0)?,
                        mode: p.mode,
                        default: p.default.map(|id| self.string(id)).transpose()?,
                        variadic: p.variadic,
                        receiver: p.receiver,
                    })
                })
                .collect::<Result<_>>()?,
            generics: signature
                .generics
                .into_iter()
                .map(|g| {
                    Ok(GenericParameter {
                        name: self.string(g.name)?,
                        variance: g.variance,
                        constraints: g
                            .constraints
                            .into_iter()
                            .map(|id| self.ty(id, 0))
                            .collect::<Result<_>>()?,
                        special_constraints: self.strings(&g.special)?,
                    })
                })
                .collect::<Result<_>>()?,
            bases: signature
                .bases
                .into_iter()
                .map(|id| self.ty(id, 0))
                .collect::<Result<_>>()?,
            explicit_interface: signature.explicit.map(|id| self.ty(id, 0)).transpose()?,
            implementations: signature
                .implementations
                .into_iter()
                .map(|m| {
                    Ok(WrittenMember {
                        owner: self.ty(m.owner, 0)?,
                        name: self.string(m.name)?,
                        parameters: m
                            .parameters
                            .into_iter()
                            .map(|id| self.ty(id, 0))
                            .collect::<Result<_>>()?,
                        generic_arity: m.arity,
                    })
                })
                .collect::<Result<_>>()?,
            constant: signature.constant,
            accessors: signature
                .accessors
                .into_iter()
                .map(|a| {
                    Ok(Accessor {
                        role: self.string(a.role)?,
                        access: self.string(a.access)?,
                        metadata_method: a.method,
                    })
                })
                .collect::<Result<_>>()?,
        };
        Ok((declaration, header))
    }
    pub fn all(&mut self) -> Result<DeclarationFile> {
        if !self.directory.csharp {
            return Ok(DeclarationFile {
                declarations: self.declarations()?,
                headers: Vec::new(),
                imports: Vec::new(),
            });
        }
        let mut result = DeclarationFile {
            declarations: Vec::new(),
            headers: Vec::new(),
            imports: Vec::new(),
        };
        for id in 0..self.directory.count {
            let (declaration, header) = self.declaration(id)?;
            result.declarations.push(declaration);
            if self.directory.csharp {
                result.headers.push(header);
            }
        }
        if self.directory.csharp {
            result.imports = crate::binary::decode(
                (self.read)(super::payload::IMPORTS)?.context("Missing C# imports")?,
            )?;
        }
        Ok(result)
    }
}

macro_rules! record {
    ($($item:item)*) => {$ (#[derive(Clone, Debug, Serialize, Deserialize)] $item)*};
}
record! {
    pub struct Row {
        pub text: [u32; 7],
        pub spans: [u32; 8],
        pub parameters: Vec<u32>,
        pub attributes: Vec<u32>,
        pub modifiers: Vec<u32>,
        pub bases: Vec<u32>,
        pub local: bool,
        pub declaration: u32,
        pub owner: Option<u32>,
        pub signature: u32,
    }
    pub enum TypeNode {
        MetadataParameter { method: bool, ordinal: u32 },
        External { assembly: u32, ty: u32 },
        Name { alias: Option<u32>, parts: Vec<(u32, Vec<u32>)> },
        Parameter { context: u32, key: u32, ordinal: u32 },
        Array(u32, u32), Pointer(u32), Nullable(u32),
        Tuple(Vec<(u32, Option<u32>)>), Dynamic, Inferred, Unsupported(u32),
    }
    pub struct Param { pub name: u32, pub ty: u32, pub mode: PassingMode, pub default: Option<u32>, pub variadic: bool, pub receiver: bool }
    pub struct Generic { pub name: u32, pub variance: Variance, pub constraints: Vec<u32>, pub special: Vec<u32> }
    pub struct Member { pub owner: u32, pub name: u32, pub parameters: Vec<u32>, pub arity: u32 }
    pub struct Access { pub role: u32, pub access: u32, pub method: Option<u32> }
    pub struct Signature {
        pub ty: u32, pub parameters: Vec<Param>, pub generics: Vec<Generic>, pub bases: Vec<u32>,
        pub explicit: Option<u32>, pub implementations: Vec<Member>, pub constant: Option<Constant>, pub accessors: Vec<Access>,
    }
}

pub(crate) enum Shared {
    Row(Row),
    Text(String),
    Type(TypeNode),
    Signature(Signature),
}
fn type_references(
    node: &TypeNode,
    mut text: impl FnMut(u32) -> Result<()>,
    mut ty: impl FnMut(u32) -> Result<()>,
) -> Result<()> {
    match node {
        TypeNode::External {
            assembly,
            ty: child,
        } => {
            text(*assembly)?;
            ty(*child)?;
        }
        TypeNode::Name { alias, parts } => {
            if let Some(id) = alias {
                text(*id)?;
            }
            for (name, args) in parts {
                text(*name)?;
                for &id in args {
                    ty(id)?;
                }
            }
        }
        TypeNode::Parameter { context, key, .. } => {
            text(*context)?;
            text(*key)?;
        }
        TypeNode::Array(child, _) | TypeNode::Pointer(child) | TypeNode::Nullable(child) => {
            ty(*child)?
        }
        TypeNode::Tuple(parts) => {
            for (child, name) in parts {
                ty(*child)?;
                if let Some(id) = name {
                    text(*id)?;
                }
            }
        }
        TypeNode::Unsupported(id) => text(*id)?,
        TypeNode::Dynamic | TypeNode::Inferred | TypeNode::MetadataParameter { .. } => {}
    }
    Ok(())
}
trait Record: serde::de::DeserializeOwned + Clone {
    fn get(value: &Shared) -> Option<&Self>;
    fn shared(self) -> Shared;
    fn bytes(&self) -> usize;
}
macro_rules! shared_record {
    ($ty:ty, $variant:ident, $value:ident, $size:expr) => {
        impl Record for $ty {
            fn get(value: &Shared) -> Option<&Self> {
                if let Shared::$variant(value) = value {
                    Some(value)
                } else {
                    None
                }
            }
            fn shared(self) -> Shared {
                Shared::$variant(self)
            }
            fn bytes(&self) -> usize {
                let $value = self;
                $size
            }
        }
    };
}
shared_record!(String, Text, value, value.capacity());
shared_record!(
    Row,
    Row,
    value,
    [
        &value.parameters,
        &value.attributes,
        &value.modifiers,
        &value.bases
    ]
    .into_iter()
    .map(|v| vector_bytes(v, |_| 0))
    .sum()
);
shared_record!(
    TypeNode,
    Type,
    value,
    match value {
        TypeNode::Name { parts, .. } => vector_bytes(parts, |(_, args)| vector_bytes(args, |_| 0)),
        TypeNode::Tuple(parts) => vector_bytes(parts, |_| 0),
        _ => 0,
    }
);
shared_record!(
    Signature,
    Signature,
    value,
    vector_bytes(&value.parameters, |_| 0)
        + vector_bytes(&value.generics, |g| vector_bytes(&g.constraints, |_| 0)
            + vector_bytes(&g.special, |_| 0))
        + vector_bytes(&value.bases, |_| 0)
        + vector_bytes(&value.implementations, |m| vector_bytes(
            &m.parameters,
            |_| 0
        ))
        + vector_bytes(&value.accessors, |_| 0)
);

struct Pool {
    pub strings: Vec<String>,
    pub types: Vec<TypeNode>,
    pub signatures: Vec<Signature>,
    strings_map: HashMap<String, u32>,
    types_map: HashMap<WrittenType, u32>,
    signatures_map: HashMap<Vec<u8>, u32>,
}
impl Pool {
    pub fn new() -> Self {
        Self {
            strings: Vec::new(),
            types: Vec::new(),
            signatures: Vec::new(),
            strings_map: HashMap::new(),
            types_map: HashMap::new(),
            signatures_map: HashMap::new(),
        }
    }
    fn string(&mut self, s: &str) -> u32 {
        if let Some(id) = self.strings_map.get(s) {
            return *id;
        }
        let id = u32::try_from(self.strings.len()).unwrap();
        self.strings.push(s.into());
        self.strings_map.insert(s.into(), id);
        id
    }
    fn strings(&mut self, s: &[String]) -> Vec<u32> {
        s.iter().map(|s| self.string(s)).collect()
    }
    fn ty(&mut self, ty: &WrittenType) -> u32 {
        if let Some(id) = self.types_map.get(ty) {
            return *id;
        }
        let node = match ty {
            WrittenType::MetadataParameter { method, ordinal } => TypeNode::MetadataParameter {
                method: *method,
                ordinal: *ordinal,
            },
            WrittenType::External { assembly, ty } => TypeNode::External {
                assembly: self.string(assembly),
                ty: self.ty(ty),
            },
            WrittenType::Name { alias, parts } => TypeNode::Name {
                alias: alias.as_ref().map(|s| self.string(s)),
                parts: parts
                    .iter()
                    .map(|p| {
                        (
                            self.string(&p.name),
                            p.arguments.iter().map(|t| self.ty(t)).collect(),
                        )
                    })
                    .collect(),
            },
            WrittenType::Parameter(p) => TypeNode::Parameter {
                context: self.string(&p.owner.context),
                key: self.string(&p.owner.key),
                ordinal: p.ordinal,
            },
            WrittenType::Array(t, n) => TypeNode::Array(self.ty(t), *n),
            WrittenType::Pointer(t) => TypeNode::Pointer(self.ty(t)),
            WrittenType::Nullable(t) => TypeNode::Nullable(self.ty(t)),
            WrittenType::Tuple(v) => TypeNode::Tuple(
                v.iter()
                    .map(|(t, n)| (self.ty(t), n.as_ref().map(|n| self.string(n))))
                    .collect(),
            ),
            WrittenType::Dynamic => TypeNode::Dynamic,
            WrittenType::Inferred => TypeNode::Inferred,
            WrittenType::Unsupported(s) => TypeNode::Unsupported(self.string(s)),
        };
        let id = u32::try_from(self.types.len()).unwrap();
        self.types.push(node);
        self.types_map.insert(ty.clone(), id);
        id
    }
    pub fn row(&mut self, d: &Declaration, h: &Header) -> Result<Row> {
        let signature = Signature {
            ty: self.ty(&h.ty),
            parameters: h
                .parameters
                .iter()
                .map(|p| Param {
                    name: self.string(&p.name),
                    ty: self.ty(&p.ty),
                    mode: p.mode,
                    default: p.default.as_ref().map(|s| self.string(s)),
                    variadic: p.variadic,
                    receiver: p.receiver,
                })
                .collect(),
            generics: h
                .generics
                .iter()
                .map(|g| Generic {
                    name: self.string(&g.name),
                    variance: g.variance,
                    constraints: g.constraints.iter().map(|t| self.ty(t)).collect(),
                    special: self.strings(&g.special_constraints),
                })
                .collect(),
            bases: h.bases.iter().map(|t| self.ty(t)).collect(),
            explicit: h.explicit_interface.as_ref().map(|t| self.ty(t)),
            implementations: h
                .implementations
                .iter()
                .map(|m| Member {
                    owner: self.ty(&m.owner),
                    name: self.string(&m.name),
                    parameters: m.parameters.iter().map(|t| self.ty(t)).collect(),
                    arity: m.generic_arity,
                })
                .collect(),
            constant: h.constant.clone(),
            accessors: h
                .accessors
                .iter()
                .map(|a| Access {
                    role: self.string(&a.role),
                    access: self.string(&a.access),
                    method: a.metadata_method,
                })
                .collect(),
        };
        let bytes = encode(&signature)?;
        let signature = if self.signatures_map.contains_key(&bytes) {
            self.signatures_map[&bytes]
        } else {
            let id = u32::try_from(self.signatures.len())?;
            self.signatures.push(signature);
            self.signatures_map.insert(bytes, id);
            id
        };
        let spans = [
            d.name_span.start,
            d.name_span.end,
            d.span.start,
            d.span.end,
            d.header.start,
            d.header.end,
            d.scope.start,
            d.scope.end,
        ]
        .map(|n| u32::try_from(n).expect("source exceeds 32-bit bounds"));
        Ok(Row {
            text: [
                &d.name,
                &d.qualified,
                &d.kind,
                &d.namespace,
                &d.owner,
                &d.ty,
                &d.access,
            ]
            .map(|s| self.string(s)),
            spans,
            parameters: self.strings(&d.parameters),
            attributes: self.strings(&d.attributes),
            modifiers: self.strings(&d.modifiers),
            bases: self.strings(&d.bases),
            local: h.local,
            declaration: h.declaration,
            owner: h.owner,
            signature,
        })
    }
}
