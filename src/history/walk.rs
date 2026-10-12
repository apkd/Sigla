//! Request-local parent discovery. A closed interval ends at an all-path join,
//! not at an arbitrary depth, timestamp, first intersection or missing object.
use crate::repository::history::{Commit, Store, check};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::atomic::AtomicBool,
};

pub(super) struct Graph {
    pub commits: BTreeMap<String, Commit>,
    /// Finalizable commits, in DFS-rank child-before-parent order. The boundary
    /// is deliberately excluded: its parent comparisons have not been requested.
    pub order: Vec<String>,
    /// Every path out of the interval passes through this commit. Its actual
    /// parents remain intact, and its object is optional for metadata-only logs.
    pub boundary: Option<String>,
}
impl Graph {
    pub fn merge(store: &mut Store, commit: &Commit, source: bool) -> Result<Self> {
        Self::discover(store, &commit.parents, Some(commit), source)
    }
    pub fn connect(store: &mut Store, tips: &[String]) -> Result<Self> {
        Self::discover(store, tips, None, true)
    }
    fn discover(
        store: &mut Store,
        tips: &[String],
        head: Option<&Commit>,
        source: bool,
    ) -> Result<Self> {
        let mut discovery = Discovery::new(tips)?;
        let cancel = store.cancel.clone();
        let mut graph = match discovery.run(&cancel, source, |id| store.commit(id)) {
            Ok(graph) => graph,
            Err(error) => {
                // Only failed acquisition is a coverage boundary. A successful
                // request cut, including discarded lookahead, publishes no gap.
                for node in &discovery.nodes {
                    if node.parents.iter().any(|p| discovery.failed.contains(p)) {
                        store.boundaries.insert(node.id.clone());
                    }
                }
                if let Some(head) = head
                    && head.parents.iter().any(|p| {
                        discovery
                            .ids
                            .get(p)
                            .is_some_and(|i| discovery.failed.contains(i))
                    })
                {
                    store.boundaries.insert(head.id.clone());
                }
                return Err(error);
            }
        };
        if let Some(head) = head {
            graph.order.insert(0, head.id.clone());
            graph.commits.insert(head.id.clone(), head.clone());
        }
        graph.certify(&mut store.complete);
        store.boundaries.retain(|id| !store.complete.contains(id));
        Ok(graph)
    }
    fn certify(&self, complete: &mut BTreeSet<String>) {
        // Retaining a commit does not prove its ancestry complete. Certify only
        // real roots and children whose every parent is already certified.
        for id in self.boundary.iter().chain(self.order.iter().rev()) {
            if let Some(commit) = self.commits.get(id)
                && commit.parents.iter().all(|p| complete.contains(p))
            {
                complete.insert(id.clone());
            }
        }
    }
    pub fn boundary_commit(&self, store: &mut Store) -> Result<Commit> {
        let id = self
            .boundary
            .as_ref()
            .context("Missing history continuation")?;
        if let Some(commit) = self.commits.get(id) {
            return Ok(commit.clone());
        }
        match store.commit(id) {
            Ok(commit) => Ok(commit),
            Err(error) => {
                for child in self.commits.values().filter(|c| c.parents.contains(id)) {
                    store.boundaries.insert(child.id.clone());
                }
                Err(error)
            }
        }
    }
    pub fn ancestors(&self, tip: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![tip.to_owned()];
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) || self.boundary.as_ref() == Some(&id) {
                continue;
            }
            if let Some(commit) = self.commits.get(&id) {
                pending.extend(commit.parents.iter().cloned());
            }
        }
        seen
    }
    pub fn bases(&self, tips: &[String]) -> Vec<String> {
        let Some(first) = tips.first() else {
            return Vec::new();
        };
        let mut common = self.ancestors(first);
        for tip in &tips[1..] {
            let ancestors = self.ancestors(tip);
            common.retain(|id| ancestors.contains(id));
        }
        let mut below = BTreeSet::new();
        let mut bases = Vec::new();
        for id in self.order.iter().chain(self.boundary.iter()) {
            if common.contains(id) && !below.contains(id) {
                bases.push(id.clone());
                below.extend(self.ancestors(id));
            }
        }
        bases
    }
}

