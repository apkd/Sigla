use anyhow::{Context, Result, ensure};
use serde_yaml::Value;
use std::path::Path;

pub struct Settings {
    pub defines: Vec<String>,
    pub api: u32,
    pub backend: u32,
    pub input: u32,
    pub editor_api: u32,
}

pub fn yaml(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Missing Unity input {}", path.display()))?;
    let text = text
        .lines()
        .filter(|l| !l.starts_with('%'))
        .map(|l| {
            if l.starts_with("--- !u!") {
                return "---".to_owned();
            }
            // Unity GUIDs are strings even when all 32 hexadecimal digits happen
            // to be decimal digits. YAML's integer resolver cannot represent them.
            if let Some(value) = l.trim_start().strip_prefix("guid: ")
                && value.len() == 32
                && value.bytes().all(|c| c.is_ascii_hexdigit())
            {
                return format!("{}guid: '{value}'", &l[..l.len() - l.trim_start().len()]);
            }
            l.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    serde_yaml::from_str(&text).with_context(|| format!("Malformed Unity input {}", path.display()))
}

fn standalone(value: &Value) -> Option<&Value> {
    if let Some(map) = value.as_mapping() {
        map.get(Value::String("Standalone".into()))
            .or_else(|| map.get(Value::Number(1.into())))
            .or_else(|| map.get(Value::String("1".into())))
    } else if let Some(entries) = value.as_sequence() {
        entries
            .iter()
            .find(|entry| {
                entry["first"].as_str() == Some("Standalone") || entry["first"].as_i64() == Some(1)
            })
            .map(|entry| &entry["second"])
    } else {
        None
    }
}

impl Settings {
    pub fn read(path: &Path) -> Result<Self> {
        let document = yaml(path)?;
        let settings = &document["PlayerSettings"];
        ensure!(
            settings.is_mapping(),
            "Unity settings do not contain PlayerSettings"
        );
        let number = |name: &str, fallback: u32| -> Result<u32> {
            match settings.get(name) {
                None | Some(Value::Null) => Ok(fallback),
                Some(v) => v
                    .as_u64()
                    .and_then(|v| v.try_into().ok())
                    .with_context(|| format!("Invalid Unity setting {name}")),
            }
        };
        let target_number = |name: &str, fallback: u32| -> Result<u32> {
            match standalone(&settings[name]) {
                None => Ok(fallback),
                Some(v) => v
                    .as_u64()
                    .and_then(|v| v.try_into().ok())
                    .with_context(|| format!("Invalid Standalone Unity setting {name}")),
            }
        };
        let defines = match standalone(&settings["scriptingDefineSymbols"]) {
            Some(Value::String(value)) => value
                .split(';')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(Value::Sequence(values)) => values
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .context("Invalid scripting define")
                })
                .collect::<Result<_>>()?,
            None => Vec::new(),
            _ => anyhow::bail!("Invalid Standalone scripting defines"),
        };
        let api = target_number(
            "apiCompatibilityLevelPerPlatform",
            number("apiCompatibilityLevel", 6)?,
        )?;
        ensure!(
            matches!(api, 3 | 6),
            "Unsupported Unity API compatibility setting {api}"
        );
        let backend = target_number("scriptingBackend", 0)?;
        ensure!(
            backend <= 1,
            "Unsupported Standalone scripting backend {backend}"
        );
        let input = number("activeInputHandler", 0)?;
        ensure!(input <= 2, "Invalid Unity input-system setting {input}");
        let editor_api = number("editorAssembliesCompatibilityLevel", 1)?;
        ensure!(
            (1..=3).contains(&editor_api),
            "Unsupported Editor assembly compatibility setting {editor_api}"
        );
        Ok(Self {
            defines,
            api,
            backend,
            input,
            editor_api,
        })
    }
}
