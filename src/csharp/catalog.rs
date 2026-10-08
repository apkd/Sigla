//! A query's declaration universe, independent of result filters and ranking.
use super::{
    syntax::{BodyFile, DeclarationFile, Header},
    types::DefinitionId,
};
use crate::{
    model::{Declaration, Language},
    store::{DeclarationLookup, Store},
    workspace::Manifest,
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

pub(crate) struct View<'a> {
    pub store: &'a Store,
    pub tx: &'a heed::RoTxn<'a>,
    pub manifest: &'a Manifest,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

pub(crate) struct Site<'a> {
    pub file: &'a str,
    pub project: usize,
    pub declaration: &'a Declaration,
}

#[derive(Clone)]
pub(crate) struct Symbol {
    pub file: String,
    pub project: usize,
    position: usize,
    facts: Arc<DeclarationFile>,
    pub id: DefinitionId,
}
impl Symbol {
    pub fn declaration(&self) -> &Declaration {
        &self.facts.declarations[self.position]
    }
    pub fn header(&self) -> &Header {
        &self.facts.headers[self.position]
    }
}

#[derive(Default)]
pub(crate) struct Catalog {
    headers: HashMap<String, Arc<DeclarationFile>>,
    bytes: usize,
    lookups: HashMap<(String, usize, DeclarationLookup), Vec<Symbol>>,
    imports: HashMap<String, Vec<super::syntax::Import>>,
    globals: Option<HashMap<usize, Vec<super::syntax::Import>>>,
    assemblies: Option<HashMap<String, Vec<String>>>,
    forwarders: HashMap<String, Vec<(String, String)>>,
    pub work: usize,
}
impl Catalog {
    pub fn assembly_scope(
        &mut self,
        view: &View<'_>,
        assembly: &str,
        qualified: &str,
    ) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        if self.assemblies.is_none() {
            let mut assemblies: HashMap<String, Vec<String>> = HashMap::new();
            for (key, file) in &view.manifest.files {
                if file.metadata
                    && let Some(name) = view.store.assembly_name_in(view.tx, key)?
                {
                    assemblies.entry(name).or_default().push(key.clone());
                }
            }
            self.assemblies = Some(assemblies);
        }
        let mut pending = vec![assembly.to_owned()];
        let mut scopes = BTreeSet::new();
        let mut files = BTreeSet::new();
        while let Some(assembly) = pending.pop() {
            if !scopes.insert(assembly.clone()) {
                continue;
            }
            self.check(view)?;
            ensure!(
                scopes.len() <= 32,
                "Assembly forwarding chain exceeds work bound"
            );
            for file in self
                .assemblies
                .as_ref()
                .unwrap()
                .get(&assembly)
                .cloned()
                .unwrap_or_default()
            {
                files.insert(file.clone());
                if !self.forwarders.contains_key(&file) {
                    self.forwarders.insert(
                        file.clone(),
                        view.store.assembly_forwarders(view.tx, &file)?,
                    );
                }
                for (name, destination) in &self.forwarders[&file] {
                    let name = name
                        .split('.')
                        .map(|n| n.split('`').next().unwrap_or(n))
                        .collect::<Vec<_>>()
                        .join(".");
                    if qualified == name
                        || qualified
                            .strip_prefix(&name)
                            .is_some_and(|suffix| suffix.starts_with('.'))
                    {
                        pending.push(destination.clone());
                    }
                }
            }
        }
        Ok((scopes, files))
    }
    pub fn imports(
        &mut self,
        view: &View<'_>,
        symbol: &Symbol,
    ) -> Result<Vec<super::syntax::Import>> {
        if self.globals.is_none() {
            let mut globals: HashMap<usize, Vec<super::syntax::Import>> = HashMap::new();
            for (file, imports) in view.store.csharp_global_imports(view.tx)? {
                if let Some(file) = view.manifest.files.get(&file) {
                    for membership in &file.memberships {
                        globals
                            .entry(membership.project)
                            .or_default()
                            .extend(imports.clone());
                    }
                }
            }
            self.globals = Some(globals);
        }
        if !self.imports.contains_key(&symbol.file) {
            self.imports.insert(
                symbol.file.clone(),
                view.store.csharp_imports(view.tx, &symbol.file)?,
            );
        }
        let mut imports: Vec<_> = self.imports[&symbol.file]
            .iter()
            .filter(|i| !i.global && i.scope.contains(&symbol.declaration().span.start))
            .cloned()
            .collect();
        imports.extend(
            self.globals
                .as_ref()
                .unwrap()
                .get(&symbol.project)
                .into_iter()
                .flatten()
                .cloned(),
        );
        Ok(imports)
    }
    pub fn check(&mut self, view: &View<'_>) -> Result<()> {
        if view.cancel.is_cancelled() {
            return Err(crate::diagnostics::Cancelled.into());
        }
        self.work = self.work.saturating_add(1);
        crate::diagnostics::enforce_limit("analysis_work", self.work, 100_000)?;
        Ok(())
    }
    pub fn headers(&mut self, view: &View<'_>, file: &str) -> Result<Arc<DeclarationFile>> {
        self.check(view)?;
        if let Some(data) = self.headers.get(file) {
            return Ok(data.clone());
        }
        let data = view.store.csharp_headers(view.tx, file)?;
        let data = data.ok_or_else(|| anyhow::anyhow!("Missing C# declaration record"))?;
        self.retain_headers(file, data)
    }
    fn retain_headers(
        &mut self,
        key: &str,
        data: Arc<DeclarationFile>,
    ) -> Result<Arc<DeclarationFile>> {
        if let Some(cached) = self.headers.get(key) {
            return Ok(cached.clone());
        }
        let bytes = postcard::experimental::serialized_size(data.as_ref())?.saturating_mul(4);
        crate::diagnostics::enforce_limit(
            "estimated_declaration_bytes",
            self.bytes.saturating_add(bytes),
            128 * 1024 * 1024,
        )?;
        self.headers.insert(key.into(), data.clone());
        self.bytes += bytes;
        Ok(data)
    }
    pub fn body(&mut self, view: &View<'_>, file: &str, position: usize) -> Result<Arc<BodyFile>> {
        self.check(view)?;
        let body = view
            .store
            .csharp_body(view.tx, file, position)?
            .ok_or_else(|| anyhow::anyhow!("Missing C# body record"))?;
        ensure!(
            postcard::experimental::serialized_size(body.as_ref())? * 4 <= 64 * 1024 * 1024,
            "C# body memory limit reached"
        );
        Ok(body)
    }
    pub fn symbol(
        &mut self,
        view: &View<'_>,
        file: &str,
        project: usize,
        index: u32,
    ) -> Result<Symbol> {
        let facts = self.declaration(view, file, index)?;
        let position = facts
            .headers
            .iter()
            .position(|h| h.declaration == index)
            .ok_or_else(|| anyhow::anyhow!("Missing declaration index"))?;
        self.symbol_with_facts(view, file, project, position as u32, facts)
    }
    fn declaration(
        &mut self,
        view: &View<'_>,
        file: &str,
        index: u32,
    ) -> Result<Arc<DeclarationFile>> {
        self.check(view)?;
        let key = format!("declaration:{file}:{index}");
        let facts = if let Some(facts) = self.headers.get(&key) {
            facts.clone()
        } else {
            let facts = view
                .store
                .csharp_declaration(view.tx, file, index)?
                .ok_or_else(|| anyhow::anyhow!("Missing C# declaration"))?;
            self.retain_headers(&key, facts)?
        };
        Ok(facts)
    }
    fn symbol_with_facts(
        &mut self,
        view: &View<'_>,
        file: &str,
        project: usize,
        index: u32,
        facts: Arc<DeclarationFile>,
    ) -> Result<Symbol> {
        let decl = &facts.declarations[index as usize];
        let header = &facts.headers[index as usize];
        let context = if view.manifest.files[file].metadata {
            file.into()
        } else {
            view.manifest.projects[project].identity.clone()
        };
        let mut owners = Vec::new();
        if !view.manifest.files[file].metadata {
            let mut owner = header.owner;
            while let Some(index) = owner {
                let facts = self.declaration(view, file, index)?;
                let header = &facts.headers[0];
                owners.push((
                    facts.declarations[0].name.clone(),
                    header.generics.len(),
                    header.parameters.clone(),
                ));
                owner = header.owner;
            }
        }
        let signature = postcard::to_allocvec(&(
            owners,
            &header
                .parameters
                .iter()
                .map(|p| (&p.ty, p.mode))
                .collect::<Vec<_>>(),
            header.generics.len(),
            &header.explicit_interface,
        ))?;
        // Preserve other symbol keys while distinguishing conversion destinations.
        let signature = if decl.kind == "operator" {
            postcard::to_allocvec(&(&signature, &header.ty))?
        } else {
            signature
        };
        let key = if view.manifest.files[file].metadata {
            format!("metadata:{}", header.declaration)
        } else if header.local {
            format!("{}@{}:{}", decl.qualified, file, decl.name_span.start)
        } else {
            format!(
                "{}:{}:{}",
                decl.kind,
                decl.qualified,
                blake3::hash(&signature).to_hex()
            )
        };
        Ok(Symbol {
            file: file.into(),
            project,
            position: index as usize,
            facts,
            id: DefinitionId { context, key },
        })
    }
    pub fn owner(&mut self, view: &View<'_>, symbol: &Symbol) -> Result<Option<Symbol>> {
        symbol
            .header()
            .owner
            .map(|i| self.symbol(view, &symbol.file, symbol.project, i))
            .transpose()
    }
    pub fn in_file(
        &mut self,
        view: &View<'_>,
        file: &str,
        project: usize,
        name: &str,
        kind: DeclarationLookup,
    ) -> Result<Vec<Symbol>> {
        view.store
            .csharp_lookup(view.tx, file, name, kind)?
            .into_iter()
            .map(|index| self.symbol(view, file, project, index))
            .collect()
    }
    pub fn members(&mut self, view: &View<'_>, owner: &Symbol, name: &str) -> Result<Vec<Symbol>> {
        let parts = if owner.declaration().modifiers.iter().any(|m| m == "partial") {
            self.lookup(
                view,
                &owner.declaration().name,
                owner.project,
                DeclarationLookup::Type,
            )?
            .into_iter()
            .filter(|s| s.id == owner.id)
            .collect()
        } else {
            vec![owner.clone()]
        };
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in parts {
            result.extend(
                self.in_file(
                    view,
                    &part.file,
                    part.project,
                    name,
                    DeclarationLookup::Members(part.header().declaration),
                )?
                .into_iter()
                .filter(|member| seen.insert(member.id.clone())),
            );
        }
        Ok(result)
    }
    pub fn lookup(
        &mut self,
        view: &View<'_>,
        name: &str,
        project: usize,
        kind: DeclarationLookup,
    ) -> Result<Vec<Symbol>> {
        self.check(view)?;
        let lookup_key = (name.to_owned(), project, kind);
        if let Some(cached) = self.lookups.get(&lookup_key) {
            return Ok(cached.clone());
        }
        let keys = view.store.candidates(view.tx, name, false, false)?;
        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        for key in keys {
            let Some(file) = view.manifest.files.get(&key) else {
                continue;
            };
            if file.language != Language::CSharp {
                continue;
            }
            if !file.memberships.iter().any(|m| {
                if file.metadata {
                    m.project == project && view.manifest.metadata_visible(file, project)
                } else {
                    visible(view.manifest, project, m.project)
                }
            }) {
                continue;
            }
            for membership in &file.memberships {
                if if file.metadata {
                    membership.project != project || !view.manifest.metadata_visible(file, project)
                } else {
                    !visible(view.manifest, project, membership.project)
                } {
                    continue;
                }
                for symbol in self.in_file(view, &key, membership.project, name, kind)? {
                    let declaration = symbol.declaration();
                    if seen.insert((symbol.id.context.clone(), symbol.id.key.clone()))
                        || declaration.named_type()
                            && declaration.modifiers.iter().any(|m| m == "partial")
                    {
                        result.push(symbol);
                    }
                }
            }
        }
        self.lookups.insert(lookup_key, result.clone());
        Ok(result)
    }
}

fn visible(manifest: &Manifest, from: usize, to: usize) -> bool {
    from == to
        || manifest.projects[from]
            .references
            .iter()
            .any(|reference| reference.visible(&manifest.projects[to].identity))
}
