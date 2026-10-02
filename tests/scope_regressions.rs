//! Reproductions distilled from the Highland-Keep Sigla audit.
//! No access to the game repository, Unity installation, or network is required.
use sigla::{discovery::Policy, extract, model::Language, service::App};
use std::sync::Arc;

const SOURCE: &str = r#"
struct Box {
    int raw;
    public static implicit operator int(Box /*operator_decl*/value)
        => /*operator_use*/value.raw;
    public static implicit operator long(Box /*second_operator_decl*/value)
        => /*second_operator_use*/value.raw;
    public int Length {
        get => raw;
        /*setter_decl*/set => raw = /*setter_use*/value;
    }
    public int Pick(object input) => input switch {
        _ when Probe(out char /*char_decl*/value) => Sink(/*char_use*/value),
        _ when Probe(out byte /*byte_decl*/value) => Sink(/*byte_use*/value),
        _ => 0,
    };
    static bool Probe(out char value) { value = default; return true; }
    static bool Probe(out byte value) { value = default; return true; }
    static int Sink(char value) => 1;
    static int Sink(byte value) => 2;
}
"#;

fn offset(source: &str, marker: &str) -> usize {
    assert_eq!(
        source.matches(marker).count(),
        1,
        "ambiguous marker: {marker}"
    );
    source.find(marker).unwrap() + marker.len()
}

fn selector(source: &str, marker: &str) -> String {
    let before = &source[..offset(source, marker)];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    // The MCP uses Unicode columns, not byte offsets.
    let column = before.rsplit('\n').next().unwrap().chars().count() + 1;
    format!("@Test.cs:{line}:{column}")
}

struct Fixture {
    app: Arc<App>,
    root: tempfile::TempDir,
    _cache: tempfile::TempDir,
}
impl Fixture {
    fn new(source: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Test.csproj"),
            "<Project><ItemGroup><Compile Include=\"Test.cs\"/></ItemGroup></Project>",
        )
        .unwrap();
        std::fs::write(root.path().join("Test.cs"), source).unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                2,
            )
            .unwrap(),
        );
        Self {
            app,
            root,
            _cache: cache,
        }
    }
    async fn query(&self, query: &str) -> String {
        self.app
            .search(self.root.path().to_str().unwrap(), query)
            .await
            .unwrap()
    }
}

#[test]
fn extraction_and_lowering_agree_on_switch_arm_scopes() {
    let facts = extract::extract(SOURCE, Language::CSharp, &[], "").unwrap();
    assert!(!facts.errors);
    let syntax = facts.csharp.as_ref().unwrap();
    for (decl, own_use, other_use) in [
        ("/*char_decl*/", "/*char_use*/", "/*byte_use*/"),
        ("/*byte_decl*/", "/*byte_use*/", "/*char_use*/"),
    ] {
        let declaration = facts
            .declarations
            .iter()
            .find(|d| d.name_span.start == offset(SOURCE, decl))
            .unwrap();
        let lowered = syntax
            .locals
            .iter()
            .find(|l| l.span == declaration.name_span)
            .unwrap();
        assert_eq!(declaration.scope, lowered.scope);
        assert!(declaration.scope.contains(&offset(SOURCE, own_use)));
        assert!(!declaration.scope.contains(&offset(SOURCE, other_use)));
        assert!(
            !declaration
                .scope
                .contains(&offset(SOURCE, "/*setter_use*/"))
        );
    }
}

#[test]
fn conversion_parameters_have_operator_owners_not_type_wide_scopes() {
    let facts = extract::extract(SOURCE, Language::CSharp, &[], "").unwrap();
    let syntax = facts.csharp.as_ref().unwrap();
    for (decl, own_use, other_use) in [
        (
            "/*operator_decl*/",
            "/*operator_use*/",
            "/*second_operator_use*/",
        ),
        (
            "/*second_operator_decl*/",
            "/*second_operator_use*/",
            "/*operator_use*/",
        ),
    ] {
        let index = facts
            .declarations
            .iter()
            .position(|d| d.name_span.start == offset(SOURCE, decl))
            .unwrap();
        let parameter = &facts.declarations[index];
        let owner = &facts.declarations[syntax.headers[index].owner.unwrap() as usize];
        assert_eq!(owner.kind, "operator");
        assert_eq!(owner.name, "op_Implicit");
        assert_eq!(parameter.scope, owner.span);
        assert!(parameter.scope.contains(&offset(SOURCE, own_use)));
        for use_site in [other_use, "/*char_use*/", "/*setter_use*/"] {
            assert!(!parameter.scope.contains(&offset(SOURCE, use_site)));
        }
    }
}

