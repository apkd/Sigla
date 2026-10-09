//! Sigla's complete split analysis representation, encoded once per input/profile.
use super::{
    FileData, MAX_SOURCE_BYTES,
    shared::{Encoded, Names, ObjectId},
};
use crate::{
    binary,
    model::{Facts, Language},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const DATA: &[u8] = &[1];
pub const HEADERS: &[u8] = &[3];
pub const BODY_INDEX: &[u8] = &[4];
pub const FORWARDERS: &[u8] = &[6];
pub const ASSEMBLY: &[u8] = &[7];
pub const ENVIRONMENT: &[u8] = &[8];
pub const MODULES: &[u8] = &[9];
const BODY: u8 = 10;
const DECLARATION: u8 = 12;
pub const IMPORTS: &[u8] = &[13];
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeclarationLookup {
    Name,
    Type,
    Extension,
    Members(u32),
}
pub const ANALYSIS_VERSION: &str = env!("SIGLA_ANALYSIS_FINGERPRINT");

pub fn body_key(index: u32) -> Vec<u8> {
    let mut key = vec![BODY];
    key.extend_from_slice(&index.to_be_bytes());
    key
}
pub(crate) fn lookup_key(kind: DeclarationLookup, name: &str) -> Vec<u8> {
    // 32-byte object ID + tag + 480-byte name would exceed LMDB's default key limit.
    let mut key = match kind {
        DeclarationLookup::Name => vec![14],
        DeclarationLookup::Type => vec![15],
        DeclarationLookup::Extension => vec![16],
        DeclarationLookup::Members(owner) => {
            let mut key = vec![17];
            key.extend_from_slice(&owner.to_be_bytes());
            key
        }
    };
    key.extend_from_slice(blake3::hash(name.as_bytes()).as_bytes());
    key
}
pub fn declaration_key(index: u32) -> Vec<u8> {
    let mut key = vec![DECLARATION];
    key.extend_from_slice(&index.to_be_bytes());
    key
}
pub fn canonical_defines(defines: &[String]) -> Vec<String> {
    let mut defines = defines.to_vec();
    defines.sort();
    defines.dedup();
    defines
}
#[derive(Serialize)]
enum Profile<'a> {
    CSharp([u8; 32]),
    Rust(&'a str),
    Document,
    Native,
}
pub fn source_id(
    source: &str,
    language: Language,
    defines: &[String],
    edition: &str,
) -> Result<ObjectId> {
    let profile = match language {
        Language::CSharp => {
            let active = if source.contains('#') {
                std::borrow::Cow::Owned(crate::extract::preprocess::active_source(source, defines)?)
            } else {
                std::borrow::Cow::Borrowed(source)
            };
            Profile::CSharp(*blake3::hash(active.as_bytes()).as_bytes())
        }
        Language::Rust => Profile::Rust(edition),
        Language::Markdown | Language::Text => Profile::Document,
        _ => Profile::Native,
    };
    Ok(*blake3::hash(&postcard::to_allocvec(&(
        "sigla-source-analysis",
        ANALYSIS_VERSION,
        language,
        profile,
        blake3::hash(source.as_bytes()).as_bytes(),
    ))?)
    .as_bytes())
}
pub fn metadata_id(bytes: &[u8], fallback_stem: &str) -> Result<ObjectId> {
    Ok(*blake3::hash(&postcard::to_allocvec(&(
        "sigla-metadata-analysis",
        ANALYSIS_VERSION,
        fallback_stem,
        blake3::hash(bytes).as_bytes(),
    ))?)
    .as_bytes())
}
pub fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    multiplier: usize,
) -> Result<(T, usize)> {
    let bound = MAX_SOURCE_BYTES
        .checked_mul(multiplier)
        .context("Decode bound overflow")?;
    ensure!(bytes.len() <= bound, "Analysis record exceeds safety bound");
    crate::binary::decode_with_size(bytes)
}

