//! Hydrate selected Git LFS objects without running repository filters.
use super::{
    Endpoint, Repository,
    materialize::{Prepared, Request},
};
use anyhow::{Context, Result, ensure};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

/// Inputs whose Git contents cannot yet be used for source navigation.
pub(super) fn pending(staging: &Path, prepared: &Prepared) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for path in prepared.selected.keys() {
        let target = staging.join(path);
        let file = match File::open(&target) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let mut prefix = Vec::new();
        file.take(64).read_to_end(&mut prefix)?;
        if prefix.starts_with(b"version https://git-lfs.github.com/spec/") {
            paths.push(path.clone());
        }
    }
    Ok(paths)
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct Pointer {
    oid: String,
    size: u64,
}

impl Pointer {
    fn parse(bytes: &[u8]) -> Result<Option<Self>> {
        if !bytes.starts_with(b"version https://git-lfs.github.com/spec/") {
            return Ok(None);
        }
        let text = std::str::from_utf8(bytes).context("Invalid LFS pointer encoding")?;
        let mut lines = text.lines();
        ensure!(
            lines.next() == Some("version https://git-lfs.github.com/spec/v1"),
            "Unsupported LFS pointer version"
        );
        let mut oid = None;
        let mut size = None;
        for line in lines {
            if let Some(value) = line.strip_prefix("oid sha256:") {
                ensure!(
                    oid.is_none()
                        && value.len() == 64
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "Invalid LFS object identity"
                );
                oid = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("size ") {
                ensure!(size.is_none(), "Duplicate LFS object size");
                size = Some(value.parse().context("Invalid LFS object size")?);
            } else {
                anyhow::bail!("Unsupported LFS pointer field");
            }
        }
        Ok(Some(Self {
            oid: oid.context("Missing LFS object identity")?,
            size: size.context("Missing LFS object size")?,
        }))
    }
}

#[derive(Deserialize)]
struct Action {
    href: String,
    #[serde(default)]
    header: BTreeMap<String, String>,
}

impl Action {
    fn url(&self) -> Result<url::Url> {
        crate::acquisition::https(&self.href)
    }
    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.header {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
        Ok(headers)
    }
}

#[derive(Deserialize)]
struct Batch {
    objects: Vec<Object>,
}
#[derive(Deserialize)]
struct Object {
    oid: String,
    size: u64,
    #[serde(default)]
    actions: BTreeMap<String, Action>,
}

fn ssh_auth(repository: &Repository) -> Result<Action> {
    let (host, port, user) = match &repository.identity.endpoint {
        Endpoint::Github => ("github.com", 22, "git"),
        Endpoint::Ssh {
            host, port, user, ..
        } => (host.as_str(), *port, user.as_str()),
        _ => anyhow::bail!("SSH LFS authentication is unavailable for this repository"),
    };
    let path = format!(
        "{}{}",
        repository.identity.components.join("/"),
        if matches!(repository.identity.endpoint, Endpoint::Github)
            || repository.transport.ends_with(".git")
        {
            ".git"
        } else {
            ""
        }
    );
    let path = match &repository.identity.endpoint {
        Endpoint::Ssh { absolute: true, .. } => format!("/{path}"),
        _ => path,
    };
    let destination = if user.is_empty() {
        host.to_owned()
    } else {
        format!("{user}@{host}")
    };
    let argument = shell_argument(&path);
    let result = crate::process::capture(
        Command::new("ssh")
            .args([
                "-oBatchMode=yes",
                "-oStrictHostKeyChecking=yes",
                "-oConnectTimeout=30",
                "-p",
                &port.to_string(),
                "--",
                &destination,
            ])
            .arg(format!("git-lfs-authenticate {argument} download")),
        Duration::from_secs(30),
        None,
        None,
    )?;
    ensure!(result.status.success(), "LFS SSH authentication failed");
    serde_json::from_slice(&result.stdout).context("Invalid LFS authentication response")
}

fn shell_argument(value: &str) -> String {
    // GitHub's LFS command parser expects ordinary repository names unquoted.
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn endpoint(
    repository: &Repository,
    transport: Option<&str>,
    allow_private: bool,
) -> Result<Action> {
    if allow_private && transport.is_some_and(|t| !t.starts_with("https://")) {
        return ssh_auth(repository);
    }
    let base = repository
        .transports(None)
        .into_iter()
        .find(|t| t.starts_with("https://"))
        .context("No anonymous HTTPS LFS endpoint is available")?;
    let mut action = Action {
        href: format!("{}.git/info/lfs", base.trim_end_matches(".git")),
        header: BTreeMap::new(),
    };
    if allow_private {
        let result = crate::process::capture(
            super::credentials::noninteractive(Command::new("git").args(["credential", "fill"])),
            Duration::from_secs(30),
            Some(format!("url={base}\n\n").into_bytes()),
            None,
        )?;
        if result.status.success() {
            let text =
                String::from_utf8(result.stdout).context("Invalid Git credential encoding")?;
            let fields: BTreeMap<_, _> = text
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            if let (Some(user), Some(password)) = (fields.get("username"), fields.get("password")) {
                use base64::Engine;
                action.header.insert(
                    "Authorization".into(),
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD
                            .encode(format!("{user}:{password}"))
                    ),
                );
            }
        }
    }
    Ok(action)
}

fn verified_copy(mut input: impl Read, output: &mut impl Write, pointer: &Pointer) -> Result<()> {
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .context("LFS object size overflow")?;
        ensure!(size <= pointer.size, "LFS object exceeds declared size");
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    ensure!(size == pointer.size, "LFS object transfer was truncated");
    ensure!(
        format!("{:x}", hash.finalize()) == pointer.oid,
        "LFS object checksum mismatch"
    );
    Ok(())
}

fn batch(
    client: &Client,
    endpoint: &Action,
    pointers: &[&Pointer],
) -> Result<BTreeMap<String, Object>> {
    let mut url = endpoint.url()?;
    url.set_path(&format!(
        "{}/objects/batch",
        url.path().trim_end_matches('/')
    ));
    let response = client
        .post(url)
        .headers(endpoint.headers()?)
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(
            &serde_json::json!({"operation":"download", "transfers":["basic"], "objects":pointers}),
        )
        .send()
        .context("LFS batch request failed")?;
    ensure!(
        response.status().is_success(),
        "LFS batch request failed ({})",
        response.status()
    );
    let batch: Batch = serde_json::from_reader(response.take(1024 * 1024))
        .context("Invalid LFS batch response")?;
    let mut objects = BTreeMap::new();
    for object in batch.objects {
        ensure!(
            objects.insert(object.oid.clone(), object).is_none(),
            "Duplicate LFS object in batch response"
        );
    }
    Ok(objects)
}

struct Pending {
    pointer: Pointer,
    paths: Vec<String>,
}

impl Pending {
    fn publish(&self, cached: &Path, staging: &Path) -> Result<()> {
        for path in &self.paths {
            fs::copy(cached, staging.join(path))?;
        }
        Ok(())
    }

