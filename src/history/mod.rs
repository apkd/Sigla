//! Anchored, object-only source history. Pagination is reproducible from commit
//! IDs and ordinary numeric controls; shared facts only avoid repeated work.
mod cache;
mod matching;
mod merges;
mod paths;
mod query;
mod regions;
mod render;
mod sequence;
mod walk;

use crate::repository::history::{Commit, Store, check};
use anyhow::{Context, Result, ensure};
pub(crate) use cache::Cache;
use matching::{Mapping, State, Tracker};
use query::{Command, Request, Selector};
use regions::{Region, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};
use walk::Graph;

#[derive(Default)]
struct Record {
    changed: bool,
    plus: usize,
    minus: usize,
    counted: bool,
    uncertain: bool,
    scope_unresolved: bool,
    partial: bool,
    suppressed: bool,
    notes: BTreeSet<String>,
}
#[derive(Default)]
struct Trace {
    states: BTreeMap<String, State>,
    records: BTreeMap<String, Record>,
}

pub(crate) fn validate(input: &str) -> Result<()> {
    Request::parse(input).map(|_| ())
}

#[cfg(test)]
pub(crate) fn run(store: Store, selector: Option<&str>, input: &str) -> Result<String> {
    run_cached(
        store,
        selector,
        input,
        std::sync::Arc::new(Cache::new(crate::memory::cache_budget() / 4)),
    )
}

pub(crate) fn run_cached(
    mut store: Store,
    selector: Option<&str>,
    input: &str,
    cache: std::sync::Arc<Cache>,
) -> Result<String> {
    let request = Request::parse(input)?;
    store.commit_window(match (&request.command, &request.selector) {
        (Command::Log { skip, limit }, Selector::Repository) => Some(skip.saturating_add(*limit)),
        (_, Selector::Repository) => Some(1), // Snapshot endpoints need no ancestry lookahead.
        _ => None, // Selected-source lineage has no known commit-count bound.
    });
    let mut tracker = Tracker::new(store, cache.clone());
    let result = execute(&mut tracker, selector, &request);
    let stats = cache.stats();
    tracing::debug!(
        bytes = stats.bytes,
        entries = stats.entries,
        source_hits = stats.hits[0],
        tree_hits = stats.hits[1],
        mapping_hits = stats.hits[2],
        evidence_hits = stats.hits[3],
        source_misses = stats.misses[0],
        tree_misses = stats.misses[1],
        mapping_misses = stats.misses[2],
        evidence_misses = stats.misses[3],
        "History derived cache"
    );
    // Even an incomplete request can publish verified objects, but never a claim
    // of complete ancestry for an interval whose discovery failed.
    let persisted = tracker.store.finish();
    match result {
        Ok(text) => {
            persisted?;
            Ok(text)
        }
        Err(error) => {
            let _ = persisted;
            Err(error)
        }
    }
}

fn execute(tracker: &mut Tracker, revision: Option<&str>, request: &Request) -> Result<String> {
    let at = tracker.store.resolve(revision)?;
    let anchor = tracker.store.commit(&at)?;
    let repository = matches!(request.selector, Selector::Repository);
    let state = if repository {
        State::default()
    } else {
        tracker.anchor(&anchor, &request.selector)?
    };
    let target = render::label(&target(tracker, &state, &request.selector)?, 4096);
    let header = format!("at {at}\ntarget {target}");
    match &request.command {
        Command::Log { skip, limit } => {
            log(tracker, anchor, state, repository, &header, *skip, *limit)
        }
        Command::Show {
            commit,
            parent,
            offset,
        } => {
            let to = tracker.store.resolve(Some(commit))?;
            let child = tracker.store.commit(&to)?;
            ensure!(
                *parent <= child.parents.len().max(1),
                "Commit {to} has {} parents",
                child.parents.len()
            );
            let from = child.parents.get(parent - 1).cloned();
            let empty = if from.is_none() {
                Some(tracker.store.empty_tree()?)
            } else {
                None
            };
            let base = from.as_ref().or(empty.as_ref()).unwrap();
            let header = format!(
                "{header}\nfrom {base}{}\nto {to}\nparent {parent}",
                if from.is_none() { " (empty tree)" } else { "" }
            );
            let canonical = compare(tracker, &anchor, state, repository, from.as_deref(), &to)?;
            render::page(&header, &canonical, *offset)
        }
        Command::Diff { from, to, offset } => {
            let from = tracker.store.resolve(Some(from))?;
            let to = tracker.store.resolve(Some(to))?;
            let canonical = compare(tracker, &anchor, state, repository, Some(&from), &to)?;
            render::page(
                &format!("{header}\nfrom {from}\nto {to}"),
                &canonical,
                *offset,
            )
        }
    }
}

