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
    state: String,
}
#[derive(Clone, JsonDeserialize)]
struct Release {
    id: u64,
    tag_name: String,
    draft: bool,
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
                        .is_some_and(|s| {
                            s.parse::<u32>().is_ok()
                                || s.strip_prefix('g').is_some_and(|s| {
                                    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
                                })
                        })
            })
            .collect())
    }
    fn ensure_release(&self, tag: &str) -> Result<Release> {
        if let Some(release) = self.releases()?.into_iter().find(|r| r.tag_name == tag) {
            return Ok(release);
        }
        Ok(serde_json::from_value(self.api(Method::POST, "releases", Some(json!({"tag_name":tag,"name":"Metadata archive","body":"---","draft":true,"make_latest":"false"})))?)?)
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
    selected.sort_by(|a, b| a.name.cmp(&b.name));
    let work = tempfile::tempdir()?;
    let groups = packing::groups(&selected, &base.previous.packs);
    ensure!(
        groups.len() + 1 < 1000,
        "Selection requires {} release assets; improve package grouping before publishing",
        groups.len() + 1
    );
    let mut packs = Vec::new();
    for group in groups.iter().filter(|g| g.len() > 1) {
        let members: Vec<_> = group.iter().map(|&i| &selected[i]).collect();
        if let Some(previous) = base
            .previous
            .packs
            .iter()
            .find(|p| p.members == members.iter().map(|a| a.name.clone()).collect::<Vec<_>>())
        {
            packs.push(previous.clone());
        } else {
            packs.push(packing::build(&members, work.path(), |a| {
                source(repo, a, &files, &base.previous, work.path())
            })?);
        }
    }
    let mut catalog = Catalog {
        artifacts: selected,
        unavailable,
        packs,
    };
    let packed: BTreeSet<_> = catalog
        .packs
        .iter()
        .flat_map(|p| p.members.iter().cloned())
        .collect();
    let mut blobs: Vec<_> = catalog
        .artifacts
        .iter()
        .filter(|a| !packed.contains(&a.name))
        .map(|a| (a.name.clone(), a.hash, a.size, a.release.clone()))
        .chain(
            catalog
                .packs
                .iter()
                .map(|p| (p.name.clone(), p.hash, p.size, p.release.clone())),
        )
        .collect();
    blobs.sort_by(|a, b| a.0.cmp(&b.0));
    let inventory = github
        .releases()?
        .into_iter()
        .map(|r| {
            let assets = github.assets(&r)?;
            Ok((r, assets))
        })
        .collect::<Result<Vec<_>>>()?;
    let active = inventory.iter().find(|(r, assets)| {
        !r.draft
            && assets
                .iter()
                .any(|a| matches!(a.name.as_str(), "catalog.bin" | "catalog.retiring.bin"))
            && base
                .previous
                .artifacts
                .iter()
                .all(|a| a.release == r.tag_name)
            && base.previous.packs.iter().all(|p| p.release == r.tag_name)
    });
    let target = if let Some((release, assets)) =
        active.filter(|(_, assets)| !base.previous.artifacts.is_empty() && fits(assets, &blobs))
    {
        tracing::info!(release = %release.tag_name, existing = assets.len(), "Staging update in the current release");
        release.clone()
    } else {
        // Content determines the draft name, so a retry reuses completed uploads.
        let identity: Vec<_> = blobs.iter().map(|b| (&b.0, b.1)).collect();
        let generation = blake3::hash(&binary::encode(&identity)?);
        github.ensure_release(&format!("{TAG}-g{generation}"))?
    };
    tracing::info!(artifacts = catalog.artifacts.len(), packs = catalog.packs.len(),
        assets = blobs.len() + 1, release = %target.tag_name, draft = target.draft,
        "Metadata publication plan");
    let mut existing = github.assets(&target)?;
    if !existing.iter().any(|a| a.name == "catalog.bin")
        && let Some(retiring) = existing
            .iter_mut()
            .find(|a| a.name == "catalog.retiring.bin")
    {
        github.rename(retiring, "catalog.bin")?;
        retiring.name = "catalog.bin".into();
    }
    for asset in existing.iter().filter(|a| temporary_catalog(&a.name)) {
        github.delete(asset)?;
    }
    existing.retain(|a| !temporary_catalog(&a.name));
    let mut uploads = 0;
    let mut bytes = 0;
    for (name, hash, size, release) in &blobs {
        if existing
            .iter()
            .any(|a| &a.name == name && a.size == *size && a.state == "uploaded")
        {
            continue;
        }
        if let Some(incomplete) = existing.iter().find(|a| &a.name == name) {
            github.delete(incomplete)?;
        }
        // A full replacement copies unchanged compressed files; it never reruns analysis.
        let temporary = tempfile::tempdir_in(work.path())?;
        let path = if work.path().join(name).is_file() {
            work.path().join(name)
        } else if let Some(artifact) = catalog.artifacts.iter().find(|a| &a.name == name) {
            source(repo, artifact, &files, &base.previous, temporary.path())?
        } else {
            let path = temporary.path().join(name);
            client::download(repo, release, name, &path)?;
            path
        };
        ensure!(
            fs::metadata(&path)?.len() == *size && digest(&path)? == *hash,
            "Publication source digest or size mismatch"
        );
        let at = Instant::now();
        existing.push(github.upload(&target, &path, name)?);
        tracing::info!(
            name,
            bytes = size,
            upload_ms = at.elapsed().as_millis(),
            "Uploaded metadata asset"
        );
        uploads += 1;
        bytes += size;
    }
    for artifact in &mut catalog.artifacts {
        artifact.release.clone_from(&target.tag_name);
    }
    for pack in &mut catalog.packs {
        pack.release.clone_from(&target.tag_name);
    }
    let encoded = catalog.encode()?;
    if encoded != base.previous.encode()? || !existing.iter().any(|a| a.name == "catalog.bin") {
        let path = work.path().join("catalog.bin");
        save(&path, &encoded)?;
        let pending = github.upload(&target, &path, "catalog.pending.bin")?;
        switch_catalog(
            existing.iter().find(|a| a.name == "catalog.bin"),
            &pending,
            |asset, name| github.rename(asset, name),
        )?;
    }
    // GitHub's latest-release pointer is the stable discovery URL. Publishing a
    // complete draft switches it without renaming tags or creating an empty release.
    github.api(
        Method::PATCH,
        &format!("releases/{}", target.id),
        Some(json!({
            "draft":false, "make_latest":"true", "name":"Metadata archive", "body":"---"
        })),
    )?;
    let keep: BTreeSet<_> = blobs
        .iter()
        .map(|b| b.0.as_str())
        .chain(["catalog.bin"])
        .collect();
    let mut removed = 0;
    for release in github.releases()? {
        if release.id == target.id {
            for asset in github
                .assets(&release)?
                .iter()
                .filter(|a| !keep.contains(a.name.as_str()))
            {
                github.delete(asset)?;
                removed += 1;
            }
        } else {
            // Cached catalogs already retry missing assets using the current catalog.
            // One release deletion also removes its assets, avoiding hundreds of API calls.
            let refs = github.api(
                Method::GET,
                &format!("git/matching-refs/tags/{}", release.tag_name),
                None,
            )?;
            if refs
                .as_array()
                .context("Invalid Git reference list")?
                .iter()
                .any(|r| r["ref"].as_str() == Some(&format!("refs/tags/{}", release.tag_name)))
            {
                github.api(
                    Method::DELETE,
                    &format!("git/refs/tags/{}", release.tag_name),
                    None,
                )?;
            }
            github.api(Method::DELETE, &format!("releases/{}", release.id), None)?;
        }
    }
    tracing::info!(
        artifacts = catalog.artifacts.len(),
        assets = keep.len(),
        packs = catalog.packs.len(),
        uploads,
        bytes,
        removed,
        total_seconds = started.elapsed().as_secs_f64(),
        "Published one metadata release"
    );
    Ok(())
}

