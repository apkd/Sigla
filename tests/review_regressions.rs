use sigla::{discovery::Policy, service::App};
use std::sync::Arc;

struct Fixture {
    app: Arc<App>,
    root: tempfile::TempDir,
    _cache: tempfile::TempDir,
    file: &'static str,
    source: &'static str,
}

impl Fixture {
    fn new(file: &'static str, source: &'static str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(file), source).unwrap();
        let app = Arc::new(
            App::new(
                Policy::new(vec![root.path().into()]).unwrap(),
                cache.path().into(),
                1,
            )
            .unwrap(),
        );
        Self {
            app,
            root,
            _cache: cache,
            file,
            source,
        }
    }
    fn position(&self, marker: &str) -> String {
        let offset = self.source.find(marker).unwrap() + marker.len();
        let before = &self.source[..offset];
        format!(
            "@{}:{}:{}",
            self.file,
            before.lines().count(),
            before.rsplit('\n').next().unwrap().chars().count() + 1
        )
    }
    async fn query(&self, query: &str) -> String {
        self.app
            .search(self.root.path().to_str().unwrap(), query)
            .await
            .unwrap()
    }
    async fn binds(&self, declaration: &str, uses: &[&str]) {
        let expected = self.query(&self.position(declaration)).await;
        assert!(
            !expected.contains("No matches"),
            "{declaration}: {expected}"
        );
        for usage in uses {
            assert_eq!(self.query(&self.position(usage)).await, expected, "{usage}");
        }
    }
}

#[tokio::test]
async fn explicit_csharp_receivers_preserve_identity_and_writes() {
    let f = Fixture::new(
        "Test.cs",
        r#"
class Item { public void Run() {} }
class Parent<T> {
 public T Value;
 public virtual int /*base*/Parse(int x) => x;
}
class Child : Parent<Item> {
 int /*field*/files;
 Child(int /*parameter*/files) {
  this./*write*/files = /*read*/files;
 }
 public override int /*override*/Parse(int x) => base./*call*/Parse(x);
 void Use() { base.Value./*generic*/Run(); this./*virtual*/Parse(1); }
}
"#,
    );
    f.binds("/*field*/", &["/*write*/"]).await;
    f.binds("/*parameter*/", &["/*read*/"]).await;
    f.binds("/*base*/", &["/*call*/"]).await;
    f.binds("/*override*/", &["/*virtual*/"]).await;
    assert!(
        f.query(&f.position("/*generic*/"))
            .await
            .contains("Item.Run")
    );
    let writes = f
        .query(&format!("writes:{}", f.position("/*field*/")))
        .await;
    assert!(writes.contains("/*write*/"), "{writes}");
    let incoming = f.query(&format!("calls:{}", f.position("/*base*/"))).await;
    assert!(incoming.contains("/*call*/"), "{incoming}");
    let outgoing = f.query("calls:* in:Child.Parse(int)").await;
    assert!(outgoing.contains("Parent"), "{outgoing}");
}

#[tokio::test]
async fn rust_initializers_and_path_namespaces_preserve_identity() {
    let f = Fixture::new(
        "lib.rs",
        r#"
mod url { pub struct Url; impl Url { pub fn parse(x: &str) -> Self { Self } } }
fn sample(/*parameter*/url: &str) -> /*type_root*/url::Url {
 let /*local*/url = /*call_root*/url::Url::parse(/*initializer*/url);
 let (/*tuple*/url, other) = (/*tuple_initializer*/url, 1);
 { let /*nested*/url = /*nested_initializer*/url; consume(/*nested_use*/url); }
 let Some(/*pattern*/url) = Some(/*pattern_initializer*/url) else { consume(/*else_use*/url); return url::Url::parse(""); };
 /*after*/url
}
fn consume<T>(x: T) {}
"#,
    );
    f.binds("/*parameter*/", &["/*initializer*/"]).await;
    f.binds("/*local*/", &["/*tuple_initializer*/"]).await;
    f.binds(
        "/*tuple*/",
        &[
            "/*nested_initializer*/",
            "/*pattern_initializer*/",
            "/*else_use*/",
        ],
    )
    .await;
    f.binds("/*nested*/", &["/*nested_use*/"]).await;
    f.binds("/*pattern*/", &["/*after*/"]).await;
    let parameter = f.query(&f.position("/*parameter*/")).await;
    let local = f.query(&f.position("/*local*/")).await;
    for marker in ["/*type_root*/", "/*call_root*/"] {
        let result = f.query(&f.position(marker)).await;
        assert_ne!(result, parameter);
        assert_ne!(result, local);
        assert!(result.contains("module"), "{result}");
    }
}

#[tokio::test]
async fn cpp_while_condition_has_loop_scope() {
    let f = Fixture::new(
        "test.cpp",
        r#"
int /*global*/value;
int next();
void use(int);
void run() {
 while (int /*local*/value = next()) { use(/*inside*/value); }
 use(/*after*/value);
}
"#,
    );
    f.query("file:* wait:complete").await;
    f.binds("/*local*/", &["/*inside*/"]).await;
    let after = f.query(&f.position("/*after*/")).await;
    let global = f.query(&f.position("/*global*/")).await;
    assert!(after.contains(global.lines().next().unwrap()), "{after}");
    let global_uses = f.query(&format!("uses:{}", f.position("/*global*/"))).await;
    assert!(global_uses.contains("/*after*/"), "{global_uses}");
    assert!(!global_uses.contains("/*inside*/"), "{global_uses}");
}