struct Node {
    id: String,
    commit: Option<Commit>,
    parents: Vec<usize>,
    children: usize,
    // A component shares a proven all-path continuation, not just an edge.
    component: usize,
    size: usize,
    selected: bool,
    // Distinct parent components of an unreduced node.
    pending: usize,
    // Unreduced children with a parent in this component (roots only).
    waiting: BTreeSet<usize>,
}
struct Discovery {
    nodes: Vec<Node>,
    ids: BTreeMap<String, usize>,
    tips: Vec<usize>,
    tip_components: usize,
    ready: Vec<usize>,
    bytes: usize,
    failed: BTreeSet<usize>,
    failure: Option<(usize, anyhow::Error)>,
    #[cfg(test)]
    work: Work,
}
#[cfg(test)]
#[derive(Debug, Default)]
struct Work {
    reductions: usize,
    transfers: usize,
    replays: usize,
}
impl Discovery {
    fn new(tips: &[String]) -> Result<Self> {
        let mut discovery = Self {
            nodes: Vec::new(),
            ids: BTreeMap::new(),
            tips: Vec::new(),
            tip_components: 0,
            ready: Vec::new(),
            bytes: 0,
            failed: BTreeSet::new(),
            failure: None,
            #[cfg(test)]
            work: Work::default(),
        };
        for tip in tips {
            if !discovery.ids.contains_key(tip) {
                let index = discovery.node(tip)?;
                discovery.tips.push(index);
                discovery.nodes[index].selected = true;
                discovery.tip_components += 1;
            }
        }
        ensure!(!discovery.tips.is_empty(), "Missing history graph tips");
        Ok(discovery)
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        ensure!(
            self.bytes <= 128 * 1024 * 1024,
            "history parent graph exceeds request memory bound"
        );
        Ok(())
    }
    fn node(&mut self, id: &str) -> Result<usize> {
        self.charge(512 + id.len())?;
        let index = self.nodes.len();
        self.ids.insert(id.into(), index);
        self.nodes.push(Node {
            id: id.into(),
            commit: None,
            parents: Vec::new(),
            children: 0,
            component: index,
            size: 1,
            selected: false,
            pending: 0,
            waiting: BTreeSet::new(),
        });
        Ok(index)
    }
    fn component(&mut self, mut index: usize) -> usize {
        while self.nodes[index].component != index {
            let parent = self.nodes[index].component;
            self.nodes[index].component = self.nodes[parent].component;
            index = parent;
        }
        index
    }
    fn join(&mut self, a: usize, b: usize, cancel: &AtomicBool) -> Result<()> {
        let (mut a, mut b) = (self.component(a), self.component(b));
        if a == b {
            return Ok(());
        }
        if self.nodes[a].size < self.nodes[b].size {
            std::mem::swap(&mut a, &mut b);
        }
        let size = self.nodes[b].size;
        self.nodes[b].component = a;
        self.nodes[a].size += size;
        let selected = self.nodes[b].selected;
        if self.nodes[a].selected && selected {
            self.tip_components -= 1;
        }
        self.nodes[a].selected |= selected;
        // Union by size moves each waiter at most log2(nodes) times. A duplicate
        // means one child's distinct parent obligations have just coalesced.
        for child in std::mem::take(&mut self.nodes[b].waiting) {
            check(cancel)?;
            #[cfg(test)]
            {
                self.work.transfers += 1;
            }
            if !self.nodes[a].waiting.insert(child) {
                self.nodes[child].pending -= 1;
                if self.nodes[child].pending == 1 {
                    self.ready.push(child);
                }
            }
        }
        Ok(())
    }
    fn expand(&mut self, index: usize, commit: Commit, cancel: &AtomicBool) -> Result<()> {
        for parent in &commit.parents {
            check(cancel)?;
            // Reverse waiters are additional to the retained parent vectors.
            self.charge(64)?;
            let parent = if let Some(&parent) = self.ids.get(parent) {
                parent
            } else {
                self.node(parent)?
            };
            self.nodes[index].parents.push(parent);
            self.nodes[parent].children += 1;
            let component = self.component(parent);
            if self.nodes[component].waiting.insert(index) {
                self.nodes[index].pending += 1;
            }
        }
        self.retain(index, commit)?;
        if self.nodes[index].pending == 1 {
            self.ready.push(index);
        }
        // Only a fully read node whose every parent has the same continuation
        // can reduce into that group. Roots and unread/failed nodes stay opaque;
        // speculative forks below a join need not reduce for the tips to meet.
        while let Some(index) = self.ready.pop() {
            check(cancel)?;
            #[cfg(test)]
            {
                self.work.reductions += 1;
            }
            let parent = self.component(self.nodes[index].parents[0]);
            self.nodes[parent].waiting.remove(&index);
            self.join(index, parent, cancel)?;
        }
        Ok(())
    }
    fn retain(&mut self, index: usize, commit: Commit) -> Result<()> {
        ensure!(
            commit.id == self.nodes[index].id,
            "Unexpected history commit"
        );
        self.charge(
            commit.subject.len()
                + commit.tree.len()
                + commit.date.len()
                + commit.parents.iter().map(|p| p.len() + 32).sum::<usize>(),
        )?;
        self.nodes[index].commit = Some(commit);
        Ok(())
    }
    fn run(
        &mut self,
        cancel: &AtomicBool,
        source: bool,
        mut read: impl FnMut(&str) -> Result<Commit>,
    ) -> Result<Graph> {
        check(cancel)?;
        if let [tip] = self.tips.as_slice() {
            return self.finish(Some(*tip), source, cancel, &mut read);
        }
        // Appended nodes form a deterministic breadth-first queue. A short arm
        // can look below a join before the longer arm reaches it. Defer failures
        // until closure can prove whether those objects belong to the request.
        let mut next = 0;
        while next < self.nodes.len() {
            check(cancel)?;
            let commit = match read(&self.nodes[next].id) {
                Ok(commit) => commit,
                Err(error) => {
                    check(cancel)?;
                    self.failed.insert(next);
                    // Keep one original error/backtrace, not one per open arm.
                    if self.failure.is_none() {
                        self.failure = Some((next, error));
                    }
                    next += 1;
                    continue;
                }
            };
            self.expand(next, commit, cancel)?;
            if self.tip_components == 1 {
                // Reduction proves existence, not the nearest boundary's ID.
                // Replay once to select exactly the original all-path cut.
                let boundary = self.cut(cancel)?.context("Invalid cyclic Git ancestry")?;
                return self.finish(Some(boundary), source, cancel, &mut read);
            }
            next += 1;
        }
        if let Some((_, error)) = self.failure.take() {
            return Err(error);
        }
        self.finish(None, source, cancel, &mut read)
    }
    fn cut(&mut self, cancel: &AtomicBool) -> Result<Option<usize>> {
        #[cfg(test)]
        {
            self.work.replays += 1;
        }
        let mut children: Vec<_> = self.nodes.iter().map(|n| n.children).collect();
        let mut ready: VecDeque<_> = children
            .iter()
            .enumerate()
            .filter_map(|(i, &count)| (count == 0).then_some(i))
            .collect();
        let mut frontier = vec![false; self.nodes.len()];
        for &tip in &self.tips {
            frontier[tip] = true;
        }
        let mut count = self.tips.len();
        while let Some(index) = ready.pop_front() {
            check(cancel)?;
            let node = &self.nodes[index];
            if node.parents.is_empty() {
                // Both unread objects and real roots remain terminal obligations.
                // Dropping either could fabricate a join with another branch.
                continue;
            }
            frontier[index] = false;
            count -= 1;
            for &parent in &node.parents {
                if !frontier[parent] {
                    frontier[parent] = true;
                    count += 1;
                }
                children[parent] -= 1;
                if children[parent] == 0 {
                    ready.push_back(parent);
                }
            }
            if count == 1 {
                return Ok(frontier.iter().position(|&pending| pending));
            }
        }
        Ok(None)
    }
    fn finish(
        &mut self,
        boundary: Option<usize>,
        source: bool,
        cancel: &AtomicBool,
        read: &mut impl FnMut(&str) -> Result<Commit>,
    ) -> Result<Graph> {
        if let Some(index) = boundary
            && source
            && self.nodes[index].commit.is_none()
        {
            check(cancel)?;
            if self
                .failure
                .as_ref()
                .is_some_and(|(failed, _)| *failed == index)
            {
                return Err(self.failure.take().unwrap().1);
            }
            match read(&self.nodes[index].id) {
                Ok(commit) => self.retain(index, commit)?,
                Err(error) => {
                    self.failed.insert(index);
                    return Err(error);
                }
            }
        }
        // Re-rank only the retained interval, in recorded Git parent order.
        // Speculative older reads never enter source matching or affect ordering.
        let mut ranks = vec![usize::MAX; self.nodes.len()];
        let mut retained = Vec::new();
        let mut pending: Vec<_> = self.tips.iter().rev().copied().collect();
        while let Some(index) = pending.pop() {
            check(cancel)?;
            if Some(index) == boundary || ranks[index] != usize::MAX {
                continue;
            }
            ensure!(
                self.nodes[index].commit.is_some(),
                "Unfinished history parent graph"
            );
            ranks[index] = retained.len();
            retained.push(index);
            pending.extend(self.nodes[index].parents.iter().rev().copied());
        }
        let mut children = vec![0usize; self.nodes.len()];
        for &index in &retained {
            for &parent in &self.nodes[index].parents {
                if Some(parent) != boundary {
                    children[parent] += 1;
                }
            }
        }
        // DFS ranks are unique, so a rank tie (and its object-ID tie break) is
        // impossible. Discovery indices have no influence on the ready order.
        let mut ready: BTreeSet<_> = retained
            .iter()
            .filter(|&&i| children[i] == 0)
            .map(|&i| (ranks[i], i))
            .collect();
        let mut order = Vec::new();
        while let Some((_, index)) = ready.pop_first() {
            check(cancel)?;
            order.push(self.nodes[index].id.clone());
            for &parent in &self.nodes[index].parents {
                if Some(parent) == boundary {
                    continue;
                }
                children[parent] -= 1;
                if children[parent] == 0 {
                    ready.insert((ranks[parent], parent));
                }
            }
        }
        ensure!(order.len() == retained.len(), "Invalid cyclic Git ancestry");
        let mut commits = BTreeMap::new();
        for index in retained.into_iter().chain(boundary) {
            if let Some(commit) = self.nodes[index].commit.take() {
                commits.insert(commit.id.clone(), commit);
            }
        }
        Ok(Graph {
            commits,
            order,
            boundary: boundary.map(|i| self.nodes[i].id.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::btree_map::Entry;

    fn fixture(edges: &[(&str, &[&str])]) -> BTreeMap<String, Commit> {
        edges
            .iter()
            .map(|(id, parents)| {
                let commit = Commit {
                    id: (*id).into(),
                    parents: parents.iter().map(|p| (*p).into()).collect(),
                    tree: String::new(),
                    date: String::new(),
                    subject: String::new(),
                };
                (commit.id.clone(), commit)
            })
            .collect()
    }
    fn discover(
        commits: &BTreeMap<String, Commit>,
        tips: &[&str],
        source: bool,
    ) -> (Result<Graph>, Vec<String>) {
        let mut reads = Vec::new();
        let tips: Vec<_> = tips.iter().map(|id| (*id).into()).collect();
        let graph = Discovery::new(&tips)
            .unwrap()
            .run(&AtomicBool::new(false), source, |id| {
                reads.push(id.to_owned());
                commits
                    .get(id)
                    .cloned()
                    .with_context(|| format!("missing ancestor {id}"))
            });
        (graph, reads)
    }

    #[test]
    fn a_closed_diamond_reads_the_base_but_not_its_parent() {
        let commits = fixture(&[
            ("left", &["base"]),
            ("right", &["base"]),
            ("base", &["older"]),
        ]);
        let (graph, reads) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert_eq!(reads, ["left", "right", "base"]);
        assert_eq!(graph.order, ["left", "right"]);
        assert_eq!(graph.boundary.as_deref(), Some("base"));
        assert_eq!(graph.commits["base"].parents, ["older"]);
        assert_eq!(graph.bases(&["left".into(), "right".into()]), ["base"]);
        assert_eq!(
            graph.ancestors("left"),
            BTreeSet::from(["left".into(), "base".into()])
        );
        let mut complete = BTreeSet::new();
        graph.certify(&mut complete);
        assert!(
            complete.is_empty(),
            "A traversal cut is not complete ancestry"
        );
    }

    #[test]
    fn available_older_history_does_not_change_the_read_set() {
        let mut commits = fixture(&[
            ("left", &["base"]),
            ("right", &["base"]),
            ("base", &["older0"]),
        ]);
        for index in 0..4096 {
            let id = format!("older{index}");
            let parents = if index < 4095 {
                vec![format!("older{}", index + 1)]
            } else {
                Vec::new()
            };
            commits.insert(
                id.clone(),
                Commit {
                    id,
                    parents,
                    tree: String::new(),
                    date: String::new(),
                    subject: String::new(),
                },
            );
        }
        let (graph, reads) = discover(&commits, &["left", "right"], true);
        assert_eq!(graph.unwrap().order, ["left", "right"]);
        assert_eq!(reads, ["left", "right", "base"]);
    }

    #[test]
    fn metadata_does_not_require_the_boundary_object_but_source_does() {
        let commits = fixture(&[("left", &["base"]), ("right", &["base"])]);
        let (graph, reads) = discover(&commits, &["left", "right"], false);
        let graph = graph.unwrap();
        assert_eq!(reads, ["left", "right"]);
        assert_eq!(graph.boundary.as_deref(), Some("base"));
        assert!(!graph.commits.contains_key("base"));
        let (graph, _) = discover(&commits, &["left", "right"], true);
        assert!(
            graph
                .err()
                .unwrap()
                .to_string()
                .contains("missing ancestor base")
        );
    }

    #[test]
    fn unequal_arms_discard_failed_lookahead_only_below_the_proven_cut() {
        let commits = fixture(&[
            ("left", &["base"]),
            ("right", &["third"]),
            ("third", &["second"]),
            ("second", &["first"]),
            ("first", &["base"]),
            ("base", &["older"]),
        ]);
        let (graph, reads) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert!(reads.iter().any(|id| id == "older"));
        assert_eq!(graph.boundary.as_deref(), Some("base"));
        assert_eq!(graph.order, ["left", "right", "third", "second", "first"]);
        assert!(!graph.commits.contains_key("older"));
        assert!(!graph.ancestors("right").contains("older"));
    }

    #[test]
    fn a_shared_node_is_not_a_cut_when_a_parent_path_bypasses_it() {
        let commits = fixture(&[
            ("left", &["shared"]),
            ("right", &["shared", "join"]),
            ("shared", &["middle"]),
            ("middle", &["join"]),
            ("join", &["older"]),
        ]);
        let (graph, _) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert_eq!(graph.boundary.as_deref(), Some("join"));
        assert_eq!(graph.order, ["left", "right", "shared", "middle"]);
        assert_eq!(graph.bases(&["left".into(), "right".into()]), ["shared"]);
    }

    #[test]
    fn all_merge_bases_remain_visible_above_the_cut() {
        let commits = fixture(&[
            ("left", &["a", "b"]),
            ("right", &["b", "a"]),
            ("a", &["base"]),
            ("b", &["base"]),
            ("base", &["older"]),
        ]);
        let (graph, reads) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert_eq!(graph.order, ["left", "right", "a", "b"]);
        assert_eq!(graph.boundary.as_deref(), Some("base"));
        assert_eq!(graph.bases(&["left".into(), "right".into()]), ["a", "b"]);
        assert!(!reads.iter().any(|id| id == "older"));
    }

    #[test]
    fn speculative_forks_below_the_cut_need_not_coalesce() {
        let commits = fixture(&[
            ("left", &["base"]),
            ("right", &["second"]),
            ("second", &["first"]),
            ("first", &["base"]),
            ("base", &["old-left", "old-right"]),
        ]);
        for source in [false, true] {
            let (graph, reads) = discover(&commits, &["left", "right"], source);
            let graph = graph.unwrap();
            assert_eq!(
                reads,
                [
                    "left",
                    "right",
                    "base",
                    "second",
                    "old-left",
                    "old-right",
                    "first"
                ]
            );
            assert_eq!(graph.boundary.as_deref(), Some("base"));
            assert_eq!(graph.order, ["left", "right", "second", "first"]);
            assert_eq!(graph.commits["base"].parents, ["old-left", "old-right"]);
            assert_eq!(graph.commits.len(), 5);
            let mut complete = BTreeSet::new();
            graph.certify(&mut complete);
            assert!(complete.is_empty());
        }
    }

    #[test]
    fn a_shared_root_does_not_discharge_a_missing_bypass() {
        let commits = fixture(&[
            ("left", &["root"]),
            ("right", &["root", "missing"]),
            ("root", &[]),
        ]);
        for source in [false, true] {
            let (graph, reads) = discover(&commits, &["left", "right"], source);
            assert_eq!(reads, ["left", "right", "root", "missing"]);
            assert!(
                graph
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("missing ancestor missing")
            );
        }
    }

    #[test]
    fn duplicate_parent_edges_share_one_obligation() {
        let commits = fixture(&[
            ("left", &["base", "base"]),
            ("right", &["base", "base"]),
            ("base", &["older"]),
        ]);
        let (graph, reads) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert_eq!(reads, ["left", "right", "base"]);
        assert_eq!(graph.boundary.as_deref(), Some("base"));
        assert_eq!(graph.order, ["left", "right"]);
        assert_eq!(graph.commits["left"].parents, ["base", "base"]);
    }

    #[test]
    fn nearby_endpoints_stop_in_both_directions_and_deduplicate_tips() {
        let commits = fixture(&[("tip", &["parent"]), ("parent", &["older"])]);
        for tips in [["tip", "parent"], ["parent", "tip"]] {
            let (graph, reads) = discover(&commits, &tips, true);
            let graph = graph.unwrap();
            assert_eq!(graph.order, ["tip"]);
            assert_eq!(graph.boundary.as_deref(), Some("parent"));
            assert!(!reads.iter().any(|id| id == "older"));
        }
        let (graph, reads) = discover(&commits, &["tip", "tip"], true);
        let graph = graph.unwrap();
        assert!(graph.order.is_empty());
        assert_eq!(graph.boundary.as_deref(), Some("tip"));
        assert_eq!(reads, ["tip"]);
    }

    #[test]
    fn real_roots_do_not_close_another_branches_missing_ancestry() {
        let commits = fixture(&[("left", &[]), ("right", &["missing"])]);
        for source in [false, true] {
            let (graph, _) = discover(&commits, &["left", "right"], source);
            assert!(
                graph
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("missing ancestor missing")
            );
        }
        let commits = fixture(&[("left", &[]), ("right", &[])]);
        let (graph, _) = discover(&commits, &["left", "right"], true);
        let graph = graph.unwrap();
        assert!(graph.boundary.is_none());
        assert_eq!(graph.order, ["left", "right"]);
        let mut complete = BTreeSet::new();
        graph.certify(&mut complete);
        assert_eq!(
            complete,
            graph.commits.keys().cloned().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn cancellation_and_graph_budget_do_not_become_successful_cuts() {
        let mut discovery = Discovery::new(&["tip".into()]).unwrap();
        let result = discovery.run(&AtomicBool::new(true), false, |_| {
            panic!("A cancelled traversal must not read objects")
        });
        assert!(result.err().unwrap().to_string().contains("cancelled"));
        discovery.bytes = 128 * 1024 * 1024;
        assert!(
            discovery
                .node("parent")
                .unwrap_err()
                .to_string()
                .contains("memory bound")
        );
        let mut discovery = Discovery::new(&["left".into(), "right".into()]).unwrap();
        discovery.bytes = 128 * 1024 * 1024 - 63;
        let commits = fixture(&[("left", &["right"])]);
        let error = discovery
            .expand(0, commits["left"].clone(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("memory bound"));
        assert!(discovery.nodes.iter().all(|node| node.waiting.is_empty()));
    }

    fn shared_edges(length: usize, reverse: bool) -> BTreeMap<String, Commit> {
        let mut commits = fixture(&[("left", &["base"]), ("base", &["older"])]);
        for index in 0..length {
            let id = format!("right{index}");
            let mut parents = vec!["base".into()];
            if index + 1 < length {
                parents.push(format!("right{}", index + 1));
            }
            if reverse {
                parents.reverse();
            }
            commits.insert(
                id.clone(),
                Commit {
                    id,
                    parents,
                    tree: String::new(),
                    date: String::new(),
                    subject: String::new(),
                },
            );
        }
        commits
    }

    #[test]
    fn shared_edges_replay_once_with_linear_reduction_work() {
        for length in [64, 256, 1024] {
            for reverse in [false, true] {
                let commits = shared_edges(length, reverse);
                let mut discovery = Discovery::new(&["left".into(), "right0".into()]).unwrap();
                let mut reads = Vec::new();
                let graph = discovery
                    .run(&AtomicBool::new(false), true, |id| {
                        reads.push(id.to_owned());
                        commits.get(id).cloned().context("missing ancestor")
                    })
                    .unwrap();
                let order: Vec<_> = std::iter::once("left".to_owned())
                    .chain((0..length).map(|index| format!("right{index}")))
                    .collect();
                let expected_reads: Vec<_> = ["left", "right0", "base", "right1", "older"]
                    .into_iter()
                    .map(str::to_owned)
                    .chain((2..length).map(|index| format!("right{index}")))
                    .collect();
                assert_eq!(graph.order, order);
                assert_eq!(graph.boundary.as_deref(), Some("base"));
                assert_eq!(graph.commits["base"].parents, ["older"]);
                assert_eq!(graph.commits.len(), length + 2);
                assert_eq!(reads, expected_reads);
                // Structural work bounds, not machine-dependent timing ratios.
                assert_eq!(discovery.work.replays, 1);
                assert!(discovery.work.reductions <= discovery.nodes.len());
                assert!(discovery.work.transfers <= length, "{:?}", discovery.work);
            }
        }
    }

    #[test]
    #[ignore = "manual traversal scaling check"]
    fn shared_edge_traversal_scaling() {
        for length in [1000, 2000, 4000, 8000] {
            let commits = shared_edges(length, false);
            let started = std::time::Instant::now();
            let (graph, reads) = discover(&commits, &["left", "right0"], true);
            let elapsed = started.elapsed();
            let graph = graph.unwrap();
            assert_eq!(graph.order.len(), length + 1);
            assert_eq!(graph.boundary.as_deref(), Some("base"));
            eprintln!(
                "shared edges: {length}, reads: {}, elapsed: {elapsed:?}",
                reads.len()
            );
        }
    }

    // An intentionally simple full-graph oracle, independent of interval cuts.
    fn full_order(commits: &BTreeMap<String, Commit>, tips: &[&str]) -> Vec<String> {
        let mut ranks = BTreeMap::new();
        let mut pending: Vec<_> = tips.iter().rev().map(|id| (*id).to_owned()).collect();
        while let Some(id) = pending.pop() {
            let rank = ranks.len();
            if let Entry::Vacant(entry) = ranks.entry(id.clone()) {
                entry.insert(rank);
                pending.extend(commits[&id].parents.iter().rev().cloned());
            }
        }
        let mut remaining: BTreeSet<_> = ranks.keys().cloned().collect();
        let mut order = Vec::new();
        while !remaining.is_empty() {
            let next = remaining
                .iter()
                .filter(|id| {
                    !remaining
                        .iter()
                        .any(|child| commits[child].parents.contains(*id))
                })
                .min_by_key(|id| ranks[*id])
                .unwrap()
                .clone();
            remaining.remove(&next);
            order.push(next);
        }
        order
    }

    fn ordered_dag(mask: u32, reverse: bool) -> BTreeMap<String, Commit> {
        let mut commits = BTreeMap::new();
        let mut bit = 0;
        for child in 0..5 {
            let mut parents = Vec::new();
            for parent in 0..child {
                if mask & (1 << bit) != 0 {
                    parents.push(parent.to_string());
                }
                bit += 1;
            }
            if reverse {
                parents.reverse();
            }
            let id = child.to_string();
            commits.insert(
                id.clone(),
                Commit {
                    id,
                    parents,
                    tree: String::new(),
                    date: String::new(),
                    subject: String::new(),
                },
            );
        }
        commits
    }

    #[test]
    fn continuation_groups_match_replay_on_every_discovery_prefix() {
        let cancel = AtomicBool::new(false);
        let selections: &[&[&str]] = &[
            &["4", "3"],
            &["4", "0"],
            &["0", "4"],
            &["4", "3", "2"],
            &["2", "4", "3"],
        ];
        let gaps: &[&[&str]] = &[&[], &["0"], &["0", "2"]];
        for mask in 0..(1u32 << 10) {
            for reverse in [false, true] {
                let commits = ordered_dag(mask, reverse);
                for &tips in selections {
                    let selected: Vec<_> = tips.iter().map(|id| (*id).to_owned()).collect();
                    for &missing in gaps {
                        let mut discovery = Discovery::new(&selected).unwrap();
                        let mut next = 0;
                        while next < discovery.nodes.len() {
                            let id = &discovery.nodes[next].id;
                            if !missing.contains(&id.as_str()) {
                                let commit = commits[id].clone();
                                discovery.expand(next, commit, &cancel).unwrap();
                            }
                            // The original replay remains an independent oracle.
                            // Continue below its first cut to exercise lookahead,
                            // roots, unread tips and multi-parent continuations.
                            assert_eq!(
                                discovery.tip_components == 1,
                                discovery.cut(&cancel).unwrap().is_some(),
                                "{mask} {reverse} {tips:?} {missing:?} {next}"
                            );
                            next += 1;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn bounded_pages_and_bridges_match_full_order_on_small_dags() {
        // Every DAG on five ordered vertices, with both recorded parent orders.
        for mask in 0..(1u32 << 10) {
            for reverse in [false, true] {
                let commits = ordered_dag(mask, reverse);
                let mut order = Vec::new();
                let mut next = Some("4".to_owned());
                while let Some(id) = next {
                    order.push(id.clone());
                    let parents = &commits[&id].parents;
                    if parents.len() < 2 {
                        next = parents.first().cloned();
                    } else {
                        let tips: Vec<_> = parents.iter().map(String::as_str).collect();
                        let graph = discover(&commits, &tips, false).0.unwrap();
                        order.extend(graph.order);
                        next = graph.boundary;
                    }
                }
                assert_eq!(order, full_order(&commits, &["4"]), "{mask} {reverse}");
                for tips in [["4", "3"], ["4", "0"], ["0", "4"]] {
                    let graph = discover(&commits, &tips, true).0.unwrap();
                    let full = full_order(&commits, &tips);
                    assert_eq!(graph.order, full[..graph.order.len()], "{mask} {reverse}");
                    if let Some(boundary) = &graph.boundary {
                        assert_eq!(boundary, &full[graph.order.len()]);
                        // Remove every object below the cut, including any
                        // speculative lookahead. The finalized prefix is fixed.
                        let bounded = discover(&graph.commits, &tips, true).0.unwrap();
                        assert_eq!(bounded.order, graph.order, "{mask} {reverse}");
                        assert_eq!(bounded.boundary, graph.boundary, "{mask} {reverse}");
                    }
                }
            }
        }
    }
}
