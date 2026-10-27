//! Complete passage boundaries and evidence across source revisions.
use super::{TOKENS, has_distinctive_tokens, has_distinctive_words, tokens, words};
use crate::history::sequence;
use anyhow::{Result, ensure};
use std::{collections::VecDeque, ops::Range, sync::atomic::AtomicBool};

/// Unlike declaration overlap, passage projection must account for every byte
/// between both selected boundaries. A failed projection is not absence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::history) enum TextRange {
    Present(Range<usize>),
    Absent,
    Unresolved,
}

pub(in crate::history) fn text_range(from: &str, to: &str, range: Range<usize>) -> TextRange {
    if from == to {
        return TextRange::Present(range);
    }
    let a: Vec<_> = from.split_inclusive('\n').collect();
    let b: Vec<_> = to.split_inclusive('\n').collect();
    let edits = sequence::edits(&a, &b);
    let offsets = |lines: &[&str]| {
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        offsets.push(0);
        for line in lines {
            offsets.push(offsets.last().unwrap() + line.len());
        }
        offsets
    };
    let ao = offsets(&a);
    let bo = offsets(&b);
    let mut refine = true;
    if let (Ok(start), Ok(end)) = (ao.binary_search(&range.start), ao.binary_search(&range.end)) {
        // Cuts inside replacements have no line counterpart. A resized
        // replacement touching a cut gives only an envelope: adjacent removed
        // blank lines or another occurrence need not belong to this passage.
        refine = edits.iter().any(|edit| {
            !edit.old.is_empty()
                && !edit.new.is_empty()
                && (edit.old.start < start && start < edit.old.end
                    || edit.old.start < end && end < edit.old.end
                    || edit.old.len() != edit.new.len()
                        && (start == edit.old.start || end == edit.old.end))
        });
        if !refine && let Some(mapped) = project_edits(a.len(), b.len(), &edits, start..end) {
            return if mapped.is_empty() {
                TextRange::Absent
            } else {
                TextRange::Present(bo[mapped.start]..bo[mapped.end])
            };
        }
    }

    // Reflow can place a line boundary inside a replacement hunk. Refine it
    // with tokens, but punctuation shared by unrelated lines is not evidence.
    let at = tokens(from);
    let bt = tokens(to);
    let aw = words(from, &at);
    let bw = words(to, &bt);
    let start = at.partition_point(|r| r.end <= range.start);
    let end = at.partition_point(|r| r.start < range.end);
    if start == end || at[start].start < range.start || at[end - 1].end > range.end {
        return TextRange::Unresolved;
    }
    let shared = sequence::matches(&aw, &bw);
    let words = || {
        shared
            .iter()
            .filter(|(i, _)| (start..end).contains(i))
            .map(|&(i, _)| aw[i])
    };
    let Some(mapped) = project_units(&aw, &bw, start..end) else {
        return TextRange::Unresolved;
    };
    if mapped.is_empty() {
        return TextRange::Absent;
    }
    let span = text_edges(
        to,
        bt[mapped.start].start..bt[mapped.end - 1].end,
        blank_edges(&from[range.clone()]),
    );
    let whole = !refine
        && range == (0..from.len())
        && span == (0..to.len())
        && shared.len() * 2 >= aw.len().max(bw.len());
    let contextual = has_distinctive_words(words(), 1)
        && (whole
            || TextContext::new(from, &range)
                .and_then(|context| context.range(to))
                .is_some_and(|bounds| bounds.start <= span.start && span.end <= bounds.end));
    if !has_distinctive_words(words(), 2) && !contextual {
        return TextRange::Unresolved;
    }
    // A contraction and editing one of several old copies can align alike.
    // Token refinement must corroborate the whole resized passage, not just
    // recreate the same unsupported line envelope with finer coordinates.
    if refine
        && mapped.len() != end - start
        && !matches!(
            TextTokens::new(from, range).evidence(&to[span.clone()]),
            TextEvidence::Exact | TextEvidence::Edited
        )
    {
        return TextRange::Unresolved;
    }
    TextRange::Present(span)
}