#[tokio::test]
async fn shaderlab_dialects_keep_distinct_targets() {
    let f = Fixture::new(
        "test.shader",
        r#"
Shader "Test" {
SubShader { Pass {
HLSLPROGRAM
float /*hlsl*/shadeValue() { return 1; }
float hlsl_entry() { return /*hlsl_call*/shadeValue(); }
ENDHLSL
GLSLPROGRAM
float /*glsl*/shadeValue() { return 1; }
float glsl_entry() { return /*glsl_call*/shadeValue(); }
ENDGLSL
} } }
"#,
    );
    f.query("file:* wait:complete").await;
    for (declaration, own, other) in [
        ("/*hlsl*/", "/*hlsl_call*/", "/*glsl_call*/"),
        ("/*glsl*/", "/*glsl_call*/", "/*hlsl_call*/"),
    ] {
        let declaration_result = f.query(&f.position(declaration)).await;
        assert!(
            !declaration_result.contains("No matches"),
            "{declaration_result}"
        );
        let call = f.query(&f.position(own)).await;
        let site = declaration_result
            .lines()
            .next()
            .unwrap()
            .split_once(" in ")
            .unwrap()
            .1;
        assert!(call.contains(site), "{call}");
        let other_call = f.query(&f.position(other)).await;
        assert!(!other_call.contains(site), "{other_call}");
        let calls = f.query(&format!("calls:{}", f.position(declaration))).await;
        assert!(calls.contains(own), "{calls}");
        assert!(!calls.contains(other), "{calls}");
    }
}

#[tokio::test]
async fn rust_unknown_receivers_keep_call_sites_without_unrelated_member_candidates() {
    let source = r#"use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct Progress;
impl Progress {
    pub fn new() -> Self { Self }
}
pub struct OutputTransaction;
impl OutputTransaction {
    pub fn new(_: &Path) -> Self { Self }
}
pub struct Builder;
impl Builder {
    pub fn build(&self, _: bool) {}
}
pub struct ExtraArgument;
impl ExtraArgument {
    pub fn build(&self, _: bool, _: bool) {}
}

pub fn external_calls() {
    let _ = Arc::new(
        AtomicBool::new(false),
    );
}
pub fn typed_calls(builder: Builder, path: &Path) {
    let _ = Progress::new();
    let _ = OutputTransaction::new(path);
    builder.build(true);
}
pub fn unknown_receiver() {
    let value = unavailable::factory();
    value.build(true);
}
"#;
    let fixture = Fixture::new("lib.rs", source);
    std::fs::write(
        fixture.root.path().join("Cargo.toml"),
        "[package]\nname='receiver_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n",
    ).unwrap();
    let app = &fixture.app;
    let codebase = fixture.root.path().to_str().unwrap();
    let external = app
        .search(codebase, "calls:* in:receiver_fixture::external_calls")
        .await
        .unwrap();
    assert!(
        !external.contains("Progress::new") && !external.contains("OutputTransaction::new"),
        "{external}"
    );
    // Keep the nested calls on different lines so relationship grouping cannot
    // collapse the two source locations, even if dependency resolution improves.
    let rows: Vec<_> = external
        .lines()
        .filter(|line| line.starts_with("`function:receiver_fixture::external_calls → "))
        .collect();
    assert_eq!(rows.len(), 2, "Both call sites must survive: {external}");
    for source_line in ["    let _ = Arc::new(", "        AtomicBool::new(false),"] {
        let line_number = source.lines().position(|line| line == source_line).unwrap() + 1;
        let location = format!("lib.rs:{line_number}`");
        assert!(
            rows.iter().any(|row| row.ends_with(&location)),
            "Missing call site on line {line_number}: {external}"
        );
    }

    let typed = app
        .search(codebase, "calls:* in:receiver_fixture::typed_calls")
        .await
        .unwrap();
    for target in ["Progress::new", "OutputTransaction::new", "Builder::build"] {
        assert!(typed.contains(target), "{typed}");
    }
    assert!(!typed.contains("unresolved:"), "{typed}");
    assert!(!typed.contains("Possible"), "{typed}");

    let unknown = app
        .search(codebase, "calls:* in:receiver_fixture::unknown_receiver")
        .await
        .unwrap();
    assert!(unknown.contains("value.build(true)"), "{unknown}");
    assert!(unknown.contains("unresolved:build"), "{unknown}");
    assert!(!unknown.contains("Builder::build"), "{unknown}");
    assert!(!unknown.contains("ExtraArgument::build"), "{unknown}");

    let incoming = app
        .search(codebase, "calls:receiver_fixture::Builder::build")
        .await
        .unwrap();
    assert!(incoming.contains("builder.build(true)"), "{incoming}");
    assert!(!incoming.contains("value.build(true)"), "{incoming}");
    assert!(!incoming.contains("Possible"), "{incoming}");
    app.shutdown().await;
}
