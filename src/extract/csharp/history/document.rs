//! Symbolically execute preprocessing once. Branches join when their lexical
//! and directive stacks agree; local symbols join as Boolean functions rather
//! than multiplying subsequent, independent text choices.
use super::{
    PARSE_BYTES, Projection, blank,
    conditions::{self, Conditions, FALSE, Guard, TRUE},
    lexical::{Lexical, NESTING, newline},
};
use anyhow::{Result, ensure};
use std::{collections::HashMap, ops::Range};

#[derive(Clone, PartialEq, Eq)]
struct Branch {
    parent: bool,
    taken: bool,
    otherwise: bool,
}
#[derive(Clone)]
struct State {
    guard: Guard,
    active: bool,
    stack: Vec<Branch>,
    regions: usize,
    symbols: HashMap<String, Guard>,
    lexical: Lexical,
}
impl State {
    fn same_context(&self, other: &Self) -> bool {
        self.active == other.active
            && self.stack == other.stack
            && self.regions == other.regions
            && self.lexical == other.lexical
    }
    fn choose(
        self,
        expression: &str,
        enabled: bool,
        conditions: &mut Conditions,
        next: &mut Vec<Self>,
        update: impl Fn(&mut Self, bool),
    ) -> Result<()> {
        let condition = conditions.expression(expression, &self.symbols, enabled)?;
        let condition = if enabled { condition } else { FALSE };
        let otherwise = conditions.not(condition)?;
        for (yes, branch) in [(false, otherwise), (true, condition)] {
            let guard = conditions.and(self.guard, branch)?;
            if guard != FALSE {
                let mut state = self.clone();
                state.guard = guard;
                update(&mut state, yes);
                next.push(state);
            }
        }
        Ok(())
    }
}

fn join(states: Vec<State>, conditions: &mut Conditions) -> Result<Vec<State>> {
    let mut joined = Vec::<State>::new();
    for state in states {
        let Some(previous) = joined.iter_mut().find(|s| s.same_context(&state)) else {
            ensure!(
                joined.len() < NESTING,
                "historical C# lexical context limit reached"
            );
            joined.push(state);
            continue;
        };
        if previous.symbols != state.symbols {
            let mut names: Vec<_> = previous
                .symbols
                .keys()
                .chain(state.symbols.keys())
                .cloned()
                .collect();
            names.sort();
            names.dedup();
            ensure!(
                names.len() <= NESTING,
                "historical C# local symbol limit reached"
            );
            for name in names {
                let left = match previous.symbols.get(&name) {
                    Some(&value) => value,
                    None => conditions.symbol(&name)?,
                };
                let right = match state.symbols.get(&name) {
                    Some(&value) => value,
                    None => conditions.symbol(&name)?,
                };
                let value = if left == right {
                    left
                } else {
                    let left = conditions.and(previous.guard, left)?;
                    let right = conditions.and(state.guard, right)?;
                    conditions.or(left, right)?
                };
                previous.symbols.insert(name, value);
            }
        }
        previous.guard = conditions.or(previous.guard, state.guard)?;
    }
    Ok(joined)
}

