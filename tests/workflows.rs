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
async fn empty_queries_offer_only_supported_corrections() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Navigation.csproj",
        r#"<Project><ItemGroup><Compile Include="Src/Parser.cs" /></ItemGroup></Project>"#,
    );
    write(
        root.path(),
        "Src/Parser.cs",
        "class Parser { public int Parse { get; } }\n",
    );
    let a = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    let wrong_kind = a.search(project, "method:Parser.Parse").await.unwrap();
    assert!(
        wrong_kind.contains("`property:Parser.Parse`"),
        "{wrong_kind}"
    );
    let corrected = a.search(project, "property:Parser.Parse").await.unwrap();
    assert!(corrected.contains("public int Parse"), "{corrected}");
    assert_eq!(
        a.search(project, "method:Parser.Missing").await.unwrap(),
        "No matches."
    );
    assert_eq!(
        a.search(project, "method:Parser.Parse path:Missing/**")
            .await
            .unwrap(),
        "No matches."
    );
    let wrong_project = a
        .search(project, "file:*.cs project:owner/repo")
        .await
        .unwrap();
    assert!(
        wrong_project.contains("`project:`") && wrong_project.contains("`Navigation`"),
        "{wrong_project}"
    );
    assert!(
        a.search(project, "file:*.cs project:Navigation")
            .await
            .unwrap()
            .contains("Parser.cs")
    );
    let directory = a.search(project, "file:*.cs path:Src/").await.unwrap();
    assert!(directory.contains("Src/Parser.cs"), "{directory}");
    assert_eq!(
        a.search(project, "file:Missing.cs path:Src/")
            .await
            .unwrap(),
        "No matches."
    );
    assert_eq!(
        a.search(project, "file:*.cs -path:Src/").await.unwrap(),
        "No matches."
    );
    assert!(
        a.search(project, "file:*.cs path:Src/**")
            .await
            .unwrap()
            .contains("Parser.cs")
    );
    // Source queries already accept recursive directory paths; no misleading hint.
    assert_eq!(
        a.search(project, "method:Missing path:Src/").await.unwrap(),
        "No matches."
    );
    assert!(
        a.search(project, "property:Parser.Parse path:Src/")
            .await
            .unwrap()
            .contains("public int Parse")
    );
}

#[test]
fn query_syntax_corrections_are_usable() {
    use sigla::query::Query;
    let error = Query::parse("text:'hello world'").unwrap_err().to_string();
    assert!(error.contains("`text:\"hello world\"`"), "{error}");
    assert_eq!(
        Query::parse("text:\"hello world\"").unwrap().target.name,
        "hello world"
    );
    assert_eq!(Query::parse("text:\"don't\"").unwrap().target.name, "don't");
    let error = Query::parse("method:Parse offset:20")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("`offset:`") && error.contains("`limit:`"),
        "{error}"
    );
}

#[tokio::test]
async fn concrete_kinds_and_declaration_line_ranges() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Test.csproj",
        "<Project><ItemGroup><Compile Include=\"Src/Core/Types.cs\"/></ItemGroup></Project>",
    );
    let source = "// outside\nnamespace Test;\nclass Example {\n void Run() {\n }\n}\ninterface Contract {}\nstruct Value {}\nenum Choice { One }\ndelegate void Callback();\n";
    write(root.path(), "Src/Core/Types.cs", source);
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    for (kind, name) in [
        ("class", "Example"),
        ("interface", "Contract"),
        ("struct", "Value"),
        ("enum", "Choice"),
        ("delegate", "Callback"),
    ] {
        let result = app.search(project, &format!("{kind}:*")).await.unwrap();
        assert!(
            result.starts_with(&format!("`{kind}:Test.{name}` in `")),
            "{result}"
        );
        assert_eq!(result.matches(" in `").count(), 1, "{result}");
        assert_eq!(
            result,
            app.search(project, &format!("type:{name}")).await.unwrap()
        );
    }
    let class = app.search(project, "c:Example").await.unwrap();
    assert!(class.contains("`Src/Core/Types.cs:3-6`"), "{class}");
    let method = app.search(project, "method:Example.Run").await.unwrap();
    assert!(method.contains("`Src/Core/Types.cs:4-5`"), "{method}");
    let text = app.search(project, "text:outside").await.unwrap();
    assert!(text.starts_with("`Src/Core/Types.cs:1`"), "{text}");
    let limited = app.search(project, "type:* limit:2").await.unwrap();
    assert!(
        limited.contains("`Src/Core/Types.cs:") && limited.contains("`…/Types.cs:"),
        "{limited}"
    );
}

