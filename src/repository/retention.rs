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
pub(crate) use super::state_store::Header as ProjectedState;

pub(crate) fn projected(cache: &Path) -> Result<BTreeMap<PathBuf, ProjectedState>> {
    read_states(cache, super::state_store::header)
}
pub(crate) fn cached(cache: &Path) -> Result<BTreeMap<PathBuf, State>> {
    read_states(cache, super::state_store::read)
}
fn read_states<T>(cache: &Path, read: impl Fn(&Path) -> Result<T>) -> Result<BTreeMap<PathBuf, T>> {
    let mut states = BTreeMap::new();
    let entries = match fs::read_dir(cache.join("repositories")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(states),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        anyhow::ensure!(entry.file_type()?.is_dir(), "Invalid selector directory");
        match read(&entry.path()) {
            Ok(state) => {
                states.insert(entry.path(), state);
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
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
        let default: Option<super::manager::DefaultBranch> = match crate::cache::records::read(
            &cache.join("defaults").join(state.repository.storage_key()),
            64 * 1024,
        ) {
            Ok(saved) => Some(saved),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let branch = state.prepared.branch.as_deref().or(match &state.target {
            Target::Branch(name) => Some(name.as_str()),
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
    use crate::repository::materialize::Prepared;

    fn state(repository: &Repository, tracked: BTreeMap<String, String>) -> State {
        State {
            repository: repository.identity.clone(),
            transport: repository.transport.clone(),
            branch: "main".into(),
            target: Target::Branch("main".into()),
            last_use: 12,
            refreshed: 13,
            policy: "old".into(),
            indexed_revision: Some("abc".into()),
            repair: false,
            additional: Default::default(),
            prepared: Prepared {
                branch: Some("main".into()),
                revision: "abc".into(),
                selected: tracked.clone(),
                tracked,
                directories: ["src".into()].into(),
                deferred_assets: Default::default(),
                omitted: Default::default(),
                transfer_bytes: 0,
                unavailable: Default::default(),
                transport: None,
                resolved_target: None,
            },
        }
    }

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
        super::super::state_store::write(&root, &state(&repository, tracked)).unwrap();
        let size: u64 = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
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
    fn projected_states_match_full_retention_decisions() {
        let cache = tempfile::tempdir().unwrap();
        let repository = Repository::parse("https://github.com/fixture/repo")
            .unwrap()
            .unwrap();
        let root = cache.path().join("repositories/selector");
        fs::create_dir_all(&root).unwrap();
        let tracked: BTreeMap<_, _> = (0..2000)
            .map(|index| (format!("src/{index}.cs"), format!("{index:x}")))
            .collect();
        let mut state = state(&repository, tracked);
        fs::create_dir_all(cache.path().join("defaults")).unwrap();
        crate::cache::records::write(
            &cache
                .path()
                .join("defaults")
                .join(repository.identity.storage_key()),
            &super::super::manager::DefaultBranch {
                repository: repository.identity.clone(),
                branch: "main".into(),
                refreshed: 0,
                previous: Some("stable".into()),
            },
        )
        .unwrap();
        let policy = Policy {
            rules: vec![Rule::parse_private("https://github.com/fixture/repo").unwrap()],
            repo_ttl: Duration::from_secs(40),
            branch_ttl: Duration::from_secs(20),
            selection_identity: "current".into(),
        };
        for (branch, target, ready, protected) in [
            (Some("main"), Target::Branch("main".into()), false, true),
            (None, Target::Branch("main".into()), true, false),
            (Some("stable"), Target::Branch("stable".into()), false, true),
            (Some("other"), Target::Branch("other".into()), true, false),
        ] {
            state.prepared.branch = branch.map(str::to_owned);
            state.target = target;
            super::super::state_store::write(&root, &state).unwrap();
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
        fs::write(root.join("state.bin"), b"invalid").unwrap();
        assert!(projected(cache.path()).is_err());
    }
}
