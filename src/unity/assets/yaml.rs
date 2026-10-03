//! Source-preserving YAML reader. Native parser ownership stays inside this module.
use anyhow::{Result, bail, ensure};
use std::{collections::BTreeMap, ops::Range};
use unsafe_libyaml as unsafe_yaml;

pub const VALUE_LIMIT: usize = 4096;
#[derive(Debug)]
pub enum Value {
    Scalar(String),
    Map(BTreeMap<String, Node>),
    List(Vec<(usize, Node)>),
}
#[derive(Debug)]
pub struct Node {
    pub value: Value,
    pub span: Range<usize>,
    pub reference_shape: bool,
}
impl Node {
    pub fn get(&self, name: &str) -> Option<&Node> {
        match &self.value {
            Value::Map(m) => m.get(name),
            _ => None,
        }
    }
    pub fn text(&self) -> &str {
        match &self.value {
            Value::Scalar(s) => s,
            _ => "",
        }
    }
    pub fn list(&self) -> &[(usize, Node)] {
        match &self.value {
            Value::List(v) => v,
            _ => &[],
        }
    }
    fn structural(&self) -> bool {
        match &self.value {
            Value::Map(map) => !map.is_empty(),
            Value::List(list) => !list.is_empty(),
            Value::Scalar(_) => false,
        }
    }
}
enum Event {
    Scalar(String, Range<usize>),
    Map(usize),
    List(usize),
    End(usize),
    Skip,
    Done,
}
struct Parser<'a> {
    raw: Box<unsafe_yaml::yaml_parser_t>,
    source: &'a str,
    ascii: bool,
    lines: Vec<usize>,
    nodes: usize,
    omissions: Vec<Range<usize>>,
}
impl Drop for Parser<'_> {
    fn drop(&mut self) {
        // SAFETY: initialized once, pinned in Box, exclusively owned until deletion.
        unsafe { unsafe_yaml::yaml_parser_delete(&mut *self.raw) }
    }
}
impl Parser<'_> {
    fn offset(&self, mark: unsafe_yaml::yaml_mark_t) -> usize {
        let start = self
            .lines
            .get(mark.line as usize)
            .copied()
            .unwrap_or(self.source.len());
        if self.ascii {
            return (start + mark.column as usize).min(self.source.len());
        }
        start
            + self.source[start..]
                .char_indices()
                .nth(mark.column as usize)
                .map_or(self.source.len() - start, |(i, _)| i)
    }
    fn next(&mut self) -> Result<Event> {
        let mut raw = std::mem::MaybeUninit::<unsafe_yaml::yaml_event_t>::uninit();
        // SAFETY: parser input is borrowed for its full lifetime; each successful event
        // is read using its tag and deleted exactly once before returning.
        unsafe {
            ensure!(
                !unsafe_yaml::yaml_parser_parse(&mut *self.raw, raw.as_mut_ptr()).fail,
                "Invalid Unity YAML"
            );
            let mut event = raw.assume_init();
            let start = self.offset(event.start_mark);
            let end = self.offset(event.end_mark);
            let result = match event.type_ {
                unsafe_yaml::YAML_SCALAR_EVENT => {
                    let scalar = event.data.scalar;
                    let bytes = std::slice::from_raw_parts(scalar.value, scalar.length as usize);
                    if end - start > VALUE_LIMIT || bytes.len() > VALUE_LIMIT {
                        self.omissions.push(start..end);
                        Ok(Event::Scalar(String::new(), start..end))
                    } else {
                        Ok(Event::Scalar(
                            String::from_utf8_lossy(bytes).into_owned(),
                            start..end,
                        ))
                    }
                }
                unsafe_yaml::YAML_MAPPING_START_EVENT => Ok(Event::Map(start)),
                unsafe_yaml::YAML_SEQUENCE_START_EVENT => Ok(Event::List(start)),
                unsafe_yaml::YAML_MAPPING_END_EVENT | unsafe_yaml::YAML_SEQUENCE_END_EVENT => {
                    Ok(Event::End(end))
                }
                unsafe_yaml::YAML_STREAM_END_EVENT => Ok(Event::Done),
                unsafe_yaml::YAML_ALIAS_EVENT => Err(anyhow::anyhow!(
                    "YAML aliases are unsupported in Unity assets"
                )),
                _ => Ok(Event::Skip),
            };
            unsafe_yaml::yaml_event_delete(&mut event);
            result
        }
    }
    fn node(&mut self, event: Event, depth: usize) -> Result<Node> {
        self.nodes += 1;
        ensure!(
            depth <= 128 && self.nodes <= 2_000_000,
            "Unity YAML structure exceeds parsing limits"
        );
        match event {
            Event::Scalar(s, span) => Ok(Node {
                value: Value::Scalar(s),
                span,
                reference_shape: false,
            }),
            Event::Map(start) => {
                let mut map = BTreeMap::new();
                let mut reference_shape = true;
                let mut keys = std::collections::BTreeSet::new();
                loop {
                    let event = self.next()?;
                    if let Event::End(end) = event {
                        return Ok(Node {
                            value: Value::Map(map),
                            span: start..end,
                            reference_shape,
                        });
                    }
                    let key = self.node(event, depth + 1)?;
                    ensure!(!key.text().is_empty(), "Empty or oversized Unity YAML key");
                    ensure!(
                        keys.insert(key.text().to_owned()),
                        "Duplicate Unity YAML key"
                    );
                    reference_shape &= matches!(key.text(), "fileID" | "guid" | "type");
                    let event = self.next()?;
                    let value = self.node(event, depth + 1)?;
                    if depth == 0
                        || value.structural()
                        || matches!(
                            key.text(),
                            "fileID" | "guid" | "type" | "m_Name" | "propertyPath" | "value"
                        )
                    {
                        map.insert(key.text().to_owned(), value);
                    }
                }
            }
            Event::List(start) => {
                let mut list = Vec::new();
                let mut index = 0;
                loop {
                    let event = self.next()?;
                    if let Event::End(end) = event {
                        return Ok(Node {
                            value: Value::List(list),
                            span: start..end,
                            reference_shape: false,
                        });
                    }
                    let node = self.node(event, depth + 1)?;
                    if node.structural() {
                        list.push((index, node));
                    }
                    index += 1;
                }
            }
            _ => bail!("Unexpected Unity YAML structure"),
        }
    }
}
pub fn parse(source: &str) -> Result<(Node, Vec<Range<usize>>)> {
    // SAFETY: libyaml initializes all parser fields before they are used.
    let mut raw = Box::<unsafe_yaml::yaml_parser_t>::new_uninit();
    unsafe {
        ensure!(
            !unsafe_yaml::yaml_parser_initialize(raw.as_mut_ptr()).fail,
            "Cannot initialize YAML parser"
        );
        unsafe_yaml::yaml_parser_set_input_string(
            raw.as_mut_ptr(),
            source.as_ptr(),
            source.len() as u64,
        );
    }
    let mut parser = Parser {
        raw: unsafe { raw.assume_init() },
        source,
        ascii: source.is_ascii(),
        lines: std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .collect(),
        nodes: 0,
        omissions: Vec::new(),
    };
    let root = loop {
        let event = parser.next()?;
        if matches!(event, Event::Skip) {
            continue;
        }
        break parser.node(event, 0)?;
    };
    loop {
        match parser.next()? {
            Event::Done => break,
            Event::Skip => (),
            _ => bail!("Multiple YAML roots"),
        }
    }
    Ok((root, std::mem::take(&mut parser.omissions)))
}
