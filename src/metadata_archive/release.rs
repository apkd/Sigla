use super::*;
use reqwest::{
    Method, StatusCode,
    blocking::{Client, RequestBuilder, Response},
    header::HeaderMap,
};
use serde::Deserialize as JsonDeserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::{
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, JsonDeserialize)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
}
#[derive(Clone, JsonDeserialize)]
struct Release {
    id: u64,
    tag_name: String,
}
struct Github {
    client: Client,
    repo: String,
    cooldown: Mutex<Instant>,
}
impl Github {
    fn new(repo: &str) -> Result<Self> {
        ensure!(
            repo.split('/').count() == 2
                && repo.split('/').all(|s| !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "Invalid GitHub repository"
        );
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .context("Publication requires GH_TOKEN")?;
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("sigla-metadata"),
        );
        Ok(Self {
            repo: repo.into(),
            cooldown: Mutex::new(Instant::now()),
            client: Client::builder()
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(3600))
                .build()?,
        })
    }
    fn send(&self, request: impl Fn() -> Result<RequestBuilder>) -> Result<Response> {
        for attempt in 0..=3 {
            loop {
                let delay = self
                    .cooldown
                    .lock()
                    .unwrap()
                    .saturating_duration_since(Instant::now());
                if delay.is_zero() {
                    break;
                }
                std::thread::sleep(delay);
            }
            let response = request()?.send()?;
            let Err(error) = response.error_for_status_ref() else {
                return Ok(response);
            };
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text()?;
            let Some(delay) = rate_limit_delay(status, &headers, &body, attempt) else {
                return Err(error.into());
            };
            let mut cooldown = self.cooldown.lock().unwrap();
            *cooldown = (*cooldown).max(Instant::now() + delay);
            if attempt == 3 {
                return Err(error.into());
            }
            tracing::warn!(
                seconds = delay.as_secs(),
                "GitHub rate limit; pausing publication requests"
            );
        }
        unreachable!()
    }
    fn api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("https://api.github.com/repos/{}/{}", self.repo, path);
        let response = self.send(|| {
            let mut request = self.client.request(method.clone(), &url);
            if let Some(body) = &body {
                request = request.json(body);
            }
            Ok(request)
        })?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            Ok(Value::Null)
        } else {
            Ok(response.json()?)
        }
    }
    fn pages<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut result = Vec::new();
        for page in 1..=10_000 {
            let values: Vec<T> = serde_json::from_value(self.api(
                Method::GET,
                &format!("{path}?per_page=100&page={page}"),
                None,
            )?)?;
            let count = values.len();
            result.extend(values);
            if count < 100 {
                return Ok(result);
            }
        }
        anyhow::bail!("GitHub pagination exceeds bound")
    }
    fn releases(&self) -> Result<Vec<Release>> {
        Ok(self
            .pages::<Release>("releases")?
            .into_iter()
            .filter(|r| {
                r.tag_name == TAG
                    || r.tag_name
                        .strip_prefix(&format!("{TAG}-"))
                        .is_some_and(|s| s.parse::<u32>().is_ok())
            })
            .collect())
    }
    fn ensure_release(&self, tag: &str) -> Result<Release> {
        if let Some(release) = self.releases()?.into_iter().find(|r| r.tag_name == tag) {
            return Ok(release);
        }
        Ok(serde_json::from_value(self.api(Method::POST, "releases", Some(json!({"tag_name":tag,"name":"Metadata archive","body":"---","make_latest":"false"})))?)?)
    }
    fn assets(&self, release: &Release) -> Result<Vec<Asset>> {
        self.pages(&format!("releases/{}/assets", release.id))
    }
    fn delete(&self, asset: &Asset) -> Result<()> {
        self.api(
            Method::DELETE,
            &format!("releases/assets/{}", asset.id),
            None,
        )?;
        Ok(())
    }
    fn rename(&self, asset: &Asset, name: &str) -> Result<()> {
        self.api(
            Method::PATCH,
            &format!("releases/assets/{}", asset.id),
            Some(json!({"name":name})),
        )?;
        Ok(())
    }
    fn upload(&self, release: &Release, path: &Path, name: &str) -> Result<Asset> {
        Ok(self
            .send(|| {
                let file = fs::File::open(path)?;
                Ok(self
                    .client
                    .post(format!(
                        "https://uploads.github.com/repos/{}/releases/{}/assets",
                        self.repo, release.id
                    ))
                    .query(&[("name", name)])
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .header(reqwest::header::CONTENT_LENGTH, file.metadata()?.len())
                    .body(file))
            })?
            .json()?)
    }
}

fn rate_limit_delay(
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
    attempt: u32,
) -> Option<Duration> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    if let Some(seconds) = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    if headers
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0")
    {
        if let Some(reset) = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        {
            return Some(
                (UNIX_EPOCH + Duration::from_secs(reset))
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    + Duration::from_secs(1),
            );
        }
        return Some(Duration::from_secs(60 << attempt));
    }
    (status == StatusCode::TOO_MANY_REQUESTS || body.contains("rate limit"))
        .then(|| Duration::from_secs(60 << attempt))
}