    fn unavailable(
        &self,
        error: &anyhow::Error,
        request: &Request,
        prepared: &mut Prepared,
    ) -> Result<()> {
        for path in &self.paths {
            // Never log remote response bodies, URLs or credentials.
            tracing::warn!(path, reason = %error, "Skipping unavailable LFS input");
            fs::remove_file(request.staging.join(path))?;
            prepared.unavailable.insert(path.clone());
        }
        Ok(())
    }
}

fn cached(pointer: &Pointer, cache: &Path) -> bool {
    File::open(cache.join(&pointer.oid))
        .is_ok_and(|file| verified_copy(file, &mut std::io::sink(), pointer).is_ok())
}

fn fetch(
    client: &Client,
    action: &Action,
    pointer: &Pointer,
    cache: &Path,
    files: &crate::cache::blobs::Store,
    transferred: &mut u64,
) -> Result<()> {
    let cached = cache.join(&pointer.oid);
    let response = client
        .get(action.url()?)
        .headers(action.headers()?)
        .send()
        .context("LFS download failed")?;
    ensure!(
        response.status().is_success(),
        "LFS download failed ({})",
        response.status()
    );
    let mut temporary = tempfile::NamedTempFile::new_in(cache)?;
    struct Counted<'a, R> {
        input: R,
        count: &'a mut u64,
    }
    impl<R: Read> Read for Counted<'_, R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.input.read(bytes)?;
            *self.count += n as u64;
            Ok(n)
        }
    }
    verified_copy(
        Counted {
            input: response.take(pointer.size),
            count: transferred,
        },
        &mut temporary,
        pointer,
    )?;
    temporary.as_file().sync_all()?;
    temporary.persist(&cached)?;
    files.import(&cached)?;
    Ok(())
}

