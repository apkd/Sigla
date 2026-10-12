//! Factor grammar lists, not arbitrary lines or symbol assignments. A seed
//! parse supplies candidate seams only. Every local alternative must parse as
//! a complete list fragment inside the same retained owner braces. Otherwise
//! exploration falls back to the bounded, unfactored source.
use super::{FACT_BYTES, lexical::NESTING};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
};
use tree_sitter::Tree;

pub(super) struct Context {
    pub(super) ranges: Vec<Range<usize>>,
    frames: Vec<Range<usize>>,
    namespace: Option<Range<usize>>,
}
impl Context {
    pub(super) fn whole(size: usize) -> Self {
        Self {
            ranges: std::iter::once(0..size).collect(),
            frames: Vec::new(),
            namespace: None,
        }
    }
    fn new(
        mut ranges: Vec<Range<usize>>,
        frames: Vec<Range<usize>>,
        namespace: Option<Range<usize>>,
    ) -> Self {
        ranges.sort_by_key(|r| r.start);
        let mut merged = Vec::<Range<usize>>::with_capacity(ranges.len());
        for range in ranges.into_iter().filter(|r| !r.is_empty()) {
            if let Some(last) = merged.last_mut()
                && last.end >= range.start
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        Self {
            ranges: merged,
            frames,
            namespace,
        }
    }

    pub(super) fn valid(&self, tree: &Tree) -> bool {
        let mut bodies = Vec::new();
        let mut tokens = Vec::new();
        let mut cursor = tree.walk();
        'walk: loop {
            let node = cursor.node();
            if node.kind() == "declaration_list" {
                bodies.push(node.byte_range());
            }
            // A file-scoped namespace affects later root fragments, unlike a
            // using directive. It must belong to their shared lexical prefix.
            if node.kind() == "file_scoped_namespace_declaration"
                && self
                    .namespace
                    .as_ref()
                    .is_none_or(|r| node.start_byte() < r.start || node.end_byte() > r.end)
            {
                return false;
            }
            if node.child_count() == 0 && node.kind() != "compilation_unit" && !node.is_missing() {
                let range = node.byte_range();
                let at = self.ranges.partition_point(|r| r.end <= range.start);
                if self
                    .ranges
                    .get(at)
                    .is_none_or(|r| range.start < r.start || range.end > r.end)
                {
                    // A comment or literal crossing a masked seam can make two
                    // individually valid fragments lex differently when joined.
                    return false;
                }
                if node.kind() != "comment" {
                    tokens.push(range);
                }
            }
            if node.kind() != "comment" && cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
        self.frames.iter().all(|frame| {
            bodies.contains(frame)
                // A wholly disabled owner is also valid, but cannot lend its
                // context to active children or to a token crossing its hole.
                || !tokens
                    .iter()
                    .any(|t| t.start < frame.end && frame.start < t.end)
        })
    }
}

/// Probe trees cover live text guards, not all file configurations. Their
/// union also supplies seams for declarations absent from the first witness.
#[derive(Default)]
pub(super) struct Inventory {
    lists: BTreeMap<ListKey, List>,
    namespace: Option<Range<usize>>,
    entries: usize,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListKey {
    Root,
    Body(usize, usize),
}

#[derive(Default)]
struct List {
    children: BTreeMap<usize, Option<Range<usize>>>,
}
impl Inventory {
    pub(super) fn record(&mut self, tree: &Tree) -> Result<()> {
        let mut cursor = tree.walk();
        'walk: loop {
            let node = cursor.node();
            if matches!(node.kind(), "compilation_unit" | "declaration_list") {
                let key = if node.kind() == "compilation_unit" {
                    // The root may omit leading/trailing trivia in its range.
                    ListKey::Root
                } else {
                    ListKey::Body(node.start_byte(), node.end_byte())
                };
                let list = match self.lists.entry(key) {
                    Entry::Vacant(entry) => {
                        self.entries += 1;
                        ensure!(
                            self.entries <= 16_384,
                            "historical C# context inventory limit reached"
                        );
                        entry.insert(List::default())
                    }
                    Entry::Occupied(entry) => entry.into_mut(),
                };
                for child in node
                    .named_children(&mut node.walk())
                    .filter(|n| n.kind() != "comment")
                {
                    if node.kind() == "compilation_unit"
                        && child.kind() == "file_scoped_namespace_declaration"
                    {
                        let end = self
                            .namespace
                            .as_ref()
                            .map_or(0, |r| r.end)
                            .max(child.end_byte());
                        self.namespace = Some(0..end);
                    }
                    let body = child
                        .child_by_field_name("body")
                        .filter(|n| n.kind() == "declaration_list")
                        .map(|n| n.byte_range());
                    match list.children.entry(child.end_byte()) {
                        Entry::Vacant(entry) => {
                            entry.insert(body);
                            self.entries += 1;
                        }
                        Entry::Occupied(mut entry) => {
                            if entry.get() != &body {
                                // Conflicting owner topology is not an
                                // independently replaceable declaration list.
                                entry.insert(None);
                            }
                        }
                    }
                    ensure!(
                        self.entries <= 16_384,
                        "historical C# context inventory limit reached"
                    );
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
        Ok(())
    }

    pub(super) fn plan(&self, size: usize) -> Result<Vec<Context>> {
        let mut plan = Plan {
            inventory: self,
            contexts: Vec::new(),
            bytes: 0,
        };
        plan.list(ListKey::Root, 0..size, &[], &[])?;
        Ok(plan.contexts)
    }
}

struct Plan<'a> {
    inventory: &'a Inventory,
    contexts: Vec<Context>,
    bytes: usize,
}
impl Plan<'_> {
    fn push(&mut self, ranges: Vec<Range<usize>>, frames: Vec<Range<usize>>) -> Result<()> {
        let context = Context::new(ranges, frames, self.inventory.namespace.clone());
        let capacity = self.contexts.capacity();
        self.bytes += (context.ranges.capacity() + context.frames.capacity())
            * std::mem::size_of::<Range<usize>>();
        self.contexts.push(context);
        self.bytes += (self.contexts.capacity() - capacity) * std::mem::size_of::<Context>();
        ensure!(
            self.bytes <= FACT_BYTES,
            "historical C# context memory limit reached"
        );
        Ok(())
    }

