use sigla::{discovery::Policy, service::App};
use std::{path::Path, sync::Arc};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn app(root: &Path, cache: &Path) -> Arc<App> {
    Arc::new(App::new(Policy::new(vec![root.into()]).unwrap(), cache.into(), 2).unwrap())
}

#[tokio::test]
async fn missing_dependency_and_oversized_source_preserve_other_projects() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "good/Cargo.toml",
        "[package]\nname='good'\nversion='0.1.0'\n[dependencies]\nmissing={path='../absent'}",
    );
    write(root.path(), "good/src/lib.rs", "pub struct Healthy;");
    write(root.path(), "broken/Cargo.toml", "invalid manifest");
    write(root.path(), "broken/src/lib.rs", "pub struct Recovered;");
    write(root.path(), "broken/src/huge.rs", "");
    std::fs::OpenOptions::new()
        .write(true)
        .open(root.path().join("broken/src/huge.rs"))
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let a = app(root.path(), cache.path());
    for symbol in ["Healthy", "Recovered"] {
        let found = a
            .search(root.path().to_str().unwrap(), symbol)
            .await
            .unwrap();
        assert!(found.contains(symbol), "{found}");
    }
}

#[tokio::test]
async fn broken_project_details_preserve_readable_sources() {
    for (entry, details, source) in [
        ("Cargo.toml", "[package", "src/lib.rs"),
        ("Broken.csproj", "<Project", "Code.cs"),
        (
            "ProjectSettings/ProjectVersion.txt",
            "invalid editor",
            "Assets/Code.cs",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        write(root.path(), entry, details);
        write(
            root.path(),
            source,
            if source.ends_with(".rs") {
                "pub struct Recoverable;"
            } else {
                "public class Recoverable {}"
            },
        );
        let a = app(root.path(), cache.path());
        let found = a
            .search(root.path().to_str().unwrap(), "Recoverable")
            .await
            .unwrap();
        assert!(found.contains("Recoverable"), "{found}");
        let discovered = sigla::discovery::discover(
            root.path(),
            &Policy::new(vec![root.path().into()]).unwrap(),
        )
        .unwrap();
        assert!(!discovered.diagnostics.is_empty());
    }
}

#[tokio::test]
async fn csharp_navigation_refresh_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Game.csproj",
        r#"<Project><PropertyGroup><AssemblyName>Game</AssemblyName><DefineConstants>ACTIVE</DefineConstants></PropertyGroup><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
    );
    let source = "namespace Game;\nclass Parser { public int Parse(int value) { return value; } }\nclass Other { public int Parse(int value) { return value; } }\nclass User { int count; void Run(Parser p, Other other) { p.Parse(1); other.Parse(2); count++; } }\n#if ACTIVE\nclass Live {}\n#else\nclass Dead {}\n#endif\n";
    write(root, "Code.cs", source);
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    assert!(
        a.search(path, "type:Live")
            .await
            .unwrap()
            .contains("class Live")
    );
    assert_eq!(a.search(path, "type:Dead").await.unwrap(), "No matches.");
    let calls = a.search(path, "calls:Game.Parser.Parse").await.unwrap();
    assert!(calls.contains("p.Parse"), "{calls}");
    assert!(!calls.contains("Possible"), "{calls}");
    let loc = source.find("p.Parse").unwrap() + 2;
    let (l, c) = sigla::model::position(source, loc);
    let declaration = a.search(path, &format!("@Code.cs:{l}:{c}")).await.unwrap();
    assert!(declaration.contains("Game.Parser.Parse"), "{declaration}");
    assert!(
        a.search(path, "writes:Game.User.count")
            .await
            .unwrap()
            .contains("count++")
    );
    assert!(
        a.search(path, "method:* in:Game.Parser")
            .await
            .unwrap()
            .contains("Parse")
    );
    write(
        root,
        "Code.cs",
        &source.replace("class Live", "class Changed"),
    );
    assert_eq!(a.search(path, "type:Live").await.unwrap(), "No matches.");
    assert!(
        a.search(path, "Changed")
            .await
            .unwrap()
            .contains("class Changed")
    );
    drop(a);
    let a = app(root, cache.path());
    assert!(
        a.search(path, "Changed")
            .await
            .unwrap()
            .contains("class Changed")
    );
}

