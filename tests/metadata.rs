#[test]
fn unpadded_blob_heaps_preserve_members_and_out_of_bounds_heaps_fail() {
    use dotscope::metadata::{cilassemblyview::CilAssemblyView, validation::ValidationConfig};
    use windows_metadata::{FieldAttributes, Type, TypeAttributes, writer::File};

    let mut file = File::new("HeapFixture");
    file.TypeDef(
        "Fixture",
        "Container",
        Default::default(),
        TypeAttributes::Public,
    );
    file.Field(
        "Values",
        &Type::Array(Box::new(Type::I32)),
        FieldAttributes::Public,
    );
    let mut original = file.into_stream();
    // This writer emits WinMD headers; use a CLR version for the assembly fixture.
    let winmd_version = b"WindowsRuntime 1.4";
    let version = original
        .windows(winmd_version.len())
        .position(|bytes| bytes == winmd_version)
        .unwrap();
    original[version..version + winmd_version.len()].fill(0);
    original[version..version + b"v4.0.30319".len()].copy_from_slice(b"v4.0.30319");
    let expected = sigla::metadata::extract_bytes(original.clone(), "HeapFixture").unwrap();
    assert!(!expected.members.is_empty());
    let view =
        CilAssemblyView::from_mem_with_validation(original.clone(), ValidationConfig::disabled())
            .unwrap();
    let offset = view
        .file()
        .rva_to_offset(view.cor20header().meta_data_rva as usize)
        .unwrap();
    let mut root = view.metadata_root().clone();
    let blob = root
        .stream_headers
        .iter_mut()
        .find(|stream| stream.name == "#Blob")
        .unwrap();
    // The array field signature leaves padding in the generated blob heap.
    // Drop one padding byte from its declared size, leaving every blob intact.
    assert_eq!(
        original[offset + blob.offset as usize + blob.size as usize - 1],
        0
    );
    blob.size -= 1;
    let mut unpadded = original.clone();
    root.write_to(&mut &mut unpadded[offset..]).unwrap();
    let actual = sigla::metadata::extract_bytes(unpadded, "HeapFixture").unwrap();
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );

    root.stream_headers
        .iter_mut()
        .find(|stream| stream.name == "#Blob")
        .unwrap()
        .size = u32::MAX;
    let mut corrupt = original;
    root.write_to(&mut &mut corrupt[offset..]).unwrap();
    assert!(sigla::metadata::extract_bytes(corrupt, "HeapFixture").is_err());
}

#[test]
#[ignore = "Build tests/metadata-fixture with dotnet first"]
fn metadata_signatures_cover_navigation_cases() {
    let path =
        std::path::Path::new("tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll");
    let facts = sigla::metadata::extract(path).unwrap();
    assert!(facts.members.iter().any(|m| m.name == "GlobalType"));
    let generic = facts.members.iter().find(|m| m.name == "Identity").unwrap();
    assert_eq!(generic.ty, "!!0");
    assert_eq!(generic.parameters, vec!["!!0"]);
    assert!(
        facts
            .members
            .iter()
            .any(|m| m.kind == "property" && m.name == "Property")
    );
    assert!(
        facts
            .forwarders
            .iter()
            .any(|(name, _)| name.starts_with("System.Collections.Generic.List"))
    );
    let matrix = facts.members.iter().find(|m| m.name == "Matrix").unwrap();
    assert!(
        matrix.parameters.iter().any(|p| p.ends_with("[,]")),
        "{matrix:?}"
    );
    assert!(
        facts
            .members
            .iter()
            .any(|m| m.name == "Jagged" && m.parameters.iter().any(|p| p.ends_with("[][]")))
    );
    assert!(
        facts
            .members
            .iter()
            .any(|m| m.name == "Callback" && m.ty.starts_with("delegate*"))
    );
}

#[tokio::test]
#[ignore = "Build tests/metadata-fixture with dotnet first"]
async fn external_generic_return_binds_a_source_receiver() {
    let assembly =
        std::path::Path::new("tests/metadata-fixture/bin/Release/net10.0/MetadataFixture.dll")
            .canonicalize()
            .unwrap();
    let source = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let reference = quick_xml::escape::escape(assembly.to_str().unwrap());
    std::fs::write(source.path().join("Game.csproj"),format!(r#"<Project><ItemGroup><Compile Include="Code.cs"/><Reference Include="Fixture"><HintPath>{reference}</HintPath></Reference></ItemGroup></Project>"#)).unwrap();
    std::fs::write(source.path().join("Code.cs"),"using MetadataFixture;\nclass Player { public void Play() {} }\nclass User { void Run(Generic<Player>.Nested<Player> nested) { var player = GlobalType.Make<Player>();\nplayer.Play();\nnested.Value.Play();\n} }").unwrap();
    let app = std::sync::Arc::new(
        sigla::service::App::new(
            sigla::discovery::Policy::new(vec![
                source.path().into(),
                assembly.parent().unwrap().into(),
            ])
            .unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    let calls = app
        .search(source.path().to_str().unwrap(), "calls:Player.Play")
        .await
        .unwrap();
    assert!(calls.contains("player.Play()"), "{calls}");
    assert!(calls.contains("nested.Value.Play()"), "{calls}");
    assert!(!calls.contains("Possible"), "{calls}");
    // Resolve a metadata target twice to exercise exact declaration restoration
    // from the binding cache as well as the cold member lookup.
    for _ in 0..2 {
        let calls = app
            .search(source.path().to_str().unwrap(), "calls:GlobalType.Make")
            .await
            .unwrap();
        assert!(calls.contains("User.Run"), "{calls}");
        assert!(calls.contains("GlobalType.Make<Player>()"), "{calls}");
        assert!(!calls.contains("Possible"), "{calls}");
    }
}