/// Project cuts through a diff, including whole replacements and excluding
/// insertions immediately outside the selection. A cut inside a replacement
/// has no established counterpart; equal line counts do not establish one.
fn project_units<T: Ord>(a: &[T], b: &[T], range: Range<usize>) -> Option<Range<usize>> {
    let edits = sequence::edits(a, b);
    project_edits(a.len(), b.len(), &edits, range)
}

fn project_edits(
    a: usize,
    b: usize,
    edits: &[sequence::Edit],
    range: Range<usize>,
) -> Option<Range<usize>> {
    if a != 0 && b != 0 && edits.len() == 1 && edits[0].old == (0..a) && edits[0].new == (0..b) {
        return None;
    }
    let cut = |position: usize, start: bool| {
        let (mut old, mut new) = (0, 0);
        for edit in edits {
            if position < edit.old.start {
                break;
            }
            if position == edit.old.start {
                return Some(if start && edit.old.is_empty() {
                    edit.new.end
                } else {
                    edit.new.start
                });
            }
            if position < edit.old.end {
                return edit.new.is_empty().then_some(edit.new.start);
            }
            old = edit.old.end;
            new = edit.new.end;
        }
        Some(new + position - old)
    };
    let start = cut(range.start, true)?;
    let end = cut(range.end, false)?;
    (start <= end).then_some(start..end)
}

/// Restore actual target line edges, never the source's whitespace lengths.
/// A historical passage may occupy only part of a differently wrapped line.
fn blank_edges(text: &str) -> (usize, usize) {
    let leading = text[..text.len() - text.trim_start().len()]
        .bytes()
        .filter(|&b| b == b'\n')
        .count();
    let trailing = text[text.trim_end().len()..]
        .bytes()
        .filter(|&b| b == b'\n')
        .count();
    (leading, trailing.saturating_sub(1))
}

fn text_edges(
    to: &str,
    mut mapped: Range<usize>,
    (leading, trailing): (usize, usize),
) -> Range<usize> {
    // Text regions originate as line selections. Retain that affinity even
    // after reflow temporarily maps an endpoint into the middle of a line.
    let before = to[..mapped.start]
        .char_indices()
        .rev()
        .find(|(_, c)| *c == '\n' || !c.is_whitespace());
    if before.is_none_or(|(_, c)| c == '\n') {
        let start = before.map_or(0, |(i, _)| i + 1);
        mapped.start = start;
        for _ in 0..leading {
            if mapped.start == 0 {
                break;
            }
            let before = to[..mapped.start - 1]
                .char_indices()
                .rev()
                .find(|(_, c)| *c == '\n' || !c.is_whitespace());
            if before.is_some_and(|(_, c)| c != '\n') {
                break;
            }
            mapped.start = before.map_or(0, |(i, _)| i + 1);
        }
    }
    let after = to[mapped.end..]
        .char_indices()
        .find(|(_, c)| *c == '\n' || !c.is_whitespace());
    if after.is_none_or(|(_, c)| c == '\n') {
        let end = after.map_or(to.len(), |(i, _)| mapped.end + i + 1);
        mapped.end = end;
        for _ in 0..trailing {
            let after = to[mapped.end..]
                .char_indices()
                .find(|(_, c)| *c == '\n' || !c.is_whitespace());
            if after.is_some_and(|(_, c)| c != '\n') {
                break;
            }
            mapped.end = after.map_or(to.len(), |(i, _)| mapped.end + i + 1);
        }
    }
    mapped
}

/// Complete token occurrences, including overlapping duplicates. Partial
/// matching runs are useful evidence, but are not complete passage candidates.
pub(in crate::history) struct TextTokens<'a> {
    lines: bool,
    edges: (usize, usize),
    words: Vec<&'a str>,
    prefix: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::history) enum TextEvidence {
    Exact,
    Edited,
    Weak,
    None,
}

