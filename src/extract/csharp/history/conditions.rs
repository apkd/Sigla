//! Bounded Boolean conditions over *unknown* initial symbols. A node denotes a
//! set of environments, not a selected project configuration.
use anyhow::{Result, ensure};
use std::{collections::HashMap, sync::LazyLock};

pub(super) type Guard = usize;
pub(super) const FALSE: Guard = 0;
pub(super) const TRUE: Guard = 1;
const SYMBOLS: usize = 128;
const NODES: usize = 8192;
const OPERATIONS: usize = 262_144;
const DEPTH: usize = 64;

#[derive(Clone, Copy)]
struct Node {
    variable: usize,
    low: Guard,
    high: Guard,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Op {
    And,
    Or,
    Xor,
}

pub(super) struct Conditions {
    nodes: Vec<Node>,
    unique: HashMap<(usize, Guard, Guard), Guard>,
    memo: HashMap<(Op, Guard, Guard), Guard>,
    symbols: HashMap<String, Guard>,
    work: usize,
}
impl Default for Conditions {
    fn default() -> Self {
        Self {
            nodes: vec![
                Node {
                    variable: SYMBOLS,
                    low: FALSE,
                    high: TRUE,
                };
                2
            ],
            unique: HashMap::new(),
            memo: HashMap::new(),
            symbols: HashMap::new(),
            work: 0,
        }
    }
}
impl Conditions {
    fn node(&mut self, variable: usize, low: Guard, high: Guard) -> Result<Guard> {
        if low == high {
            return Ok(low);
        }
        let key = (variable, low, high);
        if let Some(&id) = self.unique.get(&key) {
            return Ok(id);
        }
        ensure!(
            self.nodes.len() < NODES,
            "historical C# condition node limit reached"
        );
        let id = self.nodes.len();
        self.nodes.push(Node {
            variable,
            low,
            high,
        });
        self.unique.insert(key, id);
        Ok(id)
    }
    pub(super) fn symbol(&mut self, name: &str) -> Result<Guard> {
        if let Some(&id) = self.symbols.get(name) {
            return Ok(id);
        }
        ensure!(
            self.symbols.len() < SYMBOLS,
            "historical C# symbol limit reached"
        );
        let id = self.node(self.symbols.len(), FALSE, TRUE)?;
        self.symbols.insert(name.into(), id);
        Ok(id)
    }
    fn apply(&mut self, op: Op, mut a: Guard, mut b: Guard) -> Result<Guard> {
        self.work += 1;
        ensure!(
            self.work <= OPERATIONS,
            "historical C# condition work limit reached"
        );
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        match op {
            Op::And if a == FALSE || a == b => return Ok(a),
            Op::And if a == TRUE => return Ok(b),
            Op::Or if a == FALSE || a == b => return Ok(b),
            Op::Or if a == TRUE => return Ok(TRUE),
            Op::Xor if a == b => return Ok(FALSE),
            Op::Xor if a == FALSE => return Ok(b),
            _ => {}
        }
        let key = (op, a, b);
        if let Some(&id) = self.memo.get(&key) {
            return Ok(id);
        }
        let left = self.nodes[a];
        let right = self.nodes[b];
        let variable = left.variable.min(right.variable);
        let (al, ah) = if left.variable == variable {
            (left.low, left.high)
        } else {
            (a, a)
        };
        let (bl, bh) = if right.variable == variable {
            (right.low, right.high)
        } else {
            (b, b)
        };
        let low = self.apply(op, al, bl)?;
        let high = self.apply(op, ah, bh)?;
        let id = self.node(variable, low, high)?;
        // The operation counter bounds work even when the memo table is full.
        if self.memo.len() < NODES * 4 {
            self.memo.insert(key, id);
        }
        Ok(id)
    }
    pub(super) fn or(&mut self, a: Guard, b: Guard) -> Result<Guard> {
        self.apply(Op::Or, a, b)
    }
    pub(super) fn and(&mut self, a: Guard, b: Guard) -> Result<Guard> {
        self.apply(Op::And, a, b)
    }
    pub(super) fn not(&mut self, a: Guard) -> Result<Guard> {
        self.apply(Op::Xor, a, TRUE)
    }
    pub(super) fn witness(&self, mut guard: Guard, prefer: bool) -> Vec<bool> {
        let mut values = vec![prefer; SYMBOLS];
        while guard > TRUE {
            let node = self.nodes[guard];
            let yes = if prefer {
                node.high != FALSE
            } else {
                node.low == FALSE
            };
            values[node.variable] = yes;
            guard = if yes { node.high } else { node.low };
        }
        values
    }
    pub(super) fn value(&self, mut guard: Guard, values: &[bool]) -> bool {
        while guard > TRUE {
            let node = self.nodes[guard];
            guard = if values[node.variable] {
                node.high
            } else {
                node.low
            };
        }
        guard == TRUE
    }
    pub(super) fn expression(
        &mut self,
        text: &str,
        local: &HashMap<String, Guard>,
        symbolic: bool,
    ) -> Result<Guard> {
        ensure!(
            text.len() <= 4096,
            "historical C# expression length limit reached"
        );
        let mut parser = Expression {
            rest: text,
            conditions: self,
            local,
            symbolic,
        };
        let result = parser.or(0)?;
        ensure!(
            parser.rest.trim().is_empty(),
            "unsupported historical C# condition"
        );
        Ok(result)
    }
}

/// Immutable source-local membership. Retain only nodes, not the scanner's
/// symbol table, unique table, or operation cache, in historical source facts.
#[derive(Default)]
pub(crate) struct Presence {
    nodes: Vec<Node>,
}
impl Presence {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
    }
    pub(crate) fn coverage(&self) -> Coverage {
        let mut conditions = Conditions::default();
        if !self.nodes.is_empty() {
            conditions.nodes.clone_from(&self.nodes);
            conditions.unique = self
                .nodes
                .iter()
                .enumerate()
                .skip(2)
                .map(|(id, n)| ((n.variable, n.low, n.high), id))
                .collect();
        }
        Coverage {
            conditions,
            covered: FALSE,
        }
    }
}
impl Conditions {
    pub(super) fn finish(self) -> Presence {
        Presence { nodes: self.nodes }
    }
}