#[tokio::test]
async fn partial_qualification_preserves_declarations_and_relationships() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Test.csproj",
        "<Project><ItemGroup><Compile Include=\"Members.cs\"/><Compile Include=\"Calls.cs\"/></ItemGroup></Project>",
    );
    let members = r#"namespace Library;
partial class Worker {
 public void PublicMethod() { }
 internal void InternalMethod() { }
 private void PrivateMethod() { }
 public static Worker Create() => new Worker();
 public const int Code = 1;
 public void Overload(int value) { }
 public void Overload(string value) { }
 public class Nested { public void Run() { } }
}
"#;
    write(root.path(), "Members.cs", members);
    write(
        root.path(),
        "Calls.cs",
        r#"namespace Library;
partial class Worker {
 void Execute() { PublicMethod(); InternalMethod(); PrivateMethod(); var item = Worker.Create(); Overload(1); }
 int Select(int value) => value switch { Worker.Code => 1, _ => 0 };
}
"#,
    );
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    for name in [
        "PublicMethod",
        "InternalMethod",
        "PrivateMethod",
        "Create",
        "Code",
    ] {
        for selector in ["", "uses:"] {
            let full = app
                .search(project, &format!("{selector}Library.Worker.{name}"))
                .await
                .unwrap();
            assert!(
                full.contains("Members.cs:") || full.contains("Calls.cs:"),
                "{full}"
            );
            for target in [name.to_owned(), format!("Worker.{name}")] {
                assert_eq!(
                    app.search(project, &format!("{selector}{target}"))
                        .await
                        .unwrap(),
                    full
                );
            }
        }
    }
    for name in ["PublicMethod", "InternalMethod", "PrivateMethod", "Create"] {
        let full = app
            .search(project, &format!("calls:Library.Worker.{name}"))
            .await
            .unwrap();
        assert!(full.contains("Worker.Execute"), "{full}");
        assert_eq!(
            app.search(project, &format!("calls:Worker.{name}"))
                .await
                .unwrap(),
            full
        );
        let offset = members.find(&format!("{name}(")).unwrap();
        let line = members[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
        let column = members[..offset]
            .rsplit('\n')
            .next()
            .unwrap()
            .chars()
            .count()
            + 1;
        assert_eq!(
            app.search(project, &format!("calls:@Members.cs:{line}:{column}"))
                .await
                .unwrap(),
            full
        );
        assert_eq!(
            app.search(project, &format!("@Members.cs:{line}:{column}"))
                .await
                .unwrap(),
            app.search(project, &format!("method:Worker.{name}"))
                .await
                .unwrap()
        );
    }
    for target in [
        "Worker.Overload(int)",
        "Worker.Nested.Run",
        "Worker.* in:Library.Worker",
        "Worker.PrivateMethod access:private",
    ] {
        let result = app
            .search(project, &format!("method:{target}"))
            .await
            .unwrap();
        assert!(result.contains("Members.cs:"), "{target}: {result}");
    }
    let overload = app
        .search(project, "method:Worker.Overload(int)")
        .await
        .unwrap();
    assert!(!overload.contains("Overload(string"), "{overload}");
    let outgoing = app
        .search(project, "calls:* in:Worker.Execute")
        .await
        .unwrap();
    assert!(outgoing.contains("PublicMethod()"), "{outgoing}");
    assert_eq!(
        outgoing,
        app.search(project, "calls:* in:Library.Worker.Execute")
            .await
            .unwrap()
    );
    assert_eq!(
        app.search(project, "file:*Members*.cs project:Test")
            .await
            .unwrap(),
        app.search(project, "file:*Members*.cs").await.unwrap()
    );
    let correction = app
        .search(project, "file:*Members*.cs project:owner/repo")
        .await
        .unwrap();
    assert!(
        correction.contains("`project:`") && correction.contains("`Test`"),
        "{correction}"
    );
}

