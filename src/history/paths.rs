//! Directory coverage is stronger evidence than an individual file origin.
//! Infer scopes from committed inventories and already-supported links only;
//! this pass must never hydrate additional source to guess a directory name.
use super::{
    matching::{Link, Relation, directory_prefix},
    query, regions,
};
use crate::repository::history::{Tree, check};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
};

pub(super) const UNRESOLVED: &str =
    "unresolved directory scope; following supported file relationships";
const SCOPE_WORK: usize = 65_536;

struct Work<'a> {
    cancel: &'a AtomicBool,
    remaining: usize,
    exhausted: bool,
}
impl Work<'_> {
    fn step(&mut self) -> Result<bool> {
        check(self.cancel)?;
        if self.remaining == 0 {
            self.exhausted = true;
            return Ok(false);
        }
        self.remaining -= 1;
        Ok(true)
    }
}

// Preserve arbitrary negative globs in the coordinate system where they
// applied. A literal suffix can be rewritten directly in State.paths; a glob
// spanning the moved prefix cannot safely be rewritten by string substitution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(super) struct Exclusion {
    root: String,
    original: String,
    pattern: String,
}
impl Exclusion {
    pub fn heap_bytes(&self) -> usize {
        self.root.capacity() + self.original.capacity() + self.pattern.capacity()
    }
    fn matches(&self, path: &str, pattern: &query::PathPattern<'_>) -> bool {
        let relative = if path == self.root {
            ""
        } else if let Some(relative) = suffix(path, &self.root) {
            relative
        } else {
            return false;
        };
        pattern.matches(&format!("{}{relative}", self.original))
    }
    fn rebase(&self, from: &str, to: &str) -> Option<Self> {
        let (root, original) = if self.root == from {
            (to.to_owned(), self.original.clone())
        } else if let Some(relative) = suffix(&self.root, from) {
            (format!("{to}{relative}"), self.original.clone())
        } else if let Some(relative) = suffix(from, &self.root) {
            (to.to_owned(), format!("{}{relative}", self.original))
        } else {
            return None;
        };
        Some(Self {
            root,
            original,
            pattern: self.pattern.clone(),
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Selection<'a> {
    pub patterns: &'a [String],
    pub exclusions: &'a [Exclusion],
}
impl<'a> Selection<'a> {
    pub fn compile(self, cancel: &AtomicBool) -> Result<CompiledSelection<'a>> {
        check(cancel)?;
        let mut compiled = CompiledSelection {
            positive: Vec::new(),
            negative: Vec::new(),
            exclusions: Vec::new(),
        };
        for raw in self.patterns {
            check(cancel)?;
            if let Some(pattern) = raw.strip_prefix("path:") {
                compiled.positive.push(query::PathPattern::new(pattern));
            } else if let Some(pattern) = raw.strip_prefix("-path:") {
                compiled.negative.push(query::PathPattern::new(pattern));
            }
        }
        for exclusion in self.exclusions {
            check(cancel)?;
            compiled
                .exclusions
                .push((exclusion, query::PathPattern::new(&exclusion.pattern)));
        }
        Ok(compiled)
    }
}

// Scan-local only: retained states keep strings for equality and memory accounting.
pub(super) struct CompiledSelection<'a> {
    positive: Vec<query::PathPattern<'a>>,
    negative: Vec<query::PathPattern<'a>>,
    exclusions: Vec<(&'a Exclusion, query::PathPattern<'a>)>,
}
impl CompiledSelection<'_> {
    pub fn excluded(&self, path: &str) -> bool {
        self.negative.iter().any(|p| p.matches(path))
            || self.exclusions.iter().any(|(e, p)| e.matches(path, p))
    }
    pub fn matches(&self, path: &str) -> bool {
        !self.excluded(path) && self.positive.iter().any(|p| p.matches(path))
    }
}

pub(super) struct Mapping {
    pub patterns: Vec<String>,
    pub exclusions: Vec<Exclusion>,
    pub unresolved: bool,
}

fn suffix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix).filter(|s| s.starts_with('/'))
}