struct Text {
    range: Range<usize>,
    guard: Guard,
}
pub(super) struct Document<'a> {
    source: &'a str,
    blank: String,
    text: Vec<Text>,
    lines: Vec<usize>,
}
impl<'a> Document<'a> {
    pub(super) fn new(source: &'a str, conditions: &mut Conditions) -> Result<Self> {
        let mut states = vec![State {
            guard: TRUE,
            active: true,
            stack: Vec::new(),
            regions: 0,
            symbols: HashMap::new(),
            lexical: Lexical::default(),
        }];
        let mut text = Vec::<Text>::new();
        let mut offset = 0;
        let mut scanned = 0usize;
        for line in source.split_inclusive(newline) {
            let trimmed = if offset == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            }
            .trim_start();
            let mut next = Vec::new();
            let mut visible = FALSE;
            for mut state in states {
                ensure!(
                    line.len() <= PARSE_BYTES.saturating_sub(scanned),
                    "historical C# preprocessing work limit reached"
                );
                scanned += line.len();
                let directive = (!state.active || state.lexical.code())
                    .then(|| trimmed.strip_prefix('#'))
                    .flatten();
                if let Some(directive) = directive {
                    let directive = directive.trim_start();
                    let end = directive
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(directive.len());
                    let (key, rest) = directive.split_at(end);
                    let expression = rest.split_once("//").map_or(rest, |(s, _)| s).trim();
                    match key {
                        "if" => {
                            ensure!(
                                state.stack.len() < NESTING,
                                "historical C# directive nesting limit reached"
                            );
                            let enabled = state.active;
                            state.choose(
                                expression,
                                enabled,
                                conditions,
                                &mut next,
                                |s, yes| {
                                    s.stack.push(Branch {
                                        parent: s.active,
                                        taken: yes,
                                        otherwise: false,
                                    });
                                    s.active = yes;
                                },
                            )?;
                            continue;
                        }
                        "elif" => {
                            let branch = state
                                .stack
                                .last()
                                .ok_or_else(|| anyhow::anyhow!("unmatched historical C# #elif"))?;
                            ensure!(!branch.otherwise, "historical C# #elif after #else");
                            let enabled = branch.parent && !branch.taken;
                            state.choose(
                                expression,
                                enabled,
                                conditions,
                                &mut next,
                                |s, yes| {
                                    s.active = yes;
                                    s.stack.last_mut().unwrap().taken |= yes;
                                },
                            )?;
                            continue;
                        }
                        "else" => {
                            ensure!(
                                expression.is_empty(),
                                "unexpected text after historical C# #else"
                            );
                            let branch = state
                                .stack
                                .last_mut()
                                .ok_or_else(|| anyhow::anyhow!("unmatched historical C# #else"))?;
                            ensure!(!branch.otherwise, "duplicate historical C# #else");
                            state.active = branch.parent && !branch.taken;
                            branch.taken = true;
                            branch.otherwise = true;
                        }
                        "endif" => {
                            ensure!(
                                expression.is_empty(),
                                "unexpected text after historical C# #endif"
                            );
                            state.active = state
                                .stack
                                .pop()
                                .ok_or_else(|| anyhow::anyhow!("unmatched historical C# #endif"))?
                                .parent;
                        }
                        "define" | "undef" if state.active => {
                            let (name, len) = conditions::identifier(expression)?;
                            ensure!(
                                len == expression.len() && name != "true" && name != "false",
                                "invalid historical C# symbol directive"
                            );
                            ensure!(
                                state.symbols.len() < NESTING || state.symbols.contains_key(&name),
                                "historical C# local symbol limit reached"
                            );
                            state
                                .symbols
                                .insert(name, if key == "define" { TRUE } else { FALSE });
                        }
                        "region" if state.active => {
                            ensure!(
                                state.regions < NESTING,
                                "historical C# region nesting limit reached"
                            );
                            state.regions += 1;
                        }
                        "endregion" if state.active => {
                            ensure!(state.regions > 0, "unmatched historical C# #endregion");
                            state.regions -= 1;
                        }
                        "define" | "undef" | "region" | "endregion" | "line" | "pragma"
                        | "nullable" | "error" | "warning" => {}
                        _ => anyhow::bail!("unsupported historical C# directive #{key}"),
                    }
                } else if state.active {
                    state.lexical.scan(line)?;
                    visible = conditions.or(visible, state.guard)?;
                }
                next.push(state);
            }
            states = join(next, conditions)?;
            if visible != FALSE {
                if let Some(previous) = text.last_mut()
                    && previous.guard == visible
                    && previous.range.end == offset
                {
                    previous.range.end += line.len();
                } else {
                    text.push(Text {
                        range: offset..offset + line.len(),
                        guard: visible,
                    });
                }
            }
            offset += line.len();
        }
        ensure!(
            states
                .iter()
                .all(|s| s.stack.is_empty() && s.regions == 0 && s.lexical.complete()),
            "unfinished historical C# directives or lexical syntax"
        );
        let mut bytes = source.as_bytes().to_vec();
        blank(source, &mut bytes, 0..source.len());
        Ok(Self {
            source,
            blank: String::from_utf8(bytes)?,
            text,
            lines: std::iter::once(0)
                .chain(
                    source
                        .bytes()
                        .enumerate()
                        .filter_map(|(i, c)| (c == b'\n').then_some(i + 1)),
                )
                .collect(),
        })
    }

    /// Discovery needs witnesses, not their whole-file shape guards. Building
    /// those conjunctions would itself waste BDD nodes on independent choices.
    pub(super) fn sample(&self, conditions: &Conditions, values: &[bool]) -> Result<String> {
        let mut bytes = self.blank.as_bytes().to_vec();
        for text in &self.text {
            if conditions.value(text.guard, values) {
                bytes[text.range.clone()]
                    .copy_from_slice(&self.source.as_bytes()[text.range.clone()]);
            }
        }
        Ok(String::from_utf8(bytes)?)
    }

    pub(super) fn guards(&self) -> Vec<Guard> {
        let mut guards: Vec<_> = self
            .text
            .iter()
            .filter(|t| self.source[t.range.clone()] != self.blank[t.range.clone()])
            .map(|t| t.guard)
            .collect();
        guards.sort_unstable();
        guards.dedup();
        guards
    }

    /// Skip masked holes while keeping original byte/point coordinates. One
    /// whitespace character per hole preserves token separation; a line break
    /// also terminates line comments exactly as the full masked source does.
    pub(super) fn included_ranges(&self, ranges: &[Range<usize>]) -> Vec<tree_sitter::Range> {
        let mut spans = Vec::new();
        let mut end = 0;
        for range in ranges
            .iter()
            .cloned()
            .chain(std::iter::once(self.source.len()..self.source.len()))
        {
            if end < range.start {
                let gap = &self.blank[end..range.start];
                let offset = gap.rfind(newline).unwrap_or(0);
                let size = gap[offset..].chars().next().unwrap().len_utf8();
                spans.push(end + offset..end + offset + size);
            }
            if !range.is_empty() {
                spans.push(range.clone());
            }
            end = range.end;
        }
        spans
            .into_iter()
            .map(|r| tree_sitter::Range {
                start_byte: r.start,
                end_byte: r.end,
                start_point: self.point(r.start),
                end_point: self.point(r.end),
            })
            .collect()
    }

    fn point(&self, byte: usize) -> tree_sitter::Point {
        let row = self.lines.partition_point(|&start| start <= byte) - 1;
        tree_sitter::Point {
            row,
            column: byte - self.lines[row],
        }
    }

    /// Mark changed guarded runs before reusing a tree for the same context.
    /// Projections preserve byte lengths and newlines, so edits never shift
    /// later ranges, including UTF-8 text and ranges omitted from the parser.
    pub(super) fn edit_tree(
        &self,
        tree: &mut tree_sitter::Tree,
        conditions: &Conditions,
        before: &[bool],
        after: &[bool],
        ranges: &[Range<usize>],
    ) {
        for range in ranges {
            let start = self.text.partition_point(|t| t.range.end <= range.start);
            for text in self.text[start..]
                .iter()
                .take_while(|t| t.range.start < range.end)
            {
                if conditions.value(text.guard, before) == conditions.value(text.guard, after) {
                    continue;
                }
                let overlap = text.range.start.max(range.start)..text.range.end.min(range.end);
                if self.source[overlap.clone()] == self.blank[overlap.clone()] {
                    continue;
                }
                let end = self.point(overlap.end);
                tree.edit(&tree_sitter::InputEdit {
                    start_byte: overlap.start,
                    old_end_byte: overlap.end,
                    new_end_byte: overlap.end,
                    start_position: self.point(overlap.start),
                    old_end_position: end,
                    new_end_position: end,
                });
            }
        }
    }

    /// Enumerate the text of this context, not assignments to its symbols.
    /// All selected runs share one feasible environment; sibling runs omitted
    /// from `ranges` impose no choices on this context's exploration.
    pub(super) fn project(
        &self,
        conditions: &mut Conditions,
        values: &[bool],
        ranges: &[Range<usize>],
    ) -> Result<Projection> {
        let mut bytes = self.blank.as_bytes().to_vec();
        let mut guard = TRUE;
        for range in ranges {
            let start = self.text.partition_point(|t| t.range.end <= range.start);
            for text in self.text[start..]
                .iter()
                .take_while(|t| t.range.start < range.end)
            {
                let overlap = text.range.start.max(range.start)..text.range.end.min(range.end);
                let yes = conditions.value(text.guard, values);
                if yes {
                    bytes[overlap.clone()]
                        .copy_from_slice(&self.source.as_bytes()[overlap.clone()]);
                }
                // Already blank bytes do not distinguish projected text shapes.
                if self.source[overlap.clone()] != self.blank[overlap] {
                    let branch = if yes {
                        text.guard
                    } else {
                        conditions.not(text.guard)?
                    };
                    guard = conditions.and(guard, branch)?;
                }
            }
        }
        Ok(Projection {
            text: String::from_utf8(bytes)?,
            guard,
        })
    }
}
