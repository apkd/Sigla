//! A controlled SSH endpoint exercises the normal executable and real Git wire protocol.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn git(root: &Path, args: &[&str]) -> Result<()> {
    ensure!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdout(Stdio::null())
            .status()?
            .success(),
        "Fixture Git operation failed"
    );
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
            Command::new(env!("CARGO_BIN_EXE_sigla"))
        };
        let child = command
            .args([
                "serve",
                "--mode",
                "remote",
                "--allow-repo",
                "https://github.com/fixture/repo",
                "--refresh-interval",
                "1s",
                "--repo-ttl",
                if live_unity { "7d" } else { "60s" },
                "--branch-ttl",
                if live_unity { "1d" } else { "2s" },
                "--listen",
            ])
            .arg(address.to_string())
            .arg("--cache-dir")
            .arg(root.join("cache"))
            .env(
                "PATH",
                format!("{}:{}", root.join("bin").display(), std::env::var("PATH")?),
            )
            .env("SIGLA_TEST_REPOSITORY", root.join("upstream"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
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
        let id = self
            .request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let response = self.post(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"search","arguments":{"project":project,"query":query}}}))?;
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
    fs::write(upstream.join("asset.bin"), vec![123; 1024 * 1024])?;
    git(&upstream, &["add", "."])?;
    git(&upstream, &["commit", "--quiet", "-m", "main"])?;
    git(&upstream, &["switch", "--quiet", "-c", "feature/search"])?;
    fs::write(upstream.join("src/lib.rs"), "pub struct Feature;\n")?;
    git(&upstream, &["commit", "--quiet", "-am", "feature"])?;
    git(&upstream, &["switch", "--quiet", "main"])?;
    fs::create_dir(root.path().join("bin"))?;
    std::os::unix::fs::symlink(std::env::current_exe()?, root.path().join("bin/ssh"))?;
    let server = Server::start(root.path())?;
    let main = "git@github.com:fixture/repo.git#main";
    let feature = "git@github.com:fixture/repo.git#feature/search";
    ensure!(
        !server.query(main, "type:Main")?.0,
        "Initial acquisition failed"
    );
    let count = fs::read(root.path().join("requests.log"))?.len();
    let cached = server.query("https://github.com/fixture/repo#main", "type:Main")?;
    ensure!(
        !cached.0 && cached.1.contains("Main"),
        "Canonical alias did not share a workspace"
    );
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
    ensure!(
        !server.query(main, "type:Main")?.0,
        "Branches did not remain independent"
    );
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
    server.until(main, "type:Changed", "Changed")?;
    server.until(main, "type:MaterializedInput", "MaterializedInput")?;
    fs::write(root.path().join("offline"), "")?;
    std::thread::sleep(Duration::from_millis(1200));
    let (error, cached) = server.query(main, "type:Changed")?;
    ensure!(
        !error && cached.contains("Changed") && !cached.contains("stale"),
        "Fetch failure invalidated a valid index"
    );
    drop(server);
    let server = Server::start(root.path())?;
    let (error, cached) = server.query(main, "type:Changed")?;
    ensure!(
        !error && cached.contains("Changed"),
        "Restart failed to reuse a valid index while offline"
    );
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
    lifecycle()?;
    println!("Remote lifecycle checks passed");
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
            .arg(branch.join("git"))
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