fn target(tracker: &mut Tracker, state: &State, selector: &Selector) -> Result<String> {
    if matches!(selector, Selector::Repository) {
        return Ok("repository".into());
    }
    if let Selector::Paths(patterns) = selector {
        return Ok(patterns.join(" "));
    }
    let mut labels = Vec::new();
    for region in &state.regions {
        labels.push(tracker.sources.label(&mut tracker.store, region)?);
    }
    Ok(labels.join(" | "))
}

/// One comparable occurrence, including its genuine absence. Alternative and
/// split histories are deliberately not flattened into invented merge evidence.
fn single_value(tracker: &mut Tracker, state: &State) -> Result<Option<Value>> {
    if state.uncertain {
        return Ok(None);
    }
    match (state.regions.as_slice(), state.absent) {
        ([], true) if state.paths.is_empty() => Ok(Some(Value::Absent)),
        ([region], false) => tracker
            .sources
            .text(&mut tracker.store, region)
            .map(|text| Some(Value::Present(text))),
        _ => Ok(None),
    }
}

fn edge_record(tracker: &mut Tracker, state: &State, mapping: &Mapping) -> Result<Record> {
    let mut record = Record {
        uncertain: state.uncertain || mapping.state.uncertain,
        scope_unresolved: state.scope_unresolved || mapping.state.scope_unresolved,
        counted: true,
        ..Record::default()
    };
    for link in &mapping.links {
        if let Some(label) = link.label() {
            record.notes.insert(label);
        }
    }
    // Identity is independent of line numbers and blob IDs. Unchanged selected
    // bytes moving past an unrelated edit do not create a source-history record.
    let canonical = render::comparison(
        &mut tracker.store,
        &mut tracker.sources,
        &mapping.state.regions,
        &state.regions,
        &[],
    )?;
    record.changed = canonical != "No changes.\n" || !record.notes.is_empty();
    if !record.changed {
        return Ok(record);
    }
    let (old, new) = (
        single_value(tracker, &mapping.state)?,
        single_value(tracker, state)?,
    );
    if let (Some(old), Some(new)) = (old, new) {
        (record.plus, record.minus) = render::counts(old.text(), new.text());
    } else {
        // Counts in canonical patches already exclude unrelated source. Union
        // alternatives are not a single edit and therefore have no aggregate count.
        record.counted = !record.uncertain;
        if record.counted {
            record.plus = canonical
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++ "))
                .count();
            record.minus = canonical
                .lines()
                .filter(|l| l.starts_with('-') && !l.starts_with("--- "))
                .count();
        }
    }
    if record.notes.is_empty() && record.changed {
        record.notes.insert(
            if mapping.state.regions.is_empty() {
                "add"
            } else if state.regions.is_empty() {
                "delete"
            } else {
                "edit"
            }
            .into(),
        );
    }
    Ok(record)
}
fn combine(record: &mut Record, other: Record) {
    // Multiple parent comparisons are not additive authorship. Show counts only
    // for single-parent records; merge attribution is handled after the interval.
    record.changed |= other.changed;
    record.uncertain |= other.uncertain;
    record.scope_unresolved |= other.scope_unresolved;
    record.notes.extend(other.notes);
    record.counted = false;
}
fn backwards(
    tracker: &mut Tracker,
    graph: &Graph,
    at: &str,
    state: State,
    records: bool,
) -> Result<Trace> {
    let mut trace = Trace::default();
    trace.states.insert(at.into(), state);
    let mut retained_bytes: usize = 0;
    for id in &graph.order {
        check(&tracker.store.cancel)?;
        let Some(state) = trace.states.get(id).cloned() else {
            continue;
        };
        retain_state(&mut retained_bytes, &state)?;
        if state.is_absent() {
            // This commit's absence proves nothing about its parents. In
            // particular, a deletion must not erase their completed history.
            continue;
        }
        let commit = &graph.commits[id];
        let mut record = None;
        for parent in &commit.parents {
            let mapping = tracker
                .map(commit, &graph.commits[parent], &state)
                .with_context(|| format!("unfinished comparison {id} to {parent}"))?;
            if records {
                let edge = edge_record(tracker, &state, &mapping)?;
                if let Some(record) = &mut record {
                    combine(record, edge);
                } else {
                    record = Some(edge);
                }
            }
            trace
                .states
                .entry(parent.clone())
                .or_default()
                .join(&mapping.state);
        }
        if records {
            let record = match record {
                Some(record) => record,
                None => edge_record(tracker, &state, &Mapping::absent())?,
            };
            trace.records.insert(id.clone(), record);
        }
    }
    if let Some(state) = graph.boundary.as_ref().and_then(|id| trace.states.get(id)) {
        retain_state(&mut retained_bytes, state)?;
    }
    forwards(tracker, graph, &mut trace, records, &mut retained_bytes)?;
    Ok(trace)
}