#[tokio::test]
async fn qualified_suffixes_keep_namespace_collisions_and_rust_modules() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Test.csproj",
        "<Project><ItemGroup><Compile Include=\"Types.cs\"/></ItemGroup></Project>",
    );
    write(
        root.path(),
        "Types.cs",
        "namespace First { class Worker { public void Run() {} } } namespace Second { class Worker { public void Run() {} } } namespace Third { class OtherWorker { public void Run() {} } }",
    );
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='qualified_fixture'\nversion='0.1.0'\n",
    );
    write(
        root.path(),
        "src/lib.rs",
        "pub mod outer { pub mod inner { pub fn run() {} pub fn invoke() { run(); } } } pub mod other { pub fn run() {} pub fn invoke() { run(); } }",
    );
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    let matches = app.search(project, "method:Worker.Run").await.unwrap();
    assert!(
        matches.contains("First.Worker.Run") && matches.contains("Second.Worker.Run"),
        "{matches}"
    );
    assert!(!matches.contains("OtherWorker"), "{matches}");
    let precise = app
        .search(project, "method:First.Worker.Run")
        .await
        .unwrap();
    assert!(!precise.contains("Second.Worker"), "{precise}");
    for selector in ["function:", "calls:"] {
        let result = app
            .search(project, &format!("{selector}inner::run"))
            .await
            .unwrap();
        assert!(
            result.contains("src/lib.rs:"),
            "{selector}inner::run: {result}"
        );
        assert!(!result.contains("other::invoke"), "{result}");
        assert_eq!(
            result,
            app.search(project, &format!("{selector}outer::inner::run"))
                .await
                .unwrap()
        );
    }
    assert!(
        app.search(project, "function:inner::*")
            .await
            .unwrap()
            .contains("invoke")
    );
    assert!(
        app.search(project, "calls:* in:inner::invoke")
            .await
            .unwrap()
            .contains("run()")
    );
}

#[tokio::test]
async fn implementation_filters_keep_external_ancestors_and_separate_languages() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Dotnet/Test.csproj",
        "<Project><ItemGroup><Compile Include=\"Base.cs\"/><Compile Include=\"Leaf.cs\"/></ItemGroup></Project>",
    );
    write(
        root.path(),
        "Dotnet/Base.cs",
        "public interface Contract { void Run(); } public class Parent : Contract { public virtual void Run() {} } public interface Read {} public class Reader: Read {}",
    );
    write(
        root.path(),
        "Dotnet/Leaf.cs",
        "using Alias = Parent; public class Leaf : Alias { public override void Run() {} }",
    );
    write(
        root.path(),
        "Rust/Cargo.toml",
        "[package]\nname='rust_fixture'\nversion='0.1.0'\n",
    );
    write(
        root.path(),
        "Rust/src/lib.rs",
        "pub trait Read {} pub struct Reader; impl Read for Reader {}",
    );
    let app = app(root.path(), cache.path());
    let project = root.path().to_str().unwrap();
    for query in [
        "impl:Contract path:Dotnet/Leaf.cs",
        "impl:Contract.Run path:Dotnet/Leaf.cs",
        "derived:Parent path:Dotnet/Leaf.cs",
    ] {
        let result = app.search(project, query).await.unwrap();
        assert!(result.contains("Leaf"), "{query}: {result}");
        assert!(!result.contains("Base.cs:"), "{result}");
    }
    let rust = app
        .search(project, "impl:Read path:Rust/src/lib.rs")
        .await
        .unwrap();
    assert!(rust.contains("Reader") && rust.contains("rust"), "{rust}");
    assert!(!rust.contains("Dotnet/"), "{rust}");
}

#[tokio::test]
async fn missing_dependency_and_oversized_source_preserve_other_projects() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['broken','good']\nresolver='2'",
    );
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
    write(root.path(), "broken/src/huge.rs", "pub struct Repaired;");
    let found = a
        .search(root.path().to_str().unwrap(), "Repaired")
        .await
        .unwrap();
    assert!(found.contains("Repaired"), "{found}");
}

