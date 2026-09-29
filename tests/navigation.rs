use sigla::{discovery::Policy, service::App};
use std::{path::Path, sync::Arc};

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

#[tokio::test]
async fn browse_find_and_read_use_the_same_refreshed_sources() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='navigation'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(
        root.path(),
        "src/lib.rs",
        "mod first; mod second;\npub struct Entry;\n",
    );
    write(root.path(), "src/first/mod.rs", "pub struct First;\n");
    write(root.path(), "src/second/mod.rs", "pub struct Second;\n");
    write(root.path(), "README.md", "Not indexed");
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    let project = root.path().to_str().unwrap();
    let tree = app.browse(project, "").await.unwrap();
    assert!(
        tree.contains("first/") && tree.contains("second/") && tree.contains("lib.rs"),
        "{tree}"
    );
    assert!(
        !tree.contains("struct") && !tree.contains("README"),
        "{tree}"
    );
    let files = app.search(project, "file:mod.rs").await.unwrap();
    assert!(
        files.contains("src/first/mod.rs") && files.contains("src/second/mod.rs"),
        "{files}"
    );
    let ambiguous = app.view(project, "mod.rs", "exact").await.unwrap();
    assert!(
        ambiguous.contains("src/first/mod.rs") && ambiguous.contains("src/second/mod.rs"),
        "{ambiguous}"
    );
    assert!(!ambiguous.contains("pub struct"));
    let source = "// leading spaces and CRLF stay exact\r\n  pub struct Changed;\r\n";
    write(root.path(), "src/first/mod.rs", source);
    let exact = app.view(project, "first/mod.rs", "exact").await.unwrap();
    assert!(exact.contains(source), "{exact}");
    assert_eq!(
        app.view(project, "…/first/mod.rs", "exact").await.unwrap(),
        exact
    );
    let ambiguous_hint = app.view(project, "…/mod.rs", "exact").await.unwrap();
    assert_eq!(ambiguous_hint, ambiguous);
    for suffix in [":2", ":2-2", "#L2-L2", ":2:3", "(2,3)"] {
        let viewed = app
            .view(project, &format!("…/first/mod.rs{suffix}"), "exact")
            .await
            .unwrap();
        assert!(
            viewed.contains("  pub struct Changed;\r\n") && !viewed.contains("leading spaces"),
            "{viewed}"
        );
    }
    let absolute = app
        .view(
            project,
            root.path().join("src/first/mod.rs").to_str().unwrap(),
            "exact",
        )
        .await
        .unwrap();
    assert_eq!(absolute, exact);
    assert_eq!(
        app.view(project, r"src\first\mod.rs", "exact")
            .await
            .unwrap(),
        exact
    );
    assert_eq!(
        app.view(project, r"src\first\mod.rs:2", "exact")
            .await
            .unwrap(),
        app.view(project, "src/first/mod.rs:2", "exact")
            .await
            .unwrap()
    );
    assert_eq!(
        app.browse(project, r"src\first").await.unwrap(),
        app.browse(project, "src/first").await.unwrap()
    );
    let file_hint = app.browse(project, "src/first/mod.rs").await.unwrap();
    assert!(
        file_hint.contains("`view(\"src/first/mod.rs\")`")
            && file_hint.contains("`browse(\"src/first\")`"),
        "{file_hint}"
    );
    assert!(app.view(project, "../outside.rs", "exact").await.is_err());
    for suffix in [":1-abc", ":-1-5"] {
        let error = app
            .view(project, &format!("first/mod.rs{suffix}"), "exact")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("range"), "{error}");
    }
    assert!(app.view(project, "lib.rs:999", "exact").await.is_err());
    let fuzzy = app.view(project, "firs/mod.rs:2", "exact").await.unwrap();
    assert!(fuzzy.contains("Changed"), "{fuzzy}");
    let filtered = app
        .search(project, "file:*.rs path:src/first")
        .await
        .unwrap();
    assert!(
        filtered.contains("first/mod.rs") && !filtered.contains("second/mod.rs"),
        "{filtered}"
    );
}

#[tokio::test]
async fn minified_view_is_read_only_and_keeps_original_line_locations() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Code.csproj",
        "<Project><ItemGroup><Compile Include=\"Code.cs\" /></ItemGroup></Project>",
    );
    let source = "namespace Example\n{\n    class Example\n    {\n        public int Add(int left, int right) { return left + right; }\n    }\n}\n";
    write(root.path(), "Code.cs", source);
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            1,
        )
        .unwrap(),
    );
    let project = root.path().to_str().unwrap();
    let view = app.view(project, "Code.cs:5", "minified").await.unwrap();
    assert!(
        view.contains("Code.cs:5`") && view.contains("=>left+right;"),
        "{view}"
    );
    assert!(!view.contains("```cs"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("Code.cs")).unwrap(),
        source
    );
    let exact = app.view(project, "Code.cs", "exact").await.unwrap();
    assert!(exact.contains(source), "{exact}");
}
