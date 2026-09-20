//! Offline compiler comparison; no compiler dependency enters the service.
use super::{
    bind::Binder,
    catalog::View,
    types::{Primitive, Type},
};
use crate::{discovery::Policy, store::Store, workspace::Workspace};
use std::{path::PathBuf, process::Command, sync::Arc};

#[test]
#[ignore = "Requires the .NET SDK and the pinned Roslyn test package"]
fn compiler_agrees_on_source_and_framework_generic_chains() {
    let output = Command::new("dotnet")
        .args([
            "run",
            "--project",
            "tests/csharp-oracle",
            "--verbosity",
            "quiet",
            "--",
            "tests/csharp-fixtures/Chains.cs",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(oracle["errors"].as_array().unwrap().is_empty(), "{oracle}");
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let source = include_str!("../../tests/csharp-fixtures/Chains.cs");
    std::fs::write(directory.path().join("Source.cs"), source).unwrap();
    let references = oracle["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| {
            format!(
                "<Reference Include=\"Framework\"><HintPath>{}</HintPath></Reference>",
                quick_xml::escape::escape(path.as_str().unwrap()),
            )
        })
        .collect::<String>();
    std::fs::write(directory.path().join("Fixture.csproj"), format!(
        "<Project><PropertyGroup><AssemblyName>Fixture</AssemblyName></PropertyGroup><ItemGroup><Compile Include=\"Source.cs\"/>{references}</ItemGroup></Project>",
    )).unwrap();
    let policy = Policy::new(vec![PathBuf::from("/")]).unwrap();
    let assemblies = Arc::new(Store::open(&cache.path().join("assemblies")).unwrap());
    let mut workspace = Workspace::open(
        directory.path().into(),
        cache.path(),
        policy,
        assemblies,
        Arc::new(crate::watch::Monitor::default()),
    )
    .unwrap();
    workspace.refresh().unwrap();
    let source_tx = workspace.store.read().unwrap();
    let assembly_tx = workspace.assemblies.read().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let view = View {
        source: &workspace.store,
        assemblies: &workspace.assemblies,
        source_tx: &source_tx,
        assembly_tx: &assembly_tx,
        manifest: &workspace.manifest,
        cancel: &cancel,
    };
    let (file, entry) = workspace
        .manifest
        .files
        .iter()
        .find(|(_, f)| !f.metadata)
        .unwrap();
    let mut binder = Binder::default();
    for case in oracle["results"].as_array().unwrap() {
        let utf16 = case["start"].as_u64().unwrap() as usize;
        let mut units = 0;
        let position = source
            .char_indices()
            .find(|(_, ch)| {
                let here = units;
                units += ch.len_utf16();
                here == utf16
            })
            .unwrap()
            .0;
        let name = source[position..]
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap();
        let bindings = binder
            .resolve(
                &view,
                file,
                entry.memberships[0].project,
                position,
                false,
                name,
            )
            .unwrap();
        assert_eq!(bindings.len(), 1, "{}", case["label"]);
        let (symbol, uncertain) = &bindings[0];
        assert!(!uncertain, "{}", case["label"]);
        let declaration = symbol.declaration();
        let prefix = match declaration.kind.as_str() {
            "method" => "M",
            "property" => "P",
            other => panic!("Unexpected fixture target {other}"),
        };
        // These marked members have no parameters or generic arity. Their
        // documentation IDs are complete identities, including declaring type.
        assert!(symbol.header().parameters.is_empty() && symbol.header().generics.is_empty());
        assert_eq!(
            format!("{prefix}:{}", declaration.qualified),
            case["symbol"]["definition"].as_str().unwrap(),
            "{}",
            case["label"]
        );
        let assembly = if workspace.manifest.files[&symbol.file].metadata {
            workspace
                .assemblies
                .assembly_name_in(&assembly_tx, &symbol.file)
                .unwrap()
                .unwrap()
        } else {
            workspace.manifest.projects[symbol.project].name.clone()
        };
        assert_eq!(
            assembly,
            case["symbol"]["assembly"]
                .as_str()
                .unwrap()
                .split(',')
                .next()
                .unwrap()
        );
        let actual = match binder.trace_type.as_ref().unwrap() {
            Type::Primitive(primitive) => {
                assert_eq!(
                    Primitive::from_name(case["type"].as_str().unwrap()),
                    Some(*primitive),
                    "{}",
                    case["label"]
                );
                continue;
            }
            Type::Named {
                definition,
                arguments,
                ..
            } => {
                assert!(arguments.is_empty());
                // Resolve the inferred type by identity, rather than trusting its
                // displayed name or the name of the searched member.
                let symbol = binder.definition_symbol(definition).unwrap();
                format!("global::{}", symbol.declaration().qualified)
            }
            other => panic!("Unexpected inferred fixture type: {other:?}"),
        };
        assert_eq!(actual, case["type"].as_str().unwrap(), "{}", case["label"]);
    }
}
