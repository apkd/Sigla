use super::*;

#[test]
fn shared_records_preserve_declarations_and_headers() {
    let source = "global using System; namespace Example; class Outer<T> where T : class { public T[] Items { get; set; } public U Convert<U>(T value, U fallback = default) where U : struct => fallback; class Inner { public (int left, string right) Pair; } }";
    let facts = crate::extract::extract(source, crate::model::Language::CSharp, &[], "").unwrap();
    let syntax = facts.csharp.as_ref().unwrap();
    let mut records = BTreeMap::new();
    write(
        &facts.declarations,
        Some(&syntax.headers),
        true,
        &super::super::shared::Names::default(),
        &mut records,
    )
    .unwrap();
    records.insert(
        super::super::payload::IMPORTS.to_vec(),
        encode(&syntax.imports).unwrap(),
    );
    let mut reader = Reader::new(|key| Ok(records.get(key).map(Vec::as_slice))).unwrap();
    reader.validate_pages().unwrap();
    let restored = reader.all().unwrap();
    assert_eq!(
        encode(&restored.declarations).unwrap(),
        encode(&facts.declarations).unwrap()
    );
    assert_eq!(
        encode(&restored.headers).unwrap(),
        encode(&syntax.headers).unwrap()
    );
    assert_eq!(
        encode(&restored.imports).unwrap(),
        encode(&syntax.imports).unwrap()
    );
    let mut limited = Reader::new(|key| Ok(records.get(key).map(Vec::as_slice))).unwrap();
    limited.remaining = std::mem::size_of::<Declaration>();
    assert!(limited.declarations().is_err());
}

#[test]
fn preserves_types_at_the_source_lowering_limit() {
    let ty = format!("{}int{}", "Nested<".repeat(80), ">".repeat(80));
    let source = format!("class Example {{ {ty} field; }}");
    let facts = crate::extract::extract(&source, crate::model::Language::CSharp, &[], "").unwrap();
    let syntax = facts.csharp.as_ref().unwrap();
    let bytes = encode(&syntax.headers).unwrap();
    let decoded: Vec<Header> = crate::binary::decode(&bytes).unwrap();
    assert_eq!(encode(&decoded).unwrap(), bytes);
    let encoded = crate::store::payload::encode(crate::store::FileData {
        source,
        facts,
        assembly: None,
    })
    .unwrap();
    crate::store::payload::validate_encoded(&encoded).unwrap();
    let mut reader = Reader::new(|key| Ok(encoded.records.get(key).map(Vec::as_slice))).unwrap();
    assert_eq!(encode(&reader.all().unwrap().headers).unwrap(), bytes);
}

#[test]
fn rejects_invalid_offsets_and_type_graphs() {
    let mut records = BTreeMap::new();
    let types = vec![TypeNode::Dynamic, TypeNode::Pointer(1)];
    let starts = pages(TYPES, &types, &mut records).unwrap();
    let directory = Directory {
        count: 0,
        csharp: true,
        source: true,
        pages: [Vec::new(), Vec::new(), starts, Vec::new()],
    };
    records.insert(vec![DIRECTORY], encode(&directory).unwrap());
    let mut reader = Reader::new(|key| Ok(records.get(key).map(Vec::as_slice))).unwrap();
    assert!(reader.ty(1, 0).is_err());
    assert!(reader.ty(u32::MAX, 0).is_err());
    drop(reader);
    records.insert(
        key(TYPES, 0),
        encode(&Page {
            offsets: vec![0, u32::MAX],
            data: &[0],
        })
        .unwrap(),
    );
    let reader = Reader::new(|key| Ok(records.get(key).map(Vec::as_slice))).unwrap();
    assert!(reader.validate_pages().is_err());
    assert!(reader.item::<TypeNode>(TYPES, 0).is_err());
}

#[test]
fn bounds_expansion_of_shared_type_graphs() {
    let mut records = BTreeMap::new();
    let mut types = vec![TypeNode::Dynamic];
    for id in 0..32 {
        types.push(TypeNode::Tuple(vec![(id, None), (id, None)]));
    }
    let starts = pages(TYPES, &types, &mut records).unwrap();
    records.insert(
        vec![DIRECTORY],
        encode(&Directory {
            count: 0,
            csharp: true,
            source: true,
            pages: [Vec::new(), Vec::new(), starts, Vec::new()],
        })
        .unwrap(),
    );
    let mut reader = Reader::new(|key| Ok(records.get(key).map(Vec::as_slice))).unwrap();
    reader.remaining = 4096;
    assert!(reader.ty(32, 0).is_err());
}

#[test]
fn rejects_cyclic_declaration_owners() {
    let source = "class Example {}";
    let mut facts =
        crate::extract::extract(source, crate::model::Language::CSharp, &[], "").unwrap();
    facts.csharp.as_mut().unwrap().headers[0].owner = Some(0);
    let encoded = crate::store::payload::encode(crate::store::FileData {
        source: source.into(),
        facts,
        assembly: None,
    })
    .unwrap();
    assert!(crate::store::payload::validate_encoded(&encoded).is_err());
}