#[tokio::test]
async fn failed_build_target_preserves_other_projects_and_evaluated_sources() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "good/Good.csproj",
        "<Project><ItemGroup><Compile Include=\"Code.cs\"/></ItemGroup></Project>",
    );
    write(root.path(), "good/Code.cs", "public class Healthy {}");
    write(
        root.path(),
        "bad/Bad.csproj",
        "<Project><ItemGroup><Compile Include=\"Code.cs\"/></ItemGroup><Target Name=\"ResolveReferences\"><Error Text=\"Broken dependency\"/></Target></Project>",
    );
    write(root.path(), "bad/Code.cs", "public class Recoverable {}");
    let app = app(root.path(), cache.path());
    for symbol in ["Healthy", "Recoverable"] {
        let result = app
            .search(root.path().to_str().unwrap(), symbol)
            .await
            .unwrap();
        assert!(result.contains(symbol), "{result}");
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
        for diagnostic in &discovered.diagnostics {
            assert!(
                !found.contains(diagnostic),
                "Recoverable diagnostic leaked into query results"
            );
        }
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
    assert!(small.contains(&format!("of {} matches", units.len())));
    assert!(!large.contains("omitted"));
    for line in small.lines().filter(|l| l.starts_with("File.cs:")) {
        assert!(large.contains(line));
    }
}

#[tokio::test]
async fn equivalent_signature_types_preserve_namespace_identity() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Test.csproj",
        r#"<Project><ItemGroup><Compile Include="Test.cs" /></ItemGroup></Project>"#,
    );
    write(
        dir.path(),
        "Test.cs",
        r#"
using System;
using Alias = System.Type;
namespace System { public class Type {} }
namespace Other { public class Type {} }
class Inspector {
 void Inspect(Type value) {}
 void Inspect(Other.Type value) {}
 void Aliased(Alias value) {}
}
"#,
    );
    let a = app(dir.path(), cache.path());
    let project = dir.path().to_str().unwrap();
    let short = a
        .search(project, "method:Inspector.Inspect(Type)")
        .await
        .unwrap();
    let full = a
        .search(project, "method:Inspector.Inspect(System.Type)")
        .await
        .unwrap();
    assert_eq!(short, full);
    assert!(full.contains("Inspect(Type value)"), "{full}");
    assert!(!full.contains("Other.Type"), "{full}");
    let alias = a
        .search(project, "method:Inspector.Aliased(System.Type)")
        .await
        .unwrap();
    assert!(alias.contains("Aliased(Alias value)"), "{alias}");
}

#[tokio::test]
async fn call_groups_keep_targets_and_overloads_separate() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Test.csproj",
        r#"<Project><ItemGroup><Compile Include="Test.cs" /></ItemGroup></Project>"#,
    );
    write(
        dir.path(),
        "Test.cs",
        r#"
class Writer {
 void Write(int n) {} void Write(string s) {} void Flush() {}
 void Save() { Write(1); Write(2); Write("s"); Flush(); }
}
"#,
    );
    let a = app(dir.path(), cache.path());
    let project = dir.path().to_str().unwrap();
    let result = a.search(project, "calls:* in:Writer.Save").await.unwrap();
    assert_eq!(
        result.lines().filter(|l| l.contains(" → ")).count(),
        3,
        "{result}"
    );
    assert!(
        result.contains("Write(int)")
            && result.contains("Write(string)")
            && result.contains("Flush()"),
        "{result}"
    );
    assert!(result.contains("(2 occurrences)"), "{result}");
    let limited = a
        .search(project, "calls:* in:Writer.Save limit:1")
        .await
        .unwrap();
    assert!(limited.contains("of 4 call sites"), "{limited}");
}

#[tokio::test]
async fn mutation_categories_and_nested_field_previews_are_complete() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Test.csproj",
        r#"<Project><ItemGroup><Compile Include="Test.cs" /></ItemGroup></Project>"#,
    );
    let source = r#"
class Recorder {
 internal class Slot { internal int Pending, Other = 9; }
 class Data { internal Slot Slot; }
 static void Poll(out int value) { value = 1; }
 static void Change(ref int value) { value++; }
 static void Read(in int value) {}
 static void Run(Data data) {
  data.Slot.Pending = 2;
  Poll(out data.Slot.Pending);
  Change(ref data.Slot.Pending);
  Change(ref /*argument comment*/ data.Slot.Pending);
  Read(in data.Slot.Pending);
 }
}
"#;
    write(dir.path(), "Test.cs", source);
    let a = app(dir.path(), cache.path());
    let project = dir.path().to_str().unwrap();
    let writes = a
        .search(project, "writes:Recorder.Slot.Pending")
        .await
        .unwrap();
    assert!(
        writes.contains("Pending = 2")
            && writes.contains("Poll(out")
            && writes.contains("Change(ref"),
        "{writes}"
    );
    assert!(!writes.contains("Read(in"), "{writes}");
    assert!(writes.contains("Possible write through `ref`"), "{writes}");
    let uses = a
        .search(project, "uses:Recorder.Slot.Pending")
        .await
        .unwrap();
    assert!(uses.contains("Read(in"), "{uses}");
    let field = a
        .search(project, "field:Recorder.Slot.Pending")
        .await
        .unwrap();
    assert!(
        field.contains("internal int Pending;") && !field.contains("= 9"),
        "{field}"
    );
    let other = a
        .search(project, "field:Recorder.Slot.Other")
        .await
        .unwrap();
    assert!(other.contains("internal int Other = 9;"), "{other}");
}