// An edited move needs independent, one-to-one file evidence and complete
// relative membership on both sides. Unmatched members might be splits rather
// than deletions: retain their supported links, not an invented whole scope.
fn consistent(
    from: &Tree,
    to: &Tree,
    prefix: &str,
    destination: &str,
    selection: &CompiledSelection<'_>,
    links: &BTreeMap<&str, Option<&Link>>,
    work: &mut Work<'_>,
) -> Result<bool> {
    let mut before = BTreeSet::new();
    let mut after = BTreeSet::new();
    for (tree, root, members) in [(from, prefix, &mut before), (to, destination, &mut after)] {
        for (path, entry) in tree
            .range(format!("{root}/")..)
            .take_while(|(path, _)| suffix(path, root).is_some())
        {
            if !work.step()? {
                return Ok(false);
            }
            if matches!(entry.mode, 0o100644 | 0o100755) && regions::language(path).is_some() {
                members.insert(suffix(path, root).unwrap());
            }
        }
    }
    if before != after {
        return Ok(false);
    }
    let mut origins = BTreeSet::new();
    let mut successors = BTreeSet::new();
    for relative in before {
        if !work.step()? {
            return Ok(false);
        }
        let path = format!("{prefix}{relative}");
        if !selection.matches(&path) {
            continue;
        }
        let Some(Some(link)) = links.get(path.as_str()) else {
            return Ok(false);
        };
        if link.relation != Relation::Move
            || link.absence_alternative
            || link.from.kind != "file"
            || link.to.kind != "file"
            || link.from.weak
            || link.to.weak
            || suffix(&link.to.path, destination) != Some(relative)
        {
            return Ok(false);
        }
        // Repeated copies of a single object are not independent witnesses.
        origins.insert(&link.from.blob);
        successors.insert(&link.to.blob);
    }
    Ok(origins.len() >= 2 && successors.len() >= 2)
}

