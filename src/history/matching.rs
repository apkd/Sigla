//! Direction-independent matching of occurrences across actual Git edges.
//! Names are evidence, never a substitute for source/context continuity.
use super::{
    cache::{self, Cache, SourceTree},
    paths,
    query::{self, Selector},
    regions::{self, Region, Source, Sources},
    sequence,
};
use crate::repository::history::{Commit, Store, Tree, check};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(super) struct State {
    pub regions: Vec<Region>,
    pub paths: Vec<String>,
    pub scope_exclusions: Vec<paths::Exclusion>,
    // File lineage may be known without a whole historical directory location.
    pub scope_unresolved: bool,
    // A completed match proved this occurrence absent. An empty default state
    // carries no such evidence. Absence alongside regions is an alternative.
    pub absent: bool,
    pub uncertain: bool,
}
impl State {
    pub fn heap_bytes(&self) -> usize {
        self.regions.capacity() * std::mem::size_of::<Region>()
            + self.regions.iter().map(Region::heap_bytes).sum::<usize>()
            + self.paths.capacity() * std::mem::size_of::<String>()
            + self.paths.iter().map(String::capacity).sum::<usize>()
            + self.scope_exclusions.capacity() * std::mem::size_of::<paths::Exclusion>()
            + self
                .scope_exclusions
                .iter()
                .map(paths::Exclusion::heap_bytes)
                .sum::<usize>()
    }
    pub fn absent() -> Self {
        Self {
            absent: true,
            ..Self::default()
        }
    }
    pub fn is_absent(&self) -> bool {
        self.absent && self.regions.is_empty() && self.paths.is_empty()
    }
    pub fn same_occurrences(&self, other: &Self) -> bool {
        self.absent == other.absent
            && self.paths == other.paths
            && self.scope_exclusions == other.scope_exclusions
            && self.regions == other.regions
    }
    fn selection(&self) -> paths::Selection<'_> {
        paths::Selection {
            patterns: &self.paths,
            exclusions: &self.scope_exclusions,
        }
    }
    pub fn normalize(&mut self) {
        self.regions.sort_by(|a, b| {
            a.key()
                .cmp(&b.key())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.parameters.cmp(&b.parameters))
                .then_with(|| a.written.cmp(&b.written))
                .then_with(|| a.body.start.cmp(&b.body.start))
                .then_with(|| a.body.end.cmp(&b.body.end))
                .then_with(|| a.fragment.cmp(&b.fragment))
        });
        self.regions.dedup_by(|a, b| {
            if a.same_alternative(b) {
                b.weak |= a.weak;
                true
            } else {
                false
            }
        });
        self.paths.sort();
        self.paths.dedup();
        self.scope_exclusions.sort();
        self.scope_exclusions.dedup();
        self.uncertain |= self.scope_unresolved
            || self.regions.iter().any(|r| r.weak)
            || self.absent && (!self.regions.is_empty() || !self.paths.is_empty());
    }
    pub fn join(&mut self, other: &State) {
        self.regions.extend(other.regions.iter().cloned());
        self.paths.extend(other.paths.iter().cloned());
        self.scope_exclusions
            .extend(other.scope_exclusions.iter().cloned());
        self.absent |= other.absent;
        self.scope_unresolved |= other.scope_unresolved;
        self.uncertain |= other.uncertain;
        self.normalize();
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Relation {
    Continue,
    Rename,
    Move,
    Extract,
    Copy,
    Wrapper,
    Text,
}
#[derive(Clone, Debug)]
pub(super) struct Link {
    pub from: Region,
    pub to: Region,
    pub relation: Relation,
    pub uncertain: bool,
    pub reason: &'static str,
}
impl Link {
    pub fn label(&self) -> Option<String> {
        let event = match self.relation {
            Relation::Continue => return self.uncertain.then(|| format!("~edit {}", self.reason)),
            Relation::Rename => format!("rename {}→{}", self.to.signature(), self.from.signature()),
            Relation::Move => format!("move {}→{}", self.to.path, self.from.path),
            Relation::Extract => {
                format!("extract {}→{}", self.to.signature(), self.from.signature())
            }
            Relation::Copy => format!("copy {}→{}", self.to.path, self.from.path),
            Relation::Wrapper => format!(
                "extract {}→{}; retained wrapper",
                self.to.signature(),
                self.from.signature()
            ),
            Relation::Text => format!("text-move {}→{}", self.to.path, self.from.path),
        };
        Some(format!(
            "{}{event} {}",
            if self.uncertain { "~" } else { "" },
            self.reason
        ))
    }
}
#[derive(Clone, Debug, Default)]
pub(super) struct Mapping {
    pub state: State,
    pub links: Vec<Link>,
}
impl Mapping {
    pub fn heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.state.heap_bytes()
            + self.links.capacity() * std::mem::size_of::<Link>()
            + self
                .links
                .iter()
                .map(|link| link.from.heap_bytes() + link.to.heap_bytes())
                .sum::<usize>()
    }
    pub fn absent() -> Self {
        Self {
            state: State::absent(),
            ..Self::default()
        }
    }
}
const SCORE_MARGIN: usize = 6;
// Includes reciprocal searches, not just the selected occurrence. Exhaustion
// is unresolved evidence, never absence or an arbitrary assignment.
const CANDIDATE_WORK: usize = 65_536;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Competition {
    None,
    Uncertain,
    Stronger,
}

struct Candidate {
    score: usize,
    region: Region,
    similarity: usize,
    context: bool,
    origin: bool,
    exact: bool,
    reason: &'static str,
}