#[tokio::test]
async fn rust_module_ownership_and_calls() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(
        root,
        "src/lib.rs",
        "mod parser; use parser::Parser; pub fn run(p: Parser) { p.parse(\"hi\"); }\n",
    );
    write(
        root,
        "src/parser.rs",
        "pub struct Parser; impl Parser { pub fn parse(&self, text: &str) {} pub fn new() -> Self { Self } }\n",
    );
    write(root, "src/unlisted.rs", "struct Unlisted;\n");
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    assert!(
        a.search(path, "method:* in:sample::parser::Parser")
            .await
            .unwrap()
            .contains("parse")
    );
    assert_eq!(a.search(path, "Unlisted").await.unwrap(), "No matches.");
    let result = a
        .search(path, "calls:sample::parser::Parser::parse")
        .await
        .unwrap();
    assert!(result.contains("p.parse"), "{result}");
    assert!(!result.contains("Possible"), "{result}");
}

#[tokio::test]
async fn removing_and_restoring_membership_rebuilds_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    let included = r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#;
    write(root, "Game.csproj", included);
    write(root, "Code.cs", "class Restored {}");
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let before = a.search(path, "type:Restored").await.unwrap();
    assert!(before.contains("class Restored"));
    write(root, "Game.csproj", "<Project></Project>");
    assert_eq!(
        a.search(path, "type:Restored").await.unwrap(),
        "No matches."
    );
    write(root, "Game.csproj", included);
    assert_eq!(a.search(path, "type:Restored").await.unwrap(), before);
}

#[tokio::test]
async fn imported_hierarchy_member_implementation_and_scoped_text() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Game.csproj",
        r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
    );
    write(
        root,
        "Code.cs",
        r#"
using Contracts;
namespace Contracts { public interface IParser { int Parse(int x); } public class Base { public virtual int Parse(int x) => x; } }
namespace Game {
 class Parser : Base, IParser { public override int Parse(int x) { return x + 1; } }
 class Child : Parser { }
 class Hidden : Base { public new int Parse(int x) => x; }
 class Unrelated { public int Parse(int x) => x; }
}
"#,
    );
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let implementations = a
        .search(path, "impl:Contracts.IParser.Parse")
        .await
        .unwrap();
    assert!(
        implementations.contains("Game.Parser.Parse"),
        "{implementations}"
    );
    assert!(!implementations.contains("Unrelated"), "{implementations}");
    let overrides = a.search(path, "impl:Contracts.Base.Parse").await.unwrap();
    assert!(overrides.contains("Game.Parser.Parse"), "{overrides}");
    assert!(!overrides.contains("Hidden"), "{overrides}");
    let types = a.search(path, "impl:Contracts.IParser").await.unwrap();
    assert!(types.contains("Game.Child"), "{types}");
    assert!(!types.contains("Unrelated"), "{types}");
    let text = a
        .search(path, "text:return in:Game.Parser.Parse")
        .await
        .unwrap();
    assert!(text.contains("return x + 1"), "{text}");
}

#[tokio::test]
async fn concurrent_requests_and_invalid_queries() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    );
    write(root, "src/lib.rs", "pub struct Shared;\n");
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let (x, y) = tokio::join!(a.search(path, "Shared"), a.search(path, "Shared"));
    assert_eq!(x.unwrap(), y.unwrap());
    assert!(
        a.search("/does/not/exist", "unknown:value")
            .await
            .unwrap_err()
            .to_string()
            .contains("Unknown qualifier")
    );
}

#[tokio::test]
async fn idle_workspaces_reopen_after_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let a = app(dir.path(), cache.path());
    for i in 0..12 {
        let root = dir.path().join(format!("workspace{i}"));
        write(
            &root,
            "Game.csproj",
            r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
        );
        write(&root, "Code.cs", "class Reopened {}");
        assert!(
            a.search(root.to_str().unwrap(), "Reopened")
                .await
                .unwrap()
                .contains("class Reopened")
        );
    }
    let first = dir.path().join("workspace0");
    write(&first, "Code.cs", "class Reopened { int changed; }");
    assert!(
        a.search(first.to_str().unwrap(), "field:changed")
            .await
            .unwrap()
            .contains("changed")
    );
}