fn fits(existing: &[Asset], blobs: &[(String, [u8; 32], u64, String)]) -> bool {
    let additions = blobs
        .iter()
        .filter(|b| !existing.iter().any(|a| a.name == b.0))
        .count();
    let restoring = usize::from(
        !existing.iter().any(|a| a.name == "catalog.bin")
            && existing.iter().any(|a| a.name == "catalog.retiring.bin"),
    );
    // Includes old assets until commit and one extra slot for the pending catalog.
    existing
        .iter()
        .filter(|a| !temporary_catalog(&a.name))
        .count()
        + restoring
        + additions
        < 1000
}

fn temporary_catalog(name: &str) -> bool {
    matches!(name, "catalog.pending.bin" | "catalog.retiring.bin")
}

fn source(
    repo: &str,
    artifact: &Artifact,
    files: &[PathBuf],
    previous: &Catalog,
    work: &Path,
) -> Result<PathBuf> {
    if let Some(path) = files
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == artifact.name.as_str()))
    {
        return Ok(path.clone());
    }
    let leases = crate::cache::views::LeaseSet::new(work)?;
    if let Some(path) = client::packed_source(work, repo, previous, artifact, &leases)? {
        return Ok(path);
    }
    let path = work.join(&artifact.name);
    if !path.is_file() {
        client::download(repo, &artifact.release, &artifact.name, &path)?;
    }
    Ok(path)
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
    #[ignore = "requires GH_TOKEN and the migrated live metadata release"]
    fn live_release_has_exactly_the_catalog_assets_and_readable_packs() {
        let catalog = client::catalog(REPOSITORY).unwrap();
        let github = Github::new(REPOSITORY).unwrap();
        let releases = github.releases().unwrap();
        assert_eq!(releases.len(), 1);
        assert!(!releases[0].draft);
        let assets = github.assets(&releases[0]).unwrap();
        let packed: BTreeSet<_> = catalog.packs.iter().flat_map(|p| &p.members).collect();
        let expected: BTreeMap<_, _> = catalog
            .artifacts
            .iter()
            .filter(|a| !packed.contains(&a.name))
            .map(|a| (&a.name, a.size))
            .chain(catalog.packs.iter().map(|p| (&p.name, p.size)))
            .collect();
        assert_eq!(assets.len(), expected.len() + 1);
        assert!(assets.len() < 1000);
        for asset in assets.iter().filter(|a| a.name != "catalog.bin") {
            assert_eq!(expected.get(&asset.name), Some(&asset.size));
            assert_eq!(asset.state, "uploaded");
        }
        let cache = tempfile::tempdir().unwrap();
        let pack = catalog.packs.first().unwrap();
        for name in &pack.members {
            let artifact = catalog.artifacts.iter().find(|a| &a.name == name).unwrap();
            let leases = crate::cache::views::LeaseSet::new(cache.path()).unwrap();
            let path = client::materialize(cache.path(), artifact, &catalog, &leases).unwrap();
            assert!(path.join("manifest.bin").is_file());
        }
        println!(
            "One release, {} assets; all catalog references present; {} packed members materialized",
            assets.len(),
            pack.members.len()
        );
    }

    #[test]
    fn staging_counts_old_assets_replacements_and_the_pending_catalog() {
        let existing: Vec<_> = (0..998)
            .map(|i| Asset {
                id: i,
                name: format!("asset-{i}"),
                size: 1,
                state: "uploaded".into(),
            })
            .collect();
        let mut wanted = vec![("asset-0".into(), [0; 32], 1, TAG.into())];
        assert!(fits(&existing, &wanted));
        wanted.push(("replacement".into(), [1; 32], 1, TAG.into()));
        assert!(fits(&existing, &wanted));
        wanted.push(("another".into(), [2; 32], 1, TAG.into()));
        assert!(!fits(&existing, &wanted));

        let mut interrupted = existing.clone();
        interrupted[0].name = "catalog.retiring.bin".into();
        interrupted.push(Asset {
            id: 1001,
            name: "catalog.pending.bin".into(),
            size: 1,
            state: "uploaded".into(),
        });
        let replacement = vec![("replacement".into(), [1; 32], 1, TAG.into())];
        assert!(fits(&interrupted, &replacement));
        interrupted.push(Asset {
            id: 1002,
            name: "uncollected-old-bundle".into(),
            size: 1,
            state: "uploaded".into(),
        });
        assert!(!fits(&interrupted, &replacement));
    }

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
            state: "uploaded".into(),
        };
        let pending = Asset {
            id: 2,
            name: "catalog.pending.bin".into(),
            size: 0,
            state: "uploaded".into(),
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