#[tokio::test]
async fn each_local_and_operator_use_round_trips_to_its_own_declaration() {
    let fixture = Fixture::new(SOURCE);
    for (decl, use_site) in [
        ("/*operator_decl*/", "/*operator_use*/"),
        ("/*second_operator_decl*/", "/*second_operator_use*/"),
        ("/*char_decl*/", "/*char_use*/"),
        ("/*byte_decl*/", "/*byte_use*/"),
        ("/*setter_decl*/", "/*setter_use*/"),
    ] {
        let expected = fixture.query(&selector(SOURCE, decl)).await;
        let actual = fixture.query(&selector(SOURCE, use_site)).await;
        assert!(!expected.contains("No matches"), "{decl}: {expected}");
        assert!(!expected.contains("could not"), "{decl}: {expected}");
        assert_eq!(actual, expected, "{decl} <- {use_site}");
    }
}

#[tokio::test]
async fn operator_references_exclude_sibling_locals_and_accessors() {
    let fixture = Fixture::new(SOURCE);
    let result = fixture
        .query(&format!(
            "uses:{} path:Test.cs limit:40",
            selector(SOURCE, "/*operator_decl*/")
        ))
        .await;
    assert!(result.contains("/*operator_use*/"), "{result}");
    for forbidden in [
        "/*char_use*/",
        "/*byte_use*/",
        "/*setter_use*/",
        "/*second_operator_use*/",
    ] {
        assert!(!result.contains(forbidden), "{forbidden}: {result}");
    }
}

#[tokio::test]
async fn local_references_are_not_lost_after_inference() {
    let fixture = Fixture::new(SOURCE);
    for (decl, own_use, other_use) in [
        ("/*char_decl*/", "/*char_use*/", "/*byte_use*/"),
        ("/*byte_decl*/", "/*byte_use*/", "/*char_use*/"),
    ] {
        let result = fixture
            .query(&format!(
                "uses:{} path:Test.cs limit:40",
                selector(SOURCE, decl)
            ))
            .await;
        assert!(result.contains(own_use), "{result}");
        assert!(!result.contains(other_use), "{result}");
        assert!(!result.contains("/*operator_use*/"), "{result}");
    }
}

#[tokio::test]
async fn corrected_local_types_select_the_corresponding_overload() {
    let fixture = Fixture::new(SOURCE);
    for (parameter, own_use, other_use) in [
        ("char", "/*char_use*/", "/*byte_use*/"),
        ("byte", "/*byte_use*/", "/*char_use*/"),
    ] {
        let result = fixture
            .query(&format!(
                "calls:Box.Sink({parameter}) path:Test.cs limit:40"
            ))
            .await;
        assert!(result.contains(own_use), "{result}");
        assert!(!result.contains(other_use), "{result}");
        assert!(!result.contains("Possible match"), "{result}");
    }
}

#[tokio::test]
async fn conversions_with_different_destination_types_are_searchable() {
    let fixture = Fixture::new(SOURCE);
    let result = fixture
        .query("operator:op_Implicit path:Test.cs limit:10")
        .await;
    assert!(result.contains("operator int"), "{result}");
    assert!(result.contains("operator long"), "{result}");
}

#[test]
fn lambda_parameters_do_not_leak_into_siblings() {
    let source = r#"
class Lambdas {
    void Run() {
        System.Func<int, int> first = (int /*first_decl*/value) => /*first_use*/value;
        System.Func<int, int> second = (int /*second_decl*/value) => /*second_use*/value;
    }
}
"#;
    let facts = extract::extract(source, Language::CSharp, &[], "").unwrap();
    assert!(!facts.errors);
    for (decl, own, other) in [
        ("/*first_decl*/", "/*first_use*/", "/*second_use*/"),
        ("/*second_decl*/", "/*second_use*/", "/*first_use*/"),
    ] {
        let parameter = facts
            .declarations
            .iter()
            .find(|d| d.name_span.start == offset(source, decl))
            .unwrap();
        assert!(parameter.scope.contains(&offset(source, own)));
        assert!(!parameter.scope.contains(&offset(source, other)));
    }
}