#[tokio::test]
async fn self_closing_project_reference_preserves_external_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Shared/Shared.csproj",
        r#"<Project><ItemGroup><Compile Include="Api.cs" /></ItemGroup></Project>"#,
    );
    write(
        root,
        "Shared/Api.cs",
        "namespace Shared; public class Api { public void Run() {} }",
    );
    write(
        root,
        "App/App.csproj",
        r#"<Project><ItemGroup><ProjectReference Include="../Shared/Shared.csproj" /><Compile Include="User.cs" /></ItemGroup></Project>"#,
    );
    write(
        root,
        "App/User.cs",
        "using Shared; class User { void Call(Api api) { api.Run(); } }",
    );
    let a = app(root, cache.path());
    let result = a
        .search(
            root.join("App/App.csproj").to_str().unwrap(),
            "calls:Shared.Api.Run project:App",
        )
        .await
        .unwrap();
    assert!(result.contains("api.Run()"), "{result}");
    assert!(!result.contains("Possible"), "{result}");
}

#[tokio::test]
async fn legacy_source_locations_round_trip_outside_the_entry_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "App/App.csproj",
        r#"<Project><ItemGroup><Compile Include="../Shared.cs" /></ItemGroup></Project>"#,
    );
    let source = "class Café { string s = \"128 × 128 pixels\"; }\n";
    std::fs::write(
        root.join("Shared.cs"),
        source.chars().map(|c| c as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    let a = app(root, cache.path());
    let project = root.join("App/App.csproj");
    let result = a
        .search(project.to_str().unwrap(), "type:Café")
        .await
        .unwrap();
    assert!(result.contains("class Café"), "{result}");
    let (line, column) = sigla::model::position(source, source.find("Café").unwrap());
    let query = format!("@{}:{line}:{column}", root.join("Shared.cs").display());
    let resolved = a.search(project.to_str().unwrap(), &query).await.unwrap();
    assert!(resolved.contains("class Café"), "{resolved}");
    let text = a.search(project.to_str().unwrap(), "text:×").await.unwrap();
    assert!(text.contains("128 × 128"), "{text}");
}

#[tokio::test]
async fn rust_module_directory_creation_and_removal_refresh_membership() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname='sample'\nversion='0.1.0'\n",
    );
    write(root, "src/lib.rs", "mod later;");
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    assert_eq!(a.search(path, "Added").await.unwrap(), "No matches.");
    std::fs::create_dir(root.join("src/later")).unwrap();
    assert_eq!(a.search(path, "Added").await.unwrap(), "No matches.");
    write(root, "src/later/mod.rs", "pub struct Added;");
    assert!(
        a.search(path, "Added")
            .await
            .unwrap()
            .contains("struct Added")
    );
    std::fs::remove_file(root.join("src/later/mod.rs")).unwrap();
    std::fs::remove_dir(root.join("src/later")).unwrap();
    assert_eq!(a.search(path, "Added").await.unwrap(), "No matches.");
    write(root, "src/later.rs", "pub struct Added;");
    assert!(
        a.search(path, "Added")
            .await
            .unwrap()
            .contains("struct Added")
    );
}

#[tokio::test]
async fn rust_impl_in_another_module_uses_the_imported_type_owner() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(root, "src/lib.rs", "mod model; mod actions;");
    write(root, "src/model.rs", "pub struct Parser;");
    write(
        root,
        "src/actions.rs",
        "use crate::model::Parser as Reader; impl Reader { pub fn parse(&self) {} } pub fn run(p: Reader) { p.parse(); }",
    );
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let methods = a
        .search(path, "method:* in:sample::model::Parser")
        .await
        .unwrap();
    assert!(
        methods.contains("sample::model::Parser::parse"),
        "{methods}"
    );
    let calls = a
        .search(path, "calls:sample::model::Parser::parse")
        .await
        .unwrap();
    assert!(calls.contains("p.parse()"), "{calls}");
    assert!(!calls.contains("Possible"), "{calls}");
}