/// Query-local, bounded scratch space. Equal-sized positional candidates must
/// all be tested before adding their guards to the coverage of shorter spans.
pub(crate) struct Coverage {
    conditions: Conditions,
    covered: Guard,
}
impl Coverage {
    pub(crate) fn uncovered(&mut self, guard: usize) -> Result<bool> {
        let remaining = self.conditions.not(self.covered)?;
        Ok(self.conditions.and(guard, remaining)? != FALSE)
    }
    pub(crate) fn add(&mut self, guard: usize) -> Result<()> {
        self.covered = self.conditions.or(self.covered, guard)?;
        Ok(())
    }
}

static IDENTIFIER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^(?:[_\p{L}\p{Nl}]|\\u[0-9a-fA-F]{4}|\\U[0-9a-fA-F]{8})(?:[_\p{L}\p{Nl}\p{Nd}\p{Pc}\p{Mn}\p{Mc}\p{Cf}]|\\u[0-9a-fA-F]{4}|\\U[0-9a-fA-F]{8})*").unwrap()
});
static FORMATTING: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\p{Cf}").unwrap());

pub(super) fn identifier(text: &str) -> Result<(String, usize)> {
    let matched = IDENTIFIER
        .find(text)
        .ok_or_else(|| anyhow::anyhow!("invalid historical C# condition symbol"))?;
    let mut rest = matched.as_str();
    let mut name = String::new();
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap();
        if c == '\\' {
            let width = if rest.as_bytes()[1] == b'u' { 6 } else { 10 };
            let value = u32::from_str_radix(&rest[2..width], 16)?;
            name.push(
                char::from_u32(value).ok_or_else(|| anyhow::anyhow!("invalid C# symbol escape"))?,
            );
            rest = &rest[width..];
        } else {
            name.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    let name = FORMATTING.replace_all(&name, "").into_owned();
    ensure!(
        IDENTIFIER
            .find(&name)
            .is_some_and(|m| m.end() == name.len()),
        "invalid C# symbol escape"
    );
    Ok((name, matched.end()))
}

struct Expression<'a> {
    rest: &'a str,
    conditions: &'a mut Conditions,
    local: &'a HashMap<String, Guard>,
    symbolic: bool,
}
impl Expression<'_> {
    fn eat(&mut self, token: &str) -> bool {
        self.rest = self.rest.trim_start();
        if let Some(rest) = self.rest.strip_prefix(token) {
            self.rest = rest;
            true
        } else {
            false
        }
    }
    fn atom(&mut self, depth: usize) -> Result<Guard> {
        ensure!(
            depth < DEPTH,
            "historical C# expression nesting limit reached"
        );
        if self.eat("!") {
            let value = self.atom(depth + 1)?;
            return self.conditions.not(value);
        }
        if self.eat("(") {
            let value = self.or(depth + 1)?;
            ensure!(self.eat(")"), "unclosed historical C# condition");
            return Ok(value);
        }
        self.rest = self.rest.trim_start();
        let (name, len) = identifier(self.rest)?;
        self.rest = &self.rest[len..];
        match name.as_str() {
            "true" => Ok(TRUE),
            "false" => Ok(FALSE),
            _ if !self.symbolic => Ok(FALSE),
            _ => match self.local.get(&name) {
                Some(&value) => Ok(value),
                None => self.conditions.symbol(&name),
            },
        }
    }
    fn equality(&mut self, depth: usize) -> Result<Guard> {
        let mut value = self.atom(depth)?;
        loop {
            let equal = if self.eat("==") {
                true
            } else if self.eat("!=") {
                false
            } else {
                return Ok(value);
            };
            let right = self.atom(depth)?;
            value = self.conditions.apply(Op::Xor, value, right)?;
            if equal {
                value = self.conditions.not(value)?;
            }
        }
    }
    fn and(&mut self, depth: usize) -> Result<Guard> {
        let mut value = self.equality(depth)?;
        while self.eat("&&") {
            let right = self.equality(depth)?;
            value = self.conditions.and(value, right)?;
        }
        Ok(value)
    }
    fn or(&mut self, depth: usize) -> Result<Guard> {
        let mut value = self.and(depth)?;
        while self.eat("||") {
            let right = self.and(depth)?;
            value = self.conditions.apply(Op::Or, value, right)?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_guards_preserve_correlations_and_precedence() {
        let mut conditions = Conditions::default();
        let local = HashMap::new();
        for (expression, expected) in [
            ("A && !A", FALSE),
            ("A || !A", TRUE),
            ("A == A", TRUE),
            ("A != A", FALSE),
            ("false && A || true", TRUE),
            ("(A && B) == (B && A)", TRUE),
            (r"A == \u0041", TRUE),
        ] {
            assert_eq!(
                conditions.expression(expression, &local, true).unwrap(),
                expected,
                "{expression}"
            );
        }
        let guard = conditions
            .expression("A != B && (!C || A)", &local, true)
            .unwrap();
        for prefer in [false, true] {
            assert!(conditions.value(guard, &conditions.witness(guard, prefer)));
        }
    }

    #[test]
    fn unsupported_or_excessively_nested_expressions_are_not_guesses() {
        for text in [
            "A & B".into(),
            "defined(A)".into(),
            "0".into(),
            format!("{}A", "!".repeat(1000)),
        ] {
            assert!(
                Conditions::default()
                    .expression(&text, &HashMap::new(), true)
                    .is_err()
            );
        }
    }
}