    fn list(
        &mut self,
        key: ListKey,
        extent: Range<usize>,
        retained: &[Range<usize>],
        frames: &[Range<usize>],
    ) -> Result<()> {
        ensure!(
            frames.len() < NESTING,
            "historical C# declaration nesting limit reached"
        );
        let inventory = self.inventory;
        let list = &inventory.lists[&key];
        let mut retained = retained.to_vec();
        let mut start = extent.start;
        if key == ListKey::Root
            && let Some(namespace) = &self.inventory.namespace
        {
            retained.push(namespace.clone());
            start = namespace.end;
        }
        let children: Vec<_> = list
            .children
            .iter()
            .filter(|(end, _)| **end > start)
            .collect();
        if children.is_empty() {
            retained.push(start..extent.end);
            return self.push(retained, frames.to_vec());
        }
        for (index, (cut, body)) in children.iter().enumerate() {
            let end = if index + 1 == children.len() {
                extent.end
            } else {
                **cut
            };
            // Gaps, including all disabled text, belong to a fragment. No
            // inactive alternative disappears merely because a probe lacks it.
            let fragment = start..end;
            start = end;
            if let Some(body) = body
                .as_ref()
                .filter(|r| r.start >= fragment.start && r.end <= fragment.end)
            {
                let interior = body.start + 1..body.end - 1;
                let mut shell = retained.clone();
                shell.extend([fragment.start..interior.start, interior.end..fragment.end]);
                let mut nested = frames.to_vec();
                nested.push(body.clone());
                self.push(shell.clone(), nested.clone())?;
                self.list(
                    ListKey::Body(body.start, body.end),
                    interior,
                    &shell,
                    &nested,
                )?;
            } else {
                let mut ranges = retained.clone();
                ranges.push(fragment);
                self.push(ranges, frames.to_vec())?;
            }
        }
        Ok(())
    }
}