pub fn encode(mut data: FileData) -> Result<Encoded> {
    validate(&data)?;
    let mut records = BTreeMap::new();
    records.insert(ENVIRONMENT.to_vec(), declaration_revision(&data)?.to_vec());
    records.insert(
        MODULES.to_vec(),
        crate::binary::encode(&std::mem::take(&mut data.facts.modules))?,
    );
    let names = names(&data);
    let encoded_names = super::format::write(
        &data.facts.declarations,
        data.facts
            .csharp
            .as_ref()
            .map(|syntax| syntax.headers.as_slice()),
        data.assembly.is_none(),
        &names,
        &mut records,
    )?;
    if let Some(syntax) = data.facts.csharp.take() {
        records.insert(
            FORWARDERS.to_vec(),
            crate::binary::encode(&syntax.forwarders)?,
        );
        records.insert(IMPORTS.to_vec(), crate::binary::encode(&syntax.imports)?);
        let mut groups: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
        for (index, declaration) in data.facts.declarations.iter().enumerate() {
            let header = syntax
                .headers
                .get(index)
                .context("Missing C# declaration header")?;
            let index: u32 = index.try_into()?;
            if header.local {
                continue;
            }
            let mut add = |kind| {
                groups
                    .entry(lookup_key(kind, &declaration.name))
                    .or_default()
                    .push(index)
            };
            add(DeclarationLookup::Name);
            if declaration.named_type() {
                add(DeclarationLookup::Type);
            }
            if header.parameters.first().is_some_and(|p| p.receiver) {
                add(DeclarationLookup::Extension);
            }
            if let Some(owner) = header.owner {
                add(DeclarationLookup::Members(owner));
            }
        }
        for (key, indices) in groups {
            records.insert(key, crate::binary::encode(&indices)?);
        }
        if data.assembly.is_none() {
            let groups = crate::csharp::syntax::split_bodies(
                syntax,
                &data.facts.declarations,
                data.source.len(),
            )?;
            let mut index = Vec::new();
            for (ordinal, (range, body)) in groups.into_iter().enumerate() {
                let ordinal: u32 = ordinal.try_into()?;
                records.insert(body_key(ordinal), crate::binary::encode(&body)?);
                index.push((range, ordinal));
            }
            records.insert(BODY_INDEX.to_vec(), crate::binary::encode(&index)?);
        }
    }
    if let Some(assembly) = &data.assembly {
        records.insert(ASSEMBLY.to_vec(), assembly.as_bytes().to_vec());
    }
    data.facts.declarations.clear();
    data.assembly = None;
    records.insert(DATA.to_vec(), crate::binary::encode(&data)?);
    Ok(Encoded {
        names: encoded_names,
        records,
    })
}

// Preserve the existing C# binding-generation semantics. This is NOT ObjectId.
pub(crate) fn declaration_revision(data: &FileData) -> Result<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    if let Some(syntax) = &data.facts.csharp {
        for header in &syntax.headers {
            let declaration = data
                .facts
                .declarations
                .get(header.declaration as usize)
                .context("Header points outside declaration table")?;
            if header.local {
                continue;
            }
            hash.update(&postcard::to_allocvec(&(
                &declaration.name,
                &declaration.qualified,
                &declaration.owner,
                &declaration.kind,
                &declaration.access,
                &declaration.modifiers,
                &declaration.attributes,
                &header.ty,
                &header.parameters,
                &header.generics,
                &header.bases,
                &header.explicit_interface,
                &header.implementations,
                &header.constant,
                &header.accessors,
            ))?);
            if declaration.kind == "const" {
                hash.update(data.source[declaration.header.clone()].as_bytes());
            }
        }
        for import in &syntax.imports {
            let namespace = data
                .facts
                .declarations
                .iter()
                .filter(|d| {
                    d.kind == "namespace"
                        && d.span.start <= import.scope.start
                        && d.span.end >= import.scope.end
                })
                .min_by_key(|d| d.span.len())
                .map(|d| d.qualified.as_str());
            hash.update(&postcard::to_allocvec(&(
                &import.kind,
                &import.ty,
                import.global,
                namespace,
            ))?);
        }
        if data.facts.errors {
            hash.update(data.source.as_bytes());
        }
    } else {
        hash.update(data.source.as_bytes());
    }
    Ok(*hash.finalize().as_bytes())
}
fn relationship_names(facts: &Facts) -> BTreeSet<String> {
    let simple = |name: &str| {
        name.split('<')
            .next()
            .unwrap_or(name)
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(name)
            .split('`')
            .next()
            .unwrap_or(name)
            .to_owned()
    };
    facts
        .declarations
        .iter()
        .flat_map(|d| &d.bases)
        .map(|base| format!("@base:{}", simple(base)))
        .chain(
            facts
                .imports
                .iter()
                .filter(|i| !i.alias.is_empty())
                .map(|i| format!("@alias:{}", simple(&i.path))),
        )
        .filter(|name| name.len() <= 480)
        .collect()
}
pub fn validate(data: &FileData) -> Result<()> {
    let valid = |r: &std::ops::Range<usize>| {
        r.start <= r.end
            && r.end <= data.source.len()
            && data.source.is_char_boundary(r.start)
            && data.source.is_char_boundary(r.end)
    };
    for d in &data.facts.declarations {
        ensure!(
            valid(&d.span) && valid(&d.name_span) && valid(&d.header) && valid(&d.scope),
            "Invalid declaration span in analysis object"
        );
    }
    for o in &data.facts.occurrences {
        ensure!(valid(&o.span), "Invalid occurrence span in analysis object");
    }
    ensure!(
        data.facts.imports.iter().all(|import| valid(&import.scope))
            && data
                .facts
                .csharp
                .as_ref()
                .is_none_or(|syntax| syntax.imports.iter().all(|import| valid(&import.scope))),
        "Invalid import scope in analysis object"
    );
    if let Some(native) = &data.facts.native {
        ensure!(
            native.declarations.len() == data.facts.declarations.len()
                && native.occurrences.len() == data.facts.occurrences.len(),
            "Native facts have inconsistent ordinals"
        );
        let declaration = |id: u32| (id as usize) < native.declarations.len();
        for region in &native.regions {
            ensure!(valid(&region.span), "Invalid native region span");
        }
        for info in &native.declarations {
            ensure!(
                (info.region as usize) < native.regions.len()
                    && info.parent.is_none_or(declaration),
                "Invalid native declaration owner"
            );
        }
        for info in &native.occurrences {
            ensure!(
                (info.region as usize) < native.regions.len()
                    && info.owner.is_none_or(declaration)
                    && info.local.is_none_or(declaration)
                    && info.receiver.as_ref().is_none_or(&valid)
                    && info.assignment.as_ref().is_none_or(&valid),
                "Invalid native occurrence details"
            );
        }
        for include in &native.includes {
            ensure!(valid(&include.span), "Invalid native include span");
        }
        for (name, indices) in &native.names {
            ensure!(
                indices.iter().all(|&i| data
                    .facts
                    .occurrences
                    .get(i as usize)
                    .is_some_and(|o| &o.name == name)),
                "Invalid native name lookup"
            );
        }
        for (&owner, indices) in &native.calls {
            ensure!(
                declaration(owner)
                    && indices.iter().all(|&i| native
                        .occurrences
                        .get(i as usize)
                        .is_some_and(|o| o.owner == Some(owner))
                        && data.facts.occurrences[i as usize].call),
                "Invalid native call lookup"
            );
        }
    }
    Ok(())
}

