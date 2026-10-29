//! A controlled SSH endpoint exercises the normal executable and real Git wire protocol.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[path = "support/configuration_documents.rs"]
mod configuration_documents;
#[path = "support/remote_assets.rs"]
mod remote_assets;

fn git(root: &Path, args: &[&str]) -> Result<()> {
    ensure!(
        Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdout(Stdio::null())
            .status()?
            .success(),
        "Fixture Git operation failed"
    );
    if args.first() == Some(&"commit") {
        // Git transport is local in this fixture. Seed the matching host inventory
        // as well, so exercising GitHub aliases never contacts the public API.
        let repository =
            sigla::repository::Repository::parse("https://github.com/fixture/repo")?.unwrap();
        let store = root
            .parent()
            .unwrap()
            .join("cache/git")
            .join(repository.identity.storage_key())
            .join("current");
        fs::create_dir_all(&store)?;
        let revision = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()?;
        ensure!(revision.status.success(), "Cannot resolve fixture revision");
        let revision = String::from_utf8(revision.stdout)?.trim().to_owned();
        let tree = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-tree", "-rl", "HEAD"])
            .output()?;
        ensure!(tree.status.success(), "Cannot enumerate fixture tree");
        let mut sizes = std::collections::BTreeMap::new();
        for line in std::str::from_utf8(&tree.stdout)?.lines() {
            let (header, path) = line.split_once('\t').context("Invalid fixture tree")?;
            let fields: Vec<_> = header.split_whitespace().collect();
            if fields[1] == "blob" {
                sizes.insert(path, fields[3].parse::<u64>()?);
            }
        }
        fs::write(
            store.join(format!("github-tree-{revision}.json")),
            serde_json::to_vec(&json!({"version":1,"sizes":sizes}))?,
        )?;
    }
    Ok(())
}

fn ssh() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let root = PathBuf::from(
        std::env::var_os("SIGLA_TEST_REPOSITORY").context("Missing test repository")?,
    );
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("../requests.log"))?
        .write_all(b"request\n")?;
    if root.join("../offline").exists() {
        std::process::exit(1);
    }
    if std::env::args().any(|arg| arg.contains("git-lfs-authenticate")) {
        fs::write(root.join("../lfs-started"), "")?;
        let started = Instant::now();
        while root.join("../pause-lfs").exists() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("Fixture LFS server is unavailable");
    }
    let error = Command::new("git")
        .arg("upload-pack")
        .arg(&root)
        .env("GIT_PROTOCOL", "version=2")
        .exec();
    Err(error.into())
}
use std::io::Write;

