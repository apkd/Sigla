//! Sigla's complete split analysis representation, encoded once per input/profile.
use super::{
    FileData, MAX_SOURCE_BYTES,
    shared::{Encoded, Names, ObjectId},
};
use crate::model::{Facts, Language};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const DATA: &[u8] = &[1];
pub const SUMMARY: &[u8] = &[2];
pub const HEADERS: &[u8] = &[3];
pub const BODY_INDEX: &[u8] = &[4];
pub const GLOBALS: &[u8] = &[5];
pub const FORWARDERS: &[u8] = &[6];
pub const ASSEMBLY: &[u8] = &[7];
pub const ENVIRONMENT: &[u8] = &[8];
pub const MODULES: &[u8] = &[9];
const BODY: u8 = 10;
const MEMBER: u8 = 11;
const DECLARATION_NAME: u8 = 12;
pub const ANALYSIS_VERSION: &str = env!("SIGLA_ANALYSIS_FINGERPRINT");

pub fn body_key(index: u32) -> Vec<u8> {
    let mut key = vec![BODY];
    key.extend_from_slice(&index.to_be_bytes());
    key
}
pub fn member_key(name: &str) -> Vec<u8> {
    // 32-byte object ID + tag + 480-byte name would exceed LMDB's default key limit.
    let mut key = vec![MEMBER];
    key.extend_from_slice(blake3::hash(name.as_bytes()).as_bytes());
    key
}
pub fn declaration_key(index: u32) -> Vec<u8> {
    let mut key = vec![DECLARATION_NAME];
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
pub fn decode<T: rkyv::Archive>(bytes: &[u8], multiplier: usize) -> Result<(T, usize)>
where
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>>
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<rkyv::rancor::Error>>,
{
    let bound = MAX_SOURCE_BYTES
        .checked_mul(multiplier)
        .context("Decode bound overflow")?;
    ensure!(bytes.len() <= bound, "Analysis record exceeds safety bound");
    Ok((crate::binary::decode(bytes)?, bytes.len()))
}

pub fn encode(mut data: FileData) -> Result<Encoded> {
    validate(&data)?;
    let mut records = BTreeMap::new();
    records.insert(DATA.to_vec(), crate::binary::encode(&data)?);
    records.insert(
        SUMMARY.to_vec(),
        crate::binary::encode(&data.facts.declarations)?,
    );
    records.insert(ENVIRONMENT.to_vec(), declaration_revision(&data)?.to_vec());
    records.insert(
        MODULES.to_vec(),
        crate::binary::encode(&data.facts.modules)?,
    );
    let eligible = |name: &&str| !name.is_empty() && name.len() <= 480;
    let mut names = Names {
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
        global_imports: false,
    };
    if let Some(syntax) = data.facts.csharp.take() {
        let globals: Vec<_> = syntax
            .imports
            .iter()
            .filter(|i| i.global)
            .cloned()
            .collect();
        names.global_imports = !globals.is_empty();
        records.insert(GLOBALS.to_vec(), crate::binary::encode(&globals)?);
        records.insert(
            FORWARDERS.to_vec(),
            crate::binary::encode(&syntax.forwarders)?,
        );
        if data.assembly.is_some() {
            // Exactly the previous metadata grouping; original declaration indices survive.
            let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
            for (index, declaration) in data.facts.declarations.iter().enumerate() {
                ensure!(
                    index < syntax.headers.len(),
                    "Missing metadata declaration header"
                );
                groups.entry(&declaration.name).or_default().push(index);
                records.insert(
                    declaration_key(index.try_into()?),
                    declaration.name.as_bytes().to_vec(),
                );
            }
            for (name, indices) in groups {
                let declarations: Vec<_> = indices
                    .iter()
                    .map(|i| data.facts.declarations[*i].clone())
                    .collect();
                let headers: Vec<_> = indices.iter().map(|i| syntax.headers[*i].clone()).collect();
                records.insert(
                    member_key(name),
                    crate::binary::encode(&crate::csharp::syntax::DeclarationFile {
                        declarations,
                        headers,
                        imports: syntax.imports.clone(),
                    })?,
                );
            }
        } else {
            records.insert(
                HEADERS.to_vec(),
                crate::binary::encode(&crate::csharp::syntax::DeclarationFile {
                    declarations: data.facts.declarations.clone(),
                    headers: syntax.headers.clone(),
                    imports: syntax.imports.clone(),
                })?,
            );
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
    Ok(Encoded {
        names: crate::binary::encode(&names)?,
        records,
    })
}

// Preserve the existing C# binding-generation semantics. This is NOT ObjectId.
fn declaration_revision(data: &FileData) -> Result<[u8; 32]> {
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

pub fn validate_archived(data: &super::ArchivedFileData) -> Result<()> {
    let source = data.source.as_str();
    let valid = |r: &rkyv::Archived<std::ops::Range<usize>>| {
        let start = r.start.to_native() as usize;
        let end = r.end.to_native() as usize;
        start <= end
            && end <= source.len()
            && source.is_char_boundary(start)
            && source.is_char_boundary(end)
    };
    for d in data.facts.declarations.iter() {
        ensure!(
            valid(&d.span) && valid(&d.name_span) && valid(&d.header) && valid(&d.scope),
            "Invalid declaration span"
        );
    }
    for o in data.facts.occurrences.iter() {
        ensure!(valid(&o.span), "Invalid occurrence span");
    }
    if let Some(native) = data.facts.native.as_ref() {
        ensure!(
            native.declarations.len() == data.facts.declarations.len()
                && native.occurrences.len() == data.facts.occurrences.len(),
            "Inconsistent native ordinals"
        );
        let declaration =
            |id: &rkyv::Archived<u32>| (id.to_native() as usize) < native.declarations.len();
        for region in native.regions.iter() {
            ensure!(valid(&region.span), "Invalid native region");
        }
        for info in native.declarations.iter() {
            ensure!(
                (info.region.to_native() as usize) < native.regions.len()
                    && info.parent.as_ref().is_none_or(declaration),
                "Invalid declaration owner"
            );
        }
        for info in native.occurrences.iter() {
            ensure!(
                (info.region.to_native() as usize) < native.regions.len()
                    && info.owner.as_ref().is_none_or(declaration)
                    && info.local.as_ref().is_none_or(declaration)
                    && info.receiver.as_ref().is_none_or(&valid)
                    && info.assignment.as_ref().is_none_or(&valid),
                "Invalid occurrence details"
            );
        }
        for include in native.includes.iter() {
            ensure!(valid(&include.span), "Invalid include span");
        }
        for (name, indices) in native.names.iter() {
            ensure!(
                indices.iter().all(|i| data
                    .facts
                    .occurrences
                    .get(i.to_native() as usize)
                    .is_some_and(|o| o.name.as_str() == name.as_str())),
                "Invalid native name lookup"
            );
        }
        for (owner, indices) in native.calls.iter() {
            ensure!(
                declaration(owner)
                    && indices.iter().all(|i| {
                        let i = i.to_native() as usize;
                        native
                            .occurrences
                            .get(i)
                            .is_some_and(|o| o.owner.as_ref() == Some(owner))
                            && data.facts.occurrences[i].call
                    }),
                "Invalid native call lookup"
            );
        }
    }
    Ok(())
}