impl<'a> TextTokens<'a> {
    pub fn new(from: &'a str, range: Range<usize>) -> Self {
        let needle = &from[range];
        let mut words: Vec<_> = TOKENS.find_iter(needle).map(|m| m.as_str()).collect();
        // Tokenless passages still have complete, possibly repeated lines.
        // Use the same overlapping matcher, without discarding their bytes.
        let lines = words.is_empty();
        if lines {
            words.extend(needle.split_inclusive('\n'));
        }
        let mut prefix = vec![0; words.len()];
        for i in 1..words.len() {
            let mut j = prefix[i - 1];
            while j > 0 && words[i] != words[j] {
                j = prefix[j - 1];
            }
            if words[i] == words[j] {
                j += 1;
            }
            prefix[i] = j;
        }
        Self {
            lines,
            edges: blank_edges(needle),
            words,
            prefix,
        }
    }

    pub fn occurrences(&self, to: &str, cancel: &AtomicBool) -> Result<Vec<Range<usize>>> {
        let mut found = Vec::new();
        if self.words.is_empty() {
            return Ok(found);
        }
        let tokens: Box<dyn Iterator<Item = Range<usize>> + '_> = if self.lines {
            Box::new(to.split_inclusive('\n').scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some(start..*offset)
            }))
        } else {
            Box::new(TOKENS.find_iter(to).map(|m| m.range()))
        };
        let mut j = 0;
        let mut starts = VecDeque::new();
        for token in tokens {
            crate::repository::history::check(cancel)?;
            starts.push_back(token.start);
            if starts.len() > self.words.len() {
                starts.pop_front();
            }
            let word = &to[token.clone()];
            while j > 0 && word != self.words[j] {
                j = self.prefix[j - 1];
            }
            if word == self.words[j] {
                j += 1;
            }
            if j == self.words.len() {
                ensure!(
                    found.len() < 65_536,
                    "unresolved target: text candidate limit reached"
                );
                let range = starts[0]..token.end;
                found.push(if self.lines {
                    range
                } else {
                    text_edges(to, range, self.edges)
                });
                j = self.prefix[j - 1];
            }
        }
        Ok(found)
    }

    pub fn distinctive(&self) -> bool {
        has_distinctive_words(self.words.iter().copied(), 2)
    }

    pub fn meaningful(&self) -> bool {
        has_distinctive_words(self.words.iter().copied(), 1)
    }

    /// Ordered coverage supports complete paragraph candidates. Weak overlap
    /// keeps a failed search unresolved; it never supplies a clipped passage.
    pub fn evidence(&self, text: &str) -> TextEvidence {
        if self.lines {
            return TextEvidence::None;
        }
        let other: Vec<_> = TOKENS.find_iter(text).map(|m| m.as_str()).collect();
        if self.words.is_empty() || other.is_empty() {
            return TextEvidence::None;
        }
        if self.words == other {
            return TextEvidence::Exact;
        }
        let pairs = sequence::matches(&self.words, &other);
        let coverage = 200 * pairs.len() / (self.words.len() + other.len());
        let shared = || pairs.iter().map(|&(i, _)| self.words[i]);
        if coverage >= 80 && has_distinctive_words(shared(), 2) {
            TextEvidence::Edited
        } else if coverage >= 50 && has_distinctive_words(shared(), 1)
            || 100 * pairs.len() >= 80 * self.words.len() && has_distinctive_words(shared(), 2)
        {
            TextEvidence::Weak
        } else {
            TextEvidence::None
        }
    }
}

/// Paragraph boundaries are evidence only for selections that include whole
/// paragraphs. An arbitrary selected line must never expand to its paragraph.
pub(in crate::history) fn paragraphs(text: &str) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let (mut start, mut offset) = (None, 0);
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            if let Some(start) = start.take() {
                result.push(start..offset);
            }
        } else {
            start.get_or_insert(offset);
        }
        offset += line.len();
    }
    if let Some(start) = start {
        result.push(start..offset);
    }
    result
}

