use super::{
    regions::{Region, Sources},
    sequence,
};
use crate::repository::history::Store;
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, fmt::Write};

pub(super) const PAGE_BYTES: usize = 24 * 1024;
pub(super) const CANONICAL_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum FileChange {
    Move,
    Copy,
    ReverseCopy,
}

/// Whole-file pairs are established by the caller, before rendering can erase
/// the inventory or lineage evidence that distinguishes a move from a copy.
pub(super) struct FilePair {
    pub old: usize,
    pub new: usize,
    pub change: FileChange,
    pub uncertain: bool,
}

/// Metadata labels are not part of the canonical source stream.
pub(super) fn label(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [label shortened]", &text[..end])
}

fn range(start: usize, length: usize) -> String {
    if length == 1 {
        start.to_string()
    } else {
        format!(
            "{},{}",
            if length == 0 {
                start.saturating_sub(1)
            } else {
                start
            },
            length
        )
    }
}
fn line(out: &mut String, prefix: char, text: &str) {
    out.push(prefix);
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}

pub(super) fn unified(
    old: &str,
    new: &str,
    old_start: usize,
    new_start: usize,
    old_path: &str,
    new_path: &str,
) -> String {
    let a: Vec<_> = old.split_inclusive('\n').collect();
    let b: Vec<_> = new.split_inclusive('\n').collect();
    let edits = sequence::edits(&a, &b);
    if edits.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {old_path}\n+++ {new_path}\n");
    let mut i = 0;
    while i < edits.len() {
        let first = i;
        while i + 1 < edits.len() && edits[i + 1].old.start.saturating_sub(edits[i].old.end) <= 6 {
            i += 1;
        }
        let before = edits[first].old.start.min(edits[first].new.start).min(3);
        let after = (a.len() - edits[i].old.end)
            .min(b.len() - edits[i].new.end)
            .min(3);
        let ar = edits[first].old.start - before..edits[i].old.end + after;
        let br = edits[first].new.start - before..edits[i].new.end + after;
        writeln!(
            out,
            "@@ -{} +{} @@",
            range(old_start + ar.start, ar.len()),
            range(new_start + br.start, br.len())
        )
        .unwrap();
        let mut cursor = ar.start;
        for edit in &edits[first..=i] {
            for text in &a[cursor..edit.old.start] {
                line(&mut out, ' ', text);
            }
            for text in &a[edit.old.clone()] {
                line(&mut out, '-', text);
            }
            for text in &b[edit.new.clone()] {
                line(&mut out, '+', text);
            }
            cursor = edit.old.end;
        }
        for text in &a[cursor..ar.end] {
            line(&mut out, ' ', text);
        }
        i += 1;
    }
    out
}

pub(super) fn counts(old: &str, new: &str) -> (usize, usize) {
    let a: Vec<_> = old.split_inclusive('\n').collect();
    let b: Vec<_> = new.split_inclusive('\n').collect();
    sequence::edits(&a, &b)
        .iter()
        .fold((0, 0), |(plus, minus), e| {
            (plus + e.new.len(), minus + e.old.len())
        })
}