struct Server {
    child: Child,
    endpoint: String,
    client: reqwest::blocking::Client,
    session: String,
    request_id: std::sync::atomic::AtomicU64,
}
impl Drop for Server {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(root: &Path) -> Result<Self> {
        Self::launch(root, Duration::from_secs(30), false)
    }
    fn launch(root: &Path, timeout: Duration, live_unity: bool) -> Result<Self> {
        Self::launch_with_refresh(root, timeout, live_unity, Some("1s"))
    }
    fn launch_with_refresh(
        root: &Path,
        timeout: Duration,
        live_unity: bool,
        refresh: Option<&str>,
    ) -> Result<Self> {
        Self::launch_process(root, timeout, live_unity, refresh, None)
    }
    fn launch_probe(root: &Path, binary: &Path) -> Result<Self> {
        Self::launch_process(
            root,
            Duration::from_secs(3600),
            false,
            Some("1h"),
            Some(binary),
        )
    }
    fn launch_process(
        root: &Path,
        timeout: Duration,
        live_unity: bool,
        refresh: Option<&str>,
        probe: Option<&Path>,
    ) -> Result<Self> {
        // The fixture uses a local SSH executable, never a public GitHub endpoint.
        let repository =
            sigla::repository::Repository::parse("git@github.com:fixture/repo.git")?.unwrap();
        let transports = root.join("cache/transports");
        fs::create_dir_all(&transports)?;
        fs::write(
            transports.join(repository.identity.storage_key()),
            "git@github.com:fixture/repo.git",
        )?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let mut command = if live_unity {
            // -D keeps Sigla as our direct child so normal shutdown still waits
            // for the server. The separate tracer records every descendant exec.
            let mut command = Command::new("strace");
            command
                .args(["-D", "-f", "-A", "-e", "trace=execve", "-o"])
                .arg(root.join("processes.log"))
                .arg(env!("CARGO_BIN_EXE_sigla"));
            command
        } else {
            Command::new(probe.unwrap_or_else(|| Path::new(env!("CARGO_BIN_EXE_sigla"))))
        };
        command
            .args([
                "serve",
                "--mode",
                "remote",
                "--allow-repo-private",
                "https://github.com/fixture/repo",
                "--allow-repo-private",
                "ssh://git@fixture.invalid/repo",
                "--repo-ttl",
                if live_unity || probe.is_some() {
                    "7d"
                } else {
                    "60s"
                },
                "--branch-ttl",
                if live_unity || probe.is_some() {
                    "1d"
                } else {
                    "60s"
                },
                "--listen",
            ])
            .arg(address.to_string())
            .args(
                refresh
                    .into_iter()
                    .flat_map(|value| ["--refresh-interval", value]),
            )
            .arg("--cache-dir")
            .arg(root.join("cache"))
            .env(
                "PATH",
                format!("{}:{}", root.join("bin").display(), std::env::var("PATH")?),
            )
            .env("SIGLA_TEST_REPOSITORY", root.join("upstream"))
            .stdout(Stdio::null());
        if probe.is_some() {
            command
                .args(["--free-disk-space-headroom-percent", "0"])
                .env("RUST_LOG", "info")
                .stderr(Stdio::from(
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(root.join("probe.log"))?,
                ));
        } else {
            command.stderr(Stdio::inherit());
        }
        let child = command.spawn()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()?;
        let mut server = Self {
            child,
            endpoint: format!("http://{address}/mcp"),
            client,
            session: String::new(),
            request_id: std::sync::atomic::AtomicU64::new(2),
        };
        let started = Instant::now();
        loop {
            let response = server.client.post(&server.endpoint).header("Accept", "application/json, text/event-stream")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}})).send();
            match response {
                Ok(response) => {
                    ensure!(response.status().is_success(), "MCP initialization failed");
                    server.session = response
                        .headers()
                        .get("mcp-session-id")
                        .context("Missing session")?
                        .to_str()?
                        .to_owned();
                    break;
                }
                Err(_) if started.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
        server.post(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
        Ok(server)
    }
    fn post(&self, body: &Value) -> Result<reqwest::blocking::Response> {
        Ok(self
            .client
            .post(&self.endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session)
            .json(body)
            .send()?
            .error_for_status()?)
    }
    fn query(&self, project: &str, query: &str) -> Result<(bool, String)> {
        self.call("search", json!({"codebase":project,"query":query}))
    }
    fn call(&self, name: &str, arguments: Value) -> Result<(bool, String)> {
        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let response = self.post(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))?;
        let response: Value = if response
            .headers()
            .get("content-type")
            .is_some_and(|h| h.to_str().unwrap_or("").starts_with("text/event-stream"))
        {
            use std::io::BufRead;
            let mut result = None;
            for line in std::io::BufReader::new(response).lines() {
                let line = line?;
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(message) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if message["method"] == "ping" {
                    self.post(&json!({"jsonrpc":"2.0","id":message["id"],"result":{}}))?;
                } else if message["id"] == id && message.get("method").is_none() {
                    result = Some(message);
                    break;
                }
            }
            result.context("MCP stream ended without a tool response")?
        } else {
            response.json()?
        };
        let result = &response["result"];
        let text = result["content"]
            .as_array()
            .with_context(|| format!("Missing tool result: {response}"))?
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok((result["isError"].as_bool().unwrap_or(false), text))
    }
    fn until(&self, project: &str, query: &str, expected: &str) -> Result<String> {
        let started = Instant::now();
        loop {
            let (error, text) = self.query(project, query)?;
            if !error && text.contains(expected) {
                return Ok(text);
            }
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "Expected result did not become available: {text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn until_absent(&self, project: &str, name: &str) -> Result<()> {
        let started = Instant::now();
        loop {
            let (error, text) = self.query(project, &format!("type:{name}"))?;
            ensure!(!error, "Source update failed: {text}");
            if !text.contains(name) {
                return Ok(());
            }
            ensure!(
                started.elapsed() < Duration::from_secs(20),
                "Source removal was not applied: {text}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn lifecycle() -> Result<()> {
    let root = tempfile::tempdir()?;
    let upstream = root.path().join("upstream");
    fs::create_dir(&upstream)?;
    git(
        &upstream,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    )?;
    git(&upstream, &["config", "user.name", "Fixture"])?;
    git(
        &upstream,
        &["config", "user.email", "fixture@example.invalid"],
    )?;
    git(&upstream, &["config", "uploadpack.allowFilter", "true"])?;
    git(
        &upstream,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    )?;
    fs::create_dir(upstream.join("src"))?;
    fs::write(
        upstream.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n",
    )?;
    fs::write(upstream.join("src/lib.rs"), "pub struct Main;\n")?;
    fs::write(
        upstream.join("Surface.ngsurface"),
        "BEGIN_PROPERTIES\n_Tint(\"Tint\", Color) = (1,1,1,1)\nEND_PROPERTIES\nBEGIN_CODE\nfloat4 SurfaceFunction() { return _Tint; }\nEND_CODE\n",
    )?;
    fs::write(
        upstream.join("README.md"),
        "# Remote fixture\n\nKeep  document spacing.\n",
    )?;
    for &(path, source) in configuration_documents::FILES {
        let path = upstream.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, source)?;
    }
    fs::write(upstream.join("asset.bin"), vec![123; 1024 * 1024])?;
    git(&upstream, &["add", "."])?;
    git(&upstream, &["commit", "--quiet", "-m", "main"])?;
    let original_commit = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args(["rev-parse", "HEAD"])
            .output()?
            .stdout,
    )?
    .trim()
    .to_owned();
    git(&upstream, &["tag", "-a", "release", "-m", "release"])?;
    git(&upstream, &["switch", "--quiet", "-c", "feature/search"])?;
    fs::write(upstream.join("src/lib.rs"), "pub struct Feature;\n")?;
    git(&upstream, &["commit", "--quiet", "-am", "feature"])?;
    git(&upstream, &["branch", "release"])?;
    git(&upstream, &["switch", "--quiet", "main"])?;
    fs::create_dir(root.path().join("bin"))?;
    std::os::unix::fs::symlink(std::env::current_exe()?, root.path().join("bin/ssh"))?;
    // Keep background polling outside the cache-only acquisition-count checks.
    let server =
        Server::launch_with_refresh(root.path(), Duration::from_secs(30), false, Some("1h"))?;
    fs::write(root.path().join("offline"), "")?;
    ensure!(
        server
            .query("ssh://git@fixture.invalid/repo", "type:Main")?
            .0,
        "Offline lookup unexpectedly succeeded"
    );
    fs::remove_file(root.path().join("offline"))?;
    let recovered = server.query("ssh://git@fixture.invalid/repo", "type:Main")?;
    ensure!(
        !recovered.0 && recovered.1.contains("Main"),
        "Failed lookup blocked immediate recovery: {}",
        recovered.1
    );
    ensure!(
        server
            .query("ssh://git@fixture.invalid/repo#recovered", "type:Main")?
            .0,
        "Missing branch unexpectedly succeeded"
    );
    git(&upstream, &["branch", "recovered"])?;
    let recovered = server.query("ssh://git@fixture.invalid/repo#recovered", "type:Main")?;
    ensure!(
        !recovered.0 && recovered.1.contains("Main"),
        "Failed preparation blocked immediate recovery: {}",
        recovered.1
    );
    let main = "git@github.com:fixture/repo.git#main";
    let feature = "git@github.com:fixture/repo.git#feature/search";
    let (error, tree) = server.call("browse", json!({"codebase":main,"path":"src"}))?;
    ensure!(
        !error && tree.contains("lib.rs"),
        "Cold browse failed: {tree}"
    );
    let (error, source) = server.call("view", json!({"codebase":main,"path":"src/lib.rs"}))?;
    ensure!(
        !error && source.contains("pub struct Main;"),
        "Source view after cold browse failed: {source}"
    );
    let (error, first) = server.query(main, "type:Main")?;
    ensure!(
        !error && first.contains("| Repository |") && first.contains("fixture/repo"),
        "Initial summary missing: {first}"
    );
    let count = fs::read(root.path().join("requests.log"))?.len();
    server.until(main, "property:_Tint lang:shaders", "Surface.ngsurface")?;
    server.until(
        main,
        "function:SurfaceFunction lang:shaders",
        "Surface.ngsurface",
    )?;
    let (error, document) = server.call(
        "view",
        json!({"codebase":main,"path":"README.md","mode":"minified"}),
    )?;
    ensure!(
        !error && document.contains("Keep  document spacing."),
        "Remote document missing: {document}"
    );
    let (error, document_match) = server.query(main, "text:\"document spacing\"")?;
    ensure!(
        !error && document_match.contains("README.md"),
        "Remote text search missed document: {document_match}"
    );
    for &(path, source) in configuration_documents::FILES {
        let (error, found) = server.query(main, &format!("file:{path}"))?;
        ensure!(
            !error && found.contains(path),
            "Remote document missing: {found}"
        );
        let (error, found) = server.query(main, &format!("text:\"CoverageMarker\" path:{path}"))?;
        ensure!(
            !error && found.contains(path),
            "Remote document not indexed: {found}"
        );
        for mode in ["exact", "minified"] {
            let (error, viewed) =
                server.call("view", json!({"codebase":main,"path":path,"mode":mode}))?;
            ensure!(
                !error && viewed.contains(source),
                "Remote document not delivered intact: {path}: {viewed}"
            );
        }
    }
    for query in ["function:coverage_lua", "uses:coverage_lua"] {
        let (error, found) = server.query(main, query)?;
        ensure!(
            !error && found.starts_with("No matches."),
            "Text-only source invented a semantic result: {found}"
        );
    }
    let (error, tree) = server.call("browse", json!({"codebase":main}))?;
    ensure!(
        !error && tree.contains("lib.rs") && !tree.contains("asset.bin"),
        "Remote browse failed: {tree}"
    );
    let (error, viewed) = server.call(
        "view",
        json!({"codebase":main,"path":"lib.rs:1","mode":"exact"}),
    )?;
    ensure!(
        !error && viewed.contains("pub struct Main;\n") && viewed.contains("`src/lib.rs:1`"),
        "Remote view failed: {viewed}"
    );
    let (error, paths) = server.query(main, "file:*.rs")?;
    for response in [&tree, &viewed, &paths] {
        ensure!(
            !response.contains("| Repository |"),
            "Repository summary repeated: {response}"
        );
    }
    ensure!(
        !error && paths.contains("src/lib.rs"),
        "Remote filename search failed: {paths}"
    );
    ensure!(
        server
            .call("view", json!({"codebase":main,"path":"/etc/passwd"}))?
            .0,
        "Remote view accepted an absolute path"
    );
    let cached = server.query("https://github.com/fixture/repo#main", "type:Main")?;
    ensure!(
        !cached.0 && cached.1.contains("Main"),
        "Canonical alias did not share a workspace"
    );
    for alias in [
        "ssh://git@github.com/fixture/repo.git#main",
        "https://github.com/fixture/repo#main",
        "fixture/repo#main",
        "git@github.com:fixture/repo.git#main",
        "https://github.com/fixture/repo.git#main",
    ] {
        let (error, text) = server.query(alias, "type:Main")?;
        ensure!(
            !error && text.contains("Main"),
            "Repository alias failed: {alias}: {text}"
        );
    }
    ensure!(
        fs::read(root.path().join("requests.log"))?.len() == count,
        "Unchanged query performed acquisition"
    );
    ensure!(
        server.query("git@github.com:other/repo", "type:Main")?.0,
        "Unauthorized repository was accepted"
    );
    ensure!(
        fs::read(root.path().join("requests.log"))?.len() == count,
        "Unauthorized query accessed the endpoint"
    );
    ensure!(
        server.query(root.path().to_str().unwrap(), "type:Main")?.0,
        "Remote mode exposed an implicit local root"
    );
    server.until(feature, "type:Feature", "Feature")?;
    let tag = "git@github.com:fixture/repo.git#release";
    let pinned = format!("git@github.com:fixture/repo.git#{original_commit}");
    // Exercise the generic Git resolver through the local SSH endpoint. GitHub
    // abbreviations use its commit API and have separate response/live coverage.
    let abbreviated = format!("ssh://git@fixture.invalid/repo#{}", &original_commit[..8]);
    server.until(tag, "type:Main", "Main")?;
    server.until(&pinned, "type:Main", "Main")?;
    server.until(&abbreviated, "type:Main", "Main")?;
    let (error, tree) = server.call("browse", json!({"codebase":abbreviated}))?;
    ensure!(
        !error && tree.contains("lib.rs"),
        "Short-SHA browse failed: {tree}"
    );
    let (error, source) = server.call(
        "view",
        json!({"codebase":abbreviated,"path":"src/lib.rs","mode":"exact"}),
    )?;
    ensure!(
        !error && source.contains("pub struct Main;"),
        "Short-SHA view failed: {source}"
    );
    let (error, history) = server.call("history", json!({"codebase":abbreviated,"query":format!("diff {}..feature/search", &original_commit[..8])}))?;
    ensure!(
        !error && history.contains("Feature"),
        "Short-SHA history range failed: {history}"
    );
    server.until(
        "git@github.com:fixture/repo.git#refs/heads/release",
        "type:Feature",
        "Feature",
    )?;
    ensure!(
        !server.query(main, "type:Main")?.0,
        "Branches did not remain independent"
    );
    drop(server);
    let server = Server::start(root.path())?;
    fs::write(upstream.join("src/lib.rs"), "pub struct Changed;\n")?;
    fs::create_dir_all(upstream.join("Dotnet/inputs"))?;
    fs::create_dir_all(upstream.join("Dotnet/Sources"))?;
    fs::write(
        upstream.join("Dotnet/Project.csproj"),
        r#"<Project><Import Project="inputs/membership.data" Condition="Exists('inputs/membership.data')" /></Project>"#,
    )?;
    fs::write(
        upstream.join("Dotnet/inputs/membership.data"),
        r#"<Project><ItemGroup><Compile Include="Sources/*.code" Exclude="Sources/Excluded.code" /></ItemGroup></Project>"#,
    )?;
    fs::write(
        upstream.join("Dotnet/Sources/Included.code"),
        "public class MaterializedInput {}",
    )?;
    fs::write(
        upstream.join("Dotnet/Sources/Excluded.code"),
        "public class NotCompiled {}",
    )?;
    git(&upstream, &["add", "Dotnet"])?;
    git(
        &upstream,
        &["commit", "--quiet", "--amend", "-am", "force push"],
    )?;
    git(&upstream, &["branch", "-f", &original_commit[..8]])?;
    server.until(main, "type:Changed", "Changed")?;
    server.until(main, "type:MaterializedInput", "MaterializedInput")?;
    git(&upstream, &["tag", "--force", "release"])?;
    server.until(tag, "type:Changed", "Changed")?;
    server.until(&pinned, "type:Main", "Main")?;
    server.until(&abbreviated, "type:Main", "Main")?;
    fs::write(root.path().join("offline"), "")?;
    std::thread::sleep(Duration::from_millis(1200));
    let (error, cached) = server.query(main, "type:Changed")?;
    ensure!(
        !error && cached.contains("Changed") && !cached.contains("stale"),
        "Fetch failure invalidated a valid index"
    );
    drop(server);
    let server = Server::start(root.path())?;
    server.until(tag, "type:Changed", "Changed")?;
    server.until(&pinned, "type:Main", "Main")?;
    server.until(&abbreviated, "type:Main", "Main")?;
    let (error, cached) = server.query(main, "type:Changed")?;
    ensure!(
        !error && cached.contains("Changed"),
        "Restart failed to reuse a valid index while offline"
    );
    drop(server);
    fs::remove_file(root.path().join("offline"))?;
    let server = Server::launch_with_refresh(root.path(), Duration::from_secs(30), false, None)?;
    server.until(main, "type:Changed", "Changed")?;
    fs::write(upstream.join("src/lib.rs"), "pub struct Adaptive;\n")?;
    git(&upstream, &["commit", "--quiet", "-am", "adaptive refresh"])?;
    // Observe publication without requests, so only maintenance can fetch it.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut published = false;
        for entry in fs::read_dir(root.path().join("cache/repositories"))? {
            let source = entry?.path().join("source/src/lib.rs");
            published |= fs::read_to_string(source).is_ok_and(|text| text.contains("Adaptive"));
        }
        if published {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "Adaptive background refresh did not publish the new commit"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    server.until(main, "type:Adaptive", "Adaptive")?;
    server.until(&pinned, "type:Main", "Main")?;
    server.until(&abbreviated, "type:Main", "Main")?;
    Ok(())
}

fn main() -> Result<()> {
    if std::env::args_os()
        .next()
        .is_some_and(|s| Path::new(&s).file_name().is_some_and(|n| n == "ssh"))
    {
        return ssh();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("unity")) {
        let root = PathBuf::from(
            std::env::args_os()
                .nth(2)
                .context("Usage: remote unity PERSISTENT_TEST_DIRECTORY")?,
        );
        return unity_lifecycle(&root);
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("acquisition")) {
        let root = PathBuf::from(std::env::args_os().nth(2).context(
            "Usage: remote acquisition WORK_DIRECTORY SIGLA_BINARY [FILE_COUNT] [--expect-reuse]",
        )?);
        let binary = PathBuf::from(std::env::args_os().nth(3).context("Missing Sigla binary")?)
            .canonicalize()?;
        let mut count = None;
        let mut expect_reuse = false;
        for argument in std::env::args().skip(4) {
            if argument == "--expect-reuse" {
                expect_reuse = true;
            } else {
                ensure!(count.is_none(), "File count provided more than once");
                count = Some(argument.parse::<usize>()?);
            }
        }
        let count = count.unwrap_or(30_000);
        ensure!(count > 0, "File count must be positive");
        return acquisition_probe(&root, &binary, count, expect_reuse);
    }
    remote_assets::code_precedes_asset_downloads()?;
    lifecycle()?;
    println!("Remote lifecycle checks passed");
    Ok(())
}

/// A local Git/SSH fixture for comparing acquisition with different binaries.
fn acquisition_probe(
    directory: &Path,
    binary: &Path,
    count: usize,
    expect_reuse: bool,
) -> Result<()> {
    fs::create_dir_all(directory)?;
    let fixture = tempfile::Builder::new()
        .prefix("remote-acquisition-")
        .tempdir_in(directory)?;
    let root = fixture.path();
    let upstream = root.join("upstream");
    fs::create_dir_all(upstream.join("src/data"))?;
    git(
        &upstream,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    )?;
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
    ] {
        git(&upstream, &["config", key, value])?;
    }
    let filler = "x".repeat(4096);
    for index in 0..count {
        fs::write(
            upstream.join(format!("src/data/{index}.md")),
            format!("Fixture file {index}\n{filler}\n"),
        )?;
    }
    fs::write(upstream.join("src/marker.md"), "Probe marker original\n")?;
    git(&upstream, &["add", "."])?;
    git(&upstream, &["commit", "--quiet", "-m", "initial"])?;
    for branch in ["repeat", "parallel-a", "parallel-b"] {
        git(&upstream, &["branch", branch])?;
    }
    fs::create_dir(root.join("bin"))?;
    std::os::unix::fs::symlink(std::env::current_exe()?, root.join("bin/ssh"))?;
    println!(
        "binary={} files={count} bytes_per_file≈{}",
        binary.display(),
        filler.len()
    );
    let server = Server::launch_probe(root, binary)?;
    for (label, branch) in [
        ("cold", "main"),
        ("warm", "main"),
        ("new selector", "repeat"),
    ] {
        let log_offset = fs::metadata(root.join("probe.log"))?.len() as usize;
        let started = Instant::now();
        acquisition_request(&server, branch, "original")?;
        println!("{label}: {:?}", started.elapsed());
        let work = acquisition_log(root, log_offset)?;
        if expect_reuse && branch == "repeat" {
            work.check(0, 0, 1)?;
        }
        verify_acquisition(root, branch, count, &filler, "original")?;
    }
    let log_offset = fs::metadata(root.join("probe.log"))?.len() as usize;
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let first = scope.spawn(|| acquisition_request(&server, "parallel-a", "original"));
        let second = scope.spawn(|| acquisition_request(&server, "parallel-b", "original"));
        first
            .join()
            .map_err(|_| anyhow::anyhow!("First acquisition panicked"))??;
        second
            .join()
            .map_err(|_| anyhow::anyhow!("Second acquisition panicked"))??;
        Ok(())
    })?;
    println!("concurrent selectors: {:?}", started.elapsed());
    let work = acquisition_log(root, log_offset)?;
    if expect_reuse {
        work.check(0, 0, 2)?;
    }
    for branch in ["parallel-a", "parallel-b"] {
        verify_acquisition(root, branch, count, &filler, "original")?;
    }
    git(&upstream, &["switch", "--quiet", "-c", "changed"])?;
    fs::write(upstream.join("src/marker.md"), "Probe marker changed\n")?;
    git(&upstream, &["commit", "--quiet", "-am", "one file changed"])?;
    let log_offset = fs::metadata(root.join("probe.log"))?.len() as usize;
    let started = Instant::now();
    acquisition_request(&server, "changed", "changed")?;
    println!("one file changed: {:?}", started.elapsed());
    let work = acquisition_log(root, log_offset)?;
    if expect_reuse {
        work.check(1, "Probe marker changed\n".len() as u64, 1)?;
    }
    verify_acquisition(root, "changed", count, &filler, "changed")?;
    verify_acquisition(root, "main", count, &filler, "original")?;
    drop(server);
    let server = Server::launch_probe(root, binary)?;
    let log_offset = fs::metadata(root.join("probe.log"))?.len() as usize;
    let started = Instant::now();
    acquisition_request(&server, "main", "original")?;
    acquisition_request(&server, "changed", "changed")?;
    println!("restart, two selectors: {:?}", started.elapsed());
    acquisition_log(root, log_offset)?;
    drop(server);
    Ok(())
}