pub(crate) fn names(data: &FileData) -> Names {
    let eligible = |name: &&str| !name.is_empty() && name.len() <= 480;
    Names {
        declarations: data
            .facts
            .declarations
            .iter()
            .map(|d| d.name.as_str())
            .filter(eligible)
            .map(str::to_owned)
            .chain(relationship_names(&data.facts))
            .collect(),
        occurrences: data
            .facts
            .occurrences
            .iter()
            .map(|o| o.name.as_str())
            .chain(data.facts.imports.iter().map(|i| i.alias.as_str()))
            .filter(eligible)
            .map(str::to_owned)
            .collect(),
        global_imports: data
            .facts
            .csharp
            .as_ref()
            .is_some_and(|syntax| syntax.imports.iter().any(|i| i.global)),
    }
}
pub(crate) fn validate_encoded(encoded: &Encoded) -> Result<()> {
    let records = &encoded.records;
    ensure!(
        records
            .get(ENVIRONMENT)
            .is_some_and(|bytes| bytes.len() == 32),
        "Missing declaration fingerprint"
    );
    let mut data = binary::decode::<FileData>(
        records
            .iter()
            .find(|(key, _)| key.as_slice() == DATA)
            .map(|(_, bytes)| bytes.as_slice())
            .context("Missing source facts")?,
    )?;
    let mut reader =
        crate::store::format::Reader::new(|key| Ok(records.get(key).map(Vec::as_slice)))?;
    reader.validate_pages()?;
    let headers = reader.all()?;
    let mut owners = vec![0u8; headers.headers.len()];
    for start in 0..owners.len() {
        let mut current = Some(start);
        while let Some(index) = current {
            if owners[index] == 2 {
                break;
            }
            ensure!(owners[index] == 0, "Cyclic declaration owner");
            owners[index] = 1;
            current = headers.headers[index].owner.map(|owner| owner as usize);
        }
        let mut current = Some(start);
        while let Some(index) = current {
            if owners[index] != 1 {
                break;
            }
            owners[index] = 2;
            current = headers.headers[index].owner.map(|owner| owner as usize);
        }
    }
    let mut lookups: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
    for (declaration, header) in headers.declarations.iter().zip(&headers.headers) {
        if header.local {
            continue;
        }
        let mut add = |kind| {
            lookups
                .entry(lookup_key(kind, &declaration.name))
                .or_default()
                .push(header.declaration)
        };
        add(DeclarationLookup::Name);
        if declaration.named_type() {
            add(DeclarationLookup::Type);
        }
        if header
            .parameters
            .first()
            .is_some_and(|parameter| parameter.receiver)
        {
            add(DeclarationLookup::Extension);
        }
        if let Some(owner) = header.owner {
            add(DeclarationLookup::Members(owner));
        }
    }
    ensure!(
        data.facts.declarations.is_empty()
            && data.facts.modules.is_empty()
            && data.assembly.is_none(),
        "Duplicate analysis records"
    );
    data.facts.declarations = headers.declarations;
    data.facts.modules = binary::decode(records.get(MODULES).context("Missing module record")?)?;
    data.assembly = records
        .get(ASSEMBLY)
        .map(|bytes| std::str::from_utf8(bytes).map(str::to_owned))
        .transpose()?;
    ensure!(
        reader.directory.source == data.assembly.is_none(),
        "Invalid declaration source kind"
    );
    if reader.directory.csharp {
        data.facts.csharp = Some(crate::csharp::syntax::FileSyntax {
            headers: headers.headers,
            imports: headers.imports,
            forwarders: binary::decode(
                records.get(FORWARDERS).context("Missing type forwarders")?,
            )?,
            ..Default::default()
        });
    }
    validate(&data)?;
    ensure!(
        records[ENVIRONMENT] == declaration_revision(&data)?,
        "Invalid declaration fingerprint"
    );
    let valid_span = |span: &std::ops::Range<usize>| {
        span.start <= span.end
            && span.end <= data.source.len()
            && data.source.is_char_boundary(span.start)
            && data.source.is_char_boundary(span.end)
    };
    let mut bodies = BTreeSet::new();
    if reader.directory.csharp && reader.directory.source {
        let index: Vec<(std::ops::Range<usize>, u32)> =
            binary::decode(records.get(BODY_INDEX).context("Missing body index")?)?;
        for (range, id) in index {
            ensure!(
                valid_span(&range) && bodies.insert(body_key(id)),
                "Invalid body index"
            );
        }
    }
    let actual_names = crate::store::format::names(&encoded.names, |key| {
        Ok(records.get(key).map(Vec::as_slice))
    })?;
    let expected_names = names(&data);
    ensure!(
        actual_names.declarations == expected_names.declarations
            && actual_names.occurrences == expected_names.occurrences
            && actual_names.global_imports == expected_names.global_imports,
        "Invalid analysis name index"
    );
    for (key, bytes) in records.iter() {
        ensure!(!key.is_empty() && key.len() <= 479, "Invalid analysis key");
        ensure!(
            match key[0] {
                1 | 4 | 6..=9 | 13 => key.len() == 1,
                10 => key.len() == 5,
                14..=16 => key.len() == 33,
                17 => key.len() == 37,
                18..=22 => reader.contains_key(key),
                _ => false,
            },
            "Invalid analysis key"
        );
        match key[0] {
            1 => {}
            4 => {
                binary::decode::<Vec<(std::ops::Range<usize>, u32)>>(bytes)?;
            }
            13 => {
                binary::decode::<Vec<crate::csharp::syntax::Import>>(bytes)?;
            }
            6 => {
                binary::decode::<Vec<(String, String)>>(bytes)?;
            }
            7 => {
                std::str::from_utf8(bytes)?;
            }
            8 => ensure!(bytes.len() == 32, "Invalid declaration fingerprint"),
            9 => {
                binary::decode::<Vec<crate::model::ModuleFile>>(bytes)?;
            }
            10 => {
                ensure!(bodies.remove(key), "Body is absent from its index");
                let body = binary::decode::<crate::csharp::syntax::BodyFile>(bytes)?;
                ensure!(
                    body.expressions
                        .iter()
                        .all(|expression| valid_span(&expression.span))
                        && body.locals.iter().all(|local| valid_span(&local.span)
                            && valid_span(&local.scope)
                            && local
                                .value
                                .is_none_or(|id| (id as usize) < body.expressions.len())
                            && local
                                .out_argument
                                .is_none_or(|id| (id as usize) < body.expressions.len())),
                    "Invalid body span or reference"
                );
            }
            14..=17 => {
                let indices = binary::decode::<Vec<u32>>(bytes)?;
                ensure!(
                    lookups.remove(key).as_ref() == Some(&indices),
                    "Invalid declaration lookup"
                );
            }
            18..=22 => {}
            _ => anyhow::bail!("Unknown analysis record"),
        }
    }
    ensure!(lookups.is_empty(), "Missing declaration lookup");
    ensure!(bodies.is_empty(), "Missing indexed body");
    Ok(())
}
