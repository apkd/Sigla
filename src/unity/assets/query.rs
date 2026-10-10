use super::{Asset, Index, compose::Instance, content};
use crate::query::{Query, qualified_name_rank};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::fmt::Write;

fn handle(asset: &str, id: &str) -> String {
    format!("unity@{}#{id}", URL_SAFE_NO_PAD.encode(asset))
}
fn decode(input: &str) -> Option<(String, String)> {
    let (asset, id) = input.strip_prefix("unity@")?.split_once('#')?;
    Some((
        String::from_utf8(URL_SAFE_NO_PAD.decode(asset).ok()?).ok()?,
        id.into(),
    ))
}
fn selected(asset: &Asset, q: &Query) -> bool {
    q.filters.iter().all(|f| {
        let matches = match f.key.as_str() {
            "path" => {
                globset::Glob::new(&f.value)
                    .is_ok_and(|g| g.compile_matcher().is_match(&asset.path))
                    || asset
                        .path
                        .strip_prefix(f.value.trim_end_matches('/'))
                        .is_some_and(|s| s.starts_with('/'))
            }
            "unity-project" => crate::query::wildcard(&f.value, &asset.project),
            _ => return true,
        };
        matches != f.negate
    })
}
fn describe(
    index: &Index,
    key: &str,
    object: &Instance,
    composition: &super::compose::Composed,
) -> String {
    let asset = &index.assets[key];
    let ty = object
        .ty
        .as_ref()
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("Unity class {} (type unresolved)", object.object.class));
    let owner = object
        .references
        .get("m_GameObject")
        .and_then(|t| {
            (if t.asset == key {
                Some(composition)
            } else {
                index.instances.get(&t.asset).map(|c| c.as_ref())
            })?
            .objects
            .iter()
            .find(|o| Some(&o.id) == t.object.as_ref() && o.alive)
        })
        .map(|o| o.name.as_str())
        .unwrap_or(&object.name);
    format!(
        "{ty} {} — {} [{}]\n  {}",
        crate::render::inline(owner),
        asset.path,
        if object.evidence.is_empty() {
            "definition"
        } else {
            "prefab instance"
        },
        handle(key, &object.id)
    )
}
impl Index {
    pub(crate) fn visit_text(
        &self,
        q: &Query,
        mut visit: impl FnMut(usize, crate::render::SearchResult) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            q.target.name.len() <= super::yaml::VALUE_LIMIT,
            "Asset text query exceeds value size limit"
        );
        let literal = regex::RegexBuilder::new(&regex::escape(&q.target.name))
            .case_insensitive(q.loose)
            .build()?;
        for (key, asset) in &self.assets {
            if !selected(asset, q) || asset.content.is_none() {
                continue;
            }
            let parsed = content(asset)?;
            let mut previous = None;
            for fragment in &parsed.fragments {
                for matched in literal.find_iter(&fragment.text) {
                    let start = fragment.start + matched.start();
                    let line = fragment.line
                        + fragment.text[..matched.start()]
                            .bytes()
                            .filter(|b| *b == b'\n')
                            .count();
                    if previous == Some(line) {
                        continue;
                    }
                    previous = Some(line);
                    let owner = asset.objects.iter().find(|o| {
                        o.span.start <= start && fragment.start + matched.end() <= o.span.end
                    });
                    let mut excerpt_start = fragment.text[..matched.start()]
                        .rfind('\n')
                        .map_or(0, |i| i + 1)
                        .max(matched.start().saturating_sub(120));
                    while !fragment.text.is_char_boundary(excerpt_start) {
                        excerpt_start += 1;
                    }
                    let excerpt = fragment.text[excerpt_start..]
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(512)
                        .collect();
                    visit(
                        start,
                        crate::render::SearchResult {
                            symbol: owner.map(|o| ("Unity object".into(), o.name.clone())),
                            path: asset.path.clone(),
                            lines: Some((line, line)),
                            source: excerpt,
                            language: crate::model::Language::Text,
                            uncertain: false,
                            target: owner.map(|o| handle(key, &o.id.to_string())),
                            occurrences: Default::default(),
                            candidates: Vec::new(),
                            possible_write: false,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
    fn target(&self, text: &str) -> Result<(String, Option<String>)> {
        if let Some((key, id)) = decode(text) {
            ensure!(
                self.assets.contains_key(&key),
                "Object belongs to a different workspace"
            );
            return Ok((key, Some(id)));
        }
        let matches: Vec<_> = self
            .assets
            .iter()
            .filter(|(_, a)| a.path == text || a.guid == text)
            .collect();
        ensure!(!matches.is_empty(), "Asset not found: {text}");
        ensure!(
            matches.len() == 1,
            "Asset is ambiguous; use an object identifier from search results"
        );
        Ok((matches[0].0.clone(), None))
    }
    pub fn search(&self, q: &Query) -> Result<String> {
        let mut lines = Vec::new();
        let mut incomplete = false;
        let mut unresolved_types = false;
        let mut unresolved_native_types = false;
        let mut unavailable = 0;
        let target = matches!(q.selector.as_str(), "references" | "dependencies")
            .then(|| self.target(&q.target.name))
            .transpose()?;
        for (key, asset) in &self.assets {
            if !selected(asset, q) {
                continue;
            }
            if q.selector == "dependencies"
                && target.as_ref().is_some_and(|(asset, _)| asset != key)
            {
                continue;
            }
            unavailable += usize::from(asset.unavailable.is_some());
            if lines.len() <= q.limit
                && let Some((target_asset, target_object)) = &target
            {
                for control in asset.objects.iter().filter(|o| o.prefab.is_some()) {
                    let prefab = control.prefab.as_ref().unwrap();
                    let Some(source) = self.resolve(&asset.project, &prefab.source.guid) else {
                        continue;
                    };
                    let matches = if q.selector == "references" {
                        source == target_asset && target_object.is_none()
                    } else {
                        key == target_asset
                            && target_object
                                .as_ref()
                                .is_none_or(|id| id == &control.id.to_string())
                    };
                    if matches {
                        lines.push(format!(
                            "Prefab placement {}:{}\n  {}\n  m_SourcePrefab -> {}",
                            asset.path,
                            control.line,
                            handle(key, &control.id.to_string()),
                            self.assets[source].path
                        ));
                        if lines.len() > q.limit {
                            break;
                        }
                    }
                }
            }
            if !asset.objects.is_empty() {
                let composition = self.composed(key)?;
                incomplete |= !composition.incomplete.is_empty();
                unresolved_types |= composition
                    .objects
                    .iter()
                    .any(|o| o.alive && o.object.class == 114 && o.ty.is_none());
                unresolved_native_types |= composition
                    .objects
                    .iter()
                    .any(|o| o.alive && o.object.class != 114 && o.ty.is_none());
                if lines.len() > q.limit {
                    continue;
                }
                for object in composition.objects.iter().filter(|o| o.alive) {
                    if q.selector == "instance" {
                        let Some(ty) = &object.ty else {
                            continue;
                        };
                        if !ty.ancestry.iter().any(|s| {
                            matches!(
                                s.as_str(),
                                "UnityEngine.Component" | "UnityEngine.ScriptableObject"
                            )
                        }) {
                            continue;
                        }
                        let exact = q
                            .filters
                            .iter()
                            .any(|f| f.key == "type-match" && f.value == "exact");
                        let names = if exact {
                            std::slice::from_ref(&ty.name)
                        } else {
                            &ty.ancestry
                        };
                        if names.iter().any(|name| {
                            qualified_name_rank(&q.target.name, name, q.loose).is_some()
                        }) {
                            lines.push(describe(self, key, object, &composition));
                        }
                    } else if let Some((target_asset, target_object)) = &target {
                        if q.selector == "dependencies"
                            && (key != target_asset
                                || target_object.as_ref().is_some_and(|id| id != &object.id))
                        {
                            continue;
                        }
                        for (field, destination) in &object.references {
                            if destination.raw.null() {
                                continue;
                            }
                            if q.selector == "references"
                                && (&destination.asset != target_asset
                                    || target_object
                                        .as_ref()
                                        .is_some_and(|id| destination.object.as_ref() != Some(id)))
                            {
                                continue;
                            }
                            // A removed target is a missing reference, not a surviving object match.
                            if let (Some(id), Some(targets)) = (
                                &destination.object,
                                if destination.asset == *key {
                                    Some(&composition)
                                } else {
                                    self.instances.get(&destination.asset)
                                },
                            ) && targets.objects.iter().any(|o| &o.id == id && !o.alive)
                            {
                                continue;
                            }
                            let to = self
                                .assets
                                .get(&destination.asset)
                                .map(|a| a.path.clone())
                                .unwrap_or_else(|| {
                                    format!("unresolved GUID {}", destination.raw.guid)
                                });
                            let object_id = destination
                                .object
                                .as_ref()
                                .map(|id| handle(&destination.asset, id))
                                .unwrap_or_default();
                            lines.push(format!(
                                "{}\n  {field} -> {to} {object_id}",
                                describe(self, key, object, &composition)
                            ));
                            if lines.len() > q.limit {
                                break;
                            }
                        }
                    }
                    if lines.len() > q.limit {
                        break;
                    }
                }
            }
        }
        let more = lines.len() > q.limit;
        lines.truncate(q.limit);
        if lines.is_empty() {
            lines.push("No resolved asset matches found.".into());
        }
        if more {
            lines.push("More asset matches exist; increase limit: to see them.".into());
        }
        let mut reasons = Vec::new();
        if unavailable > 0 {
            reasons.push(format!("{unavailable} unavailable assets"));
        }
        if unresolved_types {
            reasons.push("unresolved script types".into());
        }
        if unresolved_native_types {
            reasons.push("unresolved native types".into());
        }
        if incomplete {
            reasons.push("incomplete prefab composition".into());
        }
        if !reasons.is_empty() {
            lines.push(format!(
                "Coverage is incomplete ({}; inspect asset views for details).",
                reasons.join("; ")
            ));
        }
        Ok(lines.join("\n\n"))
    }
    pub fn view(&self, path: &str) -> Result<Option<String>> {
        if let Some((asset, id)) = decode(path) {
            let source = self
                .assets
                .get(&asset)
                .context("Object asset unavailable")?;
            if source.objects.is_empty() {
                return Ok(Some(format!(
                    "{}\nGUID: {}\nObject ID: {id}\n{}",
                    source.path,
                    source.guid,
                    source
                        .unavailable
                        .as_deref()
                        .unwrap_or("Target object contents are not inspectable")
                )));
            }
            let composition = self.composed(&asset)?;
            let mapped = id
                .parse::<i64>()
                .ok()
                .and_then(|id| composition.aliases.get(&id))
                .unwrap_or(&id);
            let Some(object) = composition
                .objects
                .iter()
                .find(|o| &o.id == mapped && o.alive)
            else {
                if let Some(document) = source
                    .objects
                    .iter()
                    .find(|o| o.id.to_string() == id && o.prefab.is_some())
                {
                    return Ok(Some(self.saved(source, Some(document.span.clone()))?));
                }
                return Ok(Some(format!(
                    "{}\nObject ID: {id}\nNo surviving saved object is available for this target",
                    source.path
                )));
            };
            let mut result = describe(self, &asset, object, &composition);
            let definition = &self.assets[&object.definition];
            writeln!(
                result,
                "\n\nSaved definition: {}:{}",
                definition.path, object.object.line
            )?;
            result.push_str(&self.saved(definition, Some(object.object.span.clone()))?);
            for (source, id) in &object.evidence {
                if result.len() >= 64 * 1024 {
                    result.push_str("\n[View truncated]\n");
                    break;
                }
                let source = &self.assets[source];
                if let Some(control) = source.objects.iter().find(|o| o.id == *id) {
                    writeln!(
                        result,
                        "\nPrefab overrides: {}:{}",
                        source.path, control.line
                    )?;
                    result.push_str(&self.saved(source, Some(control.span.clone()))?);
                }
            }
            for (field, reference) in &object.references {
                if reference.raw.null() {
                    continue;
                }
                writeln!(
                    result,
                    "\n{field} -> {}",
                    self.assets
                        .get(&reference.asset)
                        .map(|a| a.path.as_str())
                        .unwrap_or("unresolved asset")
                )?;
                if result.len() > 64 * 1024 {
                    result.push_str("\n[View truncated]\n");
                    break;
                }
            }
            if !composition.incomplete.is_empty() {
                writeln!(
                    result,
                    "\nIncomplete prefab data: {}",
                    composition
                        .incomplete
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )?;
            }
            return Ok(Some(result));
        }
        let (path, range) = crate::navigation::location(path)?;
        let paths: Vec<_> = self.assets.values().map(|a| a.path.as_str()).collect();
        let matches = crate::navigation::matches(paths.into_iter(), path);
        let assets: Vec<_> = self
            .assets
            .values()
            .filter(|a| matches.contains(&a.path.as_str()))
            .collect();
        if assets.is_empty() {
            return Ok(None);
        }
        ensure!(
            assets.len() == 1,
            "Asset path is ambiguous across Unity projects"
        );
        let asset = assets[0];
        if let Some(reason) = &asset.unavailable {
            return Ok(Some(format!(
                "{}\nGUID: {}\n{reason}",
                asset.path, asset.guid
            )));
        }
        if asset.content.is_none() {
            if asset.path.ends_with(".cs") {
                return Ok(None);
            }
            return Ok(Some(format!(
                "{}\nGUID: {}\nAsset contents unavailable",
                asset.path, asset.guid
            )));
        }
        let parsed = content(asset)?;
        if range.is_none() {
            return Ok(Some(self.saved(asset, None)?));
        }
        let mut result = String::new();
        for (fragment_index, fragment) in parsed.fragments.iter().enumerate() {
            for (i, line) in fragment.text.split_inclusive('\n').enumerate() {
                let number = fragment.line + i;
                if range.is_none_or(|(first, last)| number >= first && number <= last) {
                    let clipped: String = line.chars().take(4096).collect();
                    write!(result, "{number}: {clipped}")?;
                }
                if result.len() >= 64 * 1024 {
                    result.push_str("\n[View truncated]\n");
                    return Ok(Some(result));
                }
            }
            if let Some(omitted) = parsed.omissions.get(fragment_index) {
                let first = fragment.line + fragment.text.bytes().filter(|b| *b == b'\n').count();
                let last = parsed
                    .fragments
                    .get(fragment_index + 1)
                    .map_or(first, |f| f.line);
                if range.is_none_or(|(start, end)| first <= end && last >= start) {
                    writeln!(
                        result,
                        "⟪omitted: {} bytes at original lines {first}–{last}⟫",
                        omitted.len()
                    )?;
                }
            }
        }
        if !parsed.omissions.is_empty() {
            writeln!(
                result,
                "\n[{} large values omitted; original source coordinates retained]",
                parsed.omissions.len()
            )?;
        }
        Ok(Some(result))
    }
    fn saved(&self, asset: &Asset, range: Option<std::ops::Range<usize>>) -> Result<String> {
        let parsed = content(asset)?;
        let range = range.unwrap_or(0..parsed.bytes);
        let mut result = String::new();
        for (i, fragment) in parsed.fragments.iter().enumerate() {
            let start = range.start.max(fragment.start);
            let end = range.end.min(fragment.start + fragment.text.len());
            if start < end {
                result.extend(
                    fragment.text[start - fragment.start..end - fragment.start]
                        .chars()
                        .take(32 * 1024 - result.len().min(32 * 1024)),
                );
            }
            if let Some(omission) = parsed.omissions.get(i)
                && omission.start < range.end
                && omission.end > range.start
            {
                write!(result, "⟪omitted: {} bytes⟫", omission.len())?;
            }
            if result.len() >= 32 * 1024 {
                result.push_str("\n[Fragment truncated]\n");
                break;
            }
        }
        Ok(result)
    }
}