fn acquisition_request(server: &Server, branch: &str, marker: &str) -> Result<()> {
    let project = format!("ssh://git@fixture.invalid/repo#{branch}");
    let (error, text) = server.query(&project, "text:\"Probe marker\" path:src/marker.md")?;
    ensure!(
        !error && text.contains("marker.md"),
        "Acquisition failed: {text}"
    );
    let (error, text) = server.call("view", json!({"codebase":project,"path":"src/marker.md"}))?;
    ensure!(
        !error && text.contains(&format!("Probe marker {marker}")),
        "Wrong source: {text}"
    );
    Ok(())
}

#[derive(Debug, Default)]
struct AcquisitionWork {
    git_records: usize,
    reuse_records: usize,
    import_records: usize,
    extracted_files: u64,
    extracted_bytes: u64,
    hashed_bytes: u64,
}

impl AcquisitionWork {
    fn check(&self, files: u64, bytes: u64, reuse_records: usize) -> Result<()> {
        ensure!(
            self.git_records > 0 && self.import_records > 0 && self.reuse_records >= reuse_records,
            "Missing acquisition reuse or import counters: {self:?}"
        );
        ensure!(
            self.extracted_files == files
                && self.extracted_bytes == bytes
                && self.hashed_bytes == bytes,
            "Unexpected extraction or hashing: {self:?}"
        );
        Ok(())
    }
}

