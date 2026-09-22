use crate::{
    model::*,
    query::{Query, Target, name_rank, wildcard},
    store::{FileData, Store},
    workspace::{FileEntry, Manifest, Membership},
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

#[derive(Clone)]
struct Hit {
    semantic_id: Option<crate::csharp::types::DefinitionId>,
    file: String,
    membership: Membership,
    decl: Declaration,
    rank: u8,
}

pub struct Search<'a> {
    store: &'a Store,
    assemblies: &'a Store,
    manifest: &'a Manifest,
    cancel: &'a tokio_util::sync::CancellationToken,
    source_tx: heed::RoTxn<'a, heed::WithTls>,
    assembly_tx: heed::RoTxn<'a, heed::WithTls>,
    cache: HashMap<String, Arc<FileData>>,
    cache_bytes: usize,
    path_matchers: HashMap<String, globset::GlobMatcher>,
    lookups: HashMap<(Target, bool, Option<usize>), Vec<Hit>>,
    lookup_bytes: usize,
    summaries: HashMap<String, Arc<Vec<Declaration>>>,
    summary_bytes: usize,
    csharp: crate::csharp::bind::Binder,
}
impl<'a> Search<'a> {
    pub fn new(
        store: &'a Store,
        assemblies: &'a Store,
        manifest: &'a Manifest,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        Ok(Self {
            store,
            assemblies,
            manifest,
            cancel,
            source_tx: store.read()?,
            assembly_tx: assemblies.read()?,
            cache: HashMap::new(),
            cache_bytes: 0,
            path_matchers: HashMap::new(),
            lookups: HashMap::new(),
            lookup_bytes: 0,
            summaries: HashMap::new(),
            summary_bytes: 0,
            csharp: Default::default(),
        })
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.cancel.is_cancelled(), "Query cancelled");
        Ok(())
    }
    fn data(&mut self, key: &str) -> Result<Arc<FileData>> {
        self.check()?;
        if let Some(d) = self.cache.get(key) {
            return Ok(d.clone());
        }
        let store = if self.manifest.files[key].metadata {
            self.assemblies
        } else {
            self.store
        };
        let tx = if self.manifest.files[key].metadata {
            &self.assembly_tx
        } else {
            &self.source_tx
        };
        let d = Arc::new(
            store
                .load(tx, key)?
                .ok_or_else(|| anyhow::anyhow!("Missing cache record; retry query"))?,
        );
        let bytes =
            d.source.len() + d.facts.occurrences.len() * 128 + d.facts.declarations.len() * 512;
        if self.cache_bytes + bytes > 32 * 1024 * 1024 {
            self.cache.clear();
            self.cache_bytes = 0;
        }
        if bytes <= 32 * 1024 * 1024 {
            self.cache.insert(key.into(), d.clone());
            self.cache_bytes += bytes;
        }
        Ok(d)
    }
    fn candidates(&self, name: &str, loose: bool, occurrences: bool) -> Result<BTreeSet<String>> {
        if name == "*" {
            return Ok(self
                .manifest
                .files
                .iter()
                .filter(|(_, file)| !occurrences || !file.metadata)
                .map(|(key, _)| key.clone())
                .collect());
        }
        let mut keys = self
            .store
            .candidates(&self.source_tx, name, loose, occurrences)?
            .into_iter()
            .filter(|k| self.manifest.files.contains_key(k))
            .collect::<BTreeSet<_>>();
        if !occurrences {
            keys.extend(
                self.assemblies
                    .candidates(&self.assembly_tx, name, loose, false)?
                    .into_iter()
                    .filter(|k| self.manifest.files.contains_key(k)),
            );
        }
        Ok(keys)
    }
    fn summary(&mut self, key: &str) -> Result<Arc<Vec<Declaration>>> {
        self.check()?;
        if let Some(declarations) = self.summaries.get(key) {
            return Ok(declarations.clone());
        }
        let store = if self.manifest.files[key].metadata {
            self.assemblies
        } else {
            self.store
        };
        let tx = if self.manifest.files[key].metadata {
            &self.assembly_tx
        } else {
            &self.source_tx
        };
        let declarations = Arc::new(store.declarations_in(tx, key)?);
        let bytes = declarations.len() * 768;
        if self.summary_bytes + bytes > 32 * 1024 * 1024 {
            self.summaries.clear();
            self.summary_bytes = 0;
        }
        if bytes <= 32 * 1024 * 1024 {
            self.summary_bytes += bytes;
            self.summaries.insert(key.into(), declarations.clone());
        }
        Ok(declarations)
    }
    fn declarations(&mut self, target: &Target, loose: bool) -> Result<Vec<Hit>> {
        self.declarations_in(target, loose, None)
    }
    fn declarations_in(
        &mut self,
        target: &Target,
        loose: bool,
        from: Option<usize>,
    ) -> Result<Vec<Hit>> {
        let key = (target.clone(), loose, from);
        if let Some(hits) = self.lookups.get(&key) {
            return Ok(hits.clone());
        }
        let mut result = Vec::new();
        self.visit_declarations(target, loose, None, None, from, |_, hit| {
            result.push(hit);
            Ok(())
        })?;
        let bytes = 256
            + target.name.len()
            + result
                .iter()
                .map(|h| {
                    512 + h.file.len()
                        + h.membership.module.len()
                        + h.decl.qualified.len()
                        + h.decl.owner.len()
                        + h.decl.ty.len()
                        + h.decl.parameters.iter().map(String::len).sum::<usize>()
                })
                .sum::<usize>();
        if self.lookup_bytes + bytes > 8 * 1024 * 1024 {
            self.lookups.clear();
            self.lookup_bytes = 0;
        }
        if bytes <= 8 * 1024 * 1024 {
            self.lookup_bytes += bytes;
            self.lookups.insert(key, result.clone());
        }
        Ok(result)
    }
    fn visit_declarations(
        &mut self,
        target: &Target,
        loose: bool,
        projection: Option<&Query>,
        restricted_files: Option<&BTreeSet<String>>,
        from: Option<usize>,
        mut visit: impl FnMut(&mut Self, Hit) -> Result<()>,
    ) -> Result<()> {
        if let Some(loc) = &target.location {
            let keys = self
                .manifest
                .files
                .iter()
                .filter(|(_, f)| {
                    !f.metadata
                        && f.memberships
                            .iter()
                            .any(|m| self.manifest.display(f, m) == loc.path)
                })
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>();
            let mut result = Vec::new();
            for key in keys {
                let data = self.data(&key)?;
                let Some(pos) = offset(&data.source, loc.line, loc.column) else {
                    continue;
                };
                let file = &self.manifest.files[&key];
                for membership in &file.memberships {
                    if self.manifest.display(file, membership) != loc.path {
                        continue;
                    }
                    if let Some(d) = data
                        .facts
                        .declarations
                        .iter()
                        .find(|d| d.name_span.contains(&pos))
                    {
                        result.push(Hit {
                            semantic_id: None,
                            file: key.clone(),
                            membership: membership.clone(),
                            decl: contextual(d, membership),
                            rank: 0,
                        });
                    } else if let Some(o) = data
                        .facts
                        .occurrences
                        .iter()
                        .find(|o| o.span.contains(&pos))
                    {
                        let bindings = self.bind(&key, membership, o, &data)?;
                        result.extend(
                            bindings
                                .into_iter()
                                .filter_map(|(h, possible)| (!possible).then_some(h)),
                        );
                    }
                }
            }
            for hit in result {
                visit(self, hit)?;
            }
            return Ok(());
        }
        let simple = simple_name(&target.name);
        let keys = match restricted_files {
            Some(files) => files.clone(),
            None => self.candidates(simple, loose, false)?,
        };
        for key in keys {
            self.check()?;
            let file = self.manifest.files[&key].clone();
            let declarations = self.summary(&key)?;
            let mut projected_contexts = BTreeSet::new();
            for membership in &file.memberships {
                self.check()?;
                if from.is_some_and(|from| {
                    if file.metadata {
                        membership.project != from || !self.manifest.metadata_visible(&file, from)
                    } else {
                        !self.visible(from, membership.project)
                    }
                }) {
                    continue;
                }
                if let Some(query) = projection {
                    if query
                        .filters
                        .iter()
                        .filter(|f| f.key == "project")
                        .any(|f| {
                            wildcard(&f.value, &self.manifest.projects[membership.project].name)
                                == f.negate
                        })
                    {
                        continue;
                    }
                    if !query.filters.iter().any(|f| f.key == "in")
                        && !projected_contexts.insert(&membership.module)
                    {
                        continue;
                    }
                }
                for decl in declarations.iter() {
                    if !loose && !simple.contains('*') && decl.name != simple {
                        continue;
                    }
                    let qualified;
                    let compared = if target.name.contains('.') || target.name.contains("::") {
                        let written = if decl.kind == "constructor"
                            && (target.parameters.is_some()
                                || projection.is_some_and(|q| q.selector == "constructor"))
                        {
                            &decl.owner
                        } else {
                            &decl.qualified
                        };
                        qualified = context_name(written, membership);
                        &qualified
                    } else {
                        &decl.name
                    };
                    if let Some(rank) = name_rank(&target.name, compared, loose) {
                        if target
                            .parameters
                            .as_ref()
                            .is_some_and(|p| !signature_matches(p, &decl.parameters))
                        {
                            continue;
                        }
                        visit(
                            self,
                            Hit {
                                semantic_id: None,
                                file: key.clone(),
                                membership: membership.clone(),
                                decl: contextual(decl, membership),
                                rank,
                            },
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
    fn visible(&self, from: usize, to: usize) -> bool {
        from == to
            || self.manifest.projects[from]
                .references
                .iter()
                .any(|reference| reference.visible(&self.manifest.projects[to].identity))
    }
    fn same_domain(&self, a: &Hit, b: &Hit) -> bool {
        match (
            self.manifest.files[&a.file].metadata,
            self.manifest.files[&b.file].metadata,
        ) {
            (true, true) => a.file == b.file,
            (false, false) => a.membership.project == b.membership.project,
            _ => false,
        }
    }
    fn same_symbol(&self, a: &Hit, b: &Hit) -> bool {
        if let (Some(a), Some(b)) = (&a.semantic_id, &b.semantic_id) {
            return a == b;
        }
        self.same_domain(a, b) && same_logical(&a.decl, &b.decl)
    }
    fn prefer_source(&self, candidates: &mut Vec<Hit>) -> Result<()> {
        let source: Vec<_> = candidates
            .iter()
            .filter(|h| !self.manifest.files[&h.file].metadata)
            .cloned()
            .collect();
        if source.is_empty() {
            return Ok(());
        }
        let mut names = HashMap::new();
        for hit in candidates
            .iter()
            .filter(|h| self.manifest.files[&h.file].metadata)
        {
            if !names.contains_key(&hit.file) {
                names.insert(
                    hit.file.clone(),
                    self.assemblies
                        .assembly_name_in(&self.assembly_tx, &hit.file)?,
                );
            }
        }
        candidates.retain(|hit| {
            !self.manifest.files[&hit.file].metadata
                || !source.iter().any(|source| {
                    same_logical(&source.decl, &hit.decl)
                        && names[&hit.file].as_deref()
                            == Some(
                                self.manifest.projects[source.membership.project]
                                    .name
                                    .as_str(),
                            )
                })
        });
        Ok(())
    }
    fn ancestor_owners(&mut self, roots: Vec<Hit>) -> Result<BTreeSet<String>> {
        let mut queue = roots;
        let mut owners = BTreeSet::new();
        while let Some(h) = queue.pop() {
            self.check()?;
            if !owners.insert(h.decl.qualified.clone()) {
                continue;
            }
            for base in &h.decl.bases {
                let base = base.split('<').next().unwrap_or(base);
                let data = self.data(&h.file)?;
                let mut found = self.declarations_in(
                    &Target {
                        name: base.into(),
                        parameters: None,
                        location: None,
                    },
                    false,
                    Some(h.membership.project),
                )?;
                found.retain(|b| {
                    b.decl.named_type()
                        && self.visible(h.membership.project, b.membership.project)
                        && self.in_scope(
                            &b.decl,
                            base,
                            Some(&h.decl),
                            &h.membership,
                            &data.facts.imports,
                        )
                });
                queue.extend(found);
            }
        }
        Ok(owners)
    }
    fn bind(
        &mut self,
        key: &str,
        m: &Membership,
        o: &Occurrence,
        data: &FileData,
    ) -> Result<Vec<(Hit, bool)>> {
        if self.manifest.files[key].language == Language::CSharp {
            let view = crate::csharp::catalog::View {
                source: self.store,
                assemblies: self.assemblies,
                source_tx: &self.source_tx,
                assembly_tx: &self.assembly_tx,
                manifest: self.manifest,
                cancel: self.cancel,
            };
            return Ok(self
                .csharp
                .resolve(&view, key, m.project, o.span.start, o.construction, &o.name)?
                .into_iter()
                .map(|(symbol, uncertain)| {
                    let decl = symbol.declaration().clone();
                    (
                        Hit {
                            semantic_id: Some(symbol.id),
                            file: symbol.file,
                            membership: Membership {
                                project: symbol.project,
                                module: String::new(),
                            },
                            decl,
                            rank: 0,
                        },
                        uncertain,
                    )
                })
                .collect());
        }
        self.bind_rust(key, m, o, data)
    }
    fn bind_rust(
        &mut self,
        key: &str,
        m: &Membership,
        o: &Occurrence,
        data: &FileData,
    ) -> Result<Vec<(Hit, bool)>> {
        let local = data
            .facts
            .declarations
            .iter()
            .filter(|d| {
                d.local()
                    && d.name == o.name
                    && d.scope.contains(&o.span.start)
                    && (d.kind == "parameter" || d.name_span.start < o.span.start)
            })
            .min_by_key(|d| (d.scope.end - d.scope.start, usize::MAX - d.name_span.start));
        if !o.construction
            && o.receiver.is_empty()
            && let Some(d) = local
        {
            return Ok(vec![(
                Hit {
                    semantic_id: None,
                    file: key.into(),
                    membership: m.clone(),
                    decl: contextual(d, m),
                    rank: 0,
                },
                o.opaque,
            )]);
        }
        let mut name = o.name.clone();
        let alias = data
            .facts
            .imports
            .iter()
            .find(|i| i.alias == name && i.scope.contains(&o.span.start));
        if let Some(alias) = alias {
            name = alias.path.clone();
        }
        let mut candidates = self.declarations_in(
            &Target {
                name: name.clone(),
                parameters: None,
                location: None,
            },
            false,
            Some(m.project),
        )?;
        if alias.is_some() && !m.module.is_empty() {
            candidates.extend(self.declarations_in(
                &Target {
                    name: context_name(&name, m),
                    parameters: None,
                    location: None,
                },
                false,
                Some(m.project),
            )?);
        }
        candidates.retain(|h| self.visible(m.project, h.membership.project) && !h.decl.local());
        self.prefer_source(&mut candidates)?;
        if o.call {
            candidates.retain(|h| h.decl.callable() || h.decl.named_type());
        }
        if o.write {
            candidates.retain(|h| {
                matches!(
                    h.decl.kind.as_str(),
                    "field" | "property" | "event" | "static" | "const"
                )
            });
        }
        let containing = data
            .facts
            .declarations
            .iter()
            .filter(|d| d.span.contains(&o.span.start) && !d.local())
            .min_by_key(|d| d.span.end - d.span.start);
        if !o.construction
            && o.receiver.is_empty()
            && candidates.iter().all(|h| h.decl.named_type())
        {
            let scoped: Vec<_> = candidates
                .iter()
                .filter(|h| self.in_scope(&h.decl, &name, containing, m, &data.facts.imports))
                .cloned()
                .collect();
            if !scoped.is_empty() {
                let possible = o.opaque || scoped.iter().any(|h| !self.same_symbol(h, &scoped[0]));
                return Ok(scoped.into_iter().map(|h| (h, possible)).collect());
            }
        }
        if o.construction {
            let written = if o.receiver.is_empty() {
                name.clone()
            } else {
                format!(
                    "{}.{}",
                    expand_alias(&o.receiver, &data.facts.imports),
                    name
                )
            };
            let types: Vec<_> = candidates
                .iter()
                .filter(|h| {
                    h.decl.named_type()
                        && self.in_scope(&h.decl, &written, containing, m, &data.facts.imports)
                })
                .cloned()
                .collect();
            if name.contains('.') || name.contains("::") {
                for ty in &types {
                    let mut constructors = self.declarations_in(
                        &Target {
                            name: ty.decl.name.clone(),
                            parameters: None,
                            location: None,
                        },
                        false,
                        Some(m.project),
                    )?;
                    constructors
                        .retain(|h| h.decl.kind == "constructor" && self.same_domain(h, ty));
                    candidates.extend(constructors);
                }
            }
            let constructors: Vec<_> = candidates
                .into_iter()
                .filter(|h| {
                    h.decl.kind == "constructor"
                        && types.iter().any(|t| t.decl.qualified == h.decl.owner)
                        && o.arguments
                            .is_none_or(|count| h.decl.parameters.len() == count)
                })
                .collect();
            let selected = if constructors.is_empty() {
                types
            } else {
                constructors
            };
            let possible = o.opaque || selected.iter().any(|h| !self.same_symbol(h, &selected[0]));
            return Ok(selected.into_iter().map(|h| (h, possible)).collect());
        }
        let owner = data
            .facts
            .declarations
            .iter()
            .filter(|d| (d.named_type() || d.kind == "impl") && d.span.contains(&o.span.start))
            .min_by_key(|d| d.span.end - d.span.start)
            .map(|d| {
                if d.kind == "impl" {
                    d.ty.clone()
                } else {
                    d.qualified.clone()
                }
            })
            .unwrap_or_default();
        let mut known_receiver = false;
        let receiver = if matches!(o.receiver.as_str(), "this" | "self" | "Self") {
            known_receiver = true;
            owner.clone()
        } else if !o.receiver.is_empty() {
            let binding = data
                .facts
                .declarations
                .iter()
                .filter(|d| {
                    d.name == o.receiver
                        && (!d.local() || d.scope.contains(&o.span.start))
                        && (d.kind == "parameter" || d.name_span.start < o.span.start)
                })
                .min_by_key(|d| d.scope.end - d.scope.start);
            if let Some(d) =
                binding.filter(|d| !d.ty.is_empty() && !matches!(d.ty.as_str(), "var" | "dynamic"))
            {
                known_receiver = true;
                d.ty.trim_start_matches('&')
                    .trim_start_matches("mut ")
                    .to_owned()
            } else {
                o.receiver.clone()
            }
        } else {
            String::new()
        };
        if !receiver.is_empty() {
            let mut receiver = expand_alias(&receiver, &data.facts.imports);
            if receiver.starts_with("crate::")
                || receiver.starts_with("self::")
                || receiver.starts_with("super::")
                || receiver.starts_with("::")
            {
                receiver = context_name(&receiver, m);
            }
            let types = self
                .declarations_in(
                    &Target {
                        name: receiver.clone(),
                        parameters: None,
                        location: None,
                    },
                    false,
                    Some(m.project),
                )?
                .into_iter()
                .filter(|h| h.decl.named_type() && self.visible(m.project, h.membership.project))
                .collect::<Vec<_>>();
            let types: Vec<_> = types
                .into_iter()
                .filter(|h| self.in_scope(&h.decl, &receiver, containing, m, &data.facts.imports))
                .collect();
            let names: Vec<_> = types.iter().map(|h| h.decl.qualified.as_str()).collect();
            let prefixed = if m.module.is_empty() {
                receiver.clone()
            } else {
                format!("{}::{receiver}", m.module)
            };
            let mut narrowed = candidates
                .iter()
                .filter(|h| {
                    names.contains(&h.decl.owner.as_str())
                        || h.decl.owner == receiver
                        || h.decl.owner == prefixed
                })
                .cloned()
                .collect::<Vec<_>>();
            if narrowed.is_empty() && !types.is_empty() {
                let ancestors = self.ancestor_owners(types.clone())?;
                narrowed = candidates
                    .iter()
                    .filter(|h| ancestors.contains(&h.decl.owner) && h.decl.access != "private")
                    .cloned()
                    .collect();
            }
            if !narrowed.is_empty() {
                candidates = narrowed;
                known_receiver = true;
            } else if known_receiver || !types.is_empty() {
                return Ok(Vec::new());
            } else {
                return Ok(candidates.into_iter().map(|h| (h, true)).collect());
            }
        } else {
            let own = context_name(&owner, m);
            let mut members = candidates
                .iter()
                .filter(|h| h.decl.owner == own)
                .cloned()
                .collect::<Vec<_>>();
            if members.is_empty() && !own.is_empty() {
                let types = self.declarations_in(
                    &Target {
                        name: own.clone(),
                        parameters: None,
                        location: None,
                    },
                    false,
                    Some(m.project),
                )?;
                let ancestors = self.ancestor_owners(types)?;
                members = candidates
                    .iter()
                    .filter(|h| ancestors.contains(&h.decl.owner) && h.decl.access != "private")
                    .cloned()
                    .collect();
            }
            if !members.is_empty() {
                candidates = members;
            } else {
                candidates.retain(|h| {
                    self.in_scope(&h.decl, &name, containing, m, &data.facts.imports)
                        && (!h.decl.callable() || h.decl.kind == "function" || alias.is_some())
                });
            }
        }
        if let Some(count) = o.arguments {
            let applicable = candidates
                .iter()
                .filter(|h| {
                    !h.decl.callable()
                        || h.decl.parameters.len() == count
                        || h.decl
                            .parameters
                            .last()
                            .is_some_and(|p| p.starts_with("params "))
                })
                .cloned()
                .collect::<Vec<_>>();
            if !applicable.is_empty() {
                candidates = applicable;
            } else {
                return Ok(Vec::new());
            }
        }
        let ambiguous = candidates.len() > 1
            && !candidates
                .iter()
                .all(|h| self.same_symbol(h, &candidates[0]));
        Ok(candidates
            .into_iter()
            .map(|h| {
                (
                    h,
                    ambiguous || o.opaque || (!o.receiver.is_empty() && !known_receiver),
                )
            })
            .collect())
    }
    fn in_scope(
        &self,
        d: &Declaration,
        written: &str,
        containing: Option<&Declaration>,
        m: &Membership,
        imports: &[Import],
    ) -> bool {
        if written.contains('.') || written.contains("::") {
            return type_path(&d.qualified) == type_path(written)
                || d.qualified == context_name(written, m);
        }
        let ns = containing.map(|d| d.namespace.as_str()).unwrap_or("");
        d.namespace == ns
            || d.namespace == m.module
            || d.owner == m.module
            || imports.iter().any(|i| {
                i.path == d.namespace
                    || i.path == d.qualified
                    || context_name(&i.path, m) == d.namespace
                    || context_name(&i.path, m) == d.qualified
            })
    }
    fn path_matches(&self, pattern: &str, display: &str) -> bool {
        self.path_matchers[pattern].is_match(display)
            || (!pattern.contains(['*', '?', '['])
                && display
                    .strip_prefix(pattern.trim_end_matches('/'))
                    .is_some_and(|rest| rest.starts_with('/')))
    }
    fn filtered_files(
        &self,
        q: &Query,
        mut files: Option<BTreeSet<String>>,
    ) -> Option<BTreeSet<String>> {
        if q.filters.iter().any(|f| f.key == "path") {
            let allowed = |key: &String| {
                let file = &self.manifest.files[key];
                file.memberships.iter().any(|m| {
                    q.filters.iter().filter(|f| f.key == "path").all(|f| {
                        self.path_matches(&f.value, &self.manifest.display(file, m)) != f.negate
                    })
                })
            };
            if let Some(files) = files.as_mut() {
                files.retain(allowed);
            } else {
                files = Some(
                    self.manifest
                        .files
                        .keys()
                        .filter(|key| allowed(key))
                        .cloned()
                        .collect(),
                );
            }
        }
        files
    }
    fn filters(
        &mut self,
        q: &Query,
        file: &FileEntry,
        m: &Membership,
        decl: Option<&Declaration>,
        at: usize,
        inside: &[(bool, Vec<Hit>)],
    ) -> bool {
        for f in &q.filters {
            if f.key == "in" {
                continue;
            }
            let yes = match f.key.as_str() {
                "path" => self.path_matches(&f.value, &self.manifest.display(file, m)),
                "project" => wildcard(&f.value, &self.manifest.projects[m.project].name),
                "namespace" => decl.is_some_and(|d| component_prefix(&d.namespace, &f.value)),
                "access" => decl.is_some_and(|d| {
                    d.access == f.value || (f.value == "public" && d.access == "pub")
                }),
                "attr" => decl.is_some_and(|d| {
                    d.attributes
                        .iter()
                        .any(|a| attribute_name(a) == attribute_name(&f.value))
                }),
                _ => false,
            };
            if yes == f.negate {
                return false;
            }
        }
        for (negate, targets) in inside {
            let yes = targets.iter().any(|h| {
                h.membership.project == m.project
                    && ((self.manifest.files[&h.file].path == file.path
                        && h.decl.span.contains(&at))
                        || decl.is_some_and(|d| {
                            d.owner == h.decl.qualified
                                || d.owner.starts_with(&format!("{}::", h.decl.qualified))
                                || d.owner.starts_with(&format!("{}.", h.decl.qualified))
                        }))
            });
            if yes == *negate {
                return false;
            }
        }
        true
    }
    pub fn run(&mut self, q: &Query) -> Result<String> {
        self.path_matchers = q
            .filters
            .iter()
            .filter(|f| f.key == "path")
            .map(|f| {
                Ok((
                    f.value.clone(),
                    globset::Glob::new(&f.value)?.compile_matcher(),
                ))
            })
            .collect::<Result<_>>()?;
        let mut inside = Vec::new();
        for f in q.filters.iter().filter(|f| f.key == "in") {
            inside.push((
                f.negate,
                self.declarations(&Target::parse(&f.value)?, false)?,
            ));
        }
        if q.selector == "text" {
            return self.text(q, &inside);
        }
        if q.relationship() {
            let targets = if q.selector == "calls" && q.target.name == "*" {
                Vec::new()
            } else {
                self.declarations(&q.target, q.loose)?
            };
            return self.relationship(q, &targets, &inside);
        }
        let mut units = crate::selection::Selection::new(q.limit);
        let files = self.containment_files(&inside)?;
        let files = self.filtered_files(q, files);
        self.visit_declarations(
            &q.target,
            q.loose,
            Some(q),
            files.as_ref(),
            None,
            |this, h| {
                let file = &this.manifest.files[&h.file];
                if (q.selector == "operator" && h.decl.name != q.target.name)
                    || !selector_matches(&q.selector, &h.decl)
                    || !this.filters(
                        q,
                        file,
                        &h.membership,
                        Some(&h.decl),
                        h.decl.name_span.start,
                        &inside,
                    )
                {
                    return Ok(());
                }
                let rank = (
                    h.rank,
                    file.metadata,
                    h.decl.local(),
                    this.manifest.display(file, &h.membership).into_owned(),
                    h.decl.name_span.start,
                    h.decl.qualified.clone(),
                );
                if !units.accepts(&rank) {
                    return Ok(());
                }
                units.insert(rank, this.declaration_unit(&h)?);
                Ok(())
            },
        )?;
        Ok(units.finish())
    }
    fn text(&mut self, q: &Query, inside: &[(bool, Vec<Hit>)]) -> Result<String> {
        let mut units = crate::selection::Selection::new(q.limit);
        let files = self.containment_files(inside)?;
        let files = self.filtered_files(q, files);
        let literal = regex::RegexBuilder::new(&regex::escape(&q.target.name))
            .case_insensitive(q.loose)
            .build()?;
        for (key, file) in &self.manifest.files {
            if file.metadata || files.as_ref().is_some_and(|files| !files.contains(key)) {
                continue;
            }
            if !file
                .memberships
                .iter()
                .any(|m| self.filters(q, file, m, None, 0, &[]))
            {
                continue;
            }
            let data = self.data(key)?;
            let mut previous_line = None;
            for matched in literal.find_iter(&data.source) {
                self.check()?;
                let containing = data
                    .facts
                    .declarations
                    .iter()
                    .filter(|d| !d.local() && d.span.contains(&matched.start()))
                    .min_by_key(|d| d.span.end - d.span.start);
                let Some(membership) = file.memberships.iter().find(|m| {
                    let containing = containing.map(|d| contextual(d, m));
                    self.filters(q, file, m, containing.as_ref(), matched.start(), inside)
                }) else {
                    continue;
                };
                let display = self.manifest.display(file, membership);
                let (line_no, column) = position(&data.source, matched.start());
                if previous_line == Some(line_no) {
                    continue;
                }
                previous_line = Some(line_no);
                let rank = (display.to_string(), matched.start());
                if !units.accepts(&rank) {
                    continue;
                }
                let end = data.source[matched.end()..]
                    .find('\n')
                    .map_or(data.source.len(), |n| matched.end() + n);
                let start = data.source[..matched.start()]
                    .rfind('\n')
                    .map_or(0, |n| n + 1);
                units.insert(
                    rank,
                    crate::render::result(
                        &containing
                            .map(|d| contextual(d, membership).qualified)
                            .unwrap_or_else(|| display.to_string()),
                        &format!("{display}:{line_no}:{column}"),
                        &data.source[start..end],
                        file.language,
                    ),
                );
            }
        }
        Ok(units.finish())
    }
    fn relationship(
        &mut self,
        q: &Query,
        targets: &[Hit],
        inside: &[(bool, Vec<Hit>)],
    ) -> Result<String> {
        if matches!(q.selector.as_str(), "derived" | "impl") {
            return self.hierarchy(q, targets, inside);
        }
        let mut targets = targets.to_vec();
        for target in &mut targets {
            if self.manifest.files[&target.file].language == Language::CSharp {
                let view = crate::csharp::catalog::View {
                    source: self.store,
                    assemblies: self.assemblies,
                    source_tx: &self.source_tx,
                    assembly_tx: &self.assembly_tx,
                    manifest: self.manifest,
                    cancel: self.cancel,
                };
                target.semantic_id = self.csharp.definition(
                    &view,
                    &target.file,
                    target.membership.project,
                    target.decl.name_span.start,
                    &target.decl.name,
                )?;
            }
        }
        let outgoing = q.target.name == "*";
        let files = self.containment_files(inside)?;
        let files = self.filtered_files(q, files);
        let mut keys = BTreeSet::new();
        let mut names = BTreeSet::new();
        if outgoing {
            keys.extend(
                self.manifest
                    .files
                    .iter()
                    .filter(|(_, f)| !f.metadata)
                    .map(|(k, _)| k.clone()),
            );
        } else {
            for h in &targets {
                names.insert(h.decl.name.clone());
                keys.extend(self.candidates(&h.decl.name, false, true)?);
                for (keyword, qualified) in BUILTIN_TYPES {
                    if h.decl.qualified == *qualified {
                        names.insert((*keyword).into());
                        keys.extend(self.candidates(keyword, false, true)?);
                    }
                }
            }
        }
        if let Some(files) = files {
            keys.retain(|key| files.contains(key));
        }
        // alias imports supply additional candidate spellings; binding still decides truth.
        for key in keys.clone() {
            let data = self.data(&key)?;
            for i in &data.facts.imports {
                if targets.iter().any(|h| {
                    i.path == h.decl.qualified
                        || i.path == h.decl.name
                        || (h.decl.kind == "constructor" && i.path == h.decl.owner)
                        || self.manifest.files[&key]
                            .memberships
                            .iter()
                            .any(|m| context_name(&i.path, m) == h.decl.qualified)
                }) && !i.alias.is_empty()
                {
                    names.insert(i.alias.clone());
                    keys.extend(self.candidates(&i.alias, false, true)?);
                }
            }
        }
        let mut units = crate::selection::Selection::new(q.limit);
        for key in keys {
            let file = &self.manifest.files[&key];
            let data = self.data(&key)?;
            for o in &data.facts.occurrences {
                if (!outgoing && !names.contains(&o.name))
                    || (q.selector == "calls" && !o.call)
                    || (q.selector == "writes" && !o.write)
                {
                    continue;
                }
                for m in &file.memberships {
                    let containing = data
                        .facts
                        .declarations
                        .iter()
                        .filter(|d| !d.local() && d.span.contains(&o.span.start))
                        .min_by_key(|d| d.span.end - d.span.start)
                        .map(|d| contextual(d, m));
                    if !self.filters(q, file, m, containing.as_ref(), o.span.start, inside) {
                        continue;
                    }
                    // Outgoing wildcard searches ask for explicit call sites, not a
                    // particular callee. Their syntax already establishes the match.
                    let uncertain = if outgoing {
                        o.opaque
                    } else {
                        let bindings = self.bind(&key, m, o, &data)?;
                        let relevant = bindings
                            .iter()
                            .filter(|(b, _)| {
                                targets.iter().any(|t| {
                                    self.same_domain(t, b)
                                        && (self.same_symbol(t, b)
                                            || (o.construction
                                                && t.decl.named_type()
                                                && b.decl.kind == "constructor"
                                                && t.decl.qualified == b.decl.owner))
                                        && (!t.decl.local()
                                            || (t.file == b.file
                                                && t.decl.name_span == b.decl.name_span))
                                })
                            })
                            .collect::<Vec<_>>();
                        if relevant.is_empty() {
                            continue;
                        }
                        relevant.iter().all(|(_, p)| *p)
                    };
                    let display = self.manifest.display(file, m);
                    let rank = (uncertain, display.to_string(), o.span.start);
                    if !units.accepts(&rank) {
                        continue;
                    }
                    let (line_no, col) = position(&data.source, o.span.start);
                    let owner = containing
                        .as_ref()
                        .map(|d| d.qualified.as_str())
                        .unwrap_or(&display);
                    let unit = crate::render::result(
                        owner,
                        &format!("{display}:{line_no}:{col}"),
                        line(&data.source, o.span.start),
                        file.language,
                    );
                    units.insert(rank, unit);
                }
            }
        }
        Ok(units.finish_with(|(uncertain, _, _), unit| {
            if uncertain {
                format!("{unit}\n\nPossible match; the target could not be determined uniquely.")
            } else {
                unit
            }
        }))
    }
    fn hierarchy(
        &mut self,
        q: &Query,
        targets: &[Hit],
        inside: &[(bool, Vec<Hit>)],
    ) -> Result<String> {
        if q.selector == "derived"
            && targets
                .iter()
                .any(|h| self.manifest.files[&h.file].language == Language::Rust)
        {
            anyhow::bail!("Rust has no class inheritance; use `impl:` for traits.");
        }
        if targets.iter().any(|h| h.decl.callable()) {
            let mut units = crate::selection::Selection::new(q.limit);
            for target in targets.iter().filter(|h| h.decl.callable()) {
                let owners = self.declarations(
                    &Target {
                        name: target.decl.owner.clone(),
                        parameters: None,
                        location: None,
                    },
                    false,
                )?;
                let interface = owners
                    .iter()
                    .any(|h| matches!(h.decl.kind.as_str(), "interface" | "trait"));
                for candidate in self.declarations(
                    &Target {
                        name: target.decl.name.clone(),
                        parameters: if self.manifest.files[&target.file].language
                            == Language::CSharp
                        {
                            None
                        } else {
                            Some(target.decl.parameters.clone())
                        },
                        location: None,
                    },
                    false,
                )? {
                    if !candidate.decl.callable()
                        || candidate.decl.owner == target.decl.owner
                        || (!interface && !candidate.decl.modifiers.iter().any(|m| m == "override"))
                    {
                        continue;
                    }
                    if self.manifest.files[&target.file].language == Language::CSharp {
                        if !self.csharp_relates(target, &candidate)? {
                            continue;
                        }
                    } else {
                        let roots: Vec<_> = self
                            .declarations(
                                &Target {
                                    name: candidate.decl.owner.clone(),
                                    parameters: None,
                                    location: None,
                                },
                                false,
                            )?
                            .into_iter()
                            .filter(|h| {
                                h.membership.project == candidate.membership.project
                                    && h.decl.qualified == candidate.decl.owner
                            })
                            .collect();
                        let ancestors = self.ancestor_owners(roots)?;
                        if !ancestors.contains(&target.decl.owner) {
                            continue;
                        }
                    }
                    let f = &self.manifest.files[&candidate.file];
                    if !self.filters(
                        q,
                        f,
                        &candidate.membership,
                        Some(&candidate.decl),
                        candidate.decl.name_span.start,
                        inside,
                    ) {
                        continue;
                    }
                    let rank = (
                        self.manifest.display(f, &candidate.membership).into_owned(),
                        candidate.decl.name_span.start,
                    );
                    if units.accepts(&rank) {
                        units.insert(rank, self.declaration_unit(&candidate)?);
                    }
                }
            }
            return Ok(units.finish());
        }
        let mut selected = targets.to_vec();
        let mut units = crate::selection::Selection::new(q.limit);
        let mut seen: BTreeSet<_> = targets
            .iter()
            .map(|h| (h.file.clone(), h.decl.name_span.start, h.membership.project))
            .collect();
        loop {
            let mut added = Vec::new();
            let names: BTreeSet<_> = selected.iter().map(|h| h.decl.name.as_str()).collect();
            let csharp = selected
                .iter()
                .all(|h| self.manifest.files[&h.file].language == Language::CSharp);
            let mut candidate_files = BTreeSet::new();
            if csharp {
                for target in &selected {
                    candidate_files.extend(self.candidates(
                        &format!("@base:{}", target.decl.name),
                        false,
                        false,
                    )?);
                    for alias_file in
                        self.candidates(&format!("@alias:{}", target.decl.name), false, false)?
                    {
                        let data = self.data(&alias_file)?;
                        for import in &data.facts.imports {
                            if !import.alias.is_empty()
                                && (type_path(&import.path) == type_path(&target.decl.qualified)
                                    || import.path == target.decl.name)
                            {
                                candidate_files.extend(self.candidates(
                                    &format!("@base:{}", import.alias),
                                    false,
                                    false,
                                )?);
                            }
                        }
                    }
                }
            }
            for (key, file) in &self.manifest.files {
                if csharp && !candidate_files.contains(key) {
                    continue;
                }
                self.check()?;
                let declarations = if file.metadata {
                    self.assemblies
                } else {
                    self.store
                }
                .declarations_in(
                    if file.metadata {
                        &self.assembly_tx
                    } else {
                        &self.source_tx
                    },
                    key,
                )?;
                for declaration in declarations.iter().filter(|d| {
                    if csharp {
                        !d.bases.is_empty()
                    } else {
                        d.bases.iter().any(|base| {
                            names.contains(simple_name(base.split('<').next().unwrap_or(base)))
                        })
                    }
                }) {
                    for membership in &file.memberships {
                        if seen.contains(&(
                            key.clone(),
                            declaration.name_span.start,
                            membership.project,
                        )) {
                            continue;
                        }
                        let h = Hit {
                            semantic_id: None,
                            file: key.clone(),
                            membership: membership.clone(),
                            decl: contextual(declaration, membership),
                            rank: 0,
                        };
                        let mut matches = false;
                        for t in &selected {
                            if (q.selector != "derived"
                                || (h.decl.kind == "class" && t.decl.kind == "class"))
                                && self.base_matches(&h, t)?
                            {
                                matches = true;
                                break;
                            }
                        }
                        if matches {
                            seen.insert((
                                h.file.clone(),
                                h.decl.name_span.start,
                                h.membership.project,
                            ));
                            if !matches!(h.decl.kind.as_str(), "interface" | "trait")
                                && self.filters(
                                    q,
                                    file,
                                    &h.membership,
                                    Some(&h.decl),
                                    h.decl.name_span.start,
                                    inside,
                                )
                            {
                                let rank = (
                                    file.metadata,
                                    self.manifest.display(file, &h.membership).into_owned(),
                                    h.decl.name_span.start,
                                );
                                if units.accepts(&rank) {
                                    units.insert(rank, self.declaration_unit(&h)?);
                                }
                            }
                            added.push(h);
                        }
                    }
                }
            }
            if added.is_empty() {
                break;
            }
            selected = added;
        }
        Ok(units.finish())
    }
    fn declaration_unit(&mut self, h: &Hit) -> Result<String> {
        let f = &self.manifest.files[&h.file];
        let data = self.data(&h.file)?;
        let header = data.source[h.decl.header.clone()].trim();
        if f.metadata {
            let signature = crate::render::external_declaration(&h.decl, header);
            Ok(crate::render::result(
                &crate::render::metadata(&h.decl.qualified),
                &f.path.file_name().unwrap_or_default().to_string_lossy(),
                &signature,
                f.language,
            ))
        } else {
            let (line, column) = position(&data.source, h.decl.name_span.start);
            let source = crate::render::source_excerpt(&data.source, h.decl.span.clone());
            Ok(crate::render::result(
                &h.decl.qualified,
                &format!(
                    "{}:{line}:{column}",
                    self.manifest.display(f, &h.membership)
                ),
                &source,
                f.language,
            ))
        }
    }
    fn containment_files(&self, inside: &[(bool, Vec<Hit>)]) -> Result<Option<BTreeSet<String>>> {
        let mut allowed: Option<BTreeSet<String>> = None;
        for (_, targets) in inside.iter().filter(|(negate, _)| !*negate) {
            if targets
                .iter()
                .any(|h| !h.decl.named_type() && !h.decl.callable())
            {
                continue;
            }
            let mut files: BTreeSet<_> = targets.iter().map(|h| h.file.clone()).collect();
            for h in targets.iter().filter(|h| h.decl.named_type()) {
                files.extend(self.candidates(&h.decl.name, false, false)?);
                if self.manifest.files[&h.file].language == Language::Rust {
                    files.extend(self.candidates(&h.decl.name, false, true)?);
                }
            }
            if let Some(allowed) = allowed.as_mut() {
                allowed.retain(|key| files.contains(key));
            } else {
                allowed = Some(files);
            }
        }
        Ok(allowed)
    }
    fn base_matches(&mut self, owner: &Hit, target: &Hit) -> Result<bool> {
        if self.manifest.files[&owner.file].language == Language::CSharp {
            return self.csharp_relates(target, owner);
        }
        if !self.visible(owner.membership.project, target.membership.project) {
            return Ok(false);
        }
        for written in &owner.decl.bases {
            let base = written.split('<').next().unwrap_or(written);
            if base == target.decl.qualified {
                return Ok(true);
            }
            if simple_name(base) != target.decl.name {
                continue;
            }
            let data = self.data(&owner.file)?;
            if self.in_scope(
                &target.decl,
                base,
                Some(&owner.decl),
                &owner.membership,
                &data.facts.imports,
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn csharp_relates(&mut self, target: &Hit, candidate: &Hit) -> Result<bool> {
        let view = crate::csharp::catalog::View {
            source: self.store,
            assemblies: self.assemblies,
            source_tx: &self.source_tx,
            assembly_tx: &self.assembly_tx,
            manifest: self.manifest,
            cancel: self.cancel,
        };
        fn site(h: &Hit) -> crate::csharp::catalog::Site<'_> {
            crate::csharp::catalog::Site {
                file: h.file.as_str(),
                project: h.membership.project,
                declaration: &h.decl,
            }
        }
        self.csharp.relates(&view, &site(target), &site(candidate))
    }
}

fn contextual(d: &Declaration, m: &Membership) -> Declaration {
    let mut d = d.clone();
    d.qualified = context_name(&d.qualified, m);
    d.owner = context_name(&d.owner, m);
    d.namespace = context_name(&d.namespace, m);
    d
}
fn context_name(name: &str, m: &Membership) -> String {
    if m.module.is_empty() {
        name.into()
    } else if name.is_empty() {
        m.module.clone()
    } else if let Some(name) = name.strip_prefix("::") {
        name.into()
    } else if let Some(name) = name.strip_prefix("crate::") {
        format!("{}::{name}", m.module.split("::").next().unwrap())
    } else if let Some(name) = name.strip_prefix("self::") {
        format!("{}::{name}", m.module)
    } else if name.starts_with("super::") {
        let mut module = m.module.as_str();
        let mut name = name;
        while let Some(rest) = name.strip_prefix("super::") {
            module = module
                .rsplit_once("::")
                .map_or(module, |(parent, _)| parent);
            name = rest;
        }
        format!("{module}::{name}")
    } else {
        format!("{}::{name}", m.module)
    }
}
fn simple_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}
fn same_logical(a: &Declaration, b: &Declaration) -> bool {
    a.kind == b.kind && a.qualified == b.qualified && a.parameters == b.parameters
}
fn component_prefix(value: &str, prefix: &str) -> bool {
    value == prefix
        || value
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('.') || s.starts_with("::"))
}
fn expand_alias(value: &str, imports: &[Import]) -> String {
    imports
        .iter()
        .find(|i| i.alias == value)
        .map(|i| i.path.clone())
        .unwrap_or_else(|| value.into())
}
fn selector_matches(selector: &str, d: &Declaration) -> bool {
    match selector {
        "symbol" => true,
        "type" => d.named_type(),
        "namespace" => matches!(d.kind.as_str(), "namespace" | "module"),
        _ => selector == d.kind,
    }
}
fn attribute_name(a: &str) -> &str {
    a.trim_start_matches("#[")
        .split(['(', ']'])
        .next()
        .unwrap_or(a)
        .trim()
        .trim_end_matches("Attribute")
}
const BUILTIN_TYPES: &[(&str, &str)] = &[
    ("bool", "System.Boolean"),
    ("byte", "System.Byte"),
    ("sbyte", "System.SByte"),
    ("short", "System.Int16"),
    ("ushort", "System.UInt16"),
    ("int", "System.Int32"),
    ("uint", "System.UInt32"),
    ("long", "System.Int64"),
    ("ulong", "System.UInt64"),
    ("nint", "System.IntPtr"),
    ("nuint", "System.UIntPtr"),
    ("char", "System.Char"),
    ("float", "System.Single"),
    ("double", "System.Double"),
    ("decimal", "System.Decimal"),
    ("string", "System.String"),
    ("object", "System.Object"),
    ("void", "System.Void"),
];
fn builtin_type(name: &str) -> Option<&'static str> {
    BUILTIN_TYPES
        .iter()
        .find_map(|(keyword, qualified)| (*keyword == name).then_some(*qualified))
}
fn type_path(written: &str) -> String {
    let mut depth = 0usize;
    let mut arity = false;
    let mut path = String::new();
    for c in written.trim().trim_start_matches("global::").chars() {
        if c == '<' {
            depth += 1;
            continue;
        }
        if c == '>' && depth > 0 {
            depth -= 1;
            continue;
        }
        if depth > 0 {
            continue;
        }
        if c == '`' {
            arity = true;
            continue;
        }
        if arity && c.is_ascii_digit() {
            continue;
        }
        arity = false;
        path.push(c);
    }
    path
}
fn normalized_type(s: &str) -> String {
    let s = s
        .split_whitespace()
        .filter(|s| !matches!(*s, "this" | "params"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::new();
    for token in s.split_inclusive(|c: char| !c.is_alphanumeric() && c != '_') {
        let len = token
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .len();
        let (word, suffix) = token.split_at(len);
        out.push_str(builtin_type(word).unwrap_or(word));
        out.extend(suffix.chars().filter(|c| !c.is_whitespace()));
    }
    out
}
fn signature_matches(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| normalized_type(a) == normalized_type(b))
}
pub fn render(units: Vec<String>, limit: usize) -> String {
    let total = units.len();
    render_selected(units.into_iter().take(limit).collect(), total)
}
pub(crate) fn render_selected(units: Vec<String>, total: usize) -> String {
    if units.is_empty() {
        return "No matches.".into();
    }
    let shown = units.len();
    let mut out = units.join("\n\n").trim_end().to_owned();
    if total > shown {
        out.push_str("\n\n");
        out.push_str(&crate::render::omission(Some(total)));
    }
    out
}