#[tokio::test]
async fn ambiguous_calls_count_sites_and_preserve_position_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Test.csproj",
        r#"<Project><ItemGroup><Compile Include="Test.cs" /></ItemGroup></Project>"#,
    );
    let mut source = String::from("class Writer {\n");
    for ty in [
        "int", "string", "char", "bool", "double", "float", "long", "short", "byte", "decimal",
    ] {
        source.push_str(&format!("void Append({ty} value) {{}}\n"));
    }
    source.push_str("void Run(Missing value) { Append(value); Append(value); }\n}");
    write(dir.path(), "Test.cs", &source);
    let a = app(dir.path(), cache.path());
    let project = dir.path().to_str().unwrap();
    let result = a
        .search(project, "calls:* in:Writer.Run limit:2")
        .await
        .unwrap();
    assert_eq!(
        result.lines().filter(|l| l.contains(" → ")).count(),
        2,
        "{result}"
    );
    assert!(
        result.contains("Possible targets:") && result.contains("(+2)"),
        "{result}"
    );
    assert!(!result.contains("omitted. Narrow"), "{result}");
    let pos = source.find("Append(value)").unwrap();
    let (line, col) = sigla::model::position(&source, pos);
    for prefix in ["", "uses:"] {
        let result = a
            .search(project, &format!("{prefix}@Test.cs:{line}:{col}"))
            .await
            .unwrap();
        assert!(
            result.contains("method:Writer.Append(") && result.contains("(+2 candidates)"),
            "{result}"
        );
        assert!(!result.contains(" → "), "{result}");
    }
}

#[tokio::test]
async fn local_function_references_stay_in_their_lexical_scope() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Test.csproj",
        r#"<Project><ItemGroup><Compile Include="*.cs" /></ItemGroup></Project>"#,
    );
    write(
        dir.path(),
        "Test.cs",
        r#"
class Save {
 void Run() { void Capture() {} Capture(); }
 void Other() { void Capture() {} Capture(); }
}
"#,
    );
    for i in 0..32 {
        write(
            dir.path(),
            &format!("Other{i}.cs"),
            &format!("class Other{i} {{ void Capture() {{}} void Run() {{ Capture(); }} }}"),
        );
    }
    let a = app(dir.path(), cache.path());
    let project = dir.path().to_str().unwrap();
    let result = a.search(project, "uses:Save.Run.Capture").await.unwrap();
    assert!(result.contains("Save.Run"), "{result}");
    assert!(
        !result.contains("Save.Other") && !result.contains("Other0"),
        "{result}"
    );
    assert_eq!(
        result,
        a.search(project, "uses:Save.Run.Capture path:Test.cs")
            .await
            .unwrap()
    );
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
            .filter(|l| l.contains(" in `") && l.contains("/Code.cs:"))
            .count(),
        3
    );
    let full = a.search(path, "calls:* in:Example limit:8").await.unwrap();
    assert_eq!(
        full.lines()
            .filter(|l| l.contains(" in `") && l.contains("/Code.cs:"))
            .count(),
        8
    );
    assert!(limited.contains("of 8 call sites"), "{limited}");
    assert!(!full.contains("omitted"), "{full}");
    let directory = a
        .search(path, "text:Unknown path:Scripts limit:3")
        .await
        .unwrap();
    assert_eq!(
        directory
            .lines()
            .filter(|l| l.contains(" in `") && l.contains("/Code.cs:"))
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
            .filter(|l| l.contains(" in `Scripts/Code.cs:"))
            .count(),
        1
    );
}