const TRACE_MEMORY: usize = 128 * 1024 * 1024;

fn retain_state(bytes: &mut usize, state: &State) -> Result<()> {
    *bytes = bytes.saturating_add(std::mem::size_of::<State>() + state.heap_bytes());
    ensure!(
        *bytes <= TRACE_MEMORY,
        "history occurrence graph exceeds request memory bound"
    );
    Ok(())
}

/// Complete branch bridges from occurrences already reached through shared
/// ancestry. Never search across an absence for an older name/path incarnation.
/// An absence is absorbing forward, but never propagated backward as evidence.
fn forwards(
    tracker: &mut Tracker,
    graph: &Graph,
    trace: &mut Trace,
    records: bool,
    retained_bytes: &mut usize,
) -> Result<()> {
    for id in graph.order.iter().rev() {
        check(&tracker.store.cancel)?;
        if trace.states.get(id).is_some_and(|state| !state.is_absent()) {
            continue;
        }
        let child = &graph.commits[id];
        let parents: Vec<_> = child
            .parents
            .iter()
            .filter_map(|id| trace.states.get(id).map(|state| (id, state.clone())))
            .collect();
        if parents.is_empty() {
            continue;
        }
        if parents.len() != child.parents.len() {
            ensure!(
                parents.iter().all(|(_, state)| state.is_absent()),
                "unresolved target at {id}: branch bridge has an unmapped parent"
            );
            continue;
        }
        let mut state = State::default();
        for (parent, before) in &parents {
            let mapped = tracker
                .map(&graph.commits[*parent], child, before)
                .with_context(|| format!("unfinished branch bridge {parent} to {id}"))?;
            state.join(&mapped.state);
        }
        ensure!(
            !trace.states.contains_key(id) || state.is_absent(),
            "unresolved target at {id}: absence disagrees with inherited occurrence"
        );
        if let Some(previous) = trace.states.get(id) {
            state.uncertain |= previous.uncertain;
            state.scope_unresolved |= previous.scope_unresolved;
        }
        if records {
            let mut record = None;
            for (parent, before) in &parents {
                let mapping = if state.is_absent() || before.is_absent() {
                    // A forward-proven disappearance has a real predecessor,
                    // unlike an uninitialized mapping or a source introduction.
                    Mapping {
                        state: before.clone(),
                        ..Mapping::default()
                    }
                } else {
                    let mapping = tracker.map(child, &graph.commits[*parent], &state)?;
                    ensure!(
                        mapping.state.same_occurrences(before),
                        "unresolved target at {id}: branch bridge has competing predecessors"
                    );
                    mapping
                };
                let edge = edge_record(tracker, &state, &mapping)?;
                if let Some(record) = &mut record {
                    combine(record, edge);
                } else {
                    record = Some(edge);
                }
            }
            if let Some(record) = record {
                trace.records.insert(id.clone(), record);
            }
        }
        retain_state(retained_bytes, &state)?;
        trace.states.insert(id.clone(), state);
    }
    Ok(())
}

