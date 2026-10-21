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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_native_trees_complete_in_an_isolated_worker() {
    const CHILD: &str = "SIGLA_TEST_DEEP_NATIVE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Stack overflow aborts cannot be caught inside the test runner.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "native::deep_native_trees_complete_in_an_isolated_worker",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("Native extraction must complete within its replay budget")
        .unwrap();
        assert!(
            output.status.success(),
            "Child failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    // Reduced from a generated FidelityFX header whose recovery tree contains
    // a deep comma expression. This used to abort a normal blocking worker.
    write(
        root.path(),
        "generated.h",
        &format!("#if 0\n{}\n}};\n", "0x1, ".repeat(3000)),
    );
    write(
        root.path(),
        "healthy.cpp",
        "int healthy() { return 1; } void caller() { healthy(); }",
    );
    let app = app(root.path(), cache.path());
    let result = app
        .search(root.path().to_str().unwrap(), "function:healthy lang:cpp")
        .await
        .unwrap();
    assert!(result.contains("healthy.cpp"), "{result}");
    let navigation = app
        .view(root.path().to_str().unwrap(), "healthy.cpp", "exact")
        .await
        .unwrap();
    assert!(navigation.contains("healthy()"), "{navigation}");
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
#[tokio::test]
async fn ng_shader_navigation_refresh_and_restart() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let source = "BEGIN_PROPERTIES\n_Tint(\"Tint\", Color) = (1,1,1,1)\nEND_PROPERTIES\nBEGIN_VARYINGS\n* : float4 lineData linear\nEND_VARYINGS\nBEGIN_CODE\n#include \"Shared.hlsl\"\nvoid Modify(inout VertexData v) { v.lineData = _Tint; }\nfloat4 SurfaceFunction() { return Shared(_Tint); }\nEND_CODE\n";
    write(root.path(), "Surface.ngsurface", source);
    write(
        root.path(),
        "Shared.hlsl",
        "float4 Shared(float4 value) { return value; }\n",
    );
    write(
        root.path(),
        "Pass.ngshader",
        "#pragma native_pass Draw _ Vert Frag\nfloat4 Vert() { return 0; }\nfloat4 Frag() { return 1; }\n",
    );
    write(
        root.path(),
        "Compute.ngcompute",
        "#pragma kernel Kernel\n[numthreads(1,1,1)] void Kernel() {}\n",
    );
    write(
        root.path(),
        "Legacy.ngsurfshader",
        "BEGIN_CODE\nvoid Legacy() {}\nEND_CODE\n",
    );
    let path = root.path().to_str().unwrap();
    for _ in 0..2 {
        let app = app(root.path(), cache.path());
        for (query, expected) in [
            ("file:*.ngsurface", "Surface.ngsurface"),
            ("property:_Tint lang:shaders", "Surface.ngsurface"),
            ("field:lineData lang:shaders", "Surface.ngsurface"),
            ("function:SurfaceFunction lang:hlsl", "Surface.ngsurface"),
            ("function:Kernel lang:shaders", "Compute.ngcompute"),
            ("function:Legacy lang:shaders", "Legacy.ngsurfshader"),
            ("writes:lineData lang:shaders", "v.lineData = _Tint"),
            ("uses:Vert lang:shaders", "#pragma native_pass"),
            ("@Pass.ngshader:1:28", "function:Vert()"),
            ("@Surface.ngsurface:2:1", "_Tint"),
            ("@Surface.ngsurface:9:48", "property:_Tint"),
            ("@Surface.ngsurface:8:12", "Include: `Shared.hlsl`"),
        ] {
            let result = app.search(path, query).await.unwrap();
            assert!(result.contains(expected), "{query}: {result}");
        }
        assert_eq!(
            app.search(path, "calls:Vert lang:shaders").await.unwrap(),
            "No matches."
        );
        let viewed = app.view(path, "Surface.ngsurface", "exact").await.unwrap();
        assert!(
            viewed.contains("BEGIN_PROPERTIES") && viewed.contains("BEGIN_CODE"),
            "{viewed}"
        );
    }
    let app = app(root.path(), cache.path());
    write(
        root.path(),
        "Surface.ngsurface",
        &source.replace("_Tint", "_Color"),
    );
    assert_eq!(
        app.search(path, "property:_Tint lang:shaders")
            .await
            .unwrap(),
        "No matches."
    );
    assert!(
        app.search(path, "property:_Color lang:shaders")
            .await
            .unwrap()
            .contains("Surface.ngsurface")
    );
    std::fs::rename(
        root.path().join("Surface.ngsurface"),
        root.path().join("Renamed.ngsurface"),
    )
    .unwrap();
    let renamed = app
        .search(path, "property:_Color lang:shaders")
        .await
        .unwrap();
    assert!(
        renamed.contains("Renamed.ngsurface") && !renamed.contains("Surface.ngsurface"),
        "{renamed}"
    );
    std::fs::remove_file(root.path().join("Renamed.ngsurface")).unwrap();
    assert_eq!(
        app.search(path, "property:_Color lang:shaders")
            .await
            .unwrap(),
        "No matches."
    );
    write(root.path(), "Added.ngsurface", source);
    assert!(
        app.search(path, "property:_Tint lang:shaders")
            .await
            .unwrap()
            .contains("Added.ngsurface")
    );
}