impl Candidate {
    fn local(&self, region: &Region) -> bool {
        self.context
            && self.similarity >= 70
            && !self.region.fragment
            && (self.exact || region.kind == "closure" || self.region.name == region.name)
            && self.region.parameters == region.parameters
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateScope<'a> {
    // Changed files, with an exact-local shortcut and an optional reciprocal claimant.
    Local(Option<&'a str>),
    Changed,
    All,
}

#[derive(Default)]
struct CandidateSearch {
    scored: Vec<Candidate>,
    incomplete: Option<String>,
    visited: BTreeSet<String>,
    // Paths skipped by the exact-local shortcut, not deferred unchanged origins.
    remaining: bool,
}

#[derive(Default)]
struct Assignment {
    links: Vec<Link>,
    settled: bool,
    weak: bool,
}

pub(super) struct Tracker {
    pub store: Store,
    pub sources: Sources,
    cache: Arc<Cache>,
}
impl Tracker {
    pub fn new(store: Store, cache: Arc<Cache>) -> Self {
        Self {
            store,
            sources: Sources::new(cache.clone()),
            cache,
        }
    }
    pub fn tree(&mut self, commit: &Commit) -> Result<Arc<SourceTree>> {
        check(&self.store.cancel)?;
        let key = cache::key(self.store.cache_scope(), &commit.tree);
        if let Some(tree) = self.cache.tree(&key) {
            return Ok(tree);
        }
        let mut tree = self.store.tree(&commit.tree)?;
        let paths: Vec<_> = tree
            .iter()
            .filter(|(p, e)| {
                matches!(e.mode, 0o100644 | 0o100755) && regions::language(p).is_some()
            })
            .map(|(p, _)| p.clone())
            .collect();
        let allowed = self.store.source_paths(&commit.tree, &tree, &paths)?;
        let excluded: BTreeSet<_> = paths.into_iter().filter(|p| !allowed.contains(p)).collect();
        tree.retain(|p, _| !excluded.contains(p));
        check(&self.store.cancel)?;
        Ok(self.cache.put_tree(
            key,
            Arc::new(SourceTree {
                entries: tree,
                excluded,
            }),
        ))
    }
    pub fn anchor(&mut self, commit: &Commit, selector: &Selector) -> Result<State> {
        let mut state = State::default();
        if matches!(selector, Selector::Repository) {
            return Ok(state);
        }
        let tree = self.tree(commit)?;
        match selector {
            Selector::Repository => return Ok(state),
            Selector::Paths(patterns) => {
                state.paths = patterns.clone();
                state.regions = members(&tree, state.selection().compile(&self.store.cancel)?);
                // Deleted-before-anchor members remain discoverable through the path scope.
            }
            Selector::File(path) | Selector::Text { path, .. } => {
                let entry = tree.get(path).filter(|e| matches!(e.mode, 0o100644 | 0o100755))
                    .with_context(|| format!("Target file is absent or excluded by committed source attributes at {}: {path}", commit.id))?;
                ensure!(
                    regions::language(path).is_some(),
                    "Unsupported source path {path}; binary/LFS payloads are not fetched"
                );
                let region = if let Selector::Text { start, end, .. } = selector {
                    let source = self.sources.get(&mut self.store, path, &entry.id)?;
                    Region::selected(
                        path,
                        &entry.id,
                        regions::line_range(&source.text, *start, *end)?,
                        &source,
                    )
                } else {
                    Region::whole(path, &entry.id, 0)
                };
                state.regions.push(region);
            }
            Selector::Declaration(query) => {
                let mut incomplete = None;
                let filters = query
                    .filters
                    .iter()
                    .map(|f| {
                        check(&self.store.cancel)?;
                        Ok((query::PathPattern::new(&f.value), f.negate))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let candidates: Vec<_> = tree
                    .iter()
                    .filter(|(path, entry)| {
                        matches!(entry.mode, 0o100644 | 0o100755)
                            && regions::language(path).is_some_and(|l| !l.document())
                            && filters.iter().all(|(p, negate)| p.matches(path) != *negate)
                            && query
                                .target
                                .location
                                .as_ref()
                                .is_none_or(|l| l.path == **path)
                    })
                    .map(|(path, entry)| (path.clone(), entry.id.clone()))
                    .collect();
                drop(filters);
                for chunk in candidates.chunks(16) {
                    self.store.prefetch(
                        &chunk.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>(),
                        "blob",
                    )?;
                    for (path, blob) in chunk {
                        let source = self.sources.get(&mut self.store, path, blob)?;
                        if source.incomplete.is_some() {
                            incomplete = source.incomplete.clone();
                        }
                        if let Some(location) = &query.target.location {
                            let offset =
                                crate::model::offset(&source.text, location.line, location.column)
                                    .context("Location is outside the anchor source")?;
                            let mut declarations: Vec<_> = source
                                .declarations
                                .iter()
                                .filter(|d| {
                                    regions::selectable(d)
                                        && regions::matches_kind(&query.selector, d)
                                        && d.span.contains(&offset)
                                })
                                .collect();
                            declarations.sort_by_key(|d| d.span.len());
                            // Innermost in each feasible environment, not the
                            // shortest span across mutually exclusive headers.
                            let mut coverage = source.presence.coverage();
                            for group in declarations.chunk_by(|a, b| a.span.len() == b.span.len())
                            {
                                for declaration in group {
                                    if coverage.uncovered(declaration.guard).context(
                                        "Unresolved anchor: historical positional alternatives",
                                    )? {
                                        state.regions.push(Region::declaration(
                                            path,
                                            blob,
                                            declaration,
                                            &source,
                                        ));
                                    }
                                }
                                for declaration in group {
                                    coverage.add(declaration.guard).context(
                                        "Unresolved anchor: historical positional alternatives",
                                    )?;
                                }
                            }
                        } else {
                            for declaration in &source.declarations {
                                if regions::selected(query, declaration) {
                                    state.regions.push(Region::declaration(
                                        path,
                                        blob,
                                        declaration,
                                        &source,
                                    ));
                                }
                            }
                        }
                    }
                }
                state.normalize();
                if let Some(reason) = &incomplete {
                    bail!("Unresolved anchor at {}: {reason}", commit.id);
                }
                ensure!(
                    !state.regions.is_empty(),
                    "No declaration at {} matches the anchor selector",
                    commit.id
                );
                if state.regions.len() > 1 {
                    let mut alternatives = Vec::new();
                    for region in state.regions.iter().take(20) {
                        let label = self.sources.label(&mut self.store, region)?;
                        alternatives.push(if region.written.is_empty() {
                            label
                        } else {
                            format!("{label} [{}]", region.written)
                        });
                    }
                    bail!(
                        "Ambiguous anchor at {} ({} declarations); narrow the signature or path:\n{}",
                        commit.id,
                        state.regions.len(),
                        alternatives.join("\n")
                    );
                }
                ensure!(
                    !state.regions.iter().any(|r| r.weak),
                    "Unresolved anchor at {}: {}",
                    commit.id,
                    incomplete
                        .as_deref()
                        .unwrap_or("incomplete historical declaration syntax")
                );
            }
        }
        state.normalize();
        Ok(state)
    }
    pub fn map(&mut self, from: &Commit, to: &Commit, state: &State) -> Result<Mapping> {
        check(&self.store.cancel)?;
        if state.regions.is_empty() && state.paths.is_empty() {
            ensure!(
                state.absent,
                "unresolved target: occurrence state is unavailable"
            );
            // Forward propagation never revives a deleted occurrence by looking
            // up its former name/path. Backward walks stop at this boundary.
            return Ok(Mapping {
                state: state.clone(),
                ..Mapping::default()
            });
        }
        let key = (!state.uncertain).then(|| {
            cache::key(
                self.store.cache_scope(),
                &(
                    &from.id,
                    &from.tree,
                    &from.parents,
                    &to.id,
                    &to.tree,
                    &to.parents,
                    state,
                ),
            )
        });
        if let Some(key) = &key
            && let Some(mapping) = self.cache.mapping(key)
        {
            return Ok((*mapping).clone());
        }
        let a = self.tree(from)?;
        let b = self.tree(to)?;
        let mut result = Mapping {
            state: State {
                absent: state.absent,
                uncertain: state.uncertain,
                scope_unresolved: state.scope_unresolved,
                ..State::default()
            },
            links: Vec::new(),
        };
        let forward = to.parents.contains(&from.id);
        let mut work = 0;
        for region in &state.regions {
            ensure!(
                !b.excluded.contains(&region.path),
                "unresolved target at {}: {} is excluded by committed source attributes",
                to.id,
                region.path
            );
            let mapped = self.region(&a, &b, region, forward, &mut work)?;
            if mapped.is_empty() && state.paths.is_empty() {
                result.state.absent = true;
            }
            for link in mapped {
                result.state.uncertain |= link.uncertain;
                result.state.regions.push(link.to.clone());
                result.links.push(link);
            }
        }
        if !state.paths.is_empty() {
            let scopes = paths::map(&a, &b, state.selection(), &result.links, &self.store.cancel)?;
            result.state.paths = scopes.patterns;
            result.state.scope_exclusions = scopes.exclusions;
            result.state.scope_unresolved |= scopes.unresolved;
            // Exclusions apply to mapped occurrences too, not just newly found
            // members. Transfer them before testing the other endpoint.
            let selection = paths::Selection {
                patterns: &result.state.paths,
                exclusions: &result.state.scope_exclusions,
            }
            .compile(&self.store.cancel)?;
            result.links.retain(|l| !selection.excluded(&l.to.path));
            result
                .state
                .regions
                .retain(|r| !selection.excluded(&r.path));
            result.state.regions.extend(members(&b, selection));
            // Keep the existing conservative region-ancestry signal separate
            // from whether the directory's path coverage is established.
            if result.links.iter().any(|l| l.from.path != l.to.path)
                && result.state.paths == state.paths
            {
                result.state.uncertain = true;
            }
        }
        result.state.normalize();
        // Multiple claims are retained as alternatives, not silently assigned to
        // an arbitrary first candidate. Shared fragments are legal extraction links.
        let mut claims = BTreeMap::<String, BTreeSet<String>>::new();
        for link in &result.links {
            if matches!(
                link.relation,
                Relation::Continue | Relation::Rename | Relation::Move
            ) {
                claims
                    .entry(format!("{:?}", link.to.key()))
                    .or_default()
                    .insert(format!("{:?}", link.from.key()));
            }
        }
        if claims.values().any(|c| c.len() > 1) {
            result.state.uncertain = true;
        }
        check(&self.store.cancel)?;
        if let Some(key) = key {
            self.cache.put_mapping(key, &result);
        }
        Ok(result)
    }
    fn region(
        &mut self,
        from: &Tree,
        to: &Tree,
        region: &Region,
        forward: bool,
        work: &mut usize,
    ) -> Result<Vec<Link>> {
        if to.get(&region.path).is_some_and(|e| e.id == region.blob) {
            return Ok(vec![Link {
                from: region.clone(),
                to: region.clone(),
                relation: Relation::Continue,
                uncertain: region.weak,
                reason: "object+context",
            }]);
        }
        // A whole unchanged file carries even tiny members across a file move.
        let exact: Vec<_> = to
            .iter()
            .filter(|(_, e)| e.id == region.blob && matches!(e.mode, 0o100644 | 0o100755))
            .collect();
        if !exact.is_empty() {
            return Ok(exact
                .iter()
                .map(|(path, _)| {
                    let mut old = region.clone();
                    old.path = (*path).clone();
                    let copy = from.get(*path).is_some_and(|e| e.id == old.blob);
                    Link {
                        from: region.clone(),
                        to: old,
                        relation: if copy { Relation::Copy } else { Relation::Move },
                        uncertain: exact.len() > 1 || copy || region.weak,
                        reason: "identical file object",
                    }
                })
                .collect());
        }
        if region.kind == "file"
            && let Some(entry) = to
                .get(&region.path)
                .filter(|e| matches!(e.mode, 0o100644 | 0o100755))
        {
            let mut previous = region.clone();
            previous.blob = entry.id.clone();
            return Ok(vec![Link {
                from: region.clone(),
                to: previous,
                relation: Relation::Continue,
                uncertain: false,
                reason: "path continuity",
            }]);
        }
        let mut search = CandidateSearch::default();
        let mut assignment = Assignment::default();
        for scope in [
            CandidateScope::Local(None),
            CandidateScope::Changed,
            CandidateScope::All,
        ] {
            // Assign changed-file continuations before acquiring wider origins.
            // Resume after rejected local claims without rescoring visited files;
            // reciprocal work still shares the same edge budget across phases.
            if self.candidates(from, to, region, work, scope, &mut search)? {
                assignment = self.assign(from, to, region, &search, forward, work)?;
            }
            if assignment.settled {
                break;
            }
        }
        if assignment.links.is_empty() {
            ensure!(
                !assignment.weak,
                "unresolved target: weak predecessor evidence after wider search"
            );
            ensure!(
                search.incomplete.is_none() && !region.weak,
                "unresolved target: {}",
                search
                    .incomplete
                    .as_deref()
                    .unwrap_or("historical parse recovery left candidate scope incomplete")
            );
        }
        Ok(assignment.links)
    }

    fn assign(
        &mut self,
        from: &Tree,
        to: &Tree,
        region: &Region,
        search: &CandidateSearch,
        forward: bool,
        work: &mut usize,
    ) -> Result<Assignment> {
        let anonymous = region.kind == "closure";
        let mut selected = Vec::new();
        let mut direct = None;
        let mut origin = None;
        let mut fragment = None;
        let mut weak = false;
        for candidate in &search.scored {
            if direct.is_some_and(|score| candidate.score + SCORE_MARGIN < score) {
                break;
            }
            if candidate.score < 60 {
                // A narrow weak result must reach wider origins before it can
                // report unresolved evidence; it never proves absence.
                weak = selected.is_empty();
                break;
            }
            let old = &candidate.region;
            let mut relation = if old.fragment && !region.fragment {
                Relation::Extract
            } else if region.kind == "text" && old.path != region.path {
                Relation::Text
            } else if old.path != region.path {
                Relation::Move
            } else if !anonymous && (old.name != region.name || old.parameters != region.parameters)
            {
                Relation::Rename
            } else {
                Relation::Continue
            };
            // Calls establish wrapper/extraction provenance separately from direct
            // continuity. A surviving name alone does not establish a copy.
            if !forward
                && !anonymous
                && let Some(entry) = from.get(&old.path)
            {
                if region.kind == "file" && entry.id == old.blob {
                    relation = Relation::Copy;
                } else if !matches!(region.kind.as_str(), "file" | "text") {
                    let fate = self.sources.get(&mut self.store, &old.path, &entry.id)?;
                    let before = self.sources.get(&mut self.store, &old.path, &old.blob)?;
                    if retained_wrapper(region, old, &entry.id, &fate, &before) {
                        relation = Relation::Wrapper;
                    }
                }
            }
            let competition = if matches!(
                relation,
                Relation::Continue | Relation::Rename | Relation::Move
            ) && !region.fragment
                && !old.fragment
                && !matches!(region.kind.as_str(), "file" | "text")
            {
                self.competition(from, to, region, old, work)?
            } else {
                Competition::None
            };
            if competition == Competition::Stronger {
                // In a forward recovery, an occurrence already belonging to another
                // predecessor must not revive this one. Backwards, shared distinctive
                // implementation can still establish an independent (possibly weak) origin.
                if forward || !candidate.origin {
                    continue;
                }
                relation = Relation::Copy;
            }
            // Origins do not win the direct-continuation score band. A claimed
            // implementation must not hide an available predecessor below it.
            let best = if old.fragment || region.fragment {
                &mut fragment
            } else if matches!(
                relation,
                Relation::Copy | Relation::Extract | Relation::Wrapper
            ) {
                &mut origin
            } else {
                &mut direct
            };
            if best.is_some_and(|score| candidate.score + SCORE_MARGIN < score) {
                continue;
            }
            best.get_or_insert(candidate.score);
            selected.push((candidate, relation, competition));
        }
        // Use fragment containment when no whole candidate was accepted.
        // Only accepted declarations can suppress it; rejected claims must
        // leave partial ancestry available, and whole ties remain alternatives.
        if selected.iter().any(|(c, _, _)| !c.region.fragment) {
            selected.retain(|(c, _, _)| !c.region.fragment);
        }
        let alternatives = selected.len() > 1
            && !selected.iter().all(|(c, _, _)| {
                c.region.fragment
                    && c.region.name == selected[0].0.region.name
                    && c.region.path == selected[0].0.region.path
            });
        // This settles acquisition scope, not identity: direct ties keep their
        // uncertainty. Provenance alone cannot close the changed neighbourhood,
        // except for the existing local-continuity shortcut. If that shortcut
        // skipped changed paths, only an accepted exact local claim can close it.
        let settled = search.incomplete.is_none()
            && !region.weak
            && region.guard == 1
            && selected
                .iter()
                .all(|(c, _, _)| !c.region.weak && c.region.guard == 1)
            && selected.iter().any(|(c, relation, _)| {
                c.local(region) && (!search.remaining || c.exact)
                    || !search.remaining
                        && !region.fragment
                        && !c.region.fragment
                        && !matches!(region.kind.as_str(), "file" | "text")
                        && matches!(
                            relation,
                            Relation::Continue | Relation::Rename | Relation::Move
                        )
            });
        let links = selected
            .into_iter()
            .map(|(candidate, relation, competition)| Link {
                from: region.clone(),
                uncertain: alternatives
                    || search.incomplete.is_some()
                    || region.weak
                    || candidate.region.weak
                    || !candidate.context
                    || competition == Competition::Uncertain
                    || relation == Relation::Copy && candidate.similarity < 70,
                to: candidate.region.clone(),
                relation,
                reason: if alternatives {
                    "competing predecessors"
                } else if competition == Competition::Uncertain {
                    "competing direct continuations"
                } else if competition == Competition::Stronger {
                    "stronger direct continuation; body origin"
                } else if search.incomplete.is_some() {
                    "incomplete candidate scope"
                } else {
                    candidate.reason
                },
            })
            .collect();
        Ok(Assignment {
            links,
            settled,
            weak,
        })
    }

    // Reuse the same directed scoring for every claimant, including the selected
    // occurrence. This is deliberately non-recursive: provenance cannot reserve
    // a direct continuation, and reciprocal searches cannot chase an entire graph.
    fn competition(
        &mut self,
        from: &Tree,
        to: &Tree,
        region: &Region,
        old: &Region,
        work: &mut usize,
    ) -> Result<Competition> {
        let mut search = CandidateSearch::default();
        self.candidates(
            to,
            from,
            old,
            work,
            CandidateScope::Local(Some(&region.path)),
            &mut search,
        )?;
        let claims = search.scored;
        let score = claims
            .iter()
            .find(|c| c.region.same_alternative(region))
            .map_or(0, |c| c.score);
        let Some(rival) = claims.iter().find(|c| {
            !c.region.fragment
                && !overlapping(&c.region, region)
                && c.score >= 60
                && c.score + SCORE_MARGIN >= score
        }) else {
            return Ok(if search.incomplete.is_some() {
                Competition::Uncertain
            } else {
                Competition::None
            });
        };
        // Shared/overlapping conditional ranges are alternative interpretations,
        // not extra occurrences. Nor can an optional or recovered declaration
        // claim an unconditional reservation in a different source environment.
        if search.incomplete.is_some()
            || region.weak
            || old.weak
            || region.guard != 1
            || old.guard != 1
            || rival.region.weak
            || rival.region.guard != 1
            || rival.similarity < 70
            || rival.score <= score + SCORE_MARGIN
            || claims.iter().any(|c| {
                !c.region.fragment
                    && !c.region.same_alternative(&rival.region)
                    && c.score + SCORE_MARGIN >= rival.score
            })
        {
            return Ok(Competition::Uncertain);
        }
        // The rival must itself prefer this occurrence. Otherwise an existing
        // neighbour (or another implementation's copy) could steal a continuation.
        let mut search = CandidateSearch::default();
        self.candidates(
            from,
            to,
            &rival.region,
            work,
            CandidateScope::Local(Some(&old.path)),
            &mut search,
        )?;
        let predecessors = search.scored;
        let Some(claim) = predecessors.iter().find(|c| c.region.same_alternative(old)) else {
            return Ok(Competition::Uncertain);
        };
        if predecessors
            .iter()
            .any(|c| !c.region.fragment && c.score > claim.score + SCORE_MARGIN)
        {
            return Ok(Competition::None);
        }
        if search.incomplete.is_some()
            || claim.score < 60
            || predecessors.iter().any(|c| {
                !c.region.fragment
                    && !c.region.same_alternative(old)
                    && c.score + SCORE_MARGIN >= claim.score
            })
        {
            return Ok(Competition::Uncertain);
        }
        Ok(Competition::Stronger)
    }

    fn candidates(
        &mut self,
        from: &Tree,
        to: &Tree,
        region: &Region,
        work: &mut usize,
        scope: CandidateScope<'_>,
        search: &mut CandidateSearch,
    ) -> Result<bool> {
        let source = self
            .sources
            .get(&mut self.store, &region.path, &region.blob)?;
        let anonymous = region.kind == "closure";
        let source_range = region.range(&source);
        let source_body = region.body_range(&source);
        if search.incomplete.is_none() {
            search.incomplete = source.incomplete.clone();
        }
        let required = match scope {
            CandidateScope::Local(required) => required,
            _ => None,
        };
        let mut paths = Vec::new();
        if to.contains_key(&region.path) {
            paths.push(region.path.clone());
        }
        if let Some(path) = required.filter(|p| to.contains_key(*p)) {
            paths.push(path.to_owned());
        }
        paths.extend(
            to.iter()
                .filter(|(p, e)| {
                    **p != region.path
                        && matches!(e.mode, 0o100644 | 0o100755)
                        && regions::compatible(p, &region.path)
                        && from.get(*p).is_none_or(|a| a.id != e.id)
                })
                .map(|(p, _)| p.clone()),
        );
        // Unchanged files already retain their own occurrences. They can supply
        // origins, but cannot reserve a direct continuation from another file.
        if scope == CandidateScope::All {
            paths.extend(
                to.iter()
                    .filter(|(p, e)| {
                        **p != region.path
                            && matches!(e.mode, 0o100644 | 0o100755)
                            && regions::compatible(p, &region.path)
                            && from.get(*p).is_some_and(|a| a.id == e.id)
                    })
                    .map(|(p, _)| p.clone()),
            );
        }
        let mut paths = paths.into_iter();
        let mut expanded = false;
        for path in paths.by_ref() {
            check(&self.store.cancel)?;
            if !search.visited.insert(path.clone()) {
                continue;
            }
            let entry = &to[&path];
            if !matches!(entry.mode, 0o100644 | 0o100755)
                || !regions::compatible(&path, &region.path)
            {
                continue;
            }
            expanded = true;
            let other = self.sources.get(&mut self.store, &path, &entry.id)?;
            if other.incomplete.is_some() {
                search.incomplete = other.incomplete.clone();
            }
            let mapped = (path == region.path)
                .then(|| regions::map_range(&source.text, &other.text, source_range.clone()))
                .flatten();
            let candidates = if region.kind == "file" {
                vec![Region::whole(&path, &entry.id, 0)]
            } else if region.kind == "text" {
                text_candidates(
                    &source.text[source_range.clone()],
                    &other,
                    &path,
                    &entry.id,
                    mapped.clone(),
                )
            } else {
                other
                    .declarations
                    .iter()
                    .filter(|d| {
                        regions::selectable(d)
                            && (d.kind == region.kind
                                || d.callable()
                                    && matches!(
                                        region.kind.as_str(),
                                        "method" | "function" | "constructor" | "operator"
                                    ))
                    })
                    .map(|d| Region::declaration(&path, &entry.id, d, &other))
                    .collect()
            };
            for mut candidate in candidates {
                if matches!(region.kind.as_str(), "file" | "text") {
                    check(&self.store.cancel)?;
                } else {
                    candidate_work(work, &self.store.cancel)?;
                }
                let cr = candidate.body_range(&other);
                let a = &source.text[source_body.clone()];
                let b = &other.text[cr.clone()];
                let body = self.cache.body(
                    cache::key(
                        self.store.cache_scope(),
                        &("body", &region.blob, &source_body, &candidate.blob, &cr),
                    ),
                    a,
                    b,
                );
                let sim = body.similarity;
                let same_path = path == region.path;
                let same_owner = region.owner == candidate.owner;
                // Anonymous labels are display text, never continuity evidence.
                let same_name = !anonymous && region.name == candidate.name;
                let signature = region.parameters == candidate.parameters;
                let overlap = mapped
                    .as_ref()
                    .is_some_and(|r| r.start < candidate.span.end && candidate.span.start < r.end);
                let context = same_path && same_owner && (same_name || overlap);
                let small = body.small;
                // A renamed owner loses owner/name evidence even for a close
                // body edit. Keep rich same-owner scores unchanged: the exact-local
                // shortcut relies on their bound against non-local competitors.
                let group = !anonymous
                    && (small || sim < 70 || !same_owner)
                    && !context
                    && owner_group(region, &source, &candidate, &other);
                if small && !context && !group {
                    continue;
                }
                if !anonymous
                    && small
                    && same_path
                    && !same_name
                    && source.declarations.iter().any(|d| {
                        d.qualified == candidate.name && d.parameters == candidate.parameters
                    })
                {
                    continue;
                } // An unchanged trivial neighbour is not a new occurrence's origin.
                let callers = !anonymous
                    && !same_name
                    && sim < 70
                    && caller_replacement(region, &source, &candidate, &other);
                // A retained lexical header can support a small anonymous edit,
                // but only together with owner and mapped-range continuity.
                let header = anonymous
                    && context
                    && !region.written.is_empty()
                    && region.written == candidate.written;
                // A reused header must not beat a distinctive unchanged body.
                // Trigram similarity alone can be 100 for non-identical token streams.
                let exact = body.exact;
                // A substantial edited body can establish a member relocation
                // without establishing an owner move. Share the corroboration
                // band with sibling evidence, preserving the exact-local bound.
                let implementation = !same_owner
                    && !group
                    && !exact
                    && !region.fragment
                    && matches!(
                        region.kind.as_str(),
                        "method" | "function" | "constructor" | "operator"
                    )
                    && sim >= 70
                    && self.cache.anchors(
                        cache::key(
                            self.store.cache_scope(),
                            &("anchors", &region.blob, &source_body, &candidate.blob, &cr),
                        ),
                        a,
                        b,
                    );
                let mut score = sim / 2
                    + usize::from(exact) * 24
                    + usize::from(same_path) * 8
                    + usize::from(same_owner) * 18
                    + usize::from(same_name || header) * 12
                    + usize::from(signature) * 10
                    + usize::from(overlap) * 18
                    + usize::from(group || implementation) * 12
                    + usize::from(callers) * 24;
                let mut reason = if callers {
                    "caller replacement+owner+range"
                } else if header {
                    "header+body+owner+range"
                } else if implementation {
                    "body+ordered source anchors"
                } else {
                    "body+owner+range"
                };
                if region.kind == "file" {
                    score = sim + usize::from(same_path) * 60;
                    reason = "file content+path";
                }
                if region.kind == "text" {
                    score = sim + usize::from(same_path) * 15;
                    reason = "tokens+neighbours";
                }
                if region.fragment {
                    if let Some(range) = mapped
                        .clone()
                        .filter(|r| r.start >= candidate.span.start && r.end <= candidate.span.end)
                    {
                        candidate.span = range.clone();
                        candidate.body = range;
                        candidate.fragment = true;
                    } else {
                        let fragments = fragments(a, b, cr.start);
                        if let Some(range) = fragments.first() {
                            candidate.span = range.clone();
                            candidate.body = range.clone();
                            candidate.fragment = true;
                        } else {
                            continue;
                        }
                    }
                } else if region.kind != "file"
                    && region.kind != "text"
                    && b.len() > a.len().saturating_mul(3) / 2
                {
                    let parts = fragments(a, b, cr.start);
                    if !parts.is_empty() && regions::distinctive(a) >= 2 {
                        for range in parts {
                            let mut fragment = candidate.clone();
                            fragment.span = range.clone();
                            fragment.body = range;
                            fragment.fragment = true;
                            search.scored.push(Candidate {
                                score: 90 + usize::from(context) * 10,
                                region: fragment,
                                similarity: sim,
                                context,
                                origin: true,
                                exact: false,
                                reason: "retained implementation fragment",
                            });
                        }
                        continue;
                    }
                }
                if score >= 45 {
                    search.scored.push(Candidate {
                        score,
                        region: candidate,
                        similarity: sim,
                        context,
                        origin: sim >= 45 && (!small || group),
                        exact,
                        reason,
                    });
                }
            }
            // Competition must inspect changed files, not stop at an edited
            // namesake. A distinctive exact local match already exceeds every
            // non-local score by more than the margin, so it can still settle here.
            if matches!(scope, CandidateScope::Local(_))
                && required.is_none_or(|p| search.visited.contains(p) || !to.contains_key(p))
                && search.scored.iter().any(|c| c.exact && c.local(region))
            {
                break;
            }
        }
        search.remaining = paths.any(|p| {
            !search.visited.contains(&p)
                && matches!(to[&p].mode, 0o100644 | 0o100755)
                && regions::compatible(&p, &region.path)
        });
        if expanded {
            search.scored.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| a.region.key().cmp(&b.region.key()))
            });
        }
        Ok(expanded)
    }
}

fn candidate_work(work: &mut usize, cancel: &AtomicBool) -> Result<()> {
    check(cancel)?;
    ensure!(
        *work < CANDIDATE_WORK,
        "unresolved target: direct-continuation competition work limit reached"
    );
    *work += 1;
    Ok(())
}

fn overlapping(a: &Region, b: &Region) -> bool {
    a.path == b.path && a.blob == b.blob && a.span.start < b.span.end && b.span.start < a.span.end
}

fn retained_wrapper(
    region: &Region,
    old: &Region,
    blob: &str,
    fate: &Source,
    before: &Source,
) -> bool {
    let old_body = &before.text[old.body_range(before)];
    fate.declarations.iter().any(|d| {
        if d.qualified != old.name || d.parameters != old.parameters {
            return false;
        }
        let retained = Region::declaration(&old.path, blob, d, fate);
        let body = retained.body_range(fate);
        (retained.path != region.path || retained.span != region.span)
            && body.len() < old_body.len()
            && fate.calls.iter().any(|call| {
                call.name == short_name(&region.name)
                    && body.contains(&call.span.start)
                    && call.arguments.is_none_or(|n| n == region.parameters.len())
                    && (call.receiver.is_empty()
                        || [
                            "this",
                            "self",
                            "Self",
                            region.owner.as_str(),
                            short_name(&region.owner),
                        ]
                        .contains(&call.receiver.as_str()))
            })
    })
}

fn caller_replacement(a: &Region, source: &Source, b: &Region, other: &Source) -> bool {
    if a.name == b.name
        || a.kind == "file"
        || a.kind == "text"
        || source.calls.is_empty()
        || other.calls.is_empty()
    {
        return false;
    }
    let (new, old) = (short_name(&a.name), short_name(&b.name));
    source
        .declarations
        .iter()
        .filter(|d| d.callable() && d.qualified != a.name)
        .any(|caller| {
            let Some(previous) = other
                .declarations
                .iter()
                .find(|d| d.qualified == caller.qualified && d.parameters == caller.parameters)
            else {
                return false;
            };
            let after = &source.text[caller.span.clone()];
            let before = &other.text[previous.span.clone()];
            let at = regions::tokens(after);
            let bt = regions::tokens(before);
            let mut aw = regions::words(after, &at);
            let mut bw = regions::words(before, &bt);
            let new_calls: Vec<_> = aw
                .windows(2)
                .enumerate()
                .filter(|(_, p)| p[0] == new && p[1] == "(")
                .map(|(i, _)| i)
                .collect();
            let old_calls: Vec<_> = bw
                .windows(2)
                .enumerate()
                .filter(|(_, p)| p[0] == old && p[1] == "(")
                .map(|(i, _)| i)
                .collect();
            if new_calls.is_empty() || new_calls.len() != old_calls.len() {
                return false;
            }
            for i in new_calls {
                aw[i] = "<selected-call>";
            }
            for i in old_calls {
                bw[i] = "<selected-call>";
            }
            let matched = sequence::matches(&aw, &bw).len();
            100 * matched >= 80 * aw.len().max(bw.len())
        })
}

fn short_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}
fn owner_group(a: &Region, source: &Source, b: &Region, other: &Source) -> bool {
    if a.owner.is_empty() || b.owner.is_empty() {
        return false;
    }
    // A member cannot witness its own owner. Require independent, unconditional
    // sibling implementations on both sides, including in reciprocal searches.
    source
        .declarations
        .iter()
        .filter(|d| {
            d.owner == a.owner
                && d.qualified != a.name
                && d.callable()
                && d.guard == 1
                && !d.uncertain
        })
        .any(|d| {
            let body = &source.text[d.header.end..d.span.end];
            regions::distinctive(body) >= 3
                && other.declarations.iter().any(|e| {
                    if e.owner != b.owner
                        || e.qualified == b.name
                        || !e.callable()
                        || e.guard != 1
                        || e.uncertain
                        || e.name != d.name
                    {
                        return false;
                    }
                    let previous = &other.text[e.header.end..e.span.end];
                    regions::distinctive(previous) >= 3 && regions::similarity(body, previous) >= 80
                })
        })
}
fn fragments(needle: &str, haystack: &str, offset: usize) -> Vec<Range<usize>> {
    let nt = regions::tokens(needle);
    let ht = regions::tokens(haystack);
    let nw = regions::words(needle, &nt);
    let hw = regions::words(haystack, &ht);
    if nt.len() < 5 {
        return Vec::new();
    }
    let pairs = sequence::matches(&nw, &hw);
    if pairs.len() * 100 < nt.len() * 80 {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut i = 0;
    while i < pairs.len() {
        let start = i;
        while i + 1 < pairs.len()
            && pairs[i + 1].0 == pairs[i].0 + 1
            && pairs[i + 1].1 == pairs[i].1 + 1
        {
            i += 1;
        }
        if i + 1 - start >= 4 {
            result.push(offset + ht[pairs[start].1].start..offset + ht[pairs[i].1].end);
        }
        i += 1;
    }
    result
}
fn text_candidates(
    needle: &str,
    source: &Source,
    path: &str,
    blob: &str,
    mapped: Option<Range<usize>>,
) -> Vec<Region> {
    if let Some(range) = &mapped {
        let leading = needle.len() - needle.trim_start().len();
        let trailing = needle.len() - needle.trim_end().len();
        if let Some(start) = range.start.checked_sub(leading)
            && let Some(end) = range.end.checked_add(trailing)
            && source.text.get(start..end) == Some(needle)
        {
            return vec![Region::passage(path, blob, start..end)];
        }
    }
    let mut ranges = fragments(needle, &source.text, 0);
    if ranges.is_empty() {
        ranges.extend(mapped);
    }
    if ranges.is_empty() {
        let mut start = 0;
        for (end, _) in source
            .text
            .match_indices("\n\n")
            .chain(std::iter::once((source.text.len(), "")))
        {
            if end > start {
                ranges.push(start..end);
            }
            start = end + 2;
        }
    }
    // Preserve a line-range selection's ending newline when its matched tokens
    // end a source line. Token alignment must not invent whitespace-only edits.
    if needle.ends_with('\n') {
        for range in &mut ranges {
            if let Some(end) = source.text[range.end..].find('\n') {
                let suffix = &source.text[range.end..range.end + end];
                if suffix.chars().all(char::is_whitespace) {
                    range.end += end + 1;
                }
            }
        }
    }
    ranges
        .into_iter()
        .map(|r| Region::passage(path, blob, r))
        .collect()
}
pub(super) fn directory_prefix(pattern: &str) -> &str {
    let end = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    let prefix = &pattern[..end];
    if end == pattern.len() {
        prefix.trim_end_matches('/')
    } else {
        prefix.rsplit_once('/').map_or("", |(p, _)| p)
    }
}
pub(super) fn members(tree: &Tree, selection: paths::CompiledSelection<'_>) -> Vec<Region> {
    tree.iter()
        .filter(|(path, entry)| {
            matches!(entry.mode, 0o100644 | 0o100755)
                && regions::language(path).is_some()
                && selection.matches(path)
        })
        .map(|(path, entry)| Region::whole(path, &entry.id, 0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competition_work_is_bounded_and_cancellable() {
        let cancel = AtomicBool::new(false);
        let mut work = CANDIDATE_WORK - 1;
        candidate_work(&mut work, &cancel).unwrap();
        let error = candidate_work(&mut work, &cancel).unwrap_err();
        assert!(error.to_string().contains("unresolved target"));
        assert_eq!(work, CANDIDATE_WORK);
        cancel.store(true, std::sync::atomic::Ordering::Release);
        let error = candidate_work(&mut work, &cancel).unwrap_err();
        assert!(error.to_string().contains("history cancelled"));
        assert_eq!(work, CANDIDATE_WORK);
    }

    #[test]
    fn normalization_preserves_written_alternatives_and_uncertainty() {
        let mut first = Region::whole("Worker.cs", "blob", 100);
        first.kind = "method".into();
        first.name = "Worker.Target".into();
        first.written = "int Target ( int value )".into();
        first.parameters = vec!["int".into()];
        let mut second = first.clone();
        second.written = "int Target ( string value )".into();
        second.parameters = vec!["string".into()];
        let mut another_header = first.clone();
        another_header.written = "string Target ( int value )".into();
        let mut recovered = first.clone();
        recovered.weak = true;
        let mut state = State {
            regions: vec![first.clone(), second, another_header, recovered],
            ..State::default()
        };
        state.normalize();
        assert_eq!(state.regions.len(), 3);
        assert!(state.uncertain);
        assert!(
            state
                .regions
                .iter()
                .find(|r| r.same_alternative(&first))
                .unwrap()
                .weak
        );
        let canonical = state.clone();
        state.regions.reverse();
        state.normalize();
        assert!(state.same_occurrences(&canonical));
    }

    #[test]
    fn absence_is_evidence_not_an_empty_or_alternative_mapping() {
        let unknown = State::default();
        let absent = State::absent();
        assert!(!unknown.is_absent());
        assert!(!unknown.same_occurrences(&absent));

        let mut joined = State::default();
        joined.join(&absent);
        assert!(joined.is_absent());
        assert!(!joined.uncertain);
        for live in [
            State {
                regions: vec![Region::whole("empty.rs", "blob", 0)],
                ..State::default()
            },
            State {
                paths: vec!["src/**".into()],
                ..State::default()
            },
        ] {
            let mut alternative = State::absent();
            alternative.join(&live);
            assert!(!alternative.is_absent());
            assert!(alternative.absent && alternative.uncertain);
        }
    }
}