pub(super) fn map(
    from: &Tree,
    to: &Tree,
    selection: Selection<'_>,
    links: &[Link],
    cancel: &AtomicBool,
) -> Result<Mapping> {
    let selected = selection.compile(cancel)?;
    let mut work = Work {
        cancel,
        remaining: SCOPE_WORK,
        exhausted: false,
    };
    let mut mapped = Mapping {
        patterns: selection.patterns.to_vec(),
        exclusions: selection.exclusions.to_vec(),
        unresolved: false,
    };
    let mut unique = BTreeMap::new();
    let mut movements = Vec::new();
    for link in links {
        check(cancel)?;
        if link.from.path == link.to.path {
            continue;
        }
        if !work.step()? {
            mapped.unresolved = true;
            return Ok(mapped);
        }
        movements.push(link);
        unique
            .entry(link.from.path.as_str())
            .and_modify(|value| *value = None)
            .or_insert(Some(link));
    }
    for raw in selection
        .patterns
        .iter()
        .filter_map(|p| p.strip_prefix("path:"))
    {
        if !work.step()? {
            break;
        }
        let prefix = directory_prefix(raw);
        let Some(entry) = from.get(prefix).filter(|e| e.mode == 0o40000) else {
            continue;
        };
        // Retained roots are imports/partial moves, not relocations. This also
        // prevents a singleton subtree identity from promoting a copied file.
        if to.contains_key(prefix) {
            continue;
        }
        let mut candidates = BTreeSet::new();
        let mut evidence = false;
        for link in &movements {
            if !work.step()? {
                break;
            }
            let Some(relative) = suffix(&link.from.path, prefix) else {
                continue;
            };
            evidence = true;
            if let Some(destination) = link.to.path.strip_suffix(relative)
                && !destination.is_empty()
                && !from.contains_key(destination)
                && to.get(destination).is_some_and(|e| e.mode == 0o40000)
            {
                candidates.insert(destination);
            }
        }
        // For a vacated root, exact file matching already enumerates every
        // identical subtree containing that member. Scan unrelated inventory
        // only when no member link can supply those candidate roots (for
        // example, a glob whose members were all deleted before the anchor).
        if !evidence {
            for (path, other) in to {
                if !work.step()? {
                    break;
                }
                if other.mode == 0o40000 && other.id == entry.id && !from.contains_key(path) {
                    candidates.insert(path.as_str());
                }
            }
        }
        if work.exhausted {
            break;
        }
        let mut established = None;
        for destination in candidates {
            let supported = to[destination].id == entry.id
                || consistent(from, to, prefix, destination, &selected, &unique, &mut work)?;
            if supported && established.replace(destination).is_some() {
                established = None;
                mapped.unresolved = true;
                break; // Competing whole locations do not establish either one.
            }
        }
        if work.exhausted {
            break; // Never publish a partially verified scope.
        }
        if let Some(destination) = established {
            let mut additions = vec![format!("path:{destination}{}", &raw[prefix.len()..])];
            let mut exclusions = Vec::new();
            for excluded in selection
                .patterns
                .iter()
                .filter_map(|p| p.strip_prefix("-path:"))
            {
                if !work.step()? {
                    break;
                }
                if excluded == prefix || suffix(prefix, excluded).is_some() {
                    additions.push(format!("-path:{destination}"));
                } else if let Some(relative) = suffix(excluded, prefix) {
                    additions.push(format!("-path:{destination}{relative}"));
                } else {
                    exclusions.push(Exclusion {
                        root: destination.into(),
                        original: prefix.into(),
                        pattern: excluded.into(),
                    });
                }
            }
            for excluded in selection.exclusions {
                if !work.step()? {
                    break;
                }
                if let Some(excluded) = excluded.rebase(prefix, destination) {
                    exclusions.push(excluded);
                }
            }
            if work.exhausted {
                break; // A positive scope must never outlive its exclusions.
            }
            mapped.patterns.extend(additions);
            mapped.exclusions.extend(exclusions);
        }
    }
    drop(selected);
    mapped.patterns.sort();
    mapped.patterns.dedup();
    mapped.exclusions.sort();
    mapped.exclusions.dedup();
    mapped.unresolved |= work.exhausted;
    let selection = Selection {
        patterns: &mapped.patterns,
        exclusions: &mapped.exclusions,
    }
    .compile(cancel)?;
    for link in movements {
        if !work.step()? {
            mapped.unresolved = true;
            break;
        }
        // Evaluate every relationship, even after another scope was resolved.
        // Moves inside an existing scope and explicitly excluded endpoints do
        // not leave an unknown directory location to recover.
        mapped.unresolved |=
            !selection.excluded(&link.to.path) && !selection.matches(&link.to.path);
    }
    drop(selection);
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{history::regions::Region, repository::history::Entry};
    use std::sync::atomic::Ordering;

    fn tree(root: &str, id: &str, files: &[(&str, &str)]) -> Tree {
        let mut tree = Tree::from([(
            root.into(),
            Entry {
                id: id.into(),
                mode: 0o40000,
            },
        )]);
        for (path, blob) in files {
            tree.insert(
                format!("{root}/{path}"),
                Entry {
                    id: (*blob).into(),
                    mode: 0o100644,
                },
            );
        }
        tree
    }
    fn link(from: &str, before: &str, to: &str, after: &str) -> Link {
        Link {
            from: Region::whole(from, before, 0o100644),
            to: Region::whole(to, after, 0o100644),
            relation: Relation::Move,
            uncertain: before != after,
            absence_alternative: false,
            reason: "file content+path",
        }
    }
    fn selection(patterns: &[String]) -> Selection<'_> {
        Selection {
            patterns,
            exclusions: &[],
        }
    }

    #[test]
    fn compiled_selection_keeps_positive_union_and_negative_precedence() {
        let patterns = [
            "path:src/",
            "path:tests/**",
            "-path:src/private",
            "-path:**/skip.rs",
        ]
        .map(String::from);
        let cancel = AtomicBool::new(false);
        let compiled = selection(&patterns).compile(&cancel).unwrap();
        for (path, matches, excluded) in [
            ("src", true, false),
            ("src/lib.rs", true, false),
            ("tests/a.rs", true, false),
            ("src/private", false, true),
            ("src/private/a.rs", false, true),
            ("tests/skip.rs", false, true),
            ("src/private-other/a.rs", true, false),
            ("other/a.rs", false, false),
        ] {
            assert_eq!(compiled.matches(path), matches, "{path}");
            assert_eq!(compiled.excluded(path), excluded, "{path}");
        }
        assert!(
            !selection(&[])
                .compile(&cancel)
                .unwrap()
                .matches("src/lib.rs")
        );
    }

    #[test]
    fn edited_scopes_and_exclusions_are_direction_independent() {
        for (from, to) in [("new", "old"), ("old", "new")] {
            let a = tree(
                from,
                "edited",
                &[("a.rs", "a2"), ("b.rs", "b"), ("private/p.rs", "p2")],
            );
            let b = tree(
                to,
                "original",
                &[("a.rs", "a1"), ("b.rs", "b"), ("private/p.rs", "p1")],
            );
            let links = [
                link(&format!("{from}/a.rs"), "a2", &format!("{to}/a.rs"), "a1"),
                link(&format!("{from}/b.rs"), "b", &format!("{to}/b.rs"), "b"),
            ];
            for excluded in [
                format!("{from}/private/**"),
                format!("{from}*/private/**"),
                "**/private/**".into(),
            ] {
                let patterns = [format!("path:{from}/**"), format!("-path:{excluded}")];
                let mapped = map(
                    &a,
                    &b,
                    selection(&patterns),
                    &links,
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert!(!mapped.unresolved);
                assert!(mapped.patterns.contains(&format!("path:{to}/**")));
                let selected = Selection {
                    patterns: &mapped.patterns,
                    exclusions: &mapped.exclusions,
                }
                .compile(&AtomicBool::new(false))
                .unwrap();
                assert!(selected.matches(&format!("{to}/a.rs")));
                assert!(selected.excluded(&format!("{to}/private/deleted.rs")));
            }
        }
    }

    #[test]
    fn scope_proofs_require_independent_unambiguous_and_complete_membership() {
        let patterns = ["path:new/**".into()];
        let cancel = AtomicBool::new(false);
        for kind in [
            "split",
            "retained",
            "ambiguous",
            "optional",
            "one object",
            "one member",
        ] {
            let mut a = tree("new", "edited", &[("a.rs", "a2"), ("b.rs", "b")]);
            let mut b = tree("old", "original", &[("a.rs", "a1"), ("b.rs", "b")]);
            let mut links = vec![
                link("new/a.rs", "a2", "old/a.rs", "a1"),
                link("new/b.rs", "b", "old/b.rs", "b"),
            ];
            match kind {
                "split" => {
                    b.extend(tree("old/other", "other", &[("c.rs", "c")]));
                }
                "retained" => {
                    a.extend(tree("old", "remaining", &[("c.rs", "c")]));
                }
                "ambiguous" => {
                    b.extend(tree("other", "other", &[("a.rs", "a1")]));
                    links.push(link("new/a.rs", "a2", "other/a.rs", "a1"));
                }
                "optional" => {
                    links[0].absence_alternative = true;
                }
                "one object" => {
                    a.get_mut("new/b.rs").unwrap().id = "a2".into();
                    b.get_mut("old/b.rs").unwrap().id = "a1".into();
                    links[1] = link("new/b.rs", "a2", "old/b.rs", "a1");
                }
                _ => {
                    a.remove("new/b.rs");
                    b.remove("old/b.rs");
                    links.pop();
                }
            }
            let mapped = map(&a, &b, selection(&patterns), &links, &cancel).unwrap();
            assert_eq!(mapped.patterns, patterns, "{kind}");
            assert!(mapped.unresolved, "{kind}");
        }
    }

    #[test]
    fn exact_singleton_moves_but_not_copies_or_competing_trees_establish_scope() {
        let a = tree("new", "same", &[("a.rs", "a")]);
        let b = tree("old", "same", &[("a.rs", "a")]);
        let patterns = ["path:new/**".into()];
        let links = [link("new/a.rs", "a", "old/a.rs", "a")];
        let cancel = AtomicBool::new(false);
        let mapped = map(&a, &b, selection(&patterns), &links, &cancel).unwrap();
        assert!(mapped.patterns.contains(&"path:old/**".into()));
        assert!(!mapped.unresolved);
        let mut copied = a.clone();
        copied.extend(b.clone());
        let mapped = map(&copied, &b, selection(&patterns), &links, &cancel).unwrap();
        assert_eq!(mapped.patterns, patterns);
        assert!(mapped.unresolved);
        let mut competing = b;
        competing.extend(tree("other", "same", &[("a.rs", "a")]));
        // Even a currently empty glob selection cannot certify an ambiguous root.
        let mapped = map(&a, &competing, selection(&patterns), &[], &cancel).unwrap();
        assert_eq!(mapped.patterns, patterns);
        assert!(mapped.unresolved);
    }

    #[test]
    fn internal_file_moves_do_not_make_a_known_directory_unresolved() {
        let a = tree("src", "before", &[("a.rs", "a")]);
        let b = tree("src", "after", &[("b.rs", "a")]);
        let patterns = ["path:src/**".into()];
        let links = [link("src/a.rs", "a", "src/b.rs", "a")];
        let mapped = map(
            &a,
            &b,
            selection(&patterns),
            &links,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!mapped.unresolved);
        assert_eq!(mapped.patterns, patterns);
    }

    #[test]
    fn exclusion_rebasing_handles_ancestors_descendants_and_path_boundaries() {
        let excluded = Exclusion {
            root: "old/sub".into(),
            original: "new/sub".into(),
            pattern: "new*/sub/private/**".into(),
        };
        let parent = excluded.rebase("old", "older").unwrap();
        let pattern = query::PathPattern::new(&parent.pattern);
        assert!(parent.matches("older/sub/private/deleted.rs", &pattern));
        assert!(!parent.matches("older/sub/public.rs", &pattern));
        assert!(excluded.rebase("oldish", "elsewhere").is_none());
        let child = parent.rebase("older/sub/private", "archive").unwrap();
        let pattern = query::PathPattern::new(&child.pattern);
        assert!(child.matches("archive/deleted.rs", &pattern));
        assert!(!child.matches("archive-other/deleted.rs", &pattern));
        assert!(
            child.heap_bytes() >= child.root.len() + child.original.len() + child.pattern.len()
        );
    }

    #[test]
    fn directory_work_is_bounded_cancellable_and_never_publishes_partial_proofs() {
        let cancel = AtomicBool::new(false);
        let mut work = Work {
            cancel: &cancel,
            remaining: 1,
            exhausted: false,
        };
        assert!(work.step().unwrap());
        assert!(!work.step().unwrap());
        assert!(work.exhausted);
        cancel.store(true, Ordering::Release);
        assert!(
            work.step()
                .unwrap_err()
                .to_string()
                .contains("history cancelled")
        );

        cancel.store(false, Ordering::Release);
        let a = tree("new", "same", &[("a.rs", "a")]);
        let mut b = tree("old", "same", &[("a.rs", "a")]);
        for i in 0..SCOPE_WORK {
            b.insert(
                format!("irrelevant/{i}.bin"),
                Entry {
                    id: "binary".into(),
                    mode: 0o100644,
                },
            );
        }
        let patterns = ["path:new/**".into()];
        let links = [link("new/a.rs", "a", "old/a.rs", "a")];
        let mapped = map(&a, &b, selection(&patterns), &links, &cancel).unwrap();
        assert!(mapped.patterns.contains(&"path:old/**".into()));
        assert!(!mapped.unresolved);
        let mapped = map(&a, &b, selection(&patterns), &[], &cancel).unwrap();
        assert_eq!(mapped.patterns, patterns);
        assert!(mapped.unresolved);
        cancel.store(true, Ordering::Release);
        assert!(map(&a, &b, selection(&patterns), &links, &cancel).is_err());
        assert!(selection(&patterns).compile(&cancel).is_err());
        assert!(selection(&[]).compile(&cancel).is_err());
    }
}