struct Page {
    text: String,
    skip: usize,
    limit: usize,
    matched: usize,
    emitted: usize,
    deferred: bool,
    scope_unresolved: bool,
}
impl Page {
    fn new(header: &str, skip: usize, limit: usize) -> Self {
        Self {
            text: format!("{header}\n"),
            skip,
            limit,
            matched: 0,
            emitted: 0,
            deferred: false,
            scope_unresolved: false,
        }
    }
    fn remaining(&self) -> usize {
        self.skip
            .saturating_add(self.limit)
            .saturating_sub(self.matched)
    }
    fn full(&self) -> bool {
        self.deferred || self.emitted >= self.limit || self.text.len() >= render::PAGE_BYTES
    }
    fn push(&mut self, commit: &Commit, record: Option<&Record>) {
        // Coverage survives unchanged/suppressed records and numeric skipping.
        self.scope_unresolved |= record.is_some_and(|r| r.scope_unresolved);
        if record.is_some_and(|r| !r.changed || r.suppressed) {
            return;
        }
        self.matched += 1;
        if self.matched <= self.skip {
            return;
        }
        let subject: String = commit.subject.chars().take(300).collect();
        let mut line = String::new();
        if let Some(record) = record {
            let mut notes = record
                .notes
                .iter()
                .take(8)
                .map(|s| render::label(s, 512))
                .collect::<Vec<_>>()
                .join("; ");
            if record.notes.len() > 8 {
                write!(notes, "; {} further relationships", record.notes.len() - 8).unwrap();
            }
            if record.uncertain && !notes.starts_with('~') {
                notes.insert(0, '~');
            }
            let counts = if record.counted {
                format!(" +{} -{}", record.plus, record.minus)
            } else {
                String::new()
            };
            writeln!(
                line,
                "{} {} {}{}{} {}",
                commit.id,
                commit.date,
                notes,
                if record.partial {
                    " partial (rejected edits excluded)"
                } else {
                    ""
                },
                counts,
                subject
            )
            .unwrap();
        } else {
            writeln!(line, "{} {} {}", commit.id, commit.date, subject).unwrap();
        }
        if self.emitted > 0 && self.text.len() + line.len() + 128 > render::PAGE_BYTES {
            self.deferred = true;
            return;
        }
        self.text.push_str(&line);
        self.emitted += 1;
    }
    fn end(mut self, more: bool, incomplete: Option<&anyhow::Error>) -> String {
        if self.scope_unresolved && (incomplete.is_some() || more || self.deferred) {
            writeln!(self.text, "coverage: {}", paths::UNRESOLVED).unwrap();
        }
        if let Some(reason) = incomplete {
            writeln!(self.text, "incomplete: {reason:#}").unwrap();
        } else if more || self.deferred {
            writeln!(
                self.text,
                "more skip:{}",
                self.skip.saturating_add(self.emitted)
            )
            .unwrap();
        } else if self.scope_unresolved {
            writeln!(self.text, "incomplete: {}", paths::UNRESOLVED).unwrap();
        } else {
            self.text.push_str("end\n");
        }
        self.text
    }
}