#[tokio::test]
async fn explicit_generic_return_infers_local_and_chained_receivers() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Game.csproj",
        r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
    );
    write(
        root,
        "Code.cs",
        r#"namespace Game;
class Player { public void Play() {} }
class Other { public void Play() {} }
class Factory { public T Get<T>() { return default; } }
class User { void Run(Factory factory, Other other) {
 var player = factory.Get<Player>();
 player.Play();
 factory.Get<Player>().Play();
 other.Play();
} }
"#,
    );
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let calls = a.search(path, "calls:Game.Player.Play").await.unwrap();
    assert!(calls.contains("player.Play()"), "{calls}");
    assert!(calls.contains("Get<Player>().Play()"), "{calls}");
    assert!(!calls.contains("Possible"), "{calls}");
    assert!(!calls.contains("other.Play()"), "{calls}");
}

#[tokio::test]
async fn constructor_targets_and_type_targets_find_construction_sites() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Game.csproj",
        r#"<Project><ItemGroup><Compile Include="Code.cs" /></ItemGroup></Project>"#,
    );
    write(
        root,
        "Code.cs",
        r#"namespace Game;
class Widget { public Widget() {} public Widget(int x) {} }
class Other { public void Widget(int x) {} }
class User { void Run(Other other) {
 var item = new Widget(1);
 other.Widget(1);
} }
"#,
    );
    let a = app(root, cache.path());
    let path = root.to_str().unwrap();
    let constructor = a
        .search(path, "constructor:Game.Widget(int)")
        .await
        .unwrap();
    assert!(constructor.contains("Widget(int x)"), "{constructor}");
    for query in ["calls:Game.Widget(int)", "calls:Game.Widget"] {
        let calls = a.search(path, query).await.unwrap();
        assert!(calls.contains("new Widget(1)"), "{calls}");
        assert!(!calls.contains("other.Widget"), "{calls}");
        assert!(!calls.contains("Possible"), "{calls}");
    }
}

#[test]
fn result_limits_preserve_complete_units_and_report_the_total() {
    let units = (0..100)
        .map(|i| format!("File.cs:{i} — λ method{i}()\n"))
        .collect::<Vec<_>>();
    let small = sigla::search::render(units.clone(), 5);
    let large = sigla::search::render(units.clone(), units.len());
    assert_eq!(
        small.lines().filter(|l| l.starts_with("File.cs:")).count(),
        5
    );
    assert!(small.contains(&format!("`limit:{}`", units.len())));
    assert!(!large.contains("omitted"));
    for line in small.lines().filter(|l| l.starts_with("File.cs:")) {
        assert!(large.contains(line));
    }
}

#[tokio::test]
async fn count_limits_and_outgoing_calls_keep_unresolved_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Game.csproj",
        r#"<Project><ItemGroup><Compile Include="Scripts/Code.cs" /></ItemGroup></Project>"#,
    );
    let calls = (0..8)
        .map(|i| format!(" Unknown{i}();\n"))
        .collect::<String>();
    let source = format!("class Example {{\n void Run() {{\n{calls} }}\n}}");
    write(dir.path(), "Scripts/Code.cs", &source);
    let a = app(dir.path(), cache.path());
    let path = dir.path().to_str().unwrap();
    let limited = a.search(path, "calls:* in:Example limit:3").await.unwrap();
    assert_eq!(
        limited
            .lines()
            .filter(|l| l.starts_with("`Scripts/Code.cs:"))
            .count(),
        3
    );
    let full = a.search(path, "calls:* in:Example limit:8").await.unwrap();
    assert_eq!(
        full.lines()
            .filter(|l| l.starts_with("`Scripts/Code.cs:"))
            .count(),
        8
    );
    assert!(limited.contains("`limit:8`"), "{limited}");
    assert!(!full.contains("omitted"), "{full}");
    let directory = a
        .search(path, "text:Unknown path:Scripts limit:3")
        .await
        .unwrap();
    assert_eq!(
        directory
            .lines()
            .filter(|l| l.starts_with("`Scripts/Code.cs:"))
            .count(),
        3
    );
    let declarations = a
        .search(path, "path:Scripts/Code.cs limit:1")
        .await
        .unwrap();
    assert_eq!(
        declarations
            .lines()
            .filter(|l| l.starts_with("`Scripts/Code.cs:"))
            .count(),
        1
    );
}
