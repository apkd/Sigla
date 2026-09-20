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
}