pub(super) fn comparison(
    store: &mut Store,
    sources: &mut Sources,
    old: &[Region],
    new: &[Region],
    files: &[FilePair],
    notes: &[String],
) -> Result<String> {
    let mut out = String::new();
    for note in notes {
        writeln!(out, "# {note}").unwrap();
    }
    // Whole files use path continuity or explicit evidence. Selected regions
    // retain their context pairing; unmatched occurrences stay independent.
    let mut file_paths = BTreeMap::<&str, Vec<usize>>::new();
    for (i, region) in old.iter().enumerate().filter(|(_, r)| r.kind == "file") {
        file_paths.entry(&region.path).or_default().push(i);
    }
    let mut file_pairs = vec![None; new.len()];
    for pair in files {
        file_pairs[pair.new] = Some(pair);
    }
    let mut consumed = vec![false; old.len()];
    for (j, current) in new.iter().enumerate() {
        let file = file_pairs[j];
        let rank = |i: usize| {
            let r = &old[i];
            (
                usize::from(r.path == current.path) * 4
                    + usize::from(r.name == current.name) * 2
                    + usize::from(r.blob == current.blob),
                std::cmp::Reverse(r.key()),
            )
        };
        let index = if current.kind == "file" {
            file_paths
                .get(current.path.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| !consumed[i])
                .max_by_key(|&i| rank(i))
                .or_else(|| file.map(|pair| pair.old).filter(|&i| !consumed[i]))
        } else {
            old.iter()
                .enumerate()
                .filter(|(i, r)| {
                    !consumed[*i]
                        && (r.path == current.path
                            || r.kind != "file"
                                && (r.blob == current.blob
                                    || old.len() == 1 && new.len() == 1
                                    || r.name == current.name))
                })
                .map(|(i, _)| i)
                .max_by_key(|&i| rank(i))
        };
        let previous = index.map(|i| {
            consumed[i] = true;
            &old[i]
        });
        append(store, sources, &mut out, previous, Some(current), file)?;
    }
    for (i, previous) in old.iter().enumerate() {
        if !consumed[i] {
            append(store, sources, &mut out, Some(previous), None, None)?;
        }
    }
    if out.is_empty() {
        out.push_str("No changes.\n");
    }
    Ok(out)
}
fn append(
    store: &mut Store,
    sources: &mut Sources,
    out: &mut String,
    old: Option<&Region>,
    new: Option<&Region>,
    file: Option<&FilePair>,
) -> Result<()> {
    if old.zip(new).is_some_and(|(a, b)| {
        a.path == b.path
            && a.blob == b.blob
            && a.span == b.span
            && a.kind == b.kind
            && a.name == b.name
    }) {
        return Ok(());
    }
    // Only this already-selected, non-identical pair is needed next. Do not
    // hydrate the remaining comparison ahead of rendering or its memory bound.
    let blobs: Vec<_> = old.into_iter().chain(new).map(|r| r.blob.clone()).collect();
    store.prefetch(&blobs, "blob")?;
    let mut read = |region: Option<&Region>| -> Result<(String, usize)> {
        if let Some(region) = region {
            let source = sources.get(store, &region.path, &region.blob)?;
            Ok((
                source.text[region.range(&source)].to_owned(),
                crate::model::position(&source.text, region.span.start).0,
            ))
        } else {
            Ok((String::new(), 1))
        }
    };
    let (a, astart) = read(old)?;
    let (b, bstart) = read(new)?;
    let old_name = old.map_or("/dev/null".into(), |r| format!("a/{}", r.path));
    let new_name = new.map_or("/dev/null".into(), |r| format!("b/{}", r.path));
    let changed_identity = old
        .zip(new)
        .is_some_and(|(a, b)| a.path != b.path || a.name != b.name);
    let changed_existence = old.is_some() != new.is_some();
    if a != b || changed_identity || changed_existence {
        writeln!(out, "diff --git {old_name} {new_name}").unwrap();
        if let (Some(a), Some(b)) = (old, new) {
            if a.path != b.path {
                let (event, from, to, suffix) = match file.map(|pair| pair.change) {
                    Some(FileChange::Copy) => ("copy", &a.path, &b.path, ""),
                    Some(FileChange::ReverseCopy) => {
                        ("copy", &b.path, &a.path, " (reverse comparison)")
                    }
                    _ => ("move", &a.path, &b.path, ""),
                };
                writeln!(
                    out,
                    "# {}{event} {from} → {to}{suffix}",
                    if file.is_some_and(|pair| pair.uncertain) {
                        "~"
                    } else {
                        ""
                    },
                )
                .unwrap();
            }
            if a.name != b.name {
                writeln!(out, "# rename {} → {}", a.signature(), b.signature()).unwrap();
            }
        }
        for region in old.into_iter().chain(new) {
            if region.fragment {
                writeln!(
                    out,
                    "# fragment {} bytes {}..{}",
                    region.path, region.span.start, region.span.end
                )
                .unwrap();
            }
        }
        if changed_existence && a == b {
            writeln!(out, "--- {old_name}\n+++ {new_name}").unwrap();
        } else {
            out.push_str(&unified(&a, &b, astart, bstart, &old_name, &new_name));
        }
    }
    ensure!(
        out.len() <= CANONICAL_LIMIT,
        "canonical diff exceeds history memory bound"
    );
    Ok(())
}

pub(super) fn page(header: &str, canonical: &str, offset: usize) -> Result<String> {
    ensure!(
        offset <= canonical.len(),
        "Offset is beyond the canonical diff"
    );
    ensure!(
        canonical.is_char_boundary(offset),
        "Offset is inside a UTF-8 character"
    );
    let budget = PAGE_BYTES.saturating_sub(header.len() + 256);
    ensure!(
        budget >= 256,
        "History page header exceeds the response budget"
    );
    let mut end = offset.saturating_add(budget).min(canonical.len());
    while !canonical.is_char_boundary(end) {
        end -= 1;
    }
    if end < canonical.len() {
        let window = &canonical[offset..end];
        for separator in ["\ndiff --git ", "\n@@ ", "\n"] {
            if let Some(i) = window.rfind(separator) {
                let candidate = offset + i + 1;
                if candidate > offset {
                    end = candidate;
                    break;
                }
            }
        }
    }
    let fragment = offset > 0 && !canonical[..offset].ends_with('\n')
        || end < canonical.len() && !canonical[..end].ends_with('\n');
    let next = if end < canonical.len() {
        format!("more offset:{end}")
    } else {
        "end".into()
    };
    Ok(format!(
        "{header}\nbytes {offset}..{end}/{}{}\n{next}\n\n{}",
        canonical.len(),
        if fragment { " fragment" } else { "" },
        &canonical[offset..end]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_reconstruct_utf8_even_when_one_line_exceeds_the_budget() {
        let canonical = format!("--- a/x\n+++ b/x\n+{}\n", "λ🦀".repeat(PAGE_BYTES));
        let mut offset = 0;
        let mut rebuilt = String::new();
        loop {
            let page = page("at pinned", &canonical, offset).unwrap();
            let (header, body) = page.split_once("\n\n").unwrap();
            rebuilt.push_str(body);
            let Some(next) = header.lines().find_map(|s| s.strip_prefix("more offset:")) else {
                break;
            };
            let next = next.parse().unwrap();
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(canonical, rebuilt);
        let inside = canonical.find('λ').unwrap() + 1;
        assert!(page("at pinned", &canonical, inside).is_err());
    }
    #[test]
    fn whitespace_and_missing_final_newlines_survive() {
        let result = unified("x = 1;", "x  = 1;\n", 10, 20, "a/x", "b/x");
        assert!(result.contains("@@ -10 +20 @@"));
        assert!(result.contains("-x = 1;\n\\ No newline at end of file"));
        assert!(result.contains("+x  = 1;\n"));
    }
}
