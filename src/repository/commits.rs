//! Prepared selector roots that the Git resolver proved refer to an immutable commit.
//! The original selector state owns its proof, transfer budget and lifetime.
use super::{
    Identity,
    materialize::{Prepared, Target},
    revision::is_prefix,
    state_store::Header,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Owner {
    pub root: PathBuf,
    pub target: Target,
    ready: bool,
}

#[derive(Default)]
pub(crate) struct Pins(HashMap<Identity, BTreeMap<String, Arc<Vec<Owner>>>>);

fn immutable<'a>(target: &Target, resolved: Option<&Target>, revision: &'a str) -> Option<&'a str> {
    let proven = match target {
        Target::Commit(id) => id == revision,
        Target::Named(name) => {
            matches!(resolved, Some(Target::Commit(id)) if id == revision)
                && matches_prefix(name, revision)
        }
        _ => false,
    };
    proven.then_some(revision)
}

impl Prepared {
    pub(super) fn immutable_commit(&self, target: &Target) -> Option<&str> {
        immutable(target, self.resolved_target.as_ref(), &self.revision)
    }
}

impl Pins {
    pub fn restore_owner(&mut self, root: &Path, header: Header) {
        if header.repair {
            return;
        }
        if let Some(id) = immutable(
            &header.target,
            header.prepared.resolved_target.as_ref(),
            &header.prepared.revision,
        ) {
            let owners = self.owners(&header.repository, id);
            owners.push(Owner {
                root: root.into(),
                target: header.target,
                ready: header.indexed_revision.as_deref() == Some(id),
            });
        }
    }

    pub fn prefer_indexed(&mut self) {
        for revisions in self.0.values_mut() {
            for owners in revisions.values_mut() {
                Arc::make_mut(owners)
                    .sort_by(|a, b| b.ready.cmp(&a.ready).then_with(|| a.root.cmp(&b.root)));
            }
        }
    }

    fn owners(&mut self, repository: &Identity, id: &str) -> &mut Vec<Owner> {
        Arc::make_mut(
            self.0
                .entry(repository.clone())
                .or_default()
                .entry(id.into())
                .or_default(),
        )
    }

    pub fn register(&mut self, repository: &Identity, id: &str, target: &Target, root: &Path) {
        let owners = self.owners(repository, id);
        if !owners.iter().any(|o| o.root == root) {
            // The first live publisher owns discovery, even if another spelling publishes
            // its Git inputs before the first index finishes.
            owners.push(Owner {
                root: root.into(),
                target: target.clone(),
                ready: false,
            });
        }
    }

    pub(super) fn candidates(&self, repository: &Identity, id: &str) -> Option<&Arc<Vec<Owner>>> {
        self.0.get(repository).and_then(|r| r.get(id))
    }

    /// Only a scheduling hint. Git must still resolve the exact spelling.
    pub fn may_share(&self, repository: &Identity, prefix: &str) -> bool {
        let prefix = prefix.to_ascii_lowercase();
        self.0
            .get(repository)
            .and_then(|ids| ids.range(prefix.clone()..).next())
            .is_some_and(|(id, _)| id.starts_with(&prefix))
    }

    pub fn retire(&mut self, root: &Path) {
        self.0.retain(|_, revisions| {
            revisions.retain(|_, owners| {
                if owners.iter().any(|o| o.root == root) {
                    Arc::make_mut(owners).retain(|o| o.root != root);
                }
                !owners.is_empty()
            });
            !revisions.is_empty()
        });
    }
}

pub(super) fn matches_prefix(name: &str, revision: &str) -> bool {
    is_prefix(name)
        && revision
            .get(..name.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
}
