//! Suppress only identifiable still-outstanding rejected edits. Returning to the
//! base state closes a rejection interval, preserving earlier edit/revert cycles.
//! Rejecting a deletion exposes its real predecessor for content attribution;
//! absence is neither an empty source nor permission to search by an old name.
use super::{Trace, matching::Tracker, regions::Value, render, sequence, walk::Graph};
use anyhow::Result;
use std::{collections::BTreeSet, ops::Range};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    base: Range<usize>,
    replacement: Vec<String>,
}
fn edits(base: &[&str], value: &str) -> Vec<Edit> {
    let lines: Vec<_> = value.split_inclusive('\n').collect();
    let mut result = Vec::new();
    for edit in sequence::edits(base, &lines) {
        // Equal-length replacement runs can be attributed line by line. This
        // keeps adjacent independent accepted/rejected edits from being fused.
        if edit.old.len() == edit.new.len() {
            for (old, new) in edit.old.zip(edit.new) {
                result.push(Edit {
                    base: old..old + 1,
                    replacement: vec![lines[new].into()],
                });
            }
        } else {
            result.push(Edit {
                base: edit.old,
                replacement: lines[edit.new].iter().map(|s| (*s).into()).collect(),
            });
        }
    }
    result
}
fn overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() || b.is_empty() {
        a.start <= b.end && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}
fn inside(edit: &Range<usize>, mask: &Range<usize>) -> bool {
    if mask.is_empty() {
        edit.is_empty() && edit.start == mask.start
    } else if edit.is_empty() {
        mask.start < edit.start && edit.start < mask.end
    } else {
        edit.start >= mask.start && edit.end <= mask.end
    }
}
fn retained(base: &[&str], edits: &[Edit], masks: &[Range<usize>]) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    for edit in edits
        .iter()
        .filter(|e| !masks.iter().any(|m| inside(&e.base, m)))
    {
        for line in &base[cursor..edit.base.start] {
            result.push_str(line);
        }
        for line in &edit.replacement {
            result.push_str(line);
        }
        cursor = edit.base.end;
    }
    for line in &base[cursor..] {
        result.push_str(line);
    }
    result
}

