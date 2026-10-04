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
}
impl From<&crate::config::RemoteOptions> for Policy {
    fn from(options: &crate::config::RemoteOptions) -> Self {
        Self {
            rules: options.rules.clone(),
            repo_ttl: options.repo_ttl,
            branch_ttl: options.branch_ttl,
        }
    }
}
#[derive(Deserialize)]
struct DefaultBranch {
    branch: String,
    #[serde(default)]
    previous: Option<String>,
}
pub(crate) fn cached(cache: &Path) -> Result<BTreeMap<PathBuf, State>> {
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
        default_ready: impl FnOnce(&Identity, &str) -> Result<bool>,
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
        let protection = if !exact {
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
