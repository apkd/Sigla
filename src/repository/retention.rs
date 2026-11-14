//! Read-only retention rules, shared by maintenance and inspection.
use super::{Identity, Repository, Rule, manager::State, materialize::Target};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Policy {
    pub rules: Vec<Rule>,
    pub repo_ttl: Duration,
    pub branch_ttl: Duration,
    #[serde(default)]
    pub selection_identity: String,
}
impl From<&crate::config::RemoteOptions> for Policy {
    fn from(options: &crate::config::RemoteOptions) -> Self {
        Self {
            rules: options.rules.clone(),
            repo_ttl: options.repo_ttl,
            branch_ttl: options.branch_ttl,
            selection_identity: options.selection.identity.clone(),
        }
    }
}
#[derive(Deserialize)]
struct DefaultBranch {
    branch: String,
    #[serde(default)]
    previous: Option<String>,
}

/// The fields used by maintenance, without the potentially large file inventories.
#[derive(Deserialize)]
pub(crate) struct ProjectedState {
    pub repository: Identity,
    transport: String,
    pub branch: String,
    #[serde(default)]
    target: Option<Target>,
    pub last_use: u64,
    pub policy: String,
    pub prepared: ProjectedPrepared,
    pub indexed_revision: Option<String>,
    pub repair: bool,
}

#[derive(Deserialize)]
pub(crate) struct ProjectedPrepared {
    pub branch: Option<String>,
    pub revision: String,
}

impl From<&State> for ProjectedState {
    fn from(state: &State) -> Self {
        Self {
            repository: state.repository.clone(),
            transport: state.transport.clone(),
            branch: state.branch.clone(),
            target: state.target.clone(),
            last_use: state.last_use,
            policy: state.policy.clone(),
            prepared: ProjectedPrepared {
                branch: state.prepared.branch.clone(),
                revision: state.prepared.revision.clone(),
            },
            indexed_revision: state.indexed_revision.clone(),
            repair: state.repair,
        }
    }
}

