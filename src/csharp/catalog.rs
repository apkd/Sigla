//! A query's declaration universe, independent of result filters and ranking.
use super::{
    syntax::{BodyFile, DeclarationFile, Header},
    types::DefinitionId,
};
use crate::{
    model::{Declaration, Language},
    store::Store,
    workspace::Manifest,
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

pub(crate) struct View<'a> {
    pub source: &'a Store,
    pub assemblies: &'a Store,
    pub source_tx: &'a heed::RoTxn<'a>,
    pub assembly_tx: &'a heed::RoTxn<'a>,
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
    pub facts: Arc<DeclarationFile>,
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
    lookups: HashMap<(String, usize), Vec<Symbol>>,
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
                    && let Some(name) = view.assemblies.assembly_name_in(view.assembly_tx, key)?
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
                        view.assemblies
                            .assembly_forwarders(view.assembly_tx, &file)?,
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
            for (file, imports) in view.source.csharp_global_imports(view.source_tx)? {
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
        let mut imports: Vec<_> = symbol
            .facts
            .imports
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
        ensure!(!view.cancel.is_cancelled(), "Query cancelled");
        self.work += 1;
        ensure!(
            self.work <= 100_000,
            "C# analysis work limit reached; narrow the query"
        );
        Ok(())
    }
    pub fn headers(&mut self, view: &View<'_>, file: &str) -> Result<Arc<DeclarationFile>> {
        self.check(view)?;
        if let Some(data) = self.headers.get(file) {
            return Ok(data.clone());
        }
        let data = if view.manifest.files[file].metadata {
            view.assemblies.csharp_headers(view.assembly_tx, file)?
        } else {
            view.source.csharp_headers(view.source_tx, file)?
        };
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
        let bytes = postcard::to_allocvec(data.as_ref())?.len() * 4;
        ensure!(
            self.bytes + bytes <= 128 * 1024 * 1024,
            "C# declaration memory limit reached; narrow the query"
        );
        self.headers.insert(key.into(), data.clone());
        self.bytes += bytes;
        Ok(data)
    }
    pub fn body(&mut self, view: &View<'_>, file: &str, position: usize) -> Result<Arc<BodyFile>> {
        self.check(view)?;
        let body = view
            .source
            .csharp_body(view.source_tx, file, position)?
            .ok_or_else(|| anyhow::anyhow!("Missing C# body record"))?;
        ensure!(
            postcard::to_allocvec(body.as_ref())?.len() * 4 <= 64 * 1024 * 1024,
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
        let facts = if view.manifest.files[file].metadata {
            let facts = view
                .assemblies
                .csharp_declaration(view.assembly_tx, file, index)?
                .ok_or_else(|| anyhow::anyhow!("Missing metadata declaration"))?;
            let key = format!("name:{file}:{}", facts.declarations[0].name);
            self.retain_headers(&key, facts)?
        } else {
            self.headers(view, file)?
        };
        let position = facts
            .headers
            .iter()
            .position(|h| h.declaration == index)
            .ok_or_else(|| anyhow::anyhow!("Missing declaration index"))?;
        Self::symbol_with_facts(view, file, project, position as u32, facts)
    }
    fn symbol_with_facts(
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
                let header = &facts.headers[index as usize];
                owners.push((
                    facts.declarations[index as usize].name.as_str(),
                    header.generics.len(),
                    &header.parameters,
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
    pub fn lookup(&mut self, view: &View<'_>, name: &str, project: usize) -> Result<Vec<Symbol>> {
        self.check(view)?;
        let lookup_key = (name.to_owned(), project);
        if let Some(cached) = self.lookups.get(&lookup_key) {
            return Ok(cached.clone());
        }
        let mut keys = view.source.candidates(view.source_tx, name, false, false)?;
        keys.extend(
            view.assemblies
                .candidates(view.assembly_tx, name, false, false)?,
        );
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
            let facts = if file.metadata {
                let facts = view
                    .assemblies
                    .csharp_members(view.assembly_tx, &key, name)?
                    .ok_or_else(|| anyhow::anyhow!("Missing metadata name record"))?;
                self.retain_headers(&format!("name:{key}:{name}"), facts)?
            } else {
                self.headers(view, &key)?
            };
            for membership in &file.memberships {
                if if file.metadata {
                    membership.project != project || !view.manifest.metadata_visible(file, project)
                } else {
                    !visible(view.manifest, project, membership.project)
                } {
                    continue;
                }
                for (index, declaration) in facts.declarations.iter().enumerate() {
                    if declaration.name != name || facts.headers[index].local {
                        continue;
                    }
                    let symbol = Self::symbol_with_facts(
                        view,
                        &key,
                        membership.project,
                        index as u32,
                        facts.clone(),
                    )?;
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