pub(super) fn filter(graph: &Graph, tracker: &mut Tracker, trace: &mut Trace) -> Result<()> {
    for id in &graph.order {
        let merge = &graph.commits[id];
        if merge.parents.len() < 2 || !trace.states.contains_key(id) {
            continue;
        }
        let bases = graph.bases(&merge.parents);
        let merged = super::single_value(tracker, &trace.states[id])?;
        let parents = merge
            .parents
            .iter()
            .map(|id| {
                trace
                    .states
                    .get(id)
                    .map(|state| super::single_value(tracker, state))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let mut parents: Vec<_> = parents.into_iter().map(Option::flatten).collect();
        // Per-edge equality establishes no merge-authored edit, not rejection of
        // every ordinary change on the other branch.
        let same_identity = |parent: &str| {
            trace.states.get(parent).is_some_and(|state| {
                state.absent == trace.states[id].absent
                    && state.regions.len() == trace.states[id].regions.len()
                    && state
                        .regions
                        .iter()
                        .zip(&trace.states[id].regions)
                        .all(|(a, b)| a.path == b.path && a.name == b.name && a.kind == b.kind)
            })
        };
        let identity_continues = merge.parents.iter().any(|p| same_identity(p));
        if !trace.states[id].uncertain
            && merged.as_ref().is_some_and(|m| {
                parents
                    .iter()
                    .enumerate()
                    .any(|(i, p)| p.as_ref() == Some(m) && same_identity(&merge.parents[i]))
            })
            && let Some(record) = trace.records.get_mut(id)
        {
            record.suppressed = true;
        }
        if bases.len() != 1
            || merged.is_none()
            || parents.iter().any(Option::is_none)
            || trace.states[id].uncertain
        {
            if let Some(record) = trace.records.get_mut(id) {
                record.uncertain = true;
                record
                    .notes
                    .insert("merge attribution unresolved (alternative regions or bases)".into());
            }
            continue;
        }
        let base_id = &bases[0];
        let Some(base_state) = trace.states.get(base_id) else {
            continue;
        };
        let Some(Value::Present(base_text)) = super::single_value(tracker, base_state)? else {
            continue;
        };
        let exclusive: Vec<_> = merge
            .parents
            .iter()
            .enumerate()
            .map(|(index, parent)| {
                let mut commits = graph.ancestors(parent);
                for (other_index, other) in merge.parents.iter().enumerate() {
                    if other_index != index {
                        let shared = graph.ancestors(other);
                        commits.retain(|id| !shared.contains(id));
                    }
                }
                commits
            })
            .collect();
        let mut contribution_tips = merge.parents.clone();
        for (index, parent) in merge.parents.iter().enumerate() {
            if matches!(merged, Some(Value::Present(_)))
                && matches!(parents[index], Some(Value::Absent))
            {
                // Reject existence separately from content. Once the deletion
                // is removed, its real predecessor supplies any still-outstanding
                // content edits. Completed edit/revert cycles remain ordinary.
                if let Some(before) = reject_deletion(graph, trace, parent, &exclusive[index]) {
                    parents[index] = super::single_value(tracker, &trace.states[&before])?;
                    contribution_tips[index] = before;
                } else {
                    parents[index] = None;
                    for affected in &exclusive[index] {
                        if let Some(record) = trace.records.get_mut(affected) {
                            record.uncertain = true;
                            record
                                .notes
                                .insert("deletion lineage attribution unresolved".into());
                        }
                    }
                }
            }
        }
        if parents.iter().any(Option::is_none) {
            continue;
        }
        let base: Vec<_> = base_text.split_inclusive('\n').collect();
        let merged_edits = edits(&base, merged.as_ref().unwrap().text());
        let parent_edits: Vec<_> = parents
            .iter()
            .map(|p| edits(&base, p.as_ref().unwrap().text()))
            .collect();
        if identity_continues
            && merged_edits
                .iter()
                .all(|e| parent_edits.iter().any(|p| p.contains(e)))
            && let Some(record) = trace.records.get_mut(id)
        {
            record.suppressed = true;
        }
        for (index, parent) in contribution_tips.iter().enumerate() {
            let rejected: Vec<_> = parent_edits[index]
                .iter()
                .filter(|edit| !merged_edits.iter().any(|m| overlap(&m.base, &edit.base)))
                .map(|e| e.base.clone())
                .collect();
            let exclusive = &exclusive[index];
            if parent_edits[index].iter().any(|e| {
                !merged_edits.contains(e) && merged_edits.iter().any(|m| overlap(&m.base, &e.base))
            }) {
                for commit in exclusive {
                    if let Some(record) = trace.records.get_mut(commit) {
                        record.uncertain = true;
                        record
                            .notes
                            .insert("merge-overlap attribution unresolved".into());
                    }
                }
            }
            if rejected.is_empty() {
                continue;
            }
            let mut pending = vec![(parent.clone(), rejected)];
            let mut visited = BTreeSet::new();
            while let Some((current, masks)) = pending.pop() {
                if !exclusive.contains(&current)
                    || !visited.insert((current.clone(), format!("{masks:?}")))
                {
                    continue;
                }
                let Some(state) = trace.states.get(&current) else {
                    continue;
                };
                let Some(text) = super::single_value(tracker, state)? else {
                    continue;
                };
                let current_edits = edits(&base, text.text());
                let masks: Vec<_> = masks
                    .into_iter()
                    .filter(|m| current_edits.iter().any(|e| overlap(&e.base, m)))
                    .collect();
                if masks.is_empty() {
                    continue;
                } // Earlier completed cycles remain visible.
                let commit = &graph.commits[&current];
                if commit.parents.len() != 1 || state.uncertain {
                    for affected in graph.ancestors(&current).intersection(exclusive) {
                        if let Some(record) = trace.records.get_mut(affected) {
                            record.uncertain = true;
                            record.notes.insert(
                                "nested or alternative merge attribution unresolved".into(),
                            );
                        }
                    }
                    continue; // Nested/ambiguous attribution is retained, never guessed away.
                }
                let before_id = &commit.parents[0];
                let Some(before_state) = trace.states.get(before_id) else {
                    continue;
                };
                let Some(before) = super::single_value(tracker, before_state)? else {
                    continue;
                };
                let before_edits = edits(&base, before.text());
                let changed: Vec<_> = current_edits
                    .iter()
                    .filter(|e| !before_edits.contains(e))
                    .chain(before_edits.iter().filter(|e| !current_edits.contains(e)))
                    .collect();
                let suppressed = changed
                    .iter()
                    .any(|e| masks.iter().any(|m| inside(&e.base, m)));
                if suppressed {
                    let before = retained(&base, &before_edits, &masks);
                    let after = retained(&base, &current_edits, &masks);
                    if let Some(record) = trace.records.get_mut(&current) {
                        if before == after {
                            record.suppressed = true;
                        } else {
                            record.partial = true;
                            (record.plus, record.minus) = render::counts(&before, &after);
                        }
                    }
                }
                pending.push((before_id.clone(), masks));
            }
        }
    }
    Ok(())
}

/// A rejected absence is not an empty textual replacement. Walk only the
/// forward-established lifetime, and suppress its actual disappearance edge.
/// No lookup by the deleted occurrence's former name/path is permitted here.
fn reject_deletion(
    graph: &Graph,
    trace: &mut Trace,
    parent: &str,
    exclusive: &BTreeSet<String>,
) -> Option<String> {
    let mut current = parent;
    while exclusive.contains(current) {
        let state = trace.states.get(current)?;
        if !state.is_absent() || state.uncertain {
            return None;
        }
        let [before_id] = graph.commits[current].parents.as_slice() else {
            return None;
        };
        let before = trace.states.get(before_id)?;
        if before.uncertain {
            return None;
        }
        if before.is_absent() {
            current = before_id;
            continue;
        }
        if before.absent || before.regions.len() != 1 || !before.paths.is_empty() {
            return None;
        }
        trace.records.get_mut(current)?.suppressed = true;
        return Some(before_id.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::{Record, matching::State, regions::Region};
    use super::*;
    use crate::repository::history::Commit;
    use std::collections::BTreeMap;

    fn fixture() -> (Graph, Trace) {
        let mut commits = BTreeMap::new();
        for (id, parents) in [
            ("base", vec![]),
            ("delete", vec!["base"]),
            ("gap", vec!["delete"]),
        ] {
            commits.insert(
                id.into(),
                Commit {
                    id: id.into(),
                    tree: String::new(),
                    parents: parents.into_iter().map(String::from).collect(),
                    date: String::new(),
                    subject: String::new(),
                },
            );
        }
        let graph = Graph {
            commits,
            order: ["gap", "delete", "base"].map(String::from).into(),
            boundary: None,
        };
        let trace = Trace {
            states: BTreeMap::from([
                (
                    "base".into(),
                    State {
                        regions: vec![Region::whole("empty.txt", "blob", 0)],
                        ..State::default()
                    },
                ),
                ("delete".into(), State::absent()),
                ("gap".into(), State::absent()),
            ]),
            records: ["base", "delete", "gap"]
                .into_iter()
                .map(|id| (id.into(), Record::default()))
                .collect(),
        };
        (graph, trace)
    }

    #[test]
    fn rejection_marks_the_disappearance_not_the_later_absent_tip() {
        let (graph, mut trace) = fixture();
        let exclusive = ["delete", "gap"].map(String::from).into();
        assert_eq!(
            reject_deletion(&graph, &mut trace, "gap", &exclusive),
            Some("base".into())
        );
        assert!(trace.records["delete"].suppressed);
        assert!(!trace.records["gap"].suppressed);
        assert!(!trace.records["base"].suppressed);
    }

    #[test]
    fn unknown_uncertain_and_nonexclusive_absences_are_not_rejected() {
        for state in [
            State::default(),
            State {
                uncertain: true,
                ..State::absent()
            },
        ] {
            let (graph, mut trace) = fixture();
            trace.states.insert("delete".into(), state);
            let exclusive = ["delete", "gap"].map(String::from).into();
            assert_eq!(reject_deletion(&graph, &mut trace, "gap", &exclusive), None);
            assert!(trace.records.values().all(|record| !record.suppressed));
        }
        let (graph, mut trace) = fixture();
        let exclusive = ["gap"].map(String::from).into();
        assert_eq!(reject_deletion(&graph, &mut trace, "gap", &exclusive), None);
        assert!(trace.records.values().all(|record| !record.suppressed));
    }
}
