use sigla::discovery::{Policy, discover_cached};
use std::path::Path;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn package_views_share_storage_and_isolate_project_writes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let cache = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for owner in ["a", "b"] {
        let packages = cache.path().join(owner).join("packages");
        write(&packages, "fixture/1.0/run", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(
            packages.join("fixture/1.0/run"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        write(&packages, "fixture/1.0/removed.txt", "remove this file");
        write(
            &packages,
            "fixture/1.0/replaced/old.txt",
            "remove this directory",
        );
    }
    let run = |source: &Path, owner: &str, value: &str| {
        write(
            source,
            "NuGet.Config",
            "<configuration><packageSources><clear /></packageSources></configuration>",
        );
        write(source, "App.cs", "public class App {}");
        write(
            source,
            "App.csproj",
            &format!(
                r#"<Project Sdk="Microsoft.NET.Sdk">
          <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
          <Target Name="PackageFixture" BeforeTargets="PrepareForBuild">
            <Delete Files="$(NUGET_PACKAGES)/fixture/1.0/removed.txt" Condition="'{value}' == 'modified'" />
            <RemoveDir Directories="$(NUGET_PACKAGES)/fixture/1.0/replaced" Condition="'{value}' == 'modified'" />
            <MakeDir Directories="$(NUGET_PACKAGES)/fixture/1.0/replaced" Condition="'{value}' == 'modified'" />
            <WriteLinesToFile File="$(NUGET_PACKAGES)/fixture/1.0/replaced/new.txt" Lines="new" Condition="'{value}' == 'modified'" />
            <MakeDir Directories="$(NUGET_PACKAGES)/fixture/1.0" />
            <WriteLinesToFile File="$(NUGET_PACKAGES)/fixture/1.0/input.txt" Lines="{value}" Overwrite="true" />
          </Target>
        </Project>"#
            ),
        );
        let writable = cache.path().join(owner);
        let mut policy = Policy::new(vec![source.into()]).unwrap();
        policy.remote = Some(sigla::discovery::RemoteContext {
            workspace: source.into(),
            writable: writable.clone(),
            shared: cache.path().into(),
            repositories: vec![],
            selection_identity: String::new(),
            tracked: Default::default(),
        });
        let discovery = discover_cached(
            &source.join("App.csproj"),
            &policy,
            &writable.join("analysis"),
        )
        .unwrap();
        assert!(
            discovery.sources.iter().any(|s| s.path.ends_with("App.cs")),
            "{:?}",
            discovery.diagnostics
        );
        writable.join("packages/fixture/1.0/input.txt")
    };
    let first = run(a.path(), "a", "original");
    let second = run(b.path(), "b", "original");
    let before = std::fs::read(&second).unwrap();
    assert_eq!(
        std::fs::metadata(&first).unwrap().ino(),
        std::fs::metadata(&second).unwrap().ino()
    );
    run(a.path(), "a", "modified");
    assert_eq!(std::fs::read(&second).unwrap(), before);
    assert_ne!(std::fs::read(&first).unwrap(), before);
    assert_ne!(
        std::fs::metadata(&first).unwrap().ino(),
        std::fs::metadata(&second).unwrap().ino()
    );
    let changed = first.parent().unwrap();
    let untouched = second.parent().unwrap();
    assert!(!changed.join("removed.txt").exists());
    assert!(!changed.join("replaced/old.txt").exists());
    assert!(changed.join("replaced/new.txt").is_file());
    assert!(untouched.join("removed.txt").is_file());
    assert!(untouched.join("replaced/old.txt").is_file());
    let executable = std::fs::metadata(changed.join("run")).unwrap();
    assert!(executable.mode() & 0o111 != 0);
    assert_eq!(
        executable.ino(),
        std::fs::metadata(untouched.join("run")).unwrap().ino()
    );
}

#[test]
fn restore_failure_keeps_project_context_and_readable_sources() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let diagnostic = "Restore deliberately rejected by project";
    write(
        root.path(),
        "Broken.csproj",
        &format!(
            r#"<Project Sdk="Microsoft.NET.Sdk">
      <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
      <Target Name="RejectRestore" BeforeTargets="Restore">
        <Error Text="{diagnostic}" />
      </Target>
    </Project>"#
        ),
    );
    write(root.path(), "Broken.cs", "public class Broken {}");
    let entry = root.path().join("Broken.csproj");
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let result = discover_cached(&entry, &policy, cache.path()).unwrap();
    let message = result.diagnostics.join("\n");
    assert!(!message.is_empty());
    assert!(message.contains(&entry.display().to_string()), "{message}");
    assert!(
        result
            .sources
            .iter()
            .any(|source| source.path.ends_with("Broken.cs"))
    );
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
        snapshot.diagnostics.is_empty(),
        "{:?}",
        snapshot.diagnostics
    );
    for project in ["App", "Library"] {
        assert!(!root.path().join(project).join("obj").exists());
        assert!(!root.path().join(project).join("bin").exists());
    }
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
fn same_named_projects_restore_to_separate_cached_outputs() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "NuGet.Config",
        "<configuration><packageSources><clear /></packageSources></configuration>",
    );
    write(
        root.path(),
        "Directory.Build.props",
        "<Project><PropertyGroup><ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable></PropertyGroup></Project>",
    );
    write(
        root.path(),
        "Library/Shared.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFrameworks>net10.0;net10.0-windows</TargetFrameworks><AssemblyName>Library</AssemblyName><LangVersion>latest</LangVersion></PropertyGroup></Project>"#,
    );
    write(
        root.path(),
        "Library/Library.cs",
        "public class LibraryType {}",
    );
    write(
        root.path(),
        "App/Shared.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework><AssemblyName>App</AssemblyName></PropertyGroup><ItemGroup><ProjectReference Include="../Library/Shared.csproj" /></ItemGroup></Project>"#,
    );
    write(
        root.path(),
        "App/App.cs",
        "public class AppType : LibraryType {}",
    );
    write(
        root.path(),
        "App/obj/Old.cs",
        "#error Stale generated source must stay excluded",
    );
    write(
        root.path(),
        "App/bin/Old.cs",
        "#error Stale output must stay excluded",
    );
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let entry = root.path().join("App/Shared.csproj");
    for _ in 0..2 {
        let result = discover_cached(&entry, &policy, cache.path()).unwrap();
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let assets: std::collections::HashSet<_> = result
            .projects
            .iter()
            .filter(|p| p.origin.is_some())
            .map(|p| p.compiler_options["ProjectAssetsFile"].clone())
            .collect();
        let origins: std::collections::HashSet<_> = result
            .projects
            .iter()
            .filter(|p| p.origin.is_some())
            .map(|p| p.origin.clone())
            .collect();
        assert_eq!(assets.len(), origins.len());
        let intermediates: std::collections::HashSet<_> = result
            .projects
            .iter()
            .filter(|p| p.origin.is_some())
            .map(|p| p.compiler_options["IntermediateOutputPath"].clone())
            .collect();
        assert_eq!(
            intermediates.len(),
            result
                .projects
                .iter()
                .filter(|p| p.origin.is_some())
                .count()
        );
        for path in assets {
            assert!(Path::new(&path).starts_with(cache.path()));
            assert!(Path::new(&path).is_file());
        }
        assert!(!result.sources.iter().any(|s| s.path.ends_with("Old.cs")));
        assert!(
            result
                .sources
                .iter()
                .any(|s| s.path.starts_with(cache.path())
                    && s.path.to_string_lossy().contains("GlobalUsings"))
        );
        let app = result.projects.iter().find(|p| p.name == "App").unwrap();
        assert!(
            result
                .projects
                .iter()
                .filter(|p| p.name == "Library")
                .any(|library| app.references.iter().any(|r| r.visible(&library.identity)))
        );
    }
    assert!(!root.path().join("Library/obj").exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("App/obj"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("App/bin"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn local_custom_targets_cannot_write_to_checkout() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "NuGet.Config",
        "<configuration><packageSources><clear /></packageSources></configuration>",
    );
    write(
        root.path(),
        "Project.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk">
      <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
      <Target Name="WriteIntoCheckout" BeforeTargets="PrepareForBuild">
        <WriteLinesToFile File="$(MSBuildProjectDirectory)/unexpected.txt" Lines="unexpected" />
      </Target>
    </Project>"#,
    );
    write(root.path(), "Source.cs", "class StillReadable {}");
    let policy = Policy::new(vec![root.path().into()]).unwrap();
    let result =
        discover_cached(&root.path().join("Project.csproj"), &policy, cache.path()).unwrap();
    assert!(!root.path().join("unexpected.txt").exists());
    assert!(!result.diagnostics.is_empty());
    assert!(result.sources.iter().any(|s| s.path.ends_with("Source.cs")));
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
    let sources: Vec<_> = snapshot
        .sources
        .iter()
        .filter(|s| s.language == sigla::model::Language::CSharp)
        .collect();
    assert_eq!(sources.len(), 1);
    assert!(sources[0].path.ends_with("Selected.cs"));
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
    let sources: Vec<_> = result
        .sources
        .iter()
        .filter(|s| s.language == sigla::model::Language::CSharp)
        .collect();
    assert_eq!(sources.len(), 1);
    assert!(sources[0].path.ends_with("Included.code"));
}
