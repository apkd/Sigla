//! Candidate matching over prepared native facts. No source parsing or type inference.
use super::*;
use crate::{
    native::{self, Role},
    render::SearchResult,
    selection::Selection,
};

type ReferenceResults = Selection<(String, usize, Option<usize>, String), SearchResult>;
type HierarchyResults = Selection<(bool, String, usize), SearchResult>;

impl Search<'_> {
    fn native_declaration_language(&mut self, hit: &Hit) -> Result<Language> {
        let data = self.data(&hit.file)?;
        let native = data.facts.native.as_ref().unwrap();
        Ok(native
            .regions
            .iter()
            .find(|region| region.span.contains(&hit.decl.name_span.start))
            .unwrap()
            .language)
    }
    pub(crate) fn file_language_filters(q: &Query, language: Language) -> bool {
        q.file_language_filters(language)
    }
    pub(super) fn language_filters(q: &Query, dialect: Language, container: Language) -> bool {
        q.language_filters(dialect, container)
    }

    pub(super) fn bind_native(
        &mut self,
        key: &str,
        membership: &Membership,
        occurrence: &Occurrence,
        data: &FileData,
    ) -> Result<Vec<(Hit, bool)>> {
        let facts = data.facts.native.as_ref().unwrap();
        let Some(index) = facts.names.get(&occurrence.name).and_then(|indices| {
            indices
                .iter()
                .copied()
                .find(|&i| data.facts.occurrences[i as usize].span == occurrence.span)
        }) else {
            return Ok(Vec::new());
        };
        let info = &facts.occurrences[index as usize];
        if let Some(local) = info.local {
            return Ok(vec![(
                Hit {
                    semantic_id: None,
                    file: key.into(),
                    membership: membership.clone(),
                    decl: data.facts.declarations[local as usize].clone(),
                    rank: 0,
                },
                false,
            )]);
        }
        if info.role == Role::Label {
            return Ok(Vec::new());
        }
        let name = written_name(data, occurrence, info);
        let hits = self.declarations(
            &Target {
                name: occurrence.name.clone(),
                ..Default::default()
            },
            false,
        )?;
        let language = facts.regions[info.region as usize].language;
        let mut compatible = Vec::new();
        for hit in hits {
            if self.manifest.files[&hit.file].language.native()
                && native::compatible(language, self.native_declaration_language(&hit)?)
                && !hit.decl.local()
                && (!info.qualified
                    || native::name_rank(&name, &hit.decl.qualified, false).is_some())
            {
                compatible.push(hit);
            }
        }
        let mut hits = compatible;
        let included = self.native_includes(key, facts);
        for hit in &mut hits {
            hit.rank = u8::from(hit.file != key) + u8::from(!included.contains(&hit.file));
        }
        hits.sort_by_key(|h| {
            (
                h.file != key,
                !included.contains(&h.file),
                h.file.clone(),
                h.decl.name_span.start,
            )
        });
        Ok(hits.into_iter().map(|h| (h, true)).collect())
    }

    /// Only selected files participate. Ambiguous suffixes remain unresolved.
    fn native_includes(&self, key: &str, facts: &native::File) -> BTreeSet<String> {
        facts
            .includes
            .iter()
            .filter_map(|include| self.native_include(key, include))
            .collect()
    }

    pub(super) fn native_include_hint(
        &self,
        key: &str,
        facts: &native::File,
        at: usize,
    ) -> Option<String> {
        let include = facts.includes.iter().find(|i| i.span.contains(&at))?;
        let target = self.native_include(key, include);
        Some(if let Some(target) = target {
            let file = &self.manifest.files[&target];
            format!(
                "Include: {}",
                crate::render::inline(&self.manifest.display(file, &file.memberships[0]))
            )
        } else {
            format!(
                "Include {} has no unique match among the selected files.",
                crate::render::inline(&include.path)
            )
        })
    }

    fn native_include(&self, key: &str, include: &native::Include) -> Option<String> {
        let source = &self.manifest.files[key];
        let relative = include
            .relative
            .then(|| lexical_path(&source.path.parent().unwrap().join(&include.path)));
        if let Some((key, _)) = self
            .manifest
            .files
            .iter()
            .find(|(_, f)| Some(&f.path) == relative.as_ref())
        {
            return Some(key.clone());
        }
        let mut matches = self.manifest.files.iter().filter(|(_, f)| {
            f.language.native()
                && (f.path.ends_with(&include.path)
                    || f.memberships
                        .iter()
                        .any(|m| self.manifest.display(f, m).as_ref() == include.path))
        });
        let (key, _) = matches.next()?;
        matches.next().is_none().then(|| key.clone())
    }

    pub(super) fn native_relationship(
        &mut self,
        q: &Query,
        targets: &[Hit],
        inside: &[(bool, Vec<Hit>)],
        units: &mut ReferenceResults,
    ) -> Result<()> {
        let targets: Vec<_> = targets
            .iter()
            .filter(|h| self.manifest.files[&h.file].language.native())
            .collect();
        let labels: BTreeMap<_, _> = targets
            .iter()
            .map(|hit| {
                Ok((
                    (hit.file.as_str(), hit.decl.name_span.start),
                    self.target_label(hit)?,
                ))
            })
            .collect::<Result<_>>()?;
        let outgoing = q.target.name == "*";
        let dialects: BTreeMap<_, _> = targets
            .iter()
            .map(|hit| {
                Ok((
                    (hit.file.as_str(), hit.decl.name_span.start),
                    self.native_declaration_language(hit)?,
                ))
            })
            .collect::<Result<_>>()?;
        let mut names: BTreeSet<String> = targets.iter().map(|h| h.decl.name.clone()).collect();
        if q.target.location.is_none() {
            names.insert(native::simple_name(&q.target.name));
        }
        let mut keys = BTreeSet::new();
        for name in &names {
            keys.extend(self.candidates(name, q.loose, true)?);
        }
        let files = self.containment_files(inside)?;
        let files = self.filtered_files(q, files);
        for key in keys {
            let file = &self.manifest.files[&key];
            if !file.language.native() || files.as_ref().is_some_and(|f| !f.contains(&key)) {
                continue;
            }
            let data = self.data(&key)?;
            let facts = data.facts.native.as_ref().unwrap();
            let indices: BTreeSet<u32> =
                if outgoing && q.selector == "calls" && inside.iter().any(|(negate, _)| !negate) {
                    facts
                        .calls
                        .iter()
                        .filter(|(owner, _)| {
                            inside
                                .iter()
                                .filter(|(negate, _)| !negate)
                                .all(|(_, hits)| {
                                    hits.iter().any(|h| {
                                        let owner = &data.facts.declarations[**owner as usize];
                                        h.file == key
                                            && if h.decl.callable() {
                                                h.decl.name_span == owner.name_span
                                            } else {
                                                h.decl.span.contains(&owner.name_span.start)
                                            }
                                    })
                                })
                        })
                        .flat_map(|(_, calls)| calls.iter().copied())
                        .collect()
                } else {
                    facts
                        .names
                        .iter()
                        .filter(|(name, _)| {
                            names
                                .iter()
                                .any(|p| crate::query::name_rank(p, name, q.loose).is_some())
                        })
                        .flat_map(|(_, indices)| indices.iter().copied())
                        .collect()
                };
            for index in indices {
                self.check()?;
                let o = &data.facts.occurrences[index as usize];
                let info = &facts.occurrences[index as usize];
                let language = facts.regions[info.region as usize].language;
                if info.role == Role::Label
                    || !Self::language_filters(q, language, file.language)
                    || q.selector == "calls" && !o.call
                    || q.selector == "writes" && o.write == WriteKind::None
                {
                    continue;
                }
                let local = info.local.map(|i| &data.facts.declarations[i as usize]);
                let written = written_name(&data, o, info);
                let matches: Vec<_> = targets
                    .iter()
                    .copied()
                    .filter(|h| {
                        if !native::compatible(
                            language,
                            dialects[&(h.file.as_str(), h.decl.name_span.start)],
                        ) {
                            return false;
                        }
                        if let Some(local) = local {
                            return h.file == key && h.decl.name_span == local.name_span;
                        }
                        !h.decl.local()
                            && native::name_rank(&o.name, &h.decl.name, q.loose).is_some()
                            && (!info.qualified
                                || native::name_rank(&written, &h.decl.qualified, q.loose)
                                    .is_some())
                    })
                    .collect();
                if !outgoing && matches.is_empty() {
                    // Written-name searches also work when the declaration was not selected.
                    if !targets.is_empty()
                        || q.target.location.is_some()
                        || q.target.parameters.is_some()
                        || q.target.qualifiers.is_some()
                        || native::name_rank(
                            if info.qualified {
                                &q.target.name
                            } else {
                                native::written_components(&q.target.name)
                                    .last()
                                    .copied()
                                    .unwrap_or("")
                            },
                            if info.qualified { &written } else { &o.name },
                            q.loose,
                        )
                        .is_none()
                    {
                        continue;
                    }
                }
                let containing = info.owner.map(|i| &data.facts.declarations[i as usize]);
                for membership in &file.memberships {
                    if !self.filters(q, file, membership, containing, o.span.start, inside) {
                        continue;
                    }
                    let display = self.manifest.display(file, membership).into_owned();
                    let rank = (
                        display.clone(),
                        o.span.start,
                        containing.map(|d| d.name_span.start),
                        o.name.clone(),
                    );
                    if !units.accepts(&rank) {
                        continue;
                    }
                    let labels: BTreeSet<_> = matches
                        .iter()
                        .map(|h| labels[&(h.file.as_str(), h.decl.name_span.start)].clone())
                        .collect();
                    let label = if let Some(local) = local {
                        format!("{}:{}", local.kind, local.qualified)
                    } else if labels.len() == 1 {
                        labels.first().unwrap().clone()
                    } else {
                        written.clone()
                    };
                    units.insert(
                        rank,
                        SearchResult {
                            symbol: containing.map(|d| (d.kind.clone(), d.qualified.clone())),
                            path: display,
                            lines: Some(crate::render::lines(&data.source, o.span.clone())),
                            source: line(&data.source, o.span.start).into(),
                            language,
                            uncertain: local.is_none(),
                            target: Some(label),
                            occurrences: BTreeSet::from([o.span.start]),
                            candidates: if labels.len() > 1 {
                                labels.into_iter().collect()
                            } else {
                                Vec::new()
                            },
                            possible_write: q.selector == "writes" && info.indirect_write,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn native_hierarchy(
        &mut self,
        q: &Query,
        targets: &[Hit],
        inside: &[(bool, Vec<Hit>)],
        units: &mut HierarchyResults,
    ) -> Result<()> {
        if q.selector != "derived" {
            if targets
                .iter()
                .any(|h| self.manifest.files[&h.file].language.native())
            {
                anyhow::bail!(
                    "Native source has no `impl:` query; use `derived:` for direct written base clauses."
                );
            }
            return Ok(());
        }
        let patterns: Vec<_> = if q.target.location.is_some() {
            targets
                .iter()
                .filter(|h| self.manifest.files[&h.file].language.native())
                .map(|h| h.decl.qualified.as_str())
                .collect()
        } else {
            vec![q.target.name.as_str()]
        };
        let files = self.containment_files(inside)?;
        let files = self.filtered_files(q, files);
        for (key, file) in &self.manifest.files {
            if !file.language.native() || files.as_ref().is_some_and(|f| !f.contains(key)) {
                continue;
            }
            let data = self.data(key)?;
            let facts = data.facts.native.as_ref().unwrap();
            for (i, decl) in data.facts.declarations.iter().enumerate() {
                if !decl.named_type()
                    || !Self::language_filters(
                        q,
                        facts.regions[facts.declarations[i].region as usize].language,
                        file.language,
                    )
                {
                    continue;
                }
                let bases: Vec<_> = decl
                    .bases
                    .iter()
                    .filter(|b| {
                        let base = native::base_name(b);
                        patterns.iter().any(|p| {
                            native::name_rank(p, &base, q.loose).is_some()
                                || native::written_components(&base).len() == 1
                                    && native::written_components(p).last().is_some_and(|p| {
                                        native::name_rank(p, &base, q.loose).is_some()
                                    })
                                || native::name_rank(p, &format!("{}::{base}", decl.owner), q.loose)
                                    .is_some()
                        })
                    })
                    .cloned()
                    .collect();
                if bases.is_empty() {
                    continue;
                }
                for membership in &file.memberships {
                    if !self.filters(
                        q,
                        file,
                        membership,
                        Some(decl),
                        decl.name_span.start,
                        inside,
                    ) {
                        continue;
                    }
                    let rank = (
                        false,
                        self.manifest.display(file, membership).into_owned(),
                        decl.name_span.start,
                    );
                    if !units.accepts(&rank) {
                        continue;
                    }
                    let hit = Hit {
                        semantic_id: None,
                        file: key.clone(),
                        membership: membership.clone(),
                        decl: decl.clone(),
                        rank: 0,
                    };
                    let mut unit = self.declaration_unit(&hit)?;
                    unit.uncertain = true;
                    unit.target = Some(bases.join(", "));
                    units.insert(rank, unit);
                }
            }
        }
        Ok(())
    }
}

fn written_name(data: &FileData, occurrence: &Occurrence, info: &native::OccurrenceInfo) -> String {
    if info.qualified
        && let Some(receiver) = &info.receiver
    {
        format!("{}::{}", &data.source[receiver.clone()], occurrence.name)
    } else {
        occurrence.name.clone()
    }
}

fn lexical_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            component => result.push(component),
        }
    }
    result
}
