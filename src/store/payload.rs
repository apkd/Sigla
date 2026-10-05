//! Sigla's complete split analysis representation, encoded once per input/profile.
use super::{
    FileData, MAX_SOURCE_BYTES,
    shared::{Encoded, Names, ObjectId},
};
use crate::model::{Facts, Language};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{BufWriter, Read, Write},
};

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
pub const ANALYSIS_VERSION: u32 = 4; // Bump for extractor, binder, profile, or record changes.

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
    CSharp(Vec<String>),
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
        Language::CSharp => Profile::CSharp(canonical_defines(defines)),
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
fn compressed(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)?;
    {
        let mut writer = BufWriter::with_capacity(64 * 1024, &mut encoder);
        postcard::to_io(value, &mut writer)?;
        writer.flush()?;
    }
    Ok(encoder.finish()?)
}

#[cfg(test)]
#[test]
fn streaming_preserves_postcard_across_buffer_boundaries() {
    for length in [0, 17, 200_000] {
        let value = (
            "source",
            (0..length).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
        );
        let encoded = compressed(&value).unwrap();
        assert_eq!(
            zstd::stream::decode_all(encoded.as_slice()).unwrap(),
            postcard::to_allocvec(&value).unwrap()
        );
        let (decoded, _): ((String, Vec<u8>), _) = decode(&encoded, 1).unwrap();
        assert_eq!(decoded.1, value.1);
    }
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8], multiplier: usize) -> Result<(T, usize)> {
    let bound = MAX_SOURCE_BYTES
        .checked_mul(multiplier)
        .context("Decode bound overflow")?;
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(bytes)?
        .take((bound + 1) as u64)
        .read_to_end(&mut decoded)?;
    ensure!(
        decoded.len() <= bound,
        "Analysis record exceeds safety bound"
    );
    Ok((
        postcard::from_bytes(&decoded).context("Invalid encoded analysis")?,
        decoded.len(),
    ))
}

pub fn encode(mut data: FileData) -> Result<Encoded> {
    validate(&data)?;
    let mut records = BTreeMap::new();
    records.insert(DATA.to_vec(), compressed(&data)?); // Facts.csharp is stored separately below.
    records.insert(SUMMARY.to_vec(), compressed(&data.facts.declarations)?);
    records.insert(ENVIRONMENT.to_vec(), declaration_revision(&data)?.to_vec());
    records.insert(
        MODULES.to_vec(),
        postcard::to_allocvec(&data.facts.modules)?,
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
        let globals: Vec<_> = syntax.imports.iter().filter(|i| i.global).collect();
        names.global_imports = !globals.is_empty();
        records.insert(GLOBALS.to_vec(), postcard::to_allocvec(&globals)?);
        records.insert(
            FORWARDERS.to_vec(),
            postcard::to_allocvec(&syntax.forwarders)?,
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
                    .map(|i| &data.facts.declarations[*i])
                    .collect();
                let headers: Vec<_> = indices.iter().map(|i| &syntax.headers[*i]).collect();
                records.insert(
                    member_key(name),
                    compressed(&(declarations, headers, &syntax.imports))?,
                );
            }
        } else {
            records.insert(
                HEADERS.to_vec(),
                compressed(&(&data.facts.declarations, &syntax.headers, &syntax.imports))?,
            );
            let groups = crate::csharp::syntax::split_bodies(
                syntax,
                &data.facts.declarations,
                data.source.len(),
            )?;
            let mut index = Vec::new();
            for (ordinal, (range, body)) in groups.into_iter().enumerate() {
                let ordinal: u32 = ordinal.try_into()?;
                records.insert(body_key(ordinal), compressed(&body)?);
                index.push((range, ordinal));
            }
            records.insert(BODY_INDEX.to_vec(), postcard::to_allocvec(&index)?);
        }
    }
    if let Some(assembly) = &data.assembly {
        records.insert(ASSEMBLY.to_vec(), assembly.as_bytes().to_vec());
    }
    Ok(Encoded { names, records })
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