/// Independent neighbouring lines can locate an edited passage in another
/// file. Repeated headings and punctuation-only separators are not witnesses.
pub(in crate::history) struct TextContext<'a> {
    before: &'a str,
    after: &'a str,
}
impl<'a> TextContext<'a> {
    pub fn new(text: &'a str, selected: &Range<usize>) -> Option<Self> {
        let (mut before, mut after, mut offset) = (None, None, 0);
        for line in text.split_inclusive('\n') {
            let content = line.trim_end_matches(['\r', '\n']);
            if !content.trim().is_empty() {
                if offset + line.len() <= selected.start {
                    before = Some(content);
                }
                if offset >= selected.end {
                    after = Some(content);
                    break;
                }
            }
            offset += line.len();
        }
        let context = Self {
            before: before?,
            after: after?,
        };
        if !has_distinctive_tokens(context.before, 1) || !has_distinctive_tokens(context.after, 1) {
            return None;
        }
        context.range(text)?;
        Some(context)
    }

    pub fn range(&self, text: &str) -> Option<Range<usize>> {
        let (mut before, mut after, mut offset) = (None, None, 0);
        for line in text.split_inclusive('\n') {
            let content = line.trim_end_matches(['\r', '\n']);
            if content == self.before && before.replace(offset + line.len()).is_some() {
                return None;
            }
            if content == self.after && after.replace(offset).is_some() {
                return None;
            }
            offset += line.len();
        }
        let range = before?..after?;
        (range.start <= range.end).then_some(range)
    }
}

#[cfg(test)]
mod tests {
    use super::super::line_range;
    use super::*;

    fn assert_text_projection(from: &str, to: &str, start: usize, end: usize, expected: &str) {
        let selected = line_range(from, start, end).unwrap();
        match text_range(from, to, selected) {
            TextRange::Present(span) => assert_eq!(&to[span], expected),
            result => panic!("expected selected content {expected:?}, got {result:?}"),
        }
    }

    #[test]
    fn text_projection_includes_changed_edges_and_all_selected_content() {
        let before = "Intro stays.\nSelected before.\nContext stays.\nTail before.\n";
        let after = "Intro stays.\nSelected after.\nContext stays.\nTail after.\n";
        for (from, to) in [(before, after), (after, before)] {
            for (start, end) in [(1, 4), (2, 2), (2, 3), (4, 4)] {
                assert_text_projection(
                    from,
                    to,
                    start,
                    end,
                    &to[line_range(to, start, end).unwrap()],
                );
            }
        }
        let punctuation = before.replace("Selected before.", "Selected before!");
        assert_text_projection(before, &punctuation, 2, 2, "Selected before!\n");
        assert_text_projection(&punctuation, before, 2, 2, "Selected before.\n");
    }

    #[test]
    fn text_projection_excludes_boundary_insertions_and_keeps_interior_insertions() {
        let before = "head\nalpha\nbeta\ntail\n";
        let after = "head\nbefore\nalpha\ninside\nbeta\nafter\ntail\n";
        assert_text_projection(before, after, 2, 3, "alpha\ninside\nbeta\n");
        assert_text_projection(after, before, 3, 5, "alpha\nbeta\n");
        assert_text_projection(before, after, 1, 1, "head\n");
        assert_text_projection(before, after, 4, 4, "tail\n");
        for line in [2, 4, 6] {
            assert_eq!(
                text_range(after, before, line_range(after, line, line).unwrap()),
                TextRange::Absent,
            );
        }
    }

    #[test]
    fn text_projection_proves_absence_for_subranges_of_a_deleted_block() {
        let before = "head\nremove one\nremove two\nremove three\ntail\n";
        let after = "head\ntail\n";
        for (start, end) in [(2, 2), (3, 3), (4, 4), (2, 4)] {
            assert_eq!(
                text_range(before, after, line_range(before, start, end).unwrap()),
                TextRange::Absent,
            );
        }
        assert_text_projection(before, after, 1, 3, "head\n");
        assert_text_projection(before, after, 3, 5, "tail\n");
    }

