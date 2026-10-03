use sigla::{discovery::Policy, service::App};
use std::{path::Path, sync::Arc};

fn write(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn app(root: &Path, cache: &Path) -> Arc<App> {
    Arc::new(App::new(Policy::new(vec![root.into()]).unwrap(), cache.into(), 1).unwrap())
}

#[tokio::test]
async fn native_sites_signatures_calls_writes_and_refresh() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "engine.hpp",
        r#"
namespace Game {
struct Renderer { void flush(); void flush() const; int position; };
struct Child : public Renderer {};
struct Grandchild : Child {};
void external(int value = 0);
int operator<<(Renderer&, int);
void accept(int);
void accept(System::Int32);
}
"#,
    );
    write(
        root.path(),
        "engine.cpp",
        r#"
#include "engine.hpp"
void Game::Renderer::flush() {}
void render(Game::Renderer &r, void (*callback)()) {
    r.flush();
    callback();
    unavailable();
    r.position += 2;
    Game::external();
    auto deferred = [] { hidden(); };
}
"#,
    );
    let app = app(root.path(), cache.path());
    let path = root.path().to_str().unwrap();
    let include = app.search(path, "@engine.cpp:2:12").await.unwrap();
    assert!(include.contains("engine.hpp"), "{include}");
    let operator = app.search(path, "operator:<< lang:cpp").await.unwrap();
    assert!(operator.contains("operator<<"), "{operator}");
    let sites = app
        .search(path, "method:Game::Renderer::flush lang:cpp")
        .await
        .unwrap();
    assert!(
        sites.contains("engine.hpp") && sites.contains("engine.cpp"),
        "{sites}"
    );
    let qualified = app
        .search(path, "method:\"Game::Renderer::flush() const\" lang:cpp")
        .await
        .unwrap();
    assert!(
        qualified.contains("flush() const") && !qualified.contains("engine.cpp"),
        "{qualified}"
    );
    let signature = app
        .search(path, "function:Game::accept(int) lang:cpp")
        .await
        .unwrap();
    assert!(
        signature.contains("accept(int)") && !signature.contains("System::Int32"),
        "{signature}"
    );
    let calls = app
        .search(path, "calls:* in:render lang:cpp")
        .await
        .unwrap();
    for name in [
        "r.flush()",
        "callback()",
        "unavailable()",
        "Game::external()",
    ] {
        assert!(calls.contains(name), "{calls}");
    }
    assert!(!calls.contains("hidden()"), "{calls}");
    assert!(
        app.search(path, "function:fl* lang:cpp")
            .await
            .unwrap()
            .contains("flush")
    );
    let unknown = app
        .search(path, "calls:unavailable lang:cpp")
        .await
        .unwrap();
    assert!(
        unknown.contains("unavailable()") && unknown.contains("Possible"),
        "{unknown}"
    );
    let writes = app
        .search(path, "writes:Game::Renderer::position lang:cpp")
        .await
        .unwrap();
    assert!(writes.contains("r.position += 2"), "{writes}");
    let bases = app
        .search(path, "derived:Game::Renderer lang:cpp")
        .await
        .unwrap();
    assert!(
        bases.contains("Child") && !bases.contains("Grandchild"),
        "{bases}"
    );
    write(
        root.path(),
        "engine.cpp",
        "void replacement() { newest(); }\n",
    );
    assert_eq!(
        app.search(path, "calls:unavailable lang:cpp")
            .await
            .unwrap(),
        "No matches."
    );
    assert!(
        app.search(path, "calls:newest lang:cpp")
            .await
            .unwrap()
            .contains("newest()")
    );
    std::fs::remove_file(root.path().join("engine.cpp")).unwrap();
    assert_eq!(
        app.search(path, "function:replacement lang:cpp")
            .await
            .unwrap(),
        "No matches."
    );
}

#[tokio::test]
async fn native_and_shader_files_survive_managed_discovery_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='mixed'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(root.path(), "src/lib.rs", "pub fn managed() {}\n");
    write(
        root.path(),
        "Native~/plugin.c",
        "int native_entry(void) { return external(); }\n",
    );
    write(
        root.path(),
        "Assets/Mixed.shader",
        r#"Shader "Mixed" {
Properties { _Color ("Color", Color) = (1,1,1,1) }
HLSLINCLUDE
float4 shade(float4 p) { return transform(p); }
ENDHLSL
Pass { HLSLPROGRAM
#pragma vertex vert
float4 vert(float4 p : POSITION) : SV_POSITION { return shade(p); }
ENDHLSL }
Pass { GLSLPROGRAM
vec4 color;
void main() { color = sample_color(); }
ENDGLSL }
}"#,
    );
    let path = root.path().to_str().unwrap();
    for _ in 0..2 {
        let app = app(root.path(), cache.path());
        assert!(
            app.search(path, "function:native_entry lang:c")
                .await
                .unwrap()
                .contains("Native~/plugin.c")
        );
        let shader = app.search(path, "function:shade lang:hlsl").await.unwrap();
        assert!(
            shader.contains("Mixed.shader") && shader.contains("float4 shade"),
            "{shader}"
        );
        assert!(
            app.search(path, "uses:vert lang:hlsl")
                .await
                .unwrap()
                .contains("#pragma vertex vert")
        );
        assert!(
            app.search(path, "writes:color lang:glsl")
                .await
                .unwrap()
                .contains("sample_color()")
        );
        assert_eq!(
            app.search(path, "function:main lang:hlsl").await.unwrap(),
            "No matches."
        );
        assert_eq!(
            app.search(path, "symbol:_Color wait:complete")
                .await
                .unwrap(),
            "No matches."
        );
        assert!(
            app.view(path, "Mixed.shader", "minified")
                .await
                .unwrap()
                .contains("Properties")
        );
    }
}