fn log_counter(line: &str, name: &str) -> Option<u64> {
    let prefix = format!("{name}=");
    line.split_whitespace().find_map(|part| {
        part.strip_prefix(prefix.as_str())?
            .trim_end_matches(',')
            .parse()
            .ok()
    })
}

fn plain_log(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            if chars.next() == Some('[') {
                for code in chars.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn acquisition_log(root: &Path, offset: usize) -> Result<AcquisitionWork> {
    let log = fs::read(root.join("probe.log"))?;
    let mut work = AcquisitionWork::default();
    for original in String::from_utf8_lossy(&log[offset..]).lines() {
        let line = plain_log(original);
        // Older binaries may omit counters; the optional assertion requires them.
        if line.contains("Repository acquisition phase completed") {
            println!("  {line}");
            if (line.contains("phase=\"git\"") || line.contains("phase=git"))
                && let (Some(files), Some(bytes), Some(reused)) = (
                    log_counter(&line, "extracted_files"),
                    log_counter(&line, "extracted_bytes"),
                    log_counter(&line, "reused_files"),
                )
            {
                work.git_records += 1;
                work.reuse_records += usize::from(reused > 0);
                work.extracted_files += files;
                work.extracted_bytes += bytes;
            }
        }
        if line.contains("Repository files imported") {
            println!("  {line}");
            if let Some(bytes) = log_counter(&line, "hashed_bytes") {
                work.import_records += 1;
                work.hashed_bytes += bytes;
            }
        }
    }
    Ok(work)
}

fn verify_acquisition(
    root: &Path,
    branch: &str,
    count: usize,
    filler: &str,
    marker: &str,
) -> Result<()> {
    let mut source = None;
    for entry in fs::read_dir(root.join("cache/repositories"))? {
        let entry = entry?;
        let Ok(bytes) = fs::read(entry.path().join("state.json")) else {
            continue;
        };
        let state: Value = serde_json::from_slice(&bytes)?;
        if state["branch"] == branch {
            ensure!(source.is_none(), "Duplicate branch state for {branch}");
            source = Some(entry.path().join("source/src"));
        }
    }
    let source = source.context("Missing published source")?;
    ensure!(
        fs::read_to_string(source.join("marker.md"))? == format!("Probe marker {marker}\n"),
        "Marker mismatch"
    );
    ensure!(
        fs::read_dir(source.join("data"))?.count() == count,
        "Selected file count mismatch"
    );
    for index in 0..count {
        let contents = fs::read_to_string(source.join(format!("data/{index}.md")))?;
        ensure!(
            contents == format!("Fixture file {index}\n{filler}\n"),
            "Selected file differs: {index}"
        );
    }
    Ok(())
}

/// Opt-in live editor/package acquisition. Ordinary tests never download editors.
fn unity_lifecycle(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let upstream = root.join("upstream");
    fs::create_dir_all(&upstream)?;
    for path in [
        root.join("offline"),
        upstream.join("Assets/Added.cs"),
        upstream.join("Assets/Remote.asmdef"),
    ] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    git(
        &upstream,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    )?;
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
    ] {
        git(&upstream, &["config", key, value])?;
    }
    for directory in ["Assets", "Packages", "ProjectSettings"] {
        fs::create_dir_all(upstream.join(directory))?;
    }
    fs::write(
        upstream.join("Assets/RemoteProject.cs"),
        "public class RemoteProject { public UnityEngine.Vector3 Position; public Unity.Mathematics.float3 Direction; }",
    )?;
    fs::write(
        upstream.join("ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 2022.3.62f3\nm_EditorVersionWithRevision: 2022.3.62f3 (96770f904ca7)\n",
    )?;
    fs::write(
        upstream.join("ProjectSettings/ProjectSettings.asset"),
        "PlayerSettings:\n  apiCompatibilityLevelPerPlatform:\n    Standalone: 6\n  scriptingBackend:\n    Standalone: 0\n  activeInputHandler: 0\n",
    )?;
    fs::write(
        upstream.join("Packages/manifest.json"),
        r#"{"dependencies":{"com.unity.mathematics":"1.3.2"}}"#,
    )?;
    fs::write(
        upstream.join("Packages/packages-lock.json"),
        r#"{"dependencies":{"com.unity.mathematics":{"version":"1.3.2","depth":0,"source":"registry","dependencies":{},"url":"https://packages.unity.com"}}}"#,
    )?;
    fs::write(
        upstream.join("Assets/excluded.bytes"),
        vec![127; 1024 * 1024],
    )?;
    git(&upstream, &["add", "."])?;
    git(
        &upstream,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Unity inputs without generated outputs",
        ],
    )?;
    fs::create_dir_all(root.join("bin"))?;
    if !root.join("bin/ssh").exists() {
        std::os::unix::fs::symlink(std::env::current_exe()?, root.join("bin/ssh"))?;
    }
    let project = "git@github.com:fixture/repo.git#main";
    let server = Server::launch(root, Duration::from_secs(3600), true)?;
    println!("Acquiring the pinned Unity editor and locked public mathematics package...");
    for (query, expected) in [
        ("type:RemoteProject", "RemoteProject"),
        ("type:float3", "Packages/com.unity.mathematics"),
        ("type:Vector3", "Vector3"),
    ] {
        let (error, text) = server.query(project, query)?;
        ensure!(
            !error && text.contains(expected),
            "Unity search failed ({query}): {text}"
        );
        println!("Verified {query}: {}", text.lines().next().unwrap_or(""));
    }
    server.until_absent(project, "AddedAfterAcquisition")?;
    let excluded = Command::new("git")
        .arg("-C")
        .arg(&upstream)
        .args(["rev-parse", "HEAD:Assets/excluded.bytes"])
        .output()?;
    ensure!(
        excluded.status.success(),
        "Cannot identify excluded fixture blob"
    );
    let excluded = String::from_utf8(excluded.stdout)?;
    for entry in fs::read_dir(root.join("cache/repositories"))? {
        let branch = entry?.path();
        ensure!(
            !branch.join("source/Assets/excluded.bytes").exists(),
            "Excluded asset was materialized"
        );
        ensure!(
            !branch.join("source/Library").exists(),
            "Remote discovery generated Library"
        );
        let stored = Command::new("git")
            .arg("--git-dir")
            .arg(
                root.join("cache/git")
                    .join(
                        sigla::repository::Repository::parse(project)?
                            .unwrap()
                            .identity
                            .storage_key(),
                    )
                    .join("current"),
            )
            .args(["cat-file", "-e", excluded.trim()])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        ensure!(
            !stored.success(),
            "Excluded asset blob entered the Git store"
        );
    }
    fs::write(
        upstream.join("Assets/Added.cs"),
        "public class AddedAfterAcquisition {}",
    )?;
    git(&upstream, &["add", "."])?;
    git(&upstream, &["commit", "--quiet", "-m", "Add source"])?;
    server.until(
        project,
        "type:AddedAfterAcquisition",
        "AddedAfterAcquisition",
    )?;
    fs::write(
        upstream.join("Assets/Remote.asmdef"),
        r#"{"name":"Remote","references":["Unity.Mathematics"],"defineConstraints":["EXCLUDED_CONTEXT"]}"#,
    )?;
    git(&upstream, &["add", "."])?;
    git(
        &upstream,
        &["commit", "--quiet", "-m", "Deactivate assembly"],
    )?;
    server.until_absent(project, "AddedAfterAcquisition")?;
    fs::write(
        upstream.join("Assets/Remote.asmdef"),
        r#"{"name":"Remote","references":["Unity.Mathematics"]}"#,
    )?;
    git(
        &upstream,
        &["commit", "--quiet", "-am", "Activate assembly"],
    )?;
    server.until(
        project,
        "type:AddedAfterAcquisition",
        "AddedAfterAcquisition",
    )?;
    drop(server);
    fs::write(root.join("offline"), "")?;
    let server = Server::launch(root, Duration::from_secs(60), true)?;
    server.until(
        project,
        "type:AddedAfterAcquisition",
        "AddedAfterAcquisition",
    )?;
    ensure!(
        !upstream.join("Library").exists(),
        "Fixture unexpectedly contains Library"
    );
    drop(server);
    let trace = fs::read_to_string(root.join("processes.log"))?;
    for line in trace.lines() {
        if let Some((_, path)) = line.split_once("execve(\"") {
            let executable = Path::new(path.split('"').next().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy();
            ensure!(
                !matches!(executable.as_ref(), "Unity" | "dotnet"),
                "Native Unity discovery launched {executable}"
            );
        }
    }
    println!(
        "Remote Unity acquisition, package/engine search, native source/assembly updates, and offline restart passed."
    );
    Ok(())
}