    #[test]
    fn text_projection_does_not_split_an_unsupported_replacement() {
        let before = "head\nalpha original\nbeta original\ntail\n";
        let after = "head\ngamma changed\ndelta changed\ntail\n";
        for (from, to) in [(before, after), (after, before)] {
            for line in [2, 3] {
                assert_eq!(
                    text_range(from, to, line_range(from, line, line).unwrap()),
                    TextRange::Unresolved,
                );
            }
            assert_text_projection(from, to, 2, 3, &to[line_range(to, 2, 3).unwrap()]);
        }
        assert_eq!(
            text_range("entirely new.\n", "unrelated predecessor!\n", 0..14),
            TextRange::Unresolved,
        );
    }

    #[test]
    fn text_projection_refines_reflow_without_adjoining_content() {
        let wrapped = "# Design\n\nSelected passage preserves distinctive ordered words\nacross reflow and file movement.\n\nUnrelated section.\n";
        let original = "# Design\n\nSelected passage preserves distinctive ordered words across reflow and file movement.\n\nUnrelated section changed independently.\n";
        assert_text_projection(
            wrapped,
            original,
            3,
            4,
            "Selected passage preserves distinctive ordered words across reflow and file movement.\n",
        );
        assert_text_projection(
            wrapped,
            original,
            3,
            3,
            "Selected passage preserves distinctive ordered words",
        );
        assert_text_projection(
            wrapped,
            original,
            4,
            4,
            "across reflow and file movement.\n",
        );
        let edited = original.replace("distinctive", "original");
        assert_text_projection(
            wrapped,
            &edited,
            3,
            3,
            "Selected passage preserves original ordered words",
        );
        for line in [3, 4] {
            let selected = line_range(wrapped, line, line).unwrap();
            let TextRange::Present(previous) = text_range(wrapped, original, selected.clone())
            else {
                panic!("reflowed passage must retain a historical span");
            };
            let TextRange::Present(restored) = text_range(original, wrapped, previous) else {
                panic!("historical span must recover its line selection");
            };
            assert_eq!(&wrapped[restored], &wrapped[selected]);
        }
    }

    #[test]
    fn text_projection_preserves_target_whitespace_unicode_and_eof() {
        let before = "before\n  Café λ 🦀.\r\n \t\r\nafter";
        let after = "inserted\nbefore\n\tCafé λ 🦀!\r\n \t\r\nafter\n";
        assert_text_projection(before, after, 2, 3, "\tCafé λ 🦀!\r\n \t\r\n");
        assert_text_projection(after, before, 3, 4, "  Café λ 🦀.\r\n \t\r\n");
        assert_text_projection("head\n \t\nfoot\n", "head\n\t \nfoot\n", 2, 2, "\t \n");
        assert_text_projection(
            "intro\nCafé λ 🦀.",
            "intro\n\tCafé λ 🦀!\n",
            2,
            2,
            "\tCafé λ 🦀!\n",
        );
        assert_text_projection(
            "intro\n\tCafé λ 🦀!\n",
            "intro\nCafé λ 🦀.",
            2,
            2,
            "Café λ 🦀.",
        );
    }

    #[test]
    fn empty_text_identity_is_present() {
        assert_eq!(text_range("", "", 0..0), TextRange::Present(0..0));
    }

    #[test]
    fn text_occurrences_keep_complete_reflowed_and_overlapping_alternatives() {
        let cancel = AtomicBool::new(false);
        let passage = "alpha beta alpha\n";
        let pattern = TextTokens::new(passage, 0..passage.len());
        for (target, expected) in [
            (
                "alpha beta alpha beta alpha\n",
                vec!["alpha beta alpha", "alpha beta alpha\n"],
            ),
            (
                "Before.\n  alpha beta alpha\n\nalpha\nbeta alpha\nAfter.\n",
                vec!["  alpha beta alpha\n", "alpha\nbeta alpha\n"],
            ),
        ] {
            let matches = pattern.occurrences(target, &cancel).unwrap();
            assert_eq!(
                matches
                    .into_iter()
                    .map(|span| &target[span])
                    .collect::<Vec<_>>(),
                expected,
            );
        }
        let punctuation = "alpha beta.\n";
        let pattern = TextTokens::new(punctuation, 0..punctuation.len());
        let target = "alpha beta!\nalpha beta.\n";
        let matches = pattern.occurrences(target, &cancel).unwrap();
        assert_eq!(
            matches
                .into_iter()
                .map(|span| &target[span])
                .collect::<Vec<_>>(),
            ["alpha beta.\n"]
        );
    }

