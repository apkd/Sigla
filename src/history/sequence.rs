//! Deterministic patience alignment with bounded exact fallback. A coarse fallback
//! remains an exact replacement patch; it never drops bytes or normalizes a diff.
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Edit {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

pub(super) fn edits<T: Ord>(a: &[T], b: &[T]) -> Vec<Edit> {
    let pairs = matches(a, b);
    let mut changes = Vec::new();
    let (mut x, mut y) = (0, 0);
    for (i, j) in pairs.into_iter().chain(std::iter::once((a.len(), b.len()))) {
        if x < i || y < j {
            changes.push(Edit {
                old: x..i,
                new: y..j,
            });
        }
        x = i + 1;
        y = j + 1;
    }
    changes
}

pub(super) fn matches<T: Ord>(a: &[T], b: &[T]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pending = vec![(0..a.len(), 0..b.len())];
    while let Some((mut ar, mut br)) = pending.pop() {
        while ar.start < ar.end && br.start < br.end && a[ar.start] == b[br.start] {
            out.push((ar.start, br.start));
            ar.start += 1;
            br.start += 1;
        }
        while ar.start < ar.end && br.start < br.end && a[ar.end - 1] == b[br.end - 1] {
            ar.end -= 1;
            br.end -= 1;
            out.push((ar.end, br.end));
        }
        if ar.is_empty() || br.is_empty() {
            continue;
        }
        let mut positions = BTreeMap::<&T, (usize, usize, usize, usize)>::new();
        for i in ar.clone() {
            let p = positions.entry(&a[i]).or_default();
            p.0 += 1;
            p.1 = i;
        }
        for j in br.clone() {
            let p = positions.entry(&b[j]).or_default();
            p.2 += 1;
            p.3 = j;
        }
        let mut unique: Vec<_> = positions
            .values()
            .filter(|p| p.0 == 1 && p.2 == 1)
            .map(|p| (p.1, p.3))
            .collect();
        unique.sort_unstable();
        if unique.is_empty() {
            if ar.len().saturating_mul(br.len()) <= 262_144 {
                let width = br.len() + 1;
                let mut dp = vec![0u32; (ar.len() + 1) * width];
                for i in (0..ar.len()).rev() {
                    for j in (0..br.len()).rev() {
                        dp[i * width + j] = if a[ar.start + i] == b[br.start + j] {
                            1 + dp[(i + 1) * width + j + 1]
                        } else {
                            dp[(i + 1) * width + j].max(dp[i * width + j + 1])
                        };
                    }
                }
                let (mut i, mut j) = (0, 0);
                while i < ar.len() && j < br.len() {
                    if a[ar.start + i] == b[br.start + j] {
                        out.push((ar.start + i, br.start + j));
                        i += 1;
                        j += 1;
                    } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
                        i += 1;
                    } else {
                        j += 1;
                    }
                }
            }
            continue;
        }
        let mut tails: Vec<usize> = Vec::new();
        let mut previous = vec![None; unique.len()];
        for (i, &(_, j)) in unique.iter().enumerate() {
            let k = tails.partition_point(|&index| unique[index].1 < j);
            if k > 0 {
                previous[i] = Some(tails[k - 1]);
            }
            if k == tails.len() {
                tails.push(i);
            } else {
                tails[k] = i;
            }
        }
        let mut chain = Vec::new();
        let mut index = tails.last().copied();
        while let Some(i) = index {
            chain.push(unique[i]);
            index = previous[i];
        }
        chain.reverse();
        let (mut x, mut y) = (ar.start, br.start);
        for (i, j) in chain.into_iter().chain(std::iter::once((ar.end, br.end))) {
            if x < i && y < j {
                pending.push((x..i, y..j));
            }
            if i < ar.end {
                out.push((i, j));
            }
            x = i + 1;
            y = j + 1;
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patches_reconstruct_every_short_binary_sequence() {
        for na in 0..7 {
            for nb in 0..7 {
                for va in 0..(1 << na) {
                    for vb in 0..(1 << nb) {
                        let a: Vec<_> = (0..na).map(|i| (va >> i) & 1).collect();
                        let b: Vec<_> = (0..nb).map(|i| (vb >> i) & 1).collect();
                        let mut rebuilt = Vec::new();
                        let mut cursor = 0;
                        for edit in edits(&a, &b) {
                            rebuilt.extend_from_slice(&a[cursor..edit.old.start]);
                            rebuilt.extend_from_slice(&b[edit.new]);
                            cursor = edit.old.end;
                        }
                        rebuilt.extend_from_slice(&a[cursor..]);
                        assert_eq!(rebuilt, b);
                    }
                }
            }
        }
    }
}
