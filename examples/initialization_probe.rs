//! Repeatable initialization workload with shared immutable inputs and queries
//! running while several new workspace views attach to the same analysis.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use sigla::{cache::blobs, discovery::Policy, service::App};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Parser)]
struct Options {
    /// Empty directory on the filesystem being measured.
    directory: PathBuf,
    #[arg(long, default_value_t = 5000)]
    files: usize,
    #[arg(long, default_value_t = 4)]
    workspaces: usize,
    /// Add cold and warm views over a Unity asset tree.
    #[arg(long, default_value_t = 0)]
    assets: usize,
    /// Compete with a continuous data writer on the same filesystem.
    #[arg(long)]
    io_load: bool,
}

struct IoLoad {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<u64>>>,
}
impl IoLoad {
    fn start(path: &Path, enabled: bool) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker = if enabled {
            let mut file = tempfile::tempfile_in(path)?;
            let stopping = stop.clone();
            Some(std::thread::spawn(move || {
                let bytes = vec![0x5a; 8 * 1024 * 1024];
                let mut written = 0;
                while !stopping.load(Ordering::Relaxed) {
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&bytes)?;
                    file.sync_data()?;
                    written += bytes.len() as u64;
                }
                Ok(written)
            }))
        } else {
            None
        };
        Ok(Self { stop, worker })
    }
    fn finish(mut self) -> Result<u64> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().map_or(Ok(0), |worker| {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("I/O worker panicked"))?
        })
    }
}
impl Drop for IoLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn fixture(root: &Path, files: usize) -> Result<()> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='initialization_probe'\nversion='0.1.0'\nedition='2021'\n",
    )?;
    let mut modules = String::new();
    for file in 0..files {
        modules.push_str(&format!("pub mod item{file};\n"));
        let mut text = format!("pub struct Item{file} {{ pub value: u64 }}\nimpl Item{file} {{\n");
        for method in 0..24 {
            text.push_str(&format!("pub fn operation{method}(&self, input: u64) -> u64 {{ self.value + input + {method} }}\n"));
        }
        text.push_str("}\n");
        fs::write(root.join(format!("src/item{file}.rs")), text)?;
        if file % 32 == 0 {
            fs::write(
                root.join(format!("native{file}.cpp")),
                format!(
                    "struct Native{file} {{ int value; int read() const {{ return value; }} }};"
                ),
            )?;
        }
    }
    fs::write(root.join("src/lib.rs"), modules)?;
    Ok(())
}
fn link_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            link_tree(&entry.path(), &destination)?;
        } else {
            fs::hard_link(entry.path(), destination)?;
        }
    }
    Ok(())
}
fn app(cache: &Path, roots: &[PathBuf]) -> Result<Arc<App>> {
    Ok(Arc::new(App::new(
        Policy::new(roots.to_vec())?,
        cache.into(),
        2,
    )?))
}
async fn query(app: &Arc<App>, path: &Path) -> Result<f64> {
    let start = Instant::now();
    let result = app
        .search(
            path.to_str().context("Non-UTF8 fixture path")?,
            "type:Item0 wait:complete",
        )
        .await?;
    ensure!(
        result.contains("Item0"),
        "Fixture declarations were not ready: {result}"
    );
    Ok(start.elapsed().as_secs_f64())
}
async fn run(options: Options) -> Result<()> {
    ensure!(
        options.files > 0 && options.workspaces >= 2,
        "Use at least one file and two workspaces"
    );
    fs::create_dir_all(&options.directory)?;
    ensure!(
        fs::read_dir(&options.directory)?.next().is_none(),
        "Benchmark directory must be empty"
    );
    let cache = options.directory.canonicalize()?;
    sigla::cache::reset::prepare(&cache)?;
    let roots: Vec<_> = (0..options.workspaces)
        .map(|index| cache.join(format!("view{index}")))
        .collect();
    let asset_root = cache.join("asset-view");
    let asset_source = "--- !u!1 &1\nGameObject:\n  m_Name: Item0\n  m_Component: []\n";
    if options.assets > 0 {
        fs::create_dir_all(asset_root.join("Assets"))?;
        fs::create_dir(asset_root.join("ProjectSettings"))?;
        fs::write(
            asset_root.join("ProjectSettings/ProjectVersion.txt"),
            "version",
        )?;
        fs::write(asset_root.join("code.rs"), "pub struct Available;")?;
        for file in 0..options.assets {
            fs::write(
                asset_root.join(format!("Assets/Item{file}.prefab")),
                asset_source.replace("Item0", &format!("Item{file}")),
            )?;
        }
    }
    let setup = Instant::now();
    fixture(&roots[0], options.files)?;
    let blobs = blobs::Store::open(&cache)?;
    blobs.import_tree(&roots[0])?;
    for root in &roots[1..] {
        link_tree(&roots[0], root)?;
    }
    eprintln!(
        "fixture: {} files per workspace, {:.3}s",
        options.files,
        setup.elapsed().as_secs_f64()
    );
    let load = IoLoad::start(&cache, options.io_load)?;
    let mut allowed = roots.clone();
    if options.assets > 0 {
        allowed.push(asset_root.clone());
    }
    let service = app(&cache, &allowed)?;
    let cold = query(&service, &roots[0]).await?;
    let mut pending = tokio::task::JoinSet::new();
    let attach = Instant::now();
    for root in &roots[1..] {
        let service = service.clone();
        let root = root.clone();
        pending.spawn(async move { query(&service, &root).await });
    }
    let mut reused = Vec::new();
    let mut foreground_peak = 0.0f64;
    let mut foreground_queries = 0;
    while !pending.is_empty() {
        tokio::select! {
            biased;
            result = pending.join_next() => reused.push(result.unwrap()??),
            result = query(&service, &roots[0]) => {
                foreground_peak = foreground_peak.max(result?);
                foreground_queries += 1;
            }
        }
    }
    let attach = attach.elapsed().as_secs_f64();
    let warm = query(&service, &roots[0]).await?;
    let mut queries = std::collections::BTreeMap::new();
    for (name, text) in [
        ("exact path", "text:value path:src/item0.rs"),
        ("directory path", "text:value path:src/"),
        ("all sources", "text:value wait:complete"),
    ] {
        let expected = service.search(roots[0].to_str().unwrap(), text).await?;
        let start = Instant::now();
        for _ in 0..10 {
            let result = service.search(roots[0].to_str().unwrap(), text).await?;
            ensure!(result == expected, "Repeated query output changed: {name}");
        }
        let seconds = start.elapsed().as_secs_f64() / 10.0;
        queries.insert(name, serde_json::json!({"seconds":seconds,"output_hash":blake3::hash(expected.as_bytes()).to_hex().to_string()}));
        eprintln!("warm {name}: {:.3} ms/query", seconds * 1000.0);
    }
    service
        .view(roots[0].to_str().unwrap(), "src/item0.rs", "raw")
        .await?;
    let start = Instant::now();
    for _ in 0..10 {
        service
            .view(roots[0].to_str().unwrap(), "src/item0.rs", "raw")
            .await?;
    }
    let raw_view = start.elapsed().as_secs_f64() / 10.0;
    eprintln!("warm raw view: {:.3} ms/query", raw_view * 1000.0);
    let asset_views = if options.assets > 0 {
        let entry = asset_root.join("code.rs");
        let entry = entry.to_str().context("Non-UTF8 asset fixture path")?;
        let start = Instant::now();
        let result = service.view(entry, "Assets/Item0.prefab", "raw").await?;
        let cold = start.elapsed().as_secs_f64();
        ensure!(result == asset_source, "Cold asset source changed");
        // Let the independent code job finish before measuring steady-state views.
        service
            .search(entry, "type:Available lang:rust wait:complete")
            .await?;
        let start = Instant::now();
        for _ in 0..10 {
            let result = service.view(entry, "Assets/Item0.prefab", "raw").await?;
            ensure!(result == asset_source, "Warm asset source changed");
        }
        Some(
            serde_json::json!({"cold_seconds":cold,"warm_seconds":start.elapsed().as_secs_f64()/10.0,"output_hash":blake3::hash(asset_source.as_bytes()).to_hex().to_string()}),
        )
    } else {
        None
    };
    service.shutdown().await;
    drop(service);
    let restarting = Instant::now();
    let service = app(&cache, &roots)?;
    for root in &roots {
        query(&service, root).await?;
    }
    let restart = restarting.elapsed().as_secs_f64();
    service.shutdown().await;
    let competing_writes = load.finish()?;
    let memory: std::collections::BTreeMap<_, _> = fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| matches!(*name, "VmHWM" | "VmSwap"))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect();
    println!(
        "{}",
        serde_json::json!({"files":options.files,"workspaces":options.workspaces,"assets":options.assets,"cold_seconds":cold,"reused_seconds":reused,"concurrent_attach_seconds":attach,"foreground_queries":foreground_queries,"foreground_max_seconds":foreground_peak,"warm_seconds":warm,"queries":queries,"raw_view_seconds":raw_view,"asset_views":asset_views,"restart_all_seconds":restart,"competing_write_bytes":competing_writes,"memory":memory})
    );
    Ok(())
}
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(run(Options::parse()))
}