    #[test]
    fn text_occurrence_search_is_bounded_and_cancellable() {
        let pattern = TextTokens::new("alpha", 0..5);
        let cancelled = AtomicBool::new(true);
        assert!(
            pattern
                .occurrences("alpha", &cancelled)
                .unwrap_err()
                .to_string()
                .contains("history cancelled")
        );
        let cancel = AtomicBool::new(false);
        let repeated = "alpha ".repeat(65_537);
        assert!(
            pattern
                .occurrences(&repeated, &cancel)
                .unwrap_err()
                .to_string()
                .contains("text candidate limit")
        );
    }

    #[test]
    fn text_context_requires_unique_complete_ordered_anchors() {
        let source =
            "Outer prefix.\nBefore marker.\n\nSelected original.\n\nAfter marker.\nOuter suffix.\n";
        let selected = line_range(source, 4, 4).unwrap();
        let context = TextContext::new(source, &selected).unwrap();
        assert_eq!(
            &source[context.range(source).unwrap()],
            "\nSelected original.\n\n"
        );
        let target = "Unrelated heading.\nBefore marker.\n\nCompletely rewritten selected text.\n\nAfter marker.\nUnrelated tail.\n";
        assert_eq!(
            &target[context.range(target).unwrap()],
            "\nCompletely rewritten selected text.\n\n",
        );
        for target in [
            "Before marker.\nSelected original.\n",
            "Selected original.\nAfter marker.\n",
            "After marker.\nSelected original.\nBefore marker.\n",
            "Before marker.\nSelected original.\nAfter marker.\nBefore marker.\n",
            "Before marker.\nSelected original.\nAfter marker.\nAfter marker.\n",
        ] {
            assert!(context.range(target).is_none(), "{target:?}");
        }
        for repeated in ["Before marker.\n", "After marker.\n"] {
            let ambiguous = format!("{source}{repeated}");
            assert!(TextContext::new(&ambiguous, &selected).is_none());
        }
        let punctuation = "Before marker.\n---\nSelected original.\nAfter marker.\n";
        assert!(TextContext::new(punctuation, &line_range(punctuation, 3, 3).unwrap()).is_none());
        let partial = "Before marker.\nSelected original.\nAfter marker.\n";
        let through_header = "Before ".len()..line_range(partial, 2, 2).unwrap().end;
        assert!(TextContext::new(partial, &through_header).is_none());
    }

