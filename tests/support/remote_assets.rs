use super::*;

pub fn code_precedes_asset_downloads() -> Result<()> {
    let root = tempfile::tempdir()?;
    let upstream = root.path().join("upstream");
    fs::create_dir(&upstream)?;
    git(
        &upstream,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    )?;
    for args in [
        ["config", "user.name", "Fixture"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "uploadpack.allowFilter", "true"],
        ["config", "uploadpack.allowAnySHA1InWant", "true"],
    ] {
        git(&upstream, &args)?;
    }
    fs::write(
        upstream.join("lib.rs"),
        "#[path=\"Module.asset\"] pub mod child; #[path=\"TooLarge.asset\"] pub mod rejected; pub struct Available;\n",
    )?;
    fs::write(upstream.join("Module.asset"), "pub struct RequiredCode;\n")?;
    fs::File::create(upstream.join("TooLarge.asset"))?
        .set_len(sigla::repository::selection::ASSET_LIMIT + 1)?;
    fs::create_dir(upstream.join("Assets"))?;
    fs::create_dir(upstream.join("ProjectSettings"))?;
    // An unavailable editor keeps this fixture independent of metadata downloads.
    fs::write(
        upstream.join("ProjectSettings/ProjectVersion.txt"),
        "version",
    )?;
    let ready = "%YAML 1.1\n--- !u!1 &1\nGameObject:\n  m_Name: Ready\n  m_Component: []\n";
    fs::write(upstream.join("Assets/Ready.asset"), ready)?;
    fs::create_dir(upstream.join("Assets/Other"))?;
    fs::write(upstream.join("Assets/Other/Ready.asset"), ready)?;
    fs::write(
        upstream.join("Assets/Later.asset"),
        "later asset contents\n",
    )?;
    fs::write(
        upstream.join("Assets/Object.asset"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 10\n",
            "0".repeat(64)
        ),
    )?;
    git(&upstream, &["add", "."])?;
    git(
        &upstream,
        &["commit", "--quiet", "-m", "Add source and an LFS asset"],
    )?;
    fs::create_dir(root.path().join("bin"))?;
    std::os::unix::fs::symlink(std::env::current_exe()?, root.path().join("bin/ssh"))?;
    fs::write(root.path().join("pause-lfs"), "")?;
    let project = "ssh://git@fixture.invalid/repo#main";
    let server =
        Server::launch_with_refresh(root.path(), Duration::from_secs(30), false, Some("1h"))?;
    std::thread::scope(|scope| -> Result<()> {
        let (send, receive) = std::sync::mpsc::channel();
        let server = &server;
        scope.spawn(move || {
            let _ = send.send(server.query(project, "type:Available lang:rust"));
        });
        let (error, result) = receive.recv_timeout(Duration::from_secs(10))??;
        ensure!(
            !error && result.contains("Available"),
            "Code search waited for optional assets: {result}"
        );
        ensure!(
            !root.path().join("lfs-started").exists(),
            "Code search started an unrelated asset download"
        );
        let (error, result) = server.call(
            "view",
            json!({"codebase":project,"path":"Assets/Ready.asset","mode":"raw"}),
        )?;
        ensure!(
            !error && result == ready,
            "Single asset view failed: {result}"
        );
        ensure!(
            !root.path().join("lfs-started").exists(),
            "A single asset view started an unrelated LFS download"
        );
        let sources = fs::read_dir(root.path().join("cache/repositories"))?
            .collect::<std::io::Result<Vec<_>>>()?;
        let source = sources
            .iter()
            .map(|entry| entry.path().join("source"))
            .find(|path| path.join("lib.rs").is_file())
            .context("Prepared source missing")?;
        ensure!(
            !source.join("Assets/Later.asset").exists(),
            "A single asset view fetched unrelated assets"
        );
        let (error, _) = server.call(
            "view",
            json!({"codebase":project,"path":"Ready.asset","mode":"raw"}),
        )?;
        ensure!(error, "A materialized file hid an ambiguous deferred file");
        scope.spawn(move || {
            server.call(
                "view",
                json!({"codebase":project,"path":"Assets/Object.asset"}),
            )
        });
        let started = Instant::now();
        while !root.path().join("lfs-started").exists() {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "Deferred LFS acquisition did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let (error, result) = server.query(project, "type:Available lang:rust")?;
        ensure!(
            !error && result.contains("Available"),
            "Pending assets blocked another code query: {result}"
        );
        fs::remove_file(root.path().join("pause-lfs"))?;
        Ok(())
    })?;
    let (error, result) = server.call(
        "view",
        json!({"codebase":project,"path":"Assets/Object.asset","mode":"raw"}),
    )?;
    ensure!(
        error,
        "Unavailable asset source was returned as complete text"
    );
    ensure!(
        !result.contains("oid sha256:"),
        "An LFS pointer was returned as asset contents"
    );
    let sources = fs::read_dir(root.path().join("cache/repositories"))?
        .collect::<std::io::Result<Vec<_>>>()?;
    let source = sources
        .iter()
        .map(|entry| entry.path().join("source"))
        .find(|path| path.join("lib.rs").is_file())
        .context("Prepared source missing")?;
    ensure!(
        !source.join("Assets/Later.asset").exists(),
        "A failed single-file download fetched unrelated assets"
    );
    let (error, result) = server.query(project, "text:later wait:complete")?;
    ensure!(
        !error && result.contains("later asset contents"),
        "Full asset completion failed: {result}"
    );
    ensure!(
        fs::read_to_string(source.join("Assets/Ready.asset"))? == ready,
        "Readable assets were lost after another asset failed"
    );
    ensure!(
        fs::read_to_string(source.join("Assets/Later.asset"))? == "later asset contents\n",
        "Full acquisition lost assets deferred by an earlier file view"
    );
    ensure!(
        !source.join("Assets/Object.asset").exists(),
        "Failed asset was published"
    );
    ensure!(
        !source.join("TooLarge.asset").exists(),
        "Rejected required input was published"
    );
    let (error, result) = server.query(project, "type:Available lang:rust")?;
    ensure!(
        !error && result.contains("Available"),
        "Asset failure made code unavailable: {result}"
    );
    drop(server);
    fs::write(root.path().join("offline"), "")?;
    let server =
        Server::launch_with_refresh(root.path(), Duration::from_secs(30), false, Some("1h"))?;
    let (error, result) = server.query(project, "type:Available lang:rust")?;
    ensure!(
        !error && result.contains("Available"),
        "Cached code failed after an offline restart: {result}"
    );
    let (error, result) = server.query(project, "type:RequiredCode lang:rust")?;
    ensure!(
        !error && result.contains("RequiredCode"),
        "Required input with an asset extension was lost: {result}"
    );
    Ok(())
}