pub fn hydrate(request: &Request, prepared: &mut Prepared, cache: &Path) -> Result<()> {
    if prepared.selected.is_empty() {
        return Ok(());
    }
    let repository = Repository::parse(&request.repository)?.context("Missing repository")?;
    let client = std::sync::LazyLock::new(|| {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(20))
            .build()
    });
    let transport = prepared.transport.clone();
    let mut authentication = None;
    hydrate_with(
        request,
        prepared,
        cache,
        |pointers| {
            let authentication = authentication
                .get_or_insert_with(|| {
                    endpoint(&repository, transport.as_deref(), request.allow_private)
                        .map_err(|error| error.to_string())
                })
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            batch(
                client
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error}"))?,
                authentication,
                pointers,
            )
        },
        |action, pointer, cache, files, transferred| {
            fetch(
                client
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error}"))?,
                action,
                pointer,
                cache,
                files,
                transferred,
            )
        },
    )
}

fn hydrate_with(
    request: &Request,
    prepared: &mut Prepared,
    cache: &Path,
    mut acquire: impl FnMut(&[&Pointer]) -> Result<BTreeMap<String, Object>>,
    download: impl Fn(&Action, &Pointer, &Path, &crate::cache::blobs::Store, &mut u64) -> Result<()>
    + Sync,
) -> Result<()> {
    let objects = cache.join("lfs");
    // Leave time for Git preparation and publication under the worker deadline.
    // Later preparations retry unavailable inputs and reuse verified objects.
    let started = Instant::now();
    let mut pending: BTreeMap<String, Pending> = BTreeMap::new();
    for path in prepared.selected.keys() {
        let target = request.staging.join(path);
        if !target.is_file() {
            continue;
        }
        let result = (|| -> Result<()> {
            let mut bytes = Vec::new();
            File::open(&target)?.take(8192).read_to_end(&mut bytes)?;
            let Some(pointer) = Pointer::parse(&bytes)? else {
                return Ok(());
            };
            let asset = super::selection::asset(Path::new(path))
                || super::selection::visual_graph(Path::new(path));
            let code = matches!(
                Path::new(path).extension().and_then(|e| e.to_str()),
                Some("cs" | "rs" | "dll" | "meta")
            );
            if !asset
                && !code
                && crate::documents::language(Path::new(path)).is_none()
                && crate::native::language(Path::new(path)).is_none()
            {
                prepared.omitted.insert(
                    path.clone(),
                    "LFS payload format was not selected for analysis".into(),
                );
                fs::remove_file(&target)?;
                return Ok(());
            }
            if asset && pointer.size > super::selection::ASSET_LIMIT {
                prepared.omitted.insert(
                    path.clone(),
                    format!("LFS asset exceeds size limit ({} bytes)", pointer.size),
                );
                fs::remove_file(&target)?;
                return Ok(());
            }
            if let Some(existing) = pending.get_mut(&pointer.oid) {
                ensure!(existing.pointer == pointer, "Conflicting LFS object sizes");
                existing.paths.push(path.clone());
            } else {
                pending.insert(
                    pointer.oid.clone(),
                    Pending {
                        pointer,
                        paths: vec![path.clone()],
                    },
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Log only our context, never remote response bodies, URLs or credentials.
            tracing::warn!(path, reason = %error, "Skipping unavailable LFS input");
            fs::remove_file(&target)?;
            prepared.unavailable.insert(path.clone());
        }
    }
    prepared
        .selected
        .retain(|path, _| !prepared.omitted.contains_key(path));
    if pending.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&objects)?;
    let files = crate::cache::blobs::Store::open(cache)?;
    // Protect cached objects and new downloads until every staged copy is published.
    let _lease = files.lease()?;
    let jobs: Vec<_> = pending.into_values().collect();
    // Each batch reserves its full declared size before concurrent transfers start.
    // Failures charge only bytes actually read, just as successful transfers do.
    for group in jobs.chunks(4) {
        let mut missing = Vec::new();
        for job in group {
            if cached(&job.pointer, &objects) {
                let cached = objects.join(&job.pointer.oid);
                let result = files
                    .import(&cached)
                    .and_then(|_| job.publish(&cached, &request.staging));
                if let Err(error) = result {
                    job.unavailable(&error, request, prepared)?;
                }
            } else {
                missing.push(job);
            }
        }
        if missing.is_empty() {
            continue;
        }
        let total = missing
            .iter()
            .try_fold(0u64, |total, job| total.checked_add(job.pointer.size))
            .context("LFS batch size overflow")?;
        ensure!(
            request
                .remaining(prepared.transfer_bytes)
                .is_none_or(|n| total <= n),
            "Repository transfer limit exceeded by LFS objects"
        );
        let batch = (|| -> Result<_> {
            ensure!(
                started.elapsed() < Duration::from_secs(90),
                "LFS preparation time budget exhausted; retrying on the next preparation"
            );
            acquire(&missing.iter().map(|job| &job.pointer).collect::<Vec<_>>())
        })();
        let mut batch = match batch {
            Ok(batch) => batch,
            Err(error) => {
                for job in missing {
                    job.unavailable(&error, request, prepared)?;
                }
                continue;
            }
        };
        let mut downloads = Vec::new();
        for job in missing {
            let action = batch
                .remove(&job.pointer.oid)
                .filter(|object| object.size == job.pointer.size)
                .and_then(|mut object| object.actions.remove("download"));
            match action {
                Some(action) => downloads.push((job, action)),
                None => job.unavailable(
                    &anyhow::anyhow!("LFS object is unavailable"),
                    request,
                    prepared,
                )?,
            }
        }
        let results = std::thread::scope(|scope| -> Result<Vec<_>> {
            let handles: Vec<_> = downloads
                .iter()
                .map(|(job, action)| {
                    let download = &download;
                    let objects = &objects;
                    let files = &files;
                    scope.spawn(move || {
                        let mut transferred = 0;
                        let result =
                            download(action, &job.pointer, objects, files, &mut transferred)
                                .and_then(|_| {
                                    job.publish(&objects.join(&job.pointer.oid), &request.staging)
                                });
                        (*job, transferred, result)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("LFS download worker panicked"))
                })
                .collect()
        })?;
        for (job, transferred, result) in results {
            prepared.transfer_bytes += transferred;
            if let Err(error) = result {
                job.unavailable(&error, request, prepared)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preparation(root: &Path, inputs: &[(&str, &[u8])]) -> (Request, Prepared) {
        let staging = root.join("source");
        fs::create_dir_all(&staging).unwrap();
        for (path, bytes) in inputs {
            fs::write(
                staging.join(path),
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{:x}\nsize {}\n",
                    Sha256::digest(bytes),
                    bytes.len()
                ),
            )
            .unwrap();
        }
        (
            Request {
                contents: super::super::materialize::Contents::All,
                transfer_used: 0,
                unlimited_transfer: false,
                repository: "https://example.invalid/owner/repo".into(),
                allow_private: false,
                preferred_transport: None,
                target: super::super::materialize::Target::Commit(String::new()),
                store: root.join("git"),
                staging,
                include: vec![],
                exclude: vec![],
                required: vec![],
                additional: vec![],
                previous: BTreeMap::new(),
                subdirectory: None,
            },
            Prepared {
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
                branch: None,
                revision: String::new(),
                selected: inputs
                    .iter()
                    .map(|(path, _)| (path.to_string(), "git-blob".into()))
                    .collect(),
                tracked: Default::default(),
                directories: Default::default(),
            },
        )
    }

    fn actions(pointers: &[&Pointer]) -> Result<BTreeMap<String, Object>> {
        Ok(pointers
            .iter()
            .map(|pointer| {
                (
                    pointer.oid.clone(),
                    Object {
                        oid: pointer.oid.clone(),
                        size: pointer.size,
                        actions: BTreeMap::from([(
                            "download".into(),
                            Action {
                                href: "https://example.invalid/object".into(),
                                header: BTreeMap::new(),
                            },
                        )]),
                    },
                )
            })
            .collect())
    }

    #[test]
    fn source_navigation_uses_git_contents_before_lfs_hydration() {
        let root = tempfile::tempdir().unwrap();
        let (request, prepared) = preparation(
            root.path(),
            &[
                ("Ready.cs", b"class Ready {}"),
                ("Pending.cs", b"class Pending {}"),
            ],
        );
        fs::write(request.staging.join("Ready.cs"), "class Ready {}").unwrap();
        let mut snapshot = prepared.clone();
        snapshot
            .unavailable
            .extend(pending(&request.staging, &prepared).unwrap());
        let published = root.path().join("published");
        let view = |path| {
            crate::navigation::sources::repository(
                &published,
                Some(&request.staging),
                &snapshot,
                path,
                Some(crate::navigation::Mode::Exact),
                |_| panic!("Materialized sources must not be downloaded"),
            )
            .unwrap()
        };
        let text = view("Ready.cs").unwrap();
        assert!(text.contains("class Ready {}"), "{text}");
        assert!(!text.contains(&request.staging.to_string_lossy().to_string()));
        assert!(view("Pending.cs").is_none());
        assert!(!published.exists());
    }

    #[test]
    fn batch_downloads_share_objects_and_run_concurrently() {
        use std::sync::{Condvar, Mutex};
        let root = tempfile::tempdir().unwrap();
        let inputs: &[(&str, &[u8])] = &[
            ("a.cs", b"class A {}"),
            ("b.cs", b"class B {}"),
            ("copy.cs", b"class A {}"),
        ];
        let (request, mut prepared) = preparation(root.path(), inputs);
        let contents: BTreeMap<_, _> = inputs
            .iter()
            .map(|(_, bytes)| (format!("{:x}", Sha256::digest(bytes)), *bytes))
            .collect();
        let entered = (Mutex::new(0), Condvar::new());
        hydrate_with(
            &request,
            &mut prepared,
            root.path(),
            |pointers| {
                assert_eq!(pointers.len(), contents.len());
                actions(pointers)
            },
            |_, pointer, cache, _, transferred| {
                let mut count = entered.0.lock().unwrap();
                *count += 1;
                entered.1.notify_all();
                let (count, _) = entered
                    .1
                    .wait_timeout_while(count, Duration::from_secs(5), |count| {
                        *count < contents.len()
                    })
                    .unwrap();
                ensure!(*count == contents.len(), "Downloads did not overlap");
                drop(count);
                let bytes = contents[&pointer.oid];
                *transferred += bytes.len() as u64;
                verified_copy(bytes, &mut File::create(cache.join(&pointer.oid))?, pointer)
            },
        )
        .unwrap();
        assert!(prepared.unavailable.is_empty());
        assert_eq!(
            prepared.transfer_bytes,
            contents
                .values()
                .map(|bytes| bytes.len() as u64)
                .sum::<u64>()
        );
        for (path, bytes) in inputs {
            assert_eq!(fs::read(request.staging.join(path)).unwrap(), *bytes);
        }
    }

    #[test]
    fn concurrent_batch_reserves_the_combined_transfer_budget() {
        let root = tempfile::tempdir().unwrap();
        let inputs: &[(&str, &[u8])] = &[("a.cs", b"aaa"), ("b.cs", b"bbb")];
        let (request, mut prepared) = preparation(root.path(), inputs);
        let size: u64 = inputs.iter().map(|(_, bytes)| bytes.len() as u64).sum();
        prepared.transfer_bytes = request.remaining(0).unwrap() - size + 1;
        let before = prepared.transfer_bytes;
        assert!(
            hydrate_with(
                &request,
                &mut prepared,
                root.path(),
                |_| panic!("Budget must be checked before acquisition"),
                |_, _, _, _, _| panic!("Budget must be checked before download")
            )
            .is_err()
        );
        assert_eq!(prepared.transfer_bytes, before);
    }

    #[test]
    fn failed_downloads_charge_partial_bytes_and_do_not_discard_other_objects() {
        let root = tempfile::tempdir().unwrap();
        let inputs: &[(&str, &[u8])] = &[
            ("good.cs", b"class A {}"),
            ("bad.cs", b"bad"),
            ("copy.cs", b"bad"),
        ];
        let (request, mut prepared) = preparation(root.path(), inputs);
        let good = format!("{:x}", Sha256::digest(inputs[0].1));
        hydrate_with(
            &request,
            &mut prepared,
            root.path(),
            actions,
            |_, pointer, cache, _, transferred| {
                if pointer.oid != good {
                    *transferred += 1;
                    anyhow::bail!("Interrupted download");
                }
                *transferred += inputs[0].1.len() as u64;
                fs::write(cache.join(&pointer.oid), inputs[0].1)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(request.staging.join("good.cs")).unwrap(),
            inputs[0].1
        );
        for path in ["bad.cs", "copy.cs"] {
            assert!(prepared.unavailable.contains(path));
            assert!(!request.staging.join(path).exists());
        }
        assert_eq!(prepared.transfer_bytes, inputs[0].1.len() as u64 + 1);
    }

    #[test]
    fn verified_cache_hits_require_no_remote_access() {
        let root = tempfile::tempdir().unwrap();
        let contents = b"class Cached {}";
        let (request, mut prepared) = preparation(root.path(), &[("cached.cs", contents)]);
        fs::create_dir(root.path().join("lfs")).unwrap();
        fs::write(
            root.path()
                .join("lfs")
                .join(format!("{:x}", Sha256::digest(contents))),
            contents,
        )
        .unwrap();
        hydrate_with(
            &request,
            &mut prepared,
            root.path(),
            |_| panic!("Cached object must not require authentication"),
            |_, _, _, _, _| panic!("Cached object must not be downloaded"),
        )
        .unwrap();
        assert!(prepared.unavailable.is_empty());
        assert_eq!(
            fs::read(request.staging.join("cached.cs")).unwrap(),
            contents
        );
        assert_eq!(prepared.transfer_bytes, 0);
    }

    #[test]
    fn ssh_lfs_arguments_round_trip_without_shell_expansion() {
        for path in [
            "owner/repo.git",
            "a b",
            "a'b",
            "$(printf bad)",
            "a;printf bad",
        ] {
            let output = Command::new("sh")
                .args(["-c", &format!("printf '%s' {}", shell_argument(path))])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, path.as_bytes());
        }
    }
    #[test]
    fn missing_objects_can_recover_and_cached_objects_replace_pointers() {
        let root = tempfile::tempdir().unwrap();
        let sources = root.path().join("source");
        fs::create_dir(&sources).unwrap();
        let path = "Code.cs";
        let request = Request {
            contents: super::super::materialize::Contents::All,
            transfer_used: 0,
            unlimited_transfer: false,
            repository: "https://example.invalid/owner/repo".into(),
            allow_private: false,
            preferred_transport: None,
            target: super::super::materialize::Target::Commit(String::new()),
            store: root.path().join("git"),
            staging: sources.clone(),
            include: vec![],
            exclude: vec![],
            required: vec![],
            additional: vec![],
            previous: BTreeMap::new(),
            subdirectory: None,
        };
        let prepared = || Prepared {
            omitted: Default::default(),
            transfer_bytes: 0,
            unavailable: Default::default(),
            transport: None,
            resolved_target: None,
            branch: None,
            revision: String::new(),
            selected: BTreeMap::from([(path.into(), "git-blob".into())]),
            tracked: Default::default(),
            directories: Default::default(),
        };
        fs::write(
            sources.join(path),
            "version https://git-lfs.github.com/spec/v1\nsize 1\n",
        )
        .unwrap();
        let mut failed = prepared();
        hydrate(&request, &mut failed, root.path()).unwrap();
        assert!(failed.unavailable.contains(path));
        assert!(!sources.join(path).exists());

        let contents = b"class Recovered {}";
        let oid = format!("{:x}", Sha256::digest(contents));
        let objects = root.path().join("lfs");
        fs::create_dir_all(&objects).unwrap();
        fs::write(objects.join(&oid), contents).unwrap();
        fs::write(
            sources.join(path),
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
                contents.len()
            ),
        )
        .unwrap();
        let mut recovered = prepared();
        hydrate(&request, &mut recovered, root.path()).unwrap();
        assert!(recovered.unavailable.is_empty());
        assert_eq!(fs::read(sources.join(path)).unwrap(), contents);
        assert_eq!(recovered.selected, failed.selected);
        for extension in ["cpp", "h", "vert", "frag", "rs", "hlsl"] {
            let path = format!("cached.{extension}");
            fs::write(
                sources.join(&path),
                format!(
                    "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
                    contents.len()
                ),
            )
            .unwrap();
            let mut cached = prepared();
            cached.selected = BTreeMap::from([(path.clone(), "git-blob".into())]);
            hydrate(&request, &mut cached, root.path()).unwrap();
            assert!(
                cached.omitted.is_empty(),
                "{extension}: {:?}",
                cached.omitted
            );
            assert!(cached.unavailable.is_empty());
            assert_eq!(fs::read(sources.join(path)).unwrap(), contents);
        }
    }
    #[test]
    fn pointers_and_downloads_require_valid_identity_and_size() {
        let bytes = b"class Example {}";
        let oid = format!("{:x}", Sha256::digest(bytes));
        let pointer = Pointer::parse(
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
                bytes.len()
            )
            .as_bytes(),
        )
        .unwrap()
        .unwrap();
        let mut output = Vec::new();
        verified_copy(bytes.as_slice(), &mut output, &pointer).unwrap();
        assert_eq!(output, bytes);
        assert!(verified_copy(&bytes[..bytes.len() - 1], &mut Vec::new(), &pointer).is_err());
        assert!(verified_copy(b"class Wrongxx {}".as_slice(), &mut Vec::new(), &pointer).is_err());
        assert!(Pointer::parse(bytes).unwrap().is_none());
        assert!(Pointer::parse(b"version https://git-lfs.github.com/spec/v1\nsize 1\n").is_err());
    }
}