    #[test]
    fn text_context_bounds_do_not_corroborate_other_occurrences() {
        let source = "Before marker.\nSelected original.\nAfter marker.\n";
        let selected = line_range(source, 2, 2).unwrap();
        let context = TextContext::new(source, &selected).unwrap();
        let target = "Selected original.\nBefore marker.\n\nEdited inside.\n\nAfter marker.\nSelected original.\n";
        let bounds = context.range(target).unwrap();
        assert_eq!(&target[bounds.clone()], "\nEdited inside.\n\n");
        let pattern = TextTokens::new(source, selected);
        let occurrences = pattern
            .occurrences(target, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(occurrences.len(), 2);
        for occurrence in occurrences {
            assert_eq!(&target[occurrence.clone()], "Selected original.\n");
            assert!(!(bounds.start <= occurrence.start && occurrence.end <= bounds.end));
        }
    }

    #[test]
    fn tokenless_passages_find_overlapping_complete_line_occurrences() {
        let source = "head\n\n\nfoot\n";
        let selected = line_range(source, 2, 3).unwrap();
        let pattern = TextTokens::new(source, selected.clone());
        let cancel = AtomicBool::new(false);
        let original = pattern.occurrences(source, &cancel).unwrap();
        assert_eq!(original, [selected]);
        assert_eq!(&source[original[0].clone()], "\n\n");

        let target = "head\n\n\n\nfoot\n";
        let alternatives = pattern.occurrences(target, &cancel).unwrap();
        assert_eq!(alternatives.len(), 2);
        for span in &alternatives {
            assert_eq!(&target[span.clone()], "\n\n");
        }
        assert!(alternatives[0].start < alternatives[1].start);
        assert!(alternatives[1].start < alternatives[0].end);
    }

    #[test]
    fn text_occurrences_preserve_selected_blank_lines_with_target_whitespace() {
        let source = "\n    alpha beta.\n\n";
        let pattern = TextTokens::new(source, 0..source.len());
        let target = "Before.\n \t\nalpha\nbeta.\n \t\nAfter.\n";
        let occurrences = pattern
            .occurrences(target, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(occurrences.len(), 1);
        assert_eq!(&target[occurrences[0].clone()], " \t\nalpha\nbeta.\n \t\n");
    }

    #[test]
    fn text_context_normalizes_line_endings_without_changing_selected_bytes() {
        let lf = "Before marker.\n\nSelected original.\n\nAfter marker.\n";
        let crlf = lf.replace('\n', "\r\n");
        for source in [lf, crlf.as_str()] {
            let selected = line_range(source, 3, 3).unwrap();
            let context = TextContext::new(source, &selected).unwrap();
            for (target, expected) in [
                (
                    "Before marker.\n\nRewritten selected text.\n\nAfter marker.\n",
                    "\nRewritten selected text.\n\n",
                ),
                (
                    "Before marker.\r\n\r\nRewritten selected text.\r\n\r\nAfter marker.\r\n",
                    "\r\nRewritten selected text.\r\n\r\n",
                ),
                (
                    "Before marker.\n\nRewritten selected text.\n\nAfter marker.",
                    "\nRewritten selected text.\n\n",
                ),
            ] {
                assert_eq!(&target[context.range(target).unwrap()], expected);
            }
            for duplicate in ["Before marker.\r\n", "After marker.\r\n"] {
                let target = format!("{lf}{duplicate}");
                assert!(context.range(&target).is_none());
                assert!(TextContext::new(&target, &line_range(&target, 3, 3).unwrap()).is_none());
            }
        }
    }

    #[test]
    fn text_projection_excludes_deleted_blank_lines_next_to_an_edit() {
        for prefix in ["Selected", "Selected passage"] {
            let selected_before = format!("{prefix} before.\n");
            let selected_after = format!("{prefix} after.\n");
            let after = format!("head\n{selected_after}foot\n");
            for (leading, trailing, line) in [("", "\n", 2), ("\n", "", 3), ("\n", "\n", 3)] {
                let before = format!("head\n{leading}{selected_before}{trailing}foot\n");
                assert_text_projection(&after, &before, 2, 2, &selected_before);
                assert_text_projection(&before, &after, line, line, &selected_after);
            }
        }
    }

    #[test]
    fn text_projection_does_not_join_edited_duplicate_predecessors() {
        let before = "head\nSelected passage before.\nSelected passage before.\nfoot\n";
        let after = "head\nSelected passage after.\nfoot\n";
        assert_eq!(
            text_range(after, before, line_range(after, 2, 2).unwrap()),
            TextRange::Unresolved,
        );
    }

    #[test]
    fn text_projection_does_not_join_copies_inside_a_replacement() {
        let before = "head\nPrevious upper.\nSelected passage before.\nSelected passage before.\nPrevious lower.\nfoot\n";
        let after = "head\n  Previous upper.\nSelected passage after.\n  Previous lower.\nfoot\n";
        // Surrounding indentation merges the line changes into one hunk.
        // Token prefix/suffix matches must not join two predecessor copies.
        assert_eq!(
            text_range(after, before, line_range(after, 3, 3).unwrap()),
            TextRange::Unresolved,
        );
    }
}