#[test]
fn implicit_accessor_values_are_typed_and_accessor_local() {
    let source = r#"
delegate void Handler();
class Accessors {
    int field;
    public int A { get => field; /*set_decl*/set => field = /*set_use*/value; }
    public int B { get => field; /*init_decl*/init => field = /*init_use*/value; }
    public int this[int index] { get => field; /*index_decl*/set => field = /*index_use*/value; }
    public event Handler Event {
        /*add_decl*/add { var handler = /*add_use*/value; }
        /*remove_decl*/remove { var handler = /*remove_use*/value; }
    }
}
"#;
    let facts = extract::extract(source, Language::CSharp, &[], "").unwrap();
    assert!(!facts.errors);
    let cases = [
        ("/*set_decl*/", "/*set_use*/", "int"),
        ("/*init_decl*/", "/*init_use*/", "int"),
        ("/*index_decl*/", "/*index_use*/", "int"),
        ("/*add_decl*/", "/*add_use*/", "Handler"),
        ("/*remove_decl*/", "/*remove_use*/", "Handler"),
    ];
    for (decl, own, ty) in cases {
        let parameter = facts
            .declarations
            .iter()
            .find(|d| d.name_span.start == offset(source, decl))
            .unwrap();
        assert_eq!(parameter.kind, "parameter");
        assert_eq!(parameter.name, "value");
        assert_eq!(parameter.ty, ty);
        assert!(parameter.scope.contains(&offset(source, own)));
        for (_, other, _) in cases {
            if other != own {
                assert!(!parameter.scope.contains(&offset(source, other)));
            }
        }
    }
}

#[tokio::test]
async fn incomplete_incoming_binding_is_not_reported_as_proven_absence() {
    let fixture = Fixture::new(
        r#"
class Target { public void Run() {} }
class Usage { void Test(dynamic unknown) { unknown.Run(); } }
"#,
    );
    let result = fixture.query("calls:Target.Run path:Test.cs").await;
    assert_eq!(result, "No resolved references found.");
}

#[tokio::test]
async fn resolved_nonmatching_candidates_do_not_create_incompleteness_warnings() {
    let fixture = Fixture::new(
        r#"
class Target { public void Run() {} }
class Other { public void Run() {} }
class Usage { void Test(Other other) { other.Run(); } }
"#,
    );
    let result = fixture.query("calls:Target.Run path:Test.cs").await;
    assert_eq!(result, "No matches.");
}

#[tokio::test]
async fn unresolved_candidates_do_not_add_noise_to_positive_results() {
    let source = r#"
class Target { public void Run() {} }
class Usage { void Test(Target known, dynamic unknown) {
    known.Run();
    unknown.Run();
} }
"#;
    let fixture = Fixture::new(source);
    let result = fixture.query("calls:Target.Run path:Test.cs").await;
    assert!(result.contains("known.Run()"), "{result}");
    let resolved_only = Fixture::new(&source.replace("unknown.Run();", ""));
    assert_eq!(
        result,
        resolved_only.query("calls:Target.Run path:Test.cs").await
    );
}

#[test]
fn the_new_analysis_epoch_changes_csharp_source_identity() {
    #[derive(serde::Serialize)]
    enum LegacyProfile {
        CSharp(Vec<String>),
    }
    let legacy = *blake3::hash(
        &postcard::to_allocvec(&(
            "sigla-source-analysis",
            1u32,
            Language::CSharp,
            LegacyProfile::CSharp(vec![]),
            blake3::hash(SOURCE.as_bytes()).as_bytes(),
        ))
        .unwrap(),
    )
    .as_bytes();
    let current = sigla::store::source_id(SOURCE, Language::CSharp, &[], "").unwrap();
    assert_ne!(current, legacy);
}
