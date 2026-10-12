//! Build a native class catalog from saved Unity documentation and editor metadata.
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use regex::Regex;
use serde::{Deserialize, Serialize, de};
use sha2::{Digest, Sha256};
use sigla::unity::{ReleaseBranch, UnityVersion};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Options {
    #[arg(long)]
    branch: ReleaseBranch,
    /// Saved official ClassIDReference HTML for this release branch.
    #[arg(long)]
    class_ids: PathBuf,
    /// JSON from the unity_native_metadata example.
    #[arg(long)]
    managed: PathBuf,
    #[arg(long, default_value = "src/unity/assets/native/bindings.json")]
    bindings: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
struct Definition {
    assembly: String,
    base: Option<String>,
}
#[derive(Deserialize)]
struct Metadata {
    #[serde(deserialize_with = "editor_version")]
    version: UnityVersion,
    sources: Vec<String>,
    #[serde(deserialize_with = "unique_map")]
    types: BTreeMap<String, Option<Definition>>,
}
#[derive(Deserialize)]
struct Bindings(#[serde(deserialize_with = "unique_map")] BTreeMap<String, String>);
#[derive(Serialize)]
struct Catalog {
    branch: String,
    sources: Vec<String>,
    classes: Vec<(i32, String, Option<String>)>,
    types: BTreeMap<String, Definition>,
}

fn editor_version<'de, D: de::Deserializer<'de>>(
    deserializer: D,
) -> Result<UnityVersion, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(de::Error::custom)
}

fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: de::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Visitor<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an object with unique keys")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = input.next_entry::<String, T>()? {
                match result.entry(key) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(value);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => {
                        return Err(de::Error::custom(format!("Duplicate key: {}", entry.key())));
                    }
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 32 * 1024 * 1024,
        "Catalog input is too large: {}",
        path.display()
    );
    Ok(bytes)
}

fn class_ids(branch: ReleaseBranch, html: &str) -> Result<BTreeMap<i32, String>> {
    let labels = Regex::new(r"Version:\s*<b>(?:Unity [^<]+|(\d+\.\d+))</b>(?:\s*\((\d+\.\d+)\))?")?;
    let versions: BTreeSet<_> = labels
        .captures_iter(html)
        .filter_map(|c| c.get(2).or_else(|| c.get(1)).map(|m| m.as_str().to_owned()))
        .collect();
    ensure!(
        versions == BTreeSet::from([branch.to_string()]),
        "Documentation version does not match {branch}"
    );
    let rows = Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>")?;
    let cells = Regex::new(r"(?is)<td\b[^>]*>(.*?)</td>")?;
    let tooltip = Regex::new(
        r#"(?is)<span\b[^>]*class=["'][^"']*\btooltiptext\b[^"']*["'][^>]*>.*?</span>"#,
    )?;
    let tags = Regex::new(r"(?s)<[^>]*>")?;
    let name_pattern = Regex::new(r"^[A-Za-z_][A-Za-z_0-9:.]*$")?;
    let mut result = BTreeMap::new();
    for row in rows.captures_iter(html) {
        let values: Vec<_> = cells
            .captures_iter(&row[1])
            .map(|cell| {
                let visible = tooltip.replace_all(&cell[1], "");
                tags.replace_all(&visible, "").trim().to_owned()
            })
            .collect();
        if values.len() != 2
            || values[0].is_empty()
            || !values[0].bytes().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let id: i32 = values[0]
            .parse()
            .context("Native class ID is out of range")?;
        ensure!(
            name_pattern.is_match(&values[1]),
            "Invalid native class name: {}",
            values[1]
        );
        ensure!(
            result.insert(id, values[1].clone()).is_none(),
            "Duplicate native class ID: {id}"
        );
    }
    ensure!(
        result.get(&1).map(String::as_str) == Some("GameObject")
            && result.get(&114).map(String::as_str) == Some("MonoBehaviour"),
        "No recognized native class table"
    );
    Ok(result)
}

fn build(
    branch: ReleaseBranch,
    html: &str,
    managed: Metadata,
    bindings: Bindings,
) -> Result<Catalog> {
    ensure!(
        managed.version.branch == branch && managed.version.stable(),
        "Managed editor version does not match stable branch {branch}"
    );
    ensure!(
        !managed.sources.is_empty(),
        "Missing editor metadata provenance"
    );
    let mut catalog = Catalog {
        branch: branch.to_string(),
        sources: vec![
            format!("https://docs.unity3d.com/{branch}/Documentation/Manual/ClassIDReference.html"),
            format!("Managed editor metadata: {}", managed.version),
        ],
        classes: Vec::new(),
        types: BTreeMap::new(),
    };
    catalog.sources.extend(managed.sources);
    let mut names = BTreeSet::new();
    for (id, native) in class_ids(branch, html)? {
        let name = (id != 114).then(|| bindings.0.get(&native)).flatten();
        if let Some(name) = name {
            ensure!(
                names.insert(native.clone()),
                "Multiple native IDs for {native}"
            );
            ensure!(
                name.starts_with("UnityEngine.") || name.starts_with("UnityEditor."),
                "Invalid managed identity: {name}"
            );
            let mut current = name.as_str();
            let mut seen = BTreeSet::new();
            loop {
                ensure!(
                    seen.len() < 64 && seen.insert(current),
                    "Cyclic or excessive ancestry: {name}"
                );
                let definition = managed
                    .types
                    .get(current)
                    .and_then(Option::as_ref)
                    .with_context(|| format!("Missing or ambiguous metadata: {current}"))?;
                ensure!(
                    !definition.assembly.is_empty(),
                    "Missing assembly: {current}"
                );
                catalog.types.insert(current.into(), definition.clone());
                if current == "UnityEngine.Object" {
                    break;
                }
                current = definition.base.as_deref().context("Missing managed base")?;
                if current == "System.Object" {
                    bail!("{name} does not derive from UnityEngine.Object");
                }
            }
        }
        catalog.classes.push((id, native, name.cloned()));
    }
    Ok(catalog)
}

