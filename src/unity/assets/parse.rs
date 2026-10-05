use super::yaml::{self, Node, Value};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pointer {
    pub guid: String,
    pub id: i64,
}
impl Pointer {
    pub fn null(&self) -> bool {
        self.id == 0
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub target: Pointer,
    pub span: Range<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Modification {
    pub target: Pointer,
    pub property: String,
    pub reference: Pointer,
    pub value: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Prefab {
    pub source: Pointer,
    pub parent: Pointer,
    pub modifications: Vec<Modification>,
    pub removed: Vec<Pointer>,
    pub added: Vec<(Pointer, i64)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Object {
    pub id: i64,
    pub class: i32,
    pub stripped: bool,
    pub name: String,
    pub span: Range<usize>,
    pub line: usize,
    pub references: BTreeMap<String, Reference>,
    pub prefab: Option<Prefab>,
}
impl Object {
    pub fn pointer(&self, field: &str) -> Pointer {
        self.references
            .get(field)
            .map(|r| r.target.clone())
            .unwrap_or_default()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fragment {
    pub start: usize,
    pub line: usize,
    pub text: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Parsed {
    pub objects: Vec<Object>,
    pub fragments: Vec<Fragment>,
    pub omissions: Vec<Range<usize>>,
    pub bytes: usize,
}

fn pointer(node: &Node) -> Option<Pointer> {
    if !node.reference_shape {
        return None;
    }
    let Value::Map(map) = &node.value else {
        return None;
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "fileID" | "guid" | "type"))
    {
        return None;
    }
    Some(Pointer {
        id: map.get("fileID")?.text().parse().ok()?,
        guid: map
            .get("guid")
            .map(|n| n.text().to_ascii_lowercase())
            .unwrap_or_default(),
    })
}
fn field_pointer(node: &Node, field: &str) -> Pointer {
    node.get(field).and_then(pointer).unwrap_or_default()
}
fn references(node: &Node, path: &str, offset: usize, out: &mut BTreeMap<String, Reference>) {
    if let Some(target) = pointer(node) {
        out.insert(
            path.into(),
            Reference {
                target,
                span: node.span.start + offset..node.span.end + offset,
            },
        );
        return;
    }
    match &node.value {
        Value::Map(map) => {
            for (name, value) in map {
                references(
                    value,
                    &if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    },
                    offset,
                    out,
                );
            }
        }
        Value::List(list) => {
            for (i, value) in list {
                references(value, &format!("{path}.Array.data[{i}]"), offset, out);
            }
        }
        _ => (),
    }
}
fn prefab(node: &Node) -> Prefab {
    let mut prefab = Prefab {
        source: field_pointer(
            node,
            if node.get("m_SourcePrefab").is_some() {
                "m_SourcePrefab"
            } else {
                "m_ParentPrefab"
            },
        ),
        ..Default::default()
    };
    if let Some(mods) = node.get("m_Modification") {
        prefab.parent = field_pointer(mods, "m_TransformParent");
        if let Some(items) = mods.get("m_Modifications") {
            for (_, item) in items.list() {
                prefab.modifications.push(Modification {
                    target: field_pointer(item, "target"),
                    property: item
                        .get("propertyPath")
                        .map(|n| n.text().into())
                        .unwrap_or_default(),
                    reference: field_pointer(item, "objectReference"),
                    value: item
                        .get("value")
                        .map(|n| n.text().into())
                        .unwrap_or_default(),
                });
            }
        }
        for key in ["m_RemovedComponents", "m_RemovedGameObjects"] {
            if let Some(items) = mods.get(key) {
                prefab
                    .removed
                    .extend(items.list().iter().filter_map(|(_, node)| pointer(node)));
            }
        }
        for key in ["m_AddedComponents", "m_AddedGameObjects"] {
            if let Some(items) = mods.get(key) {
                for (_, item) in items.list() {
                    prefab.added.push((
                        field_pointer(item, "targetCorrespondingSourceObject"),
                        field_pointer(item, "addedObject").id,
                    ));
                }
            }
        }
    }
    prefab
}

pub fn parse(source: &str) -> Result<Parsed> {
    let mut result = Parsed {
        bytes: source.len(),
        ..Default::default()
    };
    let mut headers = Vec::new();
    let mut offset = 0;
    for (line, text) in source.split_inclusive('\n').enumerate() {
        if let Some(header) = text.strip_prefix("--- !u!") {
            let (class, rest) = header
                .split_once(" &")
                .context("Invalid Unity document header")?;
            let mut fields = rest.split_whitespace();
            let id = fields
                .next()
                .context("Missing Unity object ID")?
                .parse::<i64>()?;
            headers.push((
                offset,
                offset + text.len(),
                line + 1,
                class.parse::<i32>()?,
                id,
                fields.next() == Some("stripped"),
            ));
        }
        offset += text.len();
    }
    if headers.is_empty() {
        if source.trim_start().starts_with('{') {
            // Shader Graph uses a stream of JSON objects rather than one JSON document.
            let mut values =
                serde_json::Deserializer::from_str(source).into_iter::<serde::de::IgnoredAny>();
            let mut start = 0;
            while let Some(value) = values.next() {
                value?;
                let end = values.byte_offset();
                let (_, omissions) = yaml::parse(&source[start..end])?;
                result.omissions.extend(
                    omissions
                        .into_iter()
                        .map(|r| r.start + start..r.end + start),
                );
                start = end;
            }
        } else {
            let (_, omissions) = yaml::parse(source)?;
            result.omissions = omissions;
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    for (i, &(start, body, line, class, id, stripped)) in headers.iter().enumerate() {
        ensure!(ids.insert(id), "Duplicate serialized object ID");
        let end = headers.get(i + 1).map_or(source.len(), |h| h.0);
        let (root, omissions) = yaml::parse(&source[body..end])?;
        result
            .omissions
            .extend(omissions.into_iter().map(|r| r.start + body..r.end + body));
        let Value::Map(root) = root.value else {
            anyhow::bail!("Unity object has no mapping")
        };
        ensure!(root.len() == 1, "Unity object has multiple roots");
        let node = root.values().next().unwrap();
        let mut refs = BTreeMap::new();
        references(node, "", body, &mut refs);
        result.objects.push(Object {
            id,
            class,
            stripped,
            name: node
                .get("m_Name")
                .map(|n| n.text().into())
                .unwrap_or_default(),
            line,
            span: start..end,
            references: refs,
            prefab: (class == 1001).then(|| prefab(node)),
        });
    }
    result.omissions.sort_by_key(|r| r.start);
    let mut start = 0;
    let mut line = 1;
    for range in &result.omissions {
        ensure!(
            range.start >= start
                && source.is_char_boundary(range.start)
                && source.is_char_boundary(range.end),
            "Invalid YAML source range"
        );
        result.fragments.push(Fragment {
            start,
            line,
            text: source[start..range.start].into(),
        });
        line += source[start..range.end]
            .bytes()
            .filter(|c| *c == b'\n')
            .count();
        start = range.end;
    }
    result.fragments.push(Fragment {
        start,
        line,
        text: source[start..].into(),
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_prefab_sources_preserve_identity_and_modern_precedence() {
        let guid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for modern in ["", "  m_SourcePrefab: {fileID: 0}\n"] {
            let parsed = parse(&format!("--- !u!1001 &1\nPrefabInstance:\n  m_ParentPrefab: {{fileID: 100100000, guid: {guid}, type: 3}}\n{modern}")).unwrap();
            let source = &parsed.objects[0].prefab.as_ref().unwrap().source;
            if modern.is_empty() {
                assert_eq!(source.guid, guid);
            } else {
                assert!(source.null());
            }
        }
    }
    #[test]
    fn malformed_and_ambiguous_yaml_fails_without_poisoning_the_next_parse() {
        for body in [
            "  values: [1, 2\n",
            "  m_Name: First\n  m_Name: Second\n",
            "  value: &loop {child: *loop}\n",
        ] {
            let source = format!("--- !u!114 &1\nMonoBehaviour:\n{body}");
            assert!(parse(&source).is_err(), "{source}");
        }
        let valid = parse("--- !u!114 &1\nMonoBehaviour:\n  target: {fileID: 2}\n").unwrap();
        assert_eq!(valid.objects[0].references["target"].target.id, 2);
    }
    #[test]
    fn ordinary_arrays_are_not_retained_and_reference_positions_are_preserved() {
        let source = "--- !u!114 &1\nMonoBehaviour:\n  values: [1, 2, {target: {fileID: 7}}, 4]\n  userData: {fileID: 9, other: text}\n";
        let parsed = parse(source).unwrap();
        let references = &parsed.objects[0].references;
        assert!(references.contains_key("values.Array.data[2].target"));
        assert_eq!(references.len(), 1);
    }
    #[test]
    fn json_stream_values_are_trimmed_without_dropping_neighboring_objects() {
        let source = format!(
            "{{\"payload\":\"{}\"}}\n{{\"label\":\"KeepMe\"}}",
            "x".repeat(yaml::VALUE_LIMIT + 1)
        );
        let parsed = parse(&source).unwrap();
        assert_eq!(parsed.omissions.len(), 1);
        assert!(parsed.fragments.iter().any(|f| f.text.contains("KeepMe")));
    }
    #[test]
    fn nested_references_and_trimmed_unicode_preserve_source() {
        let source = format!(
            "%YAML 1.1\n--- !u!114 &9223372036854775806\nMonoBehaviour:\n  m_Name: Café\n  nested:\n    data: '{}'\n    refs:\n    - item: {{fileID: 12, guid: abcd, type: 2}}\n",
            "é".repeat(yaml::VALUE_LIMIT / 2 + 1)
        );
        let parsed = parse(&source).unwrap();
        assert_eq!(parsed.objects.len(), 1);
        assert_eq!(parsed.omissions.len(), 1);
        let reference = &parsed.objects[0].references["nested.refs.Array.data[0].item"];
        assert!(source[reference.span.clone()].contains("fileID"));
        assert!(parsed.fragments.iter().map(|f| f.text.len()).sum::<usize>() < source.len() / 2);
    }
}
