use super::*;
use reqwest::{Method, blocking::Client};
use serde::Deserialize as JsonDeserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

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
            client: Client::builder()
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(3600))
                .build()?,
        })
    }
    fn api(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("https://api.github.com/repos/{}/{}", self.repo, path);
        let mut request = self.client.request(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send()?.error_for_status()?;
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
        Ok(serde_json::from_value(self.api(Method::POST, "releases", Some(json!({"tag_name":tag,"name":"Unity metadata archive","body":"Current precomputed Unity analysis records.","make_latest":"false"})))?)?)
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
            .body(file)
            .send()?
            .error_for_status()?
            .json()?)
    }
}

pub(super) fn publish(
    repo: &str,
    base: producer::Plan,
    packages: registry::Plan,
    input: &Path,
) -> Result<()> {
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
                continue;
            }
            let path = files
                .iter()
                .find(|p| p.file_name().is_some_and(|n| n == artifact.name.as_str()))
                .context("New artifact has no local bundle")?;
            bundle::verify(path, Some(artifact))?;
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
            artifact.release = release.tag_name.clone();
            uploaded.push(asset.clone());
            assets.push(asset);
        }
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
    for release in github.releases()? {
        let mut retained = false;
        for asset in github.assets(&release)? {
            if keep.contains(&(release.tag_name.clone(), asset.name.clone())) {
                retained = true;
            } else {
                github.delete(&asset)?;
            }
        }
        if !retained && release.tag_name != TAG {
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
