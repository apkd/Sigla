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

#[derive(Debug, PartialEq, Eq)]
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
            Command::new("git")
                .args(["credential", "fill"])
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ASKPASS", "/bin/false"),
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

fn fetch(
    client: &Client,
    endpoint: &Action,
    pointer: &Pointer,
    cache: &Path,
    target: &Path,
    request: &Request,
    transferred: &mut u64,
) -> Result<()> {
    fs::create_dir_all(cache)?;
    let cached = cache.join(&pointer.oid);
    if let Ok(file) = File::open(&cached)
        && verified_copy(file, &mut std::io::sink(), pointer).is_ok()
    {
        fs::copy(&cached, target)?;
        return Ok(());
    }
    ensure!(
        request
            .remaining(*transferred)
            .is_none_or(|n| pointer.size <= n),
        "Repository transfer limit exceeded by LFS object"
    );
    let mut url = endpoint.url()?;
    url.set_path(&format!(
        "{}/objects/batch",
        url.path().trim_end_matches('/')
    ));
    let response = client.post(url).headers(endpoint.headers()?)
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(&serde_json::json!({"operation":"download", "transfers":["basic"], "objects":[{"oid":pointer.oid,"size":pointer.size}]}))
        .send().context("LFS batch request failed")?;
    ensure!(
        response.status().is_success(),
        "LFS batch request failed ({})",
        response.status()
    );
    let batch: Batch = serde_json::from_reader(response.take(1024 * 1024))
        .context("Invalid LFS batch response")?;
    let object = batch
        .objects
        .into_iter()
        .find(|object| object.oid == pointer.oid && object.size == pointer.size)
        .context("LFS batch response omitted the requested object")?;
    let action = object
        .actions
        .get("download")
        .context("LFS object is unavailable")?;
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
    fs::copy(cached, target)?;
    Ok(())
}

pub fn hydrate(request: &Request, prepared: &mut Prepared, cache: &Path) -> Result<()> {
    if prepared.selected.is_empty() {
        return Ok(());
    }
    let repository = Repository::parse(&request.repository)?.context("Missing repository")?;
    let objects = cache.join("lfs");
    // Leave time for Git preparation and publication under the worker deadline.
    // Later preparations retry unavailable inputs and reuse verified objects.
    let started = Instant::now();
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut authentication = None;
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
            if !asset && !code && crate::documents::language(Path::new(path)).is_none() {
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
            ensure!(
                started.elapsed() < Duration::from_secs(90),
                "LFS preparation time budget exhausted; retrying on the next preparation"
            );
            let authentication = authentication
                .get_or_insert_with(|| {
                    endpoint(
                        &repository,
                        prepared.transport.as_deref(),
                        request.allow_private,
                    )
                    .map_err(|error| error.to_string())
                })
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            fetch(
                &client,
                authentication,
                &pointer,
                &objects,
                &target,
                request,
                &mut prepared.transfer_bytes,
            )
        })();
        if let Err(error) = result {
            if error.to_string().contains("transfer limit exceeded") {
                return Err(error);
            }
            // Log only our context, never remote response bodies, URLs or credentials.
            tracing::warn!(path, reason = %error, "Skipping unavailable LFS input");
            fs::remove_file(&target)?;
            prepared.unavailable.insert(path.clone());
        }
    }
    prepared
        .selected
        .retain(|path, _| !prepared.omitted.contains_key(path));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
