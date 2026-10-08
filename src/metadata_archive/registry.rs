use super::*;
use crate::{acquisition, unity::UnityVersion};
use base64::Engine;
use serde_json::Value;
use std::collections::{BTreeSet, VecDeque};

pub(super) fn package(value: &Value) -> Result<Package> {
    let name = value["name"]
        .as_str()
        .context("Package has no name")?
        .to_owned();
    safe(&name)?;
    ensure!(!name.contains('/'), "Invalid package name");
    let version = value["version"]
        .as_str()
        .context("Package has no version")?
        .to_owned();
    let minimum_editor = value["unity"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|branch| {
            let release = value["unityRelease"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("0a0");
            if release.matches('.').count() == 2 {
                release.to_owned()
            } else {
                format!("{branch}.{release}")
            }
        });
    if let Some(version) = &minimum_editor {
        version.parse::<UnityVersion>()?;
    }
    let dependencies = value
        .get("dependencies")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?
        .unwrap_or_default();
    Ok(Package {
        name,
        version,
        minimum_editor,
        dependencies,
    })
}
pub(super) fn compatible(package: &Package, editor: UnityVersion) -> bool {
    package
        .minimum_editor
        .as_ref()
        .is_none_or(|v| v.parse::<UnityVersion>().is_ok_and(|v| v <= editor))
}
pub(super) fn version(value: &str) -> Result<semver::Version> {
    let (core, suffix) = value
        .find(['-', '+'])
        .map_or((value, ""), |i| (&value[..i], &value[i..]));
    let padded = format!(
        "{}{}{}",
        core,
        ".0".repeat(2usize.saturating_sub(core.matches('.').count())),
        suffix
    );
    Ok(padded.parse()?)
}
pub(super) fn profiles(editors: &[String]) -> Result<Vec<Vec<String>>> {
    let mut result = BTreeSet::from([Vec::new()]);
    for editor in editors {
        let editor: UnityVersion = editor.parse()?;
        for platform in [
            crate::unity::Platform::EditorLinux,
            crate::unity::Platform::StandaloneLinux,
        ] {
            for api in [3, 6] {
                for editor_only in [false, true] {
                    if editor_only && platform == crate::unity::Platform::StandaloneLinux {
                        continue;
                    }
                    for backend in ["ENABLE_MONO", "ENABLE_IL2CPP"] {
                        if platform == crate::unity::Platform::EditorLinux
                            && backend == "ENABLE_IL2CPP"
                        {
                            continue;
                        }
                        let mut symbols =
                            crate::unity::symbols::symbols(editor, platform, api, editor_only);
                        symbols.extend([
                            format!("UNITY_{}", editor.branch.major),
                            format!("UNITY_{}_{}", editor.branch.major, editor.branch.minor),
                            format!(
                                "UNITY_{}_{}_{}",
                                editor.branch.major, editor.branch.minor, editor.patch
                            ),
                            backend.into(),
                            "ENABLE_LEGACY_INPUT_MANAGER".into(),
                            "ENABLE_INPUT_SYSTEM".into(),
                        ]);
                        result.insert(symbols.iter().cloned().collect());
                        if platform == crate::unity::Platform::EditorLinux {
                            symbols.insert("UNITY_INCLUDE_TESTS".into());
                            result.insert(symbols.into_iter().collect());
                        }
                    }
                }
            }
        }
    }
    Ok(result.into_iter().collect())
}

#[derive(Clone, Archive, Serialize, Deserialize)]
pub(super) struct Request {
    pub origin: Origin,
    pub editors: Vec<String>,
}
#[derive(Default, Archive, Serialize, Deserialize)]
pub(super) struct Plan {
    pub requests: Vec<Request>,
    pub unavailable: Vec<String>,
    pub previous: Catalog,
}

pub(super) fn plan(editors: &[Artifact]) -> Result<Plan> {
    let client = acquisition::client()?;
    let mut cache = BTreeMap::<String, Option<Value>>::new();
    let mut requests = BTreeMap::<(String, String), Request>::new();
    let mut unavailable = BTreeSet::new();
    fn metadata<'a>(
        name: &str,
        client: &reqwest::blocking::Client,
        cache: &'a mut BTreeMap<String, Option<Value>>,
    ) -> Result<Option<&'a Value>> {
        if !cache.contains_key(name) {
            safe(name)?;
            let mut url = acquisition::https("https://packages.unity.com")?;
            url.path_segments_mut().unwrap().push(name);
            let response = client.get(url).send()?;
            let value = if matches!(response.status().as_u16(), 401 | 403 | 404) {
                None
            } else {
                Some(response.error_for_status()?.json()?)
            };
            cache.insert(name.into(), value);
        }
        Ok(cache.get(name).unwrap().as_ref())
    }
    for artifact in editors {
        let origin = &artifact.manifest.origin;
        let editor: UnityVersion = origin.version.parse()?;
        let bundled: BTreeMap<_, _> = artifact
            .manifest
            .packages
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();
        let mut pending = VecDeque::new();
        for local in bundled.values() {
            pending.extend(local.dependencies.clone());
        }
        for name in &artifact.manifest.package_names {
            let Some(node) = metadata(name, &client, &mut cache)? else {
                if !bundled.contains_key(name.as_str()) {
                    unavailable.insert(format!(
                        "{}: {name}: no public registry metadata",
                        origin.version
                    ));
                }
                continue;
            };
            let Some(versions) = node["versions"].as_object() else {
                continue;
            };
            for preview in [false, true] {
                let candidate = versions
                    .iter()
                    .filter_map(|(v, node)| {
                        let parsed = version(v).ok()?;
                        (preview != parsed.pre.is_empty()
                            && compatible(&package(node).ok()?, editor))
                        .then_some((parsed, v))
                    })
                    .max_by(|a, b| a.0.cmp(&b.0));
                if let Some((_, version)) = candidate {
                    pending.push_back((name.clone(), version.clone()));
                }
            }
        }
        let mut seen = BTreeSet::new();
        while let Some((name, mut selected)) = pending.pop_front() {
            if selected == "default" {
                selected = bundled
                    .get(name.as_str())
                    .map(|p| &p.version)
                    .or_else(|| artifact.manifest.recommended.get(&name))
                    .cloned()
                    .unwrap_or(selected);
            }
            if !seen.insert((name.clone(), selected.clone())) {
                continue;
            }
            if let Some(local) = bundled.get(name.as_str()).filter(|p| p.version == selected) {
                pending.extend(local.dependencies.clone());
                continue;
            }
            let node =
                metadata(&name, &client, &mut cache)?.and_then(|m| m["versions"].get(&selected));
            let Some(node) = node else {
                unavailable.insert(format!(
                    "{}: {name}@{selected}: dependency unavailable",
                    origin.version
                ));
                continue;
            };
            let info = package(node)?;
            if !compatible(&info, editor) {
                unavailable.insert(format!(
                    "{}: {name}@{selected}: requires a newer editor",
                    origin.version
                ));
            }
            let dist = &node["dist"];
            let integrity = dist["integrity"].as_str().map(str::to_owned).or_else(|| {
                dist["shasum"].as_str().map(|s| {
                    format!(
                        "sha1-{}",
                        base64::engine::general_purpose::STANDARD.encode(s)
                    )
                })
            });
            let request = requests
                .entry((name.clone(), selected.clone()))
                .or_insert_with(|| Request {
                    origin: Origin {
                        kind: "package".into(),
                        name,
                        version: selected,
                        revision: String::new(),
                        url: dist["tarball"].as_str().unwrap_or("").into(),
                        integrity,
                    },
                    editors: Vec::new(),
                });
            acquisition::https(&request.origin.url)?;
            if !request.editors.contains(&origin.version) {
                request.editors.push(origin.version.clone());
            }
            pending.extend(info.dependencies);
        }
    }
    Ok(Plan {
        requests: requests.into_values().collect(),
        unavailable: unavailable.into_iter().collect(),
        previous: Catalog::default(),
    })
}