pub(super) fn publish(
    repo: &str,
    base: producer::Plan,
    packages: registry::Plan,
    input: &Path,
) -> Result<()> {
    let started = Instant::now();
    let fresh = producer::artifacts(input)?;
    let files = producer::files(input)?;
    let mut unavailable = packages.unavailable;
    let mut download_failures = Vec::<String>::new();
    for path in &files {
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("unavailable-"))
            && path.extension().is_some_and(|e| e == "bin")
        {
            download_failures.extend(binary::decode::<Vec<String>>(payload(&read(path)?)?)?);
        }
    }
    let mut selected = Vec::new();
    for origin in base
        .editors
        .iter()
        .chain(packages.requests.iter().map(|r| &r.origin))
    {
        match producer::find(&fresh, &base.previous.artifacts, origin) {
            Some(artifact) => selected.push(artifact.clone()),
            None if origin.kind == "package"
                && download_failures.iter().any(|f| {
                    f.starts_with(&format!("{}@{}: HTTP ", origin.name, origin.version))
                }) => {}
            None => anyhow::bail!(
                "Missing output {}@{}; current archive remains published",
                origin.name,
                origin.version
            ),
        }
    }
    unavailable.extend(download_failures);
    unavailable.sort();
    unavailable.dedup();
    let github = Github::new(repo)?;
    let main = github.ensure_release(TAG)?;
    let existing = github.assets(&main)?;
    if !existing.iter().any(|asset| asset.name == "catalog.bin")
        && let Some(retiring) = existing
            .iter()
            .find(|asset| asset.name == "catalog.retiring.bin")
    {
        github.rename(retiring, "catalog.bin")?;
    }
    let mut releases = github.releases()?;
    releases.sort_by(|a, b| a.tag_name.cmp(&b.tag_name));
    let mut inventory = releases
        .iter()
        .map(|r| Ok((r.clone(), github.assets(r)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut uploaded = Vec::new();
    let publication = (|| -> Result<()> {
        let mut reused = 0;
        let mut bytes = 0;
        for artifact in &mut selected {
            if let Some((release, existing)) = inventory.iter().find_map(|(r, assets)| {
                assets
                    .iter()
                    .find(|a| a.name == artifact.name)
                    .map(|a| (r, a))
            }) {
                ensure!(
                    existing.size == artifact.size,
                    "Existing release asset size differs"
                );
                artifact.release = release.tag_name.clone();
                reused += 1;
                continue;
            }
            let path = files
                .iter()
                .find(|p| p.file_name().is_some_and(|n| n == artifact.name.as_str()))
                .context("New artifact has no local bundle")?;
            let started = Instant::now();
            // The producer already validated these records. Check that the
            // handoff contains the exact bytes it verified.
            bundle::verify_digest(path, artifact)?;
            let verified = started.elapsed();
            let bucket =
                if let Some(index) = inventory.iter().position(|(_, assets)| assets.len() < 997) {
                    index
                } else {
                    let mut suffix = 2;
                    while inventory
                        .iter()
                        .any(|(r, _)| r.tag_name == format!("{TAG}-{suffix}"))
                    {
                        suffix += 1;
                    }
                    let release = github.ensure_release(&format!("{TAG}-{suffix}"))?;
                    inventory.push((release, vec![]));
                    inventory.len() - 1
                };
            let (release, assets) = &mut inventory[bucket];
            let asset = github.upload(release, path, &artifact.name)?;
            tracing::info!(
                name = %artifact.name,
                bytes = artifact.size,
                hash_ms = verified.as_millis(),
                upload_ms = (started.elapsed() - verified).as_millis(),
                "Uploaded metadata bundle"
            );
            artifact.release = release.tag_name.clone();
            bytes += artifact.size;
            uploaded.push(asset.clone());
            assets.push(asset);
        }
        tracing::info!(
            reused,
            uploads = uploaded.len(),
            bytes,
            "Published metadata bundles"
        );
        selected.sort_by(|a, b| a.name.cmp(&b.name));
        let catalog = Catalog {
            artifacts: selected.clone(),
            unavailable,
        };
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("catalog.bin");
        save(&path, &binary::encode(&catalog)?)?;
        let assets = github.assets(&main)?;
        for stale in assets
            .iter()
            .filter(|a| a.name == "catalog.pending.bin" || a.name == "catalog.retiring.bin")
        {
            github.delete(stale)?;
        }
        let pending = github.upload(&main, &path, "catalog.pending.bin")?;
        uploaded.push(pending.clone());
        let old = assets.iter().find(|a| a.name == "catalog.bin");
        switch_catalog(old, &pending, |asset, name| github.rename(asset, name))?;
        Ok(())
    })();
    if publication.is_err() {
        let assets = github
            .assets(&main)
            .context("Publication state is unknown; staging assets remain for recovery")?;
        let committed = assets
            .iter()
            .find(|a| a.name == "catalog.bin")
            .is_some_and(|current| uploaded.iter().any(|a| a.id == current.id));
        if committed {
            tracing::warn!("Catalog was published despite an interrupted response");
        } else {
            if !assets.iter().any(|a| a.name == "catalog.bin")
                && let Some(retiring) = assets.iter().find(|a| a.name == "catalog.retiring.bin")
            {
                github.rename(retiring, "catalog.bin").context(
                    "Cannot restore the current catalog; staging assets remain for recovery",
                )?;
            }
            for asset in &uploaded {
                if let Err(error) = github.delete(asset) {
                    tracing::error!("Cannot remove unpublished asset {}: {error:#}", asset.name);
                }
            }
            return publication;
        }
    }
    let keep: BTreeSet<_> = selected
        .iter()
        .map(|a| (a.release.clone(), a.name.clone()))
        .chain(std::iter::once((TAG.to_owned(), "catalog.bin".to_owned())))
        .collect();
    let cleanup = Instant::now();
    let mut removed = 0;
    for release in github.releases()? {
        let assets = github.assets(&release)?;
        let obsolete: Vec<_> = assets
            .iter()
            .filter(|a| !keep.contains(&(release.tag_name.clone(), a.name.clone())))
            .collect();
        for asset in &obsolete {
            github.delete(asset)?;
            removed += 1;
        }
        if obsolete.len() == assets.len() && release.tag_name != TAG {
            github.api(Method::DELETE, &format!("releases/{}", release.id), None)?;
            github.api(
                Method::DELETE,
                &format!("git/refs/tags/{}", release.tag_name),
                None,
            )?;
        }
    }
    tracing::info!(
        artifacts = selected.len(),
        removed,
        cleanup_seconds = cleanup.elapsed().as_secs_f64(),
        total_seconds = started.elapsed().as_secs_f64(),
        "Published metadata archive and removed obsolete assets"
    );
    Ok(())
}

fn switch_catalog(
    old: Option<&Asset>,
    pending: &Asset,
    mut rename: impl FnMut(&Asset, &str) -> Result<()>,
) -> Result<()> {
    if let Some(old) = old {
        rename(old, "catalog.retiring.bin")?;
    }
    if let Err(error) = rename(pending, "catalog.bin") {
        if let Some(old) = old {
            rename(old, "catalog.bin")
                .context("Cannot restore catalog after publication failure")?;
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_wait_but_permission_errors_do_not_retry() {
        let mut headers = HeaderMap::new();
        assert!(
            rate_limit_delay(StatusCode::FORBIDDEN, &headers, "Permission denied", 0).is_none()
        );
        let first = rate_limit_delay(StatusCode::TOO_MANY_REQUESTS, &headers, "", 0).unwrap();
        let second = rate_limit_delay(StatusCode::TOO_MANY_REQUESTS, &headers, "", 1).unwrap();
        assert!(!first.is_zero());
        assert!(second > first);
        assert!(
            rate_limit_delay(StatusCode::FORBIDDEN, &headers, "secondary rate limit", 0).is_some()
        );
        let requested = Duration::from_secs(17);
        headers.insert(
            "retry-after",
            requested.as_secs().to_string().parse().unwrap(),
        );
        assert_eq!(
            rate_limit_delay(StatusCode::TOO_MANY_REQUESTS, &headers, "", 0),
            Some(requested)
        );
        assert!(rate_limit_delay(StatusCode::BAD_REQUEST, &headers, "", 0).is_none());
        headers.remove("retry-after");
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert!(rate_limit_delay(StatusCode::FORBIDDEN, &headers, "", 0).is_some());
    }

    #[test]
    fn interrupted_catalog_switch_restores_the_current_pointer() {
        let old = Asset {
            id: 1,
            name: "catalog.bin".into(),
            size: 0,
        };
        let pending = Asset {
            id: 2,
            name: "catalog.pending.bin".into(),
            size: 0,
        };
        for failure in [Some(1), Some(2), None] {
            let mut names = BTreeMap::from([
                (old.id, old.name.clone()),
                (pending.id, pending.name.clone()),
            ]);
            let mut calls = 0;
            let result = switch_catalog(Some(&old), &pending, |asset, name| {
                calls += 1;
                if failure == Some(calls) {
                    anyhow::bail!("Interrupted");
                }
                ensure!(
                    !names.values().any(|value| value == name),
                    "Duplicate asset name"
                );
                names.insert(asset.id, name.into());
                Ok(())
            });
            let current = names
                .iter()
                .find(|(_, name)| *name == "catalog.bin")
                .unwrap()
                .0;
            if failure.is_some() {
                assert!(result.is_err());
                assert_eq!(*current, old.id);
            } else {
                assert!(result.is_ok());
                assert_eq!(*current, pending.id);
            }
        }
    }
}