pub(crate) fn projected(cache: &Path) -> Result<BTreeMap<PathBuf, ProjectedState>> {
    read_states(cache)
}
pub(crate) fn cached(cache: &Path) -> Result<BTreeMap<PathBuf, State>> {
    read_states(cache)
}
fn read_states<T: serde::de::DeserializeOwned>(cache: &Path) -> Result<BTreeMap<PathBuf, T>> {
    let mut states = BTreeMap::new();
    let entries = match fs::read_dir(cache.join("repositories")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(states),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        anyhow::ensure!(entry.file_type()?.is_dir(), "Invalid selector directory");
        match fs::read(entry.path().join("state.json")) {
            Ok(bytes) => {
                states.insert(entry.path(), serde_json::from_slice(&bytes)?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(states)
}
impl Policy {
    pub fn decision(
        &self,
        cache: &Path,
        state: &State,
        default_ready: impl Fn(&Identity, &str) -> Result<bool>,
    ) -> Result<(Option<String>, Duration)> {
        self.projected_decision(cache, &state.into(), default_ready)
    }

    pub(crate) fn projected_decision(
        &self,
        cache: &Path,
        state: &ProjectedState,
        default_ready: impl Fn(&Identity, &str) -> Result<bool>,
    ) -> Result<(Option<String>, Duration)> {
        let default: Option<DefaultBranch> =
            match fs::read(cache.join("defaults").join(state.repository.storage_key())) {
                Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
        let branch = state.prepared.branch.as_deref().or(match &state.target {
            Some(Target::Branch(name)) => Some(name.as_str()),
            None => Some(state.branch.as_str()),
            _ => None,
        });
        let is_default = default
            .as_ref()
            .is_some_and(|d| branch == Some(d.branch.as_str()));
        let repository = Repository {
            identity: state.repository.clone(),
            transport: state.transport.clone(),
            selector: None,
        };
        let exact = self.rules.iter().any(|rule| rule.exact_match(&repository));
        let protection = if !exact
            || (is_default
                && !self.selection_identity.is_empty()
                && state.policy != self.selection_identity
                && default_ready(&state.repository, &default.as_ref().unwrap().branch)?)
        {
            None
        } else if is_default {
            Some("explicitly allowed default branch")
        } else if default.is_none() {
            Some("explicitly allowed repository; default branch unknown")
        } else if let Some(default) = &default
            && default.previous.as_deref() == branch
            && branch.is_some()
            && !default_ready(&state.repository, &default.branch)?
        {
            Some("previous default branch; replacement is not indexed")
        } else {
            None
        };
        Ok((
            protection.map(str::to_owned),
            if is_default {
                self.repo_ttl
            } else {
                self.branch_ttl
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual selector state scan comparison"]
    fn projected_scan_benchmark() {
        let cache = tempfile::tempdir().unwrap();
        let repository = Repository::parse("https://github.com/fixture/repo")
            .unwrap()
            .unwrap();
        let root = cache.path().join("repositories/selector");
        fs::create_dir_all(&root).unwrap();
        let tracked: BTreeMap<_, _> = (0..100_000)
            .map(|index| {
                (
                    format!("packages/example/path/{index:08}.cs"),
                    format!("{index:040x}"),
                )
            })
            .collect();
        let json = serde_json::json!({
            "schema": 4, "repository": repository.identity, "transport": repository.transport,
            "branch": "main", "last_use": 12, "refreshed": 13, "policy": "current",
            "indexed_revision": "abc", "repair": false,
            "prepared": {"branch": "main", "revision": "abc", "selected": tracked,
                "tracked": tracked, "directories": ["packages"]}
        });
        let path = root.join("state.json");
        serde_json::to_writer(fs::File::create(&path).unwrap(), &json).unwrap();
        drop(json);
        let size = fs::metadata(&path).unwrap().len();
        for projected_only in [false, true] {
            let started = std::time::Instant::now();
            for _ in 0..3 {
                if projected_only {
                    assert_eq!(projected(cache.path()).unwrap().len(), 1);
                } else {
                    assert_eq!(cached(cache.path()).unwrap().len(), 1);
                }
            }
            eprintln!(
                "mode={} file_bytes={size} scans=3 elapsed_ms={}",
                if projected_only { "projected" } else { "full" },
                started.elapsed().as_millis()
            );
        }
    }

    #[test]
    fn projected_states_match_full_retention_decisions_across_legacy_and_current_data() {
        let cache = tempfile::tempdir().unwrap();
        let repository = Repository::parse("https://github.com/fixture/repo")
            .unwrap()
            .unwrap();
        let root = cache.path().join("repositories/selector");
        fs::create_dir_all(&root).unwrap();
        let tracked: BTreeMap<_, _> = (0..2000)
            .map(|index| (format!("src/{index}.cs"), format!("{index:x}")))
            .collect();
        let mut json = serde_json::json!({
            "schema": 4, "repository": repository.identity,
            "transport": repository.transport, "branch": "main", "last_use": 12,
            "refreshed": 13, "policy": "old", "indexed_revision": "abc", "repair": false,
            "prepared": {"branch": "main", "revision": "abc", "selected": tracked,
                "tracked": tracked, "directories": ["src"]}
        });
        fs::create_dir_all(cache.path().join("defaults")).unwrap();
        fs::write(
            cache
                .path()
                .join("defaults")
                .join(repository.identity.storage_key()),
            serde_json::to_vec(&serde_json::json!({"branch":"main", "previous":"stable"})).unwrap(),
        )
        .unwrap();
        let policy = Policy {
            rules: vec![Rule::parse_private("https://github.com/fixture/repo").unwrap()],
            repo_ttl: Duration::from_secs(40),
            branch_ttl: Duration::from_secs(20),
            selection_identity: "current".into(),
        };
        for (branch, target, ready, protected) in [
            ("main", None, false, true),
            ("main", None, true, false),
            ("stable", Some(Target::Branch("stable".into())), false, true),
            ("other", Some(Target::Branch("other".into())), true, false),
        ] {
            json["prepared"]["branch"] = serde_json::json!(branch);
            if let Some(target) = target {
                json["target"] = serde_json::to_value(target).unwrap();
            } else {
                json.as_object_mut().unwrap().remove("target");
            }
            fs::write(root.join("state.json"), serde_json::to_vec(&json).unwrap()).unwrap();
            let full = cached(cache.path()).unwrap().remove(&root).unwrap();
            let projected = projected(cache.path()).unwrap().remove(&root).unwrap();
            assert_eq!(projected.last_use, full.last_use);
            assert_eq!(projected.prepared.revision, full.prepared.revision);
            let decision = policy
                .projected_decision(cache.path(), &projected, |_, _| Ok(ready))
                .unwrap();
            assert_eq!(decision.0.is_some(), protected);
            assert_eq!(
                policy
                    .decision(cache.path(), &full, |_, _| Ok(ready))
                    .unwrap(),
                decision
            );
        }
        fs::write(root.join("state.json"), b"{invalid").unwrap();
        assert!(projected(cache.path()).is_err());
    }
}