fn render(catalog: &Catalog) -> Result<String> {
    use std::fmt::Write;
    let mut text = format!(
        "{{\n  \"branch\": {},\n  \"sources\": {},\n  \"classes\": [\n",
        serde_json::to_string(&catalog.branch)?,
        serde_json::to_string(&catalog.sources)?
    );
    for (i, class) in catalog.classes.iter().enumerate() {
        writeln!(
            text,
            "    {}{}",
            serde_json::to_string(class)?,
            if i + 1 < catalog.classes.len() {
                ","
            } else {
                ""
            }
        )?;
    }
    text.push_str("  ],\n  \"types\": {\n");
    for (i, (name, definition)) in catalog.types.iter().enumerate() {
        writeln!(
            text,
            "    {}: {}{}",
            serde_json::to_string(name)?,
            serde_json::to_string(definition)?,
            if i + 1 < catalog.types.len() { "," } else { "" }
        )?;
    }
    text.push_str("  }\n}\n");
    Ok(text)
}

fn main() -> Result<()> {
    let options = Options::parse();
    let html = read(&options.class_ids)?;
    let managed = read(&options.managed)?;
    let bindings = read(&options.bindings)?;
    let mut catalog = build(
        options.branch,
        std::str::from_utf8(&html)?,
        serde_json::from_slice(&managed)?,
        serde_json::from_slice(&bindings)?,
    )?;
    for (label, bytes) in [
        ("ClassIDReference HTML", &html),
        ("Managed JSON", &managed),
        ("Audited bindings", &bindings),
    ] {
        catalog
            .sources
            .push(format!("{label} SHA-256: {:x}", Sha256::digest(bytes)));
    }
    std::fs::write(options.output, render(&catalog)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(version: &str, rows: &str) -> String {
        format!(
            "Version: <b>{version}</b><table><tr><td>1</td><td>GameObject</td></tr><tr><td>114</td><td>MonoBehaviour</td></tr>{rows}</table>"
        )
    }
    fn managed() -> Metadata {
        Metadata {
            version: "6000.0.1f1".parse().unwrap(),
            sources: vec!["fixture.dll".into()],
            types: BTreeMap::from([
                (
                    "UnityEngine.Object".into(),
                    Some(Definition {
                        assembly: "UnityEngine.CoreModule".into(),
                        base: Some("System.Object".into()),
                    }),
                ),
                (
                    "UnityEngine.GameObject".into(),
                    Some(Definition {
                        assembly: "UnityEngine.CoreModule".into(),
                        base: Some("UnityEngine.Object".into()),
                    }),
                ),
            ]),
        }
    }
    fn bindings() -> Bindings {
        Bindings(BTreeMap::from([
            ("GameObject".into(), "UnityEngine.GameObject".into()),
            ("MonoBehaviour".into(), "UnityEngine.MonoBehaviour".into()),
        ]))
    }
    #[test]
    fn generated_catalog_preserves_ids_without_inventing_scripted_or_unknown_types() {
        let catalog = build(
            "6000.0".parse().unwrap(),
            &html(
                "Unity 6</b> (6000.0)<b>",
                "<tr><td>999</td><td>Unknown</td></tr>",
            ),
            managed(),
            bindings(),
        )
        .unwrap();
        assert_eq!(
            catalog.classes[0].2.as_deref(),
            Some("UnityEngine.GameObject")
        );
        assert!(catalog.classes[1..].iter().all(|c| c.2.is_none()));
        let rendered: serde_json::Value = serde_json::from_str(&render(&catalog).unwrap()).unwrap();
        assert_eq!(
            rendered["types"]["UnityEngine.GameObject"]["base"],
            "UnityEngine.Object"
        );
    }
    #[test]
    fn documentation_and_metadata_versions_are_checked_independently() {
        let branch = "6000.0".parse().unwrap();
        assert!(build(branch, &html("2022.3", ""), managed(), bindings()).is_err());
        let mut metadata = managed();
        metadata.version = "2022.3.1f1".parse().unwrap();
        assert!(build(branch, &html("6000.0", ""), metadata, bindings()).is_err());
    }
    #[test]
    fn duplicates_and_incomplete_ancestry_are_rejected() {
        let branch = "6000.0".parse().unwrap();
        assert!(class_ids(branch, &html("6000.0", "<tr><td>1</td><td>Other</td></tr>")).is_err());
        assert!(
            serde_json::from_str::<Bindings>(r#"{"GameObject":"A","GameObject":"B"}"#).is_err()
        );
        for base in [
            None,
            Some("System.Object"),
            Some("UnityEngine.Missing"),
            Some("UnityEngine.GameObject"),
        ] {
            let mut metadata = managed();
            metadata
                .types
                .get_mut("UnityEngine.GameObject")
                .unwrap()
                .as_mut()
                .unwrap()
                .base = base.map(Into::into);
            assert!(build(branch, &html("6000.0", ""), metadata, bindings()).is_err());
        }
    }
}
