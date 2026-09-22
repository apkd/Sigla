use sigla::discovery::{Policy, discover_cached};
use std::path::Path;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn remote_discovery_confines_generated_files_and_hides_host_files() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let private = tempfile::NamedTempFile::new().unwrap();
    write(
        root.path(),
        "NuGet.Config",
        "<configuration><packageSources><clear /></packageSources></configuration>",
    );
    write(
        root.path(),
        "Game.csproj",
        &format!(
            r#"<Project Sdk="Microsoft.NET.Sdk">
      <PropertyGroup><TargetFramework>net10.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup>
      <ItemGroup><Compile Include="Future/**/*.cs" /></ItemGroup>
      <Target Name="CheckIsolation" BeforeTargets="PrepareForBuild">
        <Error Condition="Exists('{}')" Text="Host file was exposed" />
      </Target>
    </Project>"#,
            private.path().display()
        ),
    );
    write(root.path(), "Game.cs", "public class Game {}");
    let mut policy = Policy::new(vec![root.path().into()]).unwrap();
    policy.remote = Some(sigla::discovery::RemoteContext {
        workspace: root.path().into(),
        writable: cache.path().join("writable"),
        shared: cache.path().into(),
        repositories: vec![],
        selection_identity: String::new(),
        tracked: Default::default(),
    });
    let result = discover_cached(&root.path().join("Game.csproj"), &policy, cache.path()).unwrap();
    assert!(
        result
            .sources
            .iter()
            .any(|source| source.path.starts_with(cache.path())
                && source.path.extension().is_some_and(|e| e == "cs"))
    );
    assert!(!root.path().join("obj").exists());
    assert!(!root.path().join("bin").exists());
    assert!(result.sources.iter().all(
        |source| source.path.starts_with(root.path()) || source.path.starts_with(cache.path())
    ));
}

#[test]
fn sdk_membership_generated_inputs_and_unbuilt_references() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Library/Library.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>"#,
    );
    write(
        root.path(),
        "Library/Library.cs",
        "public class FromLibrary {}",
    );
    write(
        root.path(),
        "App/App.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk">
      <PropertyGroup><TargetFramework>net10.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable></PropertyGroup>
      <ItemGroup><Compile Remove="Omitted.cs"/><ProjectReference Include="../Library/Library.csproj"/></ItemGroup>
    </Project>"#,
    );
    write(
        root.path(),
        "App/Included.cs",
        "public class Included : FromLibrary {}",
    );
    write(root.path(), "App/Omitted.cs", "public class Omitted {}");
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let snapshot =
        discover_cached(&root.path().join("App/App.csproj"), &policy, cache.path()).unwrap();
    assert!(
        snapshot
            .sources
            .iter()
            .any(|s| s.path.ends_with("Included.cs"))
    );
    assert!(
        !snapshot
            .sources
            .iter()
            .any(|s| s.path.ends_with("Omitted.cs"))
    );
    assert!(
        snapshot
            .sources
            .iter()
            .any(|s| s.path.to_string_lossy().contains("GlobalUsings"))
    );
    let app = snapshot
        .projects
        .iter()
        .find(|p| p.origin.as_ref().unwrap().ends_with("App.csproj"))
        .unwrap();
    let library = snapshot
        .projects
        .iter()
        .find(|p| p.origin.as_ref().unwrap().ends_with("Library.csproj"))
        .unwrap();
    assert!(app.references.iter().any(|r| r.visible(&library.identity)));
    assert!(!app.assemblies.is_empty());
    assert!(
        !root
            .path()
            .join("Library/bin/Debug/net10.0/Library.dll")
            .exists()
    );
    assert!(!root.path().join("App/bin/Debug/net10.0/App.dll").exists());
}

#[test]
fn msbuild_evaluates_conditions_imports_and_globs() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Directory.Build.props",
        r#"<Project><PropertyGroup><IncludeSelected>true</IncludeSelected></PropertyGroup></Project>"#,
    );
    write(
        root.path(),
        "Nested/Project.csproj",
        r#"<Project>
      <Import Project="../Directory.Build.props" />
      <ItemGroup Condition="'$(IncludeSelected)' == 'true'"><Compile Include="Sources/**/*.cs" Exclude="Sources/Excluded.cs" /></ItemGroup>
    </Project>"#,
    );
    write(
        root.path(),
        "Nested/Sources/Selected.cs",
        "class Selected {}",
    );
    write(
        root.path(),
        "Nested/Sources/Excluded.cs",
        "class Excluded {}",
    );
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let snapshot = discover_cached(root.path(), &policy, cache.path()).unwrap();
    assert_eq!(snapshot.sources.len(), 1);
    assert!(snapshot.sources[0].path.ends_with("Selected.cs"));
    assert!(
        snapshot
            .metadata
            .contains(&root.path().join("Directory.Build.props"))
    );
}

#[test]
fn remote_requests_omitted_imports_and_evaluated_sources() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "Project.csproj",
        r#"<Project>
      <Import Project="inputs/membership.data" Condition="Exists('inputs/membership.data')" />
    </Project>"#,
    );
    let mut policy = Policy::new(vec![root.path().into()]).unwrap();
    policy.remote = Some(sigla::discovery::RemoteContext {
        workspace: root.path().into(),
        writable: cache.path().join("writable"),
        shared: cache.path().into(),
        repositories: vec![],
        selection_identity: String::new(),
        tracked: std::sync::Arc::new(
            [
                "Project.csproj",
                "inputs/membership.data",
                "Sources/Included.code",
                "Sources/Excluded.code",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        ),
    });
    let entry = root.path().join("Project.csproj");
    let error = discover_cached(&entry, &policy, cache.path())
        .err()
        .expect("Omitted import must be requested");
    assert_eq!(
        error
            .downcast_ref::<sigla::discovery::RequiredInputs>()
            .unwrap()
            .0,
        ["inputs/membership.data"]
    );
    write(
        root.path(),
        "inputs/membership.data",
        r#"<Project><ItemGroup><Compile Include="Sources/**/*.code" Exclude="Sources/Excluded.code" /></ItemGroup></Project>"#,
    );
    let error = discover_cached(&entry, &policy, cache.path())
        .err()
        .expect("Omitted source must be requested");
    assert_eq!(
        error
            .downcast_ref::<sigla::discovery::RequiredInputs>()
            .unwrap_or_else(|| panic!("{error:#}"))
            .0,
        ["Sources/Included.code"]
    );
    write(root.path(), "Sources/Included.code", "class Included {}");
    let result = discover_cached(&entry, &policy, cache.path()).unwrap();
    assert_eq!(result.sources.len(), 1);
    assert!(result.sources[0].path.ends_with("Included.code"));
}
