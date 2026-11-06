//! Opt-in binding replay using an existing source tree and editor metadata archive.
use super::*;

#[test]
#[ignore = "requires a source corpus and published Unity metadata"]
fn replay_unity_bindings() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .try_init();
    let root = PathBuf::from(std::env::var("SIGLA_BINDING_ROOT")?).canonicalize()?;
    let cache = PathBuf::from(std::env::var("SIGLA_BINDING_CACHE")?);
    let data = PathBuf::from(std::env::var("SIGLA_BINDING_EDITOR")?).canonicalize()?;
    let manifest = crate::metadata_archive::manifest(&data.join("manifest.bin"))?;
    anyhow::ensure!(manifest.origin.kind == "editor", "Expected editor metadata");
    let version = std::fs::read_to_string(root.join("ProjectSettings/ProjectVersion.txt"))?;
    let declared = version
        .lines()
        .find_map(|line| line.strip_prefix("m_EditorVersion:"))
        .context("Missing editor version")?
        .trim()
        .parse()?;
    let editor = crate::unity::catalog::Editor {
        declared,
        selected: manifest.origin.version.parse()?,
        data,
        declared_revision: None,
        selected_revision: None,
    };
    let mut policy = Policy::new(vec![root.clone()])?;
    if std::env::var_os("SIGLA_BINDING_ACQUIRE").is_some() {
        policy.remote = Some(discovery::RemoteContext {
            workspace: root.clone(),
            writable: root.clone(),
            shared: cache.clone(),
            repositories: Vec::new(),
            selection_identity: String::new(),
            tracked: Default::default(),
        });
    }
    let mut workspace = Workspace::open(
        root.clone(),
        &cache,
        policy.clone(),
        &cache.join("analysis"),
        None,
        Default::default(),
    )?;
    let started = std::time::Instant::now();
    let mut discovery = discovery::Discovery {
        leases: workspace.leases.clone(),
        root: root.clone(),
        projects: Vec::new(),
        sources: Vec::new(),
        metadata: BTreeSet::new(),
        dependencies: BTreeSet::new(),
        diagnostics: Vec::new(),
    };
    crate::unity::discover_with_editor(&root, &policy, &cache, &mut discovery, &editor)?;
    eprintln!(
        "projects={} sources={} diagnostics={:?}",
        discovery.projects.len(),
        discovery.sources.len(),
        discovery.diagnostics
    );
    workspace.apply(Preparation {
        discovery,
        fence: workspace.fence,
        started,
        validation: Default::default(),
        discovery_time: started.elapsed(),
    })?;
    let query = crate::query::Query::parse(&std::env::var("SIGLA_BINDING_QUERY")?)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let started = std::time::Instant::now();
    let result =
        crate::search::Search::new(&workspace.store, &workspace.manifest, &cancel)?.run(&query);
    eprintln!(
        "query_elapsed_ms={} result={result:?}",
        started.elapsed().as_millis()
    );
    result.map(|_| ())
}