fn log(
    tracker: &mut Tracker,
    mut commit: Commit,
    mut state: State,
    repository: bool,
    header: &str,
    skip: usize,
    limit: usize,
) -> Result<String> {
    let mut page = Page::new(header, skip, limit);
    loop {
        if let Err(error) = check(&tracker.store.cancel) {
            return Ok(page.end(false, Some(&error)));
        }
        if repository {
            page.push(&commit, None);
            if commit.parents.is_empty() {
                tracker.store.complete.insert(commit.id.clone());
                return Ok(page.end(false, None));
            }
            if page.full() {
                return Ok(page.end(true, None));
            }
            // Warm commits and already emitted rows reduce useful lookahead.
            tracker.store.commit_window(Some(page.remaining()));
        }
        if commit.parents.len() > 1 {
            // Closing an all-parent interval can require more than the remaining
            // page width. The planner never decides where traversal ends.
            tracker.store.commit_window(None);
            // No record from an unfinished merge interval enters the finalized
            // prefix. DFS rank is independent of timestamps and cache state.
            let interval = (|| -> Result<(Graph, Option<Trace>)> {
                let graph = Graph::merge(&mut tracker.store, &commit, !repository)?;
                let trace = if repository {
                    None
                } else {
                    let mut trace = backwards(tracker, &graph, &commit.id, state, true)?;
                    merges::filter(&graph, tracker, &mut trace)?;
                    Some(trace)
                };
                Ok((graph, trace))
            })();
            let (graph, trace) = match interval {
                Ok(value) => value,
                Err(error) => return Ok(page.end(false, Some(&error))),
            };
            let previous = graph.boundary.as_ref().and_then(|id| {
                trace
                    .as_ref()
                    .and_then(|trace| trace.states.get(id))
                    .cloned()
            });
            let more = graph.boundary.is_some()
                && (repository
                    || previous
                        .as_ref()
                        .is_some_and(|state| !state.regions.is_empty() || !state.paths.is_empty()));
            for (index, id) in graph.order.iter().enumerate() {
                if repository {
                    if id != &commit.id {
                        page.push(&graph.commits[id], None);
                    }
                } else if let Some(record) = trace.as_ref().unwrap().records.get(id) {
                    page.push(&graph.commits[id], Some(record));
                }
                if page.full() {
                    return Ok(page.end(index + 1 < graph.order.len() || more, None));
                }
            }
            if !more {
                return Ok(page.end(false, None));
            }
            if repository {
                tracker.store.commit_window(Some(page.remaining()));
            }
            commit = match graph.boundary_commit(&mut tracker.store) {
                Ok(commit) => commit,
                Err(error) => return Ok(page.end(false, Some(&error))),
            };
            state = previous.unwrap_or_default();
            continue;
        }
        let step = (|| -> Result<Option<(Commit, State)>> {
            let parent = commit
                .parents
                .first()
                .map(|id| tracker.store.commit(id))
                .transpose()?;
            let mapping = if repository {
                Mapping::default()
            } else if let Some(parent) = &parent {
                tracker.map(&commit, parent, &state)?
            } else {
                Mapping::absent()
            };
            if !repository {
                let record = edge_record(tracker, &state, &mapping)?;
                page.push(&commit, Some(&record));
            }
            if parent.is_none() {
                tracker.store.complete.insert(commit.id.clone());
            }
            Ok(parent.map(|parent| (parent, mapping.state)))
        })();
        let next = match step {
            Ok(next) => next,
            Err(error) => {
                tracker.store.boundaries.insert(commit.id.clone());
                return Ok(page.end(false, Some(&error)));
            }
        };
        let Some((parent, previous)) = next else {
            return Ok(page.end(false, None));
        };
        if !repository && previous.regions.is_empty() && previous.paths.is_empty() {
            return Ok(page.end(false, None));
        }
        if page.full() {
            return Ok(page.end(true, None));
        }
        commit = parent;
        state = previous;
    }
}

fn endpoints(
    tracker: &mut Tracker,
    anchor: &Commit,
    state: State,
    tips: &[String],
) -> Result<BTreeMap<String, State>> {
    let mut roots = vec![anchor.id.clone()];
    roots.extend_from_slice(tips);
    let graph = Graph::connect(&mut tracker.store, &roots)?;
    let trace = backwards(tracker, &graph, &anchor.id, state, false)?;
    for tip in tips {
        ensure!(
            trace.states.contains_key(tip),
            "unresolved target at {tip}: occurrence lineage is unavailable"
        );
    }
    Ok(trace.states)
}

fn repository_regions(tracker: &mut Tracker, commit: &Commit) -> Result<Vec<Region>> {
    Ok(tracker
        .tree(commit)?
        .iter()
        .filter(|(path, e)| {
            matches!(e.mode, 0o100644 | 0o100755) && regions::language(path).is_some()
        })
        .map(|(path, e)| Region::whole(path, &e.id, 0))
        .collect())
}
fn compare(
    tracker: &mut Tracker,
    anchor: &Commit,
    state: State,
    repository: bool,
    from: Option<&str>,
    to: &str,
) -> Result<String> {
    let child = tracker.store.commit(to)?;
    let parent = from.map(|id| tracker.store.commit(id)).transpose()?;
    if repository {
        // Both snapshots are required here. Selected-source lineage below keeps
        // its lazy tree discovery, including absence and unresolved intervals.
        let trees: Vec<_> = parent
            .iter()
            .map(|p| p.tree.clone())
            .chain(std::iter::once(child.tree.clone()))
            .collect();
        tracker.store.prefetch(&trees, "tree")?;
    }
    let (mut old, mut new, mut notes) = if repository {
        (
            parent
                .as_ref()
                .map(|p| repository_regions(tracker, p))
                .transpose()?
                .unwrap_or_default(),
            repository_regions(tracker, &child)?,
            vec!["source files only; binary, symlink, submodule and LFS payloads excluded".into()],
        )
    } else {
        let mut tips = vec![to.to_owned()];
        if let Some(from) = from {
            tips.push(from.into());
        }
        let mut states = endpoints(tracker, anchor, state, &tips)?;
        let new = states.remove(to).context("unresolved target")?;
        let old = if from == Some(to) {
            new.clone()
        } else {
            from.and_then(|id| states.remove(id)).unwrap_or_default()
        };
        let mut notes = Vec::new();
        if old.scope_unresolved || new.scope_unresolved {
            notes.push(paths::UNRESOLVED.into());
        }
        if old.uncertain || new.uncertain {
            notes.push(
                "~alternative or inferred region ancestry; not independent whole-file edits".into(),
            );
        }
        (old.regions, new.regions, notes)
    };
    // Equal whole-file objects require no source hydration at all. Mode changes
    // are reported from tree metadata, including an otherwise empty comparison.
    let unchanged: BTreeSet<_> = old
        .iter()
        .filter(|a| {
            a.kind == "file"
                && new
                    .iter()
                    .any(|b| b.kind == "file" && a.path == b.path && a.blob == b.blob)
        })
        .map(|r| (r.path.clone(), r.blob.clone()))
        .collect();
    old.retain(|r| !unchanged.contains(&(r.path.clone(), r.blob.clone())));
    new.retain(|r| !unchanged.contains(&(r.path.clone(), r.blob.clone())));
    if repository {
        let after = tracker.tree(&child)?;
        if let Some(parent) = &parent {
            let before = tracker.tree(parent)?;
            for (path, entry) in after.iter() {
                if let Some(old) = before.get(path).filter(|old| old.mode != entry.mode) {
                    notes.push(format!("mode {path} {:06o}→{:06o}", old.mode, entry.mode));
                }
            }
        }
    }
    render::comparison(&mut tracker.store, &mut tracker.sources, &old, &new, &notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grammar_reuses_selectors_but_rejects_search_operations_and_misplaced_controls() {
        for query in [
            "log",
            "log m:Parser.Parse(string) path:src/**",
            "show HEAD parent:2 @a.cs:2:4",
            "diff a..b @doc.md:2-8",
        ] {
            validate(query).unwrap();
        }
        for query in [
            "log calls:Parse",
            "log method:Parse project:Build",
            "log offset:2",
            "show HEAD limit:1",
            "diff a...b",
            "log file:../outside.rs",
        ] {
            assert!(validate(query).is_err(), "{query}");
        }
    }
}
#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn written_headers_count_towards_the_trace_memory_limit() {
        let mut region = Region::whole("Worker.cs", "blob", 0);
        region.written = "header ".repeat(2048);
        let mut bytes = TRACE_MEMORY - region.written.len() / 2;
        let state = State {
            regions: vec![region],
            ..State::default()
        };
        assert!(retain_state(&mut bytes, &state).is_err());
    }

    #[test]
    fn scope_coverage_survives_parent_combination_suppression_and_skipping() {
        let commit = Commit {
            id: "commit".into(),
            tree: String::new(),
            parents: Vec::new(),
            date: String::new(),
            subject: "subject".into(),
        };
        let mut record = Record::default();
        combine(
            &mut record,
            Record {
                scope_unresolved: true,
                ..Record::default()
            },
        );
        for (changed, suppressed, skip) in [(false, false, 0), (true, true, 0), (true, false, 1)] {
            record.changed = changed;
            record.suppressed = suppressed;
            let mut page = Page::new("header", skip, 1);
            page.push(&commit, Some(&record));
            let text = page.end(false, None);
            assert!(
                text.contains("incomplete: unresolved directory scope"),
                "{text}"
            );
            assert!(!text.ends_with("end\n"), "{text}");
        }
        let mut page = Page::new("header", 0, 1);
        page.push(&commit, Some(&record));
        let text = page.end(true, None);
        assert!(
            text.contains("coverage: unresolved directory scope"),
            "{text}"
        );
        assert!(text.contains("more skip:1"), "{text}");
    }
}
