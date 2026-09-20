//! Target-independent binding. Search filters never enter this module.
use super::{
    catalog::{Catalog, Site, Symbol, View},
    inference::{BoundKind, Inference},
    syntax::*,
    types::*,
};
use anyhow::Result;
use std::collections::HashMap;

#[derive(Clone, Default)]
struct Bound {
    ty: Option<Type>,
    symbols: Vec<Symbol>,
    category: Category,
    uncertain: bool,
    constant: Option<Constant>,
    conditional: bool,
}
#[derive(Clone, Default)]
enum Category {
    #[default]
    Value,
    Type,
    Namespace(String),
    Methods {
        receiver: Option<Type>,
        explicit: Vec<WrittenType>,
    },
    Lambda(ExprId),
}

#[derive(Default)]
pub(crate) struct Binder {
    catalog: Catalog,
    definitions: HashMap<DefinitionId, Symbol>,
    observed: Vec<(ExprId, Bound)>,
    #[cfg(test)]
    pub(super) trace_type: Option<Type>,
    #[cfg(test)]
    pub(super) cache_hits: usize,
}

impl Binder {
    #[cfg(test)]
    pub(super) fn definition_symbol(&self, id: &DefinitionId) -> Option<&Symbol> {
        self.definitions.get(id)
    }
    fn site(&mut self, view: &View<'_>, site: &Site<'_>) -> Result<Option<Symbol>> {
        if view.manifest.files[site.file].metadata {
            Ok(self
                .lookup(view, &site.declaration.name, site.project)?
                .into_iter()
                .find(|s| {
                    s.file == site.file && s.declaration().name_span == site.declaration.name_span
                }))
        } else {
            let headers = self.catalog.headers(view, site.file)?;
            let Some(index) = headers
                .declarations
                .iter()
                .position(|d| d.name_span == site.declaration.name_span)
            else {
                return Ok(None);
            };
            let symbol = self
                .catalog
                .symbol(view, site.file, site.project, index as u32)?;
            Ok(Some(self.remember(symbol)))
        }
    }

    pub fn relates(
        &mut self,
        view: &View<'_>,
        target: &Site<'_>,
        candidate: &Site<'_>,
    ) -> Result<bool> {
        let (Some(target), Some(candidate)) =
            (self.site(view, target)?, self.site(view, candidate)?)
        else {
            return Ok(false);
        };
        if candidate.id == target.id {
            return Ok(false);
        }
        if target.declaration().named_type() {
            let ty = self.open_type(view, &candidate)?;
            return Ok(!self.projections(view, &ty, &target.id, 0)?.is_empty());
        }
        let (Some(target_owner), Some(candidate_owner)) =
            (self.owner(view, &target)?, self.owner(view, &candidate)?)
        else {
            return Ok(false);
        };
        let interface = target_owner.declaration().kind == "interface";
        if !interface
            && !candidate
                .declaration()
                .modifiers
                .iter()
                .any(|m| m == "override")
        {
            return Ok(false);
        }
        if candidate.header().generics.len() != target.header().generics.len()
            || candidate.header().parameters.len() != target.header().parameters.len()
        {
            return Ok(false);
        }
        if interface
            && candidate.header().explicit_interface.is_none()
            && candidate.declaration().access != "public"
        {
            return Ok(false);
        }
        let candidate_type = self.open_type(view, &candidate_owner)?;
        for projection in self.projections(view, &candidate_type, &target_owner.id, 0)? {
            if let Some(explicit) = &candidate.header().explicit_interface {
                let explicit = self.resolve_type(view, explicit, &candidate, 0)?;
                if explicit != projection {
                    continue;
                }
            }
            let mut substitutions = Self::substitutions(&projection);
            substitutions.extend(target.header().generics.iter().enumerate().map(
                |(ordinal, _)| {
                    (
                        ParameterId {
                            owner: target.id.clone(),
                            ordinal: ordinal as u32,
                        },
                        Type::Parameter(ParameterId {
                            owner: candidate.id.clone(),
                            ordinal: ordinal as u32,
                        }),
                    )
                },
            ));
            let mut equal = true;
            for (expected, actual) in target
                .header()
                .parameters
                .iter()
                .zip(&candidate.header().parameters)
            {
                let expected_type = self.resolve_type(view, &expected.ty, &target, 0)?;
                let expected_type = substitute(&expected_type, &substitutions, &mut 4096)
                    .unwrap_or(Type::Unsupported);
                let actual_type = self.resolve_type(view, &actual.ty, &candidate, 0)?;
                if expected.mode != actual.mode
                    || expected_type != actual_type
                    || matches!(expected_type, Type::Unsupported | Type::Unresolved { .. })
                {
                    equal = false;
                    break;
                }
            }
            if equal {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn projections(
        &mut self,
        view: &View<'_>,
        ty: &Type,
        target: &DefinitionId,
        depth: usize,
    ) -> Result<Vec<Type>> {
        if depth > 24 {
            return Ok(vec![]);
        }
        self.catalog.check(view)?;
        if let Type::Array(element, 1) = ty
            && self.definitions.get(target).is_some_and(|s| {
                matches!(
                    strip_arity(&s.declaration().qualified).as_str(),
                    "System.Collections.Generic.IEnumerable"
                        | "System.Collections.Generic.ICollection"
                        | "System.Collections.Generic.IList"
                        | "System.Collections.Generic.IReadOnlyCollection"
                        | "System.Collections.Generic.IReadOnlyList"
                )
            })
        {
            return Ok(vec![Type::Named {
                definition: target.clone(),
                containing: None,
                arguments: vec![*element.clone()],
            }]);
        }
        let Type::Named { definition, .. } = ty else {
            return Ok(vec![]);
        };
        if definition == target {
            return Ok(vec![ty.clone()]);
        }
        let Some(owner) = self.definitions.get(definition).cloned() else {
            return Ok(vec![]);
        };
        let mut results = Vec::new();
        for base in self.base_types(view, &owner, ty, depth + 1)? {
            results.extend(self.projections(view, &base, target, depth + 1)?);
        }
        Ok(results)
    }
    pub fn definition(
        &mut self,
        view: &View<'_>,
        file: &str,
        project: usize,
        position: usize,
        name: &str,
    ) -> Result<Option<DefinitionId>> {
        if view.manifest.files[file].metadata {
            return Ok(self
                .lookup(view, name, project)?
                .into_iter()
                .find(|s| s.file == file && s.declaration().name_span.start == position)
                .map(|s| s.id));
        }
        let headers = self.catalog.headers(view, file)?;
        let Some(index) = headers
            .declarations
            .iter()
            .position(|d| d.name_span.start == position)
        else {
            return Ok(None);
        };
        Ok(Some(
            self.catalog.symbol(view, file, project, index as u32)?.id,
        ))
    }
    fn remember(&mut self, symbol: Symbol) -> Symbol {
        self.definitions.insert(symbol.id.clone(), symbol.clone());
        symbol
    }
    fn lookup(&mut self, view: &View<'_>, name: &str, project: usize) -> Result<Vec<Symbol>> {
        let symbols = self.catalog.lookup(view, name, project)?;
        let mut seen = std::collections::HashSet::new();
        Ok(symbols
            .into_iter()
            .filter(|s| seen.insert(s.id.clone()))
            .map(|s| self.remember(s))
            .collect())
    }
    fn base_types(
        &mut self,
        view: &View<'_>,
        symbol: &Symbol,
        ty: &Type,
        depth: usize,
    ) -> Result<Vec<Type>> {
        let parts = if symbol
            .declaration()
            .modifiers
            .iter()
            .any(|m| m == "partial")
        {
            self.catalog
                .lookup(view, &symbol.declaration().name, symbol.project)?
                .into_iter()
                .filter(|s| s.id == symbol.id)
                .collect()
        } else {
            vec![symbol.clone()]
        };
        let mut result = Vec::new();
        for part in parts {
            for base in &part.header().bases {
                let base = self.resolve_type(view, base, &part, depth + 1)?;
                let base = substitute(&base, &Self::substitutions(ty), &mut 4096)
                    .unwrap_or(Type::Unsupported);
                if !result.contains(&base) {
                    result.push(base);
                }
            }
        }
        Ok(result)
    }
    fn owner(&mut self, view: &View<'_>, symbol: &Symbol) -> Result<Option<Symbol>> {
        Ok(self.catalog.owner(view, symbol)?.map(|s| self.remember(s)))
    }
    fn open_type(&mut self, view: &View<'_>, symbol: &Symbol) -> Result<Type> {
        let containing = match self.owner(view, symbol)? {
            Some(owner) if owner.declaration().named_type() => {
                Some(Box::new(self.open_type(view, &owner)?))
            }
            _ => None,
        };
        Ok(Type::Named {
            definition: symbol.id.clone(),
            containing,
            arguments: symbol
                .header()
                .generics
                .iter()
                .enumerate()
                .map(|(ordinal, _)| {
                    Type::Parameter(ParameterId {
                        owner: symbol.id.clone(),
                        ordinal: ordinal as u32,
                    })
                })
                .collect(),
        })
    }
    fn substitutions(ty: &Type) -> Vec<(ParameterId, Type)> {
        let mut result = Vec::new();
        if let Type::Named {
            definition,
            containing,
            arguments,
        } = ty
        {
            if let Some(containing) = containing {
                result.extend(Self::substitutions(containing));
            }
            result.extend(arguments.iter().enumerate().map(|(ordinal, value)| {
                (
                    ParameterId {
                        owner: definition.clone(),
                        ordinal: ordinal as u32,
                    },
                    value.clone(),
                )
            }));
        }
        result
    }
    fn resolve_type(
        &mut self,
        view: &View<'_>,
        written: &WrittenType,
        context: &Symbol,
        depth: usize,
    ) -> Result<Type> {
        self.resolve_type_scoped(view, written, context, depth, None)
    }
    fn resolve_type_scoped(
        &mut self,
        view: &View<'_>,
        written: &WrittenType,
        context: &Symbol,
        depth: usize,
        assembly: Option<&str>,
    ) -> Result<Type> {
        if depth > 48 {
            return Ok(Type::Unsupported);
        }
        self.catalog.check(view)?;
        Ok(match written {
            WrittenType::Parameter(id) => Type::Parameter(id.clone()),
            WrittenType::MetadataParameter { method, ordinal } => {
                let mut owner = context.clone();
                if !method {
                    while !owner.declaration().named_type() {
                        let Some(parent) = self.owner(view, &owner)? else {
                            return Ok(Type::Unsupported);
                        };
                        owner = parent;
                    }
                    let mut chain = vec![owner.clone()];
                    while let Some(parent) = self.owner(view, &owner)? {
                        if !parent.declaration().named_type() {
                            break;
                        }
                        chain.push(parent.clone());
                        owner = parent;
                    }
                    let mut slot = *ordinal as usize;
                    for owner in chain.into_iter().rev() {
                        if slot < owner.header().generics.len() {
                            return Ok(Type::Parameter(ParameterId {
                                owner: owner.id,
                                ordinal: slot as u32,
                            }));
                        }
                        slot -= owner.header().generics.len();
                    }
                    return Ok(Type::Unsupported);
                }
                Type::Parameter(ParameterId {
                    owner: owner.id.clone(),
                    ordinal: *ordinal,
                })
            }
            WrittenType::External { ty, assembly } => {
                self.resolve_type_scoped(view, ty, context, depth + 1, Some(assembly))?
            }
            WrittenType::Array(ty, rank) => Type::Array(
                Box::new(self.resolve_type(view, ty, context, depth + 1)?),
                *rank,
            ),
            WrittenType::Pointer(ty) => {
                Type::Pointer(Box::new(self.resolve_type(view, ty, context, depth + 1)?))
            }
            WrittenType::Nullable(ty) => {
                Type::Nullable(Box::new(self.resolve_type(view, ty, context, depth + 1)?))
            }
            WrittenType::Tuple(elements) => Type::Tuple(
                elements
                    .iter()
                    .map(|(ty, name)| {
                        Ok((
                            self.resolve_type(view, ty, context, depth + 1)?,
                            name.clone(),
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            WrittenType::Dynamic => Type::Dynamic,
            WrittenType::Inferred | WrittenType::Unsupported(_) => Type::Unsupported,
            WrittenType::Name { alias, parts } => {
                let imports = self.catalog.imports(view, context)?;
                let Some(last) = parts.last() else {
                    return Ok(Type::Unsupported);
                };
                if parts.len() == 1 && alias.is_none() && last.arguments.is_empty() {
                    let mut owner = Some(context.clone());
                    while let Some(current) = owner {
                        if let Some(ordinal) = current
                            .header()
                            .generics
                            .iter()
                            .position(|p| p.name == last.name)
                        {
                            return Ok(Type::Parameter(ParameterId {
                                owner: current.id.clone(),
                                ordinal: ordinal as u32,
                            }));
                        }
                        owner = self.owner(view, &current)?;
                    }
                    if let Some(import) = imports
                        .iter()
                        .find(|i| matches!(&i.kind, ImportKind::Alias(name) if name == &last.name))
                    {
                        return self.resolve_type(view, &import.ty.clone(), context, depth + 1);
                    }
                }
                if parts.len() > 1
                    && alias.as_deref() != Some("global")
                    && let Some(import) = imports.iter().find(
                        |i| matches!(&i.kind, ImportKind::Alias(name) if name == &parts[0].name),
                    )
                    && let WrittenType::Name {
                        alias,
                        parts: prefix,
                    } = &import.ty
                {
                    let mut expanded = prefix.clone();
                    expanded.extend_from_slice(&parts[1..]);
                    return self.resolve_type(
                        view,
                        &WrittenType::Name {
                            alias: alias.clone(),
                            parts: expanded,
                        },
                        context,
                        depth + 1,
                    );
                }
                let simple = last.name.split('`').next().unwrap_or(&last.name);
                let qualified = parts
                    .iter()
                    .map(|p| p.name.split('`').next().unwrap_or(&p.name))
                    .collect::<Vec<_>>()
                    .join(".");
                if let Some(primitive) = Primitive::from_name(&qualified) {
                    return Ok(Type::Primitive(primitive));
                }
                let sought = qualified.as_str();
                let assembly_scope = assembly
                    .map(|name| self.catalog.assembly_scope(view, name, sought))
                    .transpose()?;
                let candidates = self.lookup(
                    view,
                    sought.rsplit('.').next().unwrap_or(simple),
                    context.project,
                )?;
                let mut candidates: Vec<_> = candidates
                    .into_iter()
                    .filter(|s| {
                        s.declaration().named_type()
                            && assembly_scope.as_ref().is_none_or(|(names, files)| {
                                let file = &view.manifest.files[&s.file];
                                if file.metadata {
                                    files.contains(&s.file)
                                } else {
                                    names.contains(&view.manifest.projects[s.project].name)
                                }
                            })
                            && (s.header().generics.len() == last.arguments.len()
                                || last.name.contains('`'))
                            && self.type_in_scope(
                                s,
                                sought,
                                context,
                                alias.as_deref() == Some("global"),
                                &imports,
                            )
                    })
                    .collect();
                // A source project and its output DLL describe the same compilation.
                let source_assemblies: Vec<_> = candidates
                    .iter()
                    .filter(|s| !view.manifest.files[&s.file].metadata)
                    .map(|s| view.manifest.projects[s.project].name.clone())
                    .collect();
                candidates.retain(|s| {
                    !view.manifest.files[&s.file].metadata
                        || !source_assemblies.iter().any(|name| {
                            view.manifest.files[&s.file]
                                .path
                                .file_stem()
                                .is_some_and(|stem| stem == name.as_str())
                        })
                });
                if candidates.len() > 1 && parts.len() > 1 {
                    let prefix = WrittenType::Name {
                        alias: alias.clone(),
                        parts: parts[..parts.len() - 1].to_vec(),
                    };
                    if let Type::Named { definition, .. } =
                        self.resolve_type_scoped(view, &prefix, context, depth + 1, assembly)?
                    {
                        let mut nested = Vec::new();
                        for candidate in candidates {
                            if self
                                .owner(view, &candidate)?
                                .is_some_and(|owner| owner.id == definition)
                            {
                                nested.push(candidate);
                            }
                        }
                        candidates = nested;
                    }
                }
                if candidates.len() == 1 {
                    let symbol = &candidates[0];
                    let arguments = last
                        .arguments
                        .iter()
                        .map(|t| self.resolve_type(view, t, context, depth + 1))
                        .collect::<Result<_>>()?;
                    let containing = if parts.len() > 1
                        && self
                            .owner(view, symbol)?
                            .is_some_and(|s| s.declaration().named_type())
                    {
                        let prefix = WrittenType::Name {
                            alias: alias.clone(),
                            parts: parts[..parts.len() - 1].to_vec(),
                        };
                        Some(Box::new(self.resolve_type_scoped(
                            view,
                            &prefix,
                            context,
                            depth + 1,
                            assembly,
                        )?))
                    } else {
                        None
                    };
                    Type::Named {
                        definition: symbol.id.clone(),
                        containing,
                        arguments,
                    }
                } else {
                    Type::Unresolved {
                        written: written.clone(),
                        context: context.id.clone(),
                    }
                }
            }
        })
    }
    fn type_in_scope(
        &self,
        candidate: &Symbol,
        sought: &str,
        context: &Symbol,
        global: bool,
        imports: &[Import],
    ) -> bool {
        let qualified = strip_arity(&candidate.declaration().qualified);
        if qualified == sought {
            return true;
        }
        if global {
            return false;
        }
        let mut namespace = context.declaration().namespace.as_str();
        loop {
            if !namespace.is_empty() && qualified == format!("{namespace}.{sought}") {
                return true;
            }
            let Some((parent, _)) = namespace.rsplit_once('.') else {
                break;
            };
            namespace = parent;
        }
        if !context.declaration().owner.is_empty()
            && qualified == format!("{}.{sought}", strip_arity(&context.declaration().owner))
        {
            return true;
        }
        imports.iter().any(|import| {
            matches!(import.kind, ImportKind::Namespace | ImportKind::Static)
                && written_name(&import.ty).is_some_and(|n| qualified == format!("{n}.{sought}"))
        })
    }
    fn members(
        &mut self,
        view: &View<'_>,
        ty: &Type,
        name: &str,
        project: usize,
        depth: usize,
    ) -> Result<Vec<(Symbol, Type)>> {
        if depth > 24 {
            return Ok(vec![]);
        }
        if let Type::Primitive(primitive) = ty {
            // Intrinsic types still expose the members of their framework definition.
            // Use every visible definition conservatively if references disagree.
            let mut result = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for symbol in self.lookup(view, primitive.metadata_name(), project)? {
                if symbol.declaration().qualified == format!("System.{}", primitive.metadata_name())
                    && seen.insert(symbol.id.clone())
                {
                    let ty = self.open_type(view, &symbol)?;
                    result.extend(self.members(view, &ty, name, project, depth + 1)?);
                }
            }
            return Ok(result);
        }
        let Type::Named { definition, .. } = ty else {
            return Ok(vec![]);
        };
        let Some(owner) = self.definitions.get(definition).cloned() else {
            return Ok(vec![]);
        };
        let mut direct = Vec::new();
        for member in self.lookup(view, name, owner.project)? {
            if member.header().explicit_interface.is_none()
                && member.declaration().owner == owner.declaration().qualified
                && member.id.context == owner.id.context
                && self
                    .owner(view, &member)?
                    .is_some_and(|declaring| declaring.id == owner.id)
            {
                direct.push((member, ty.clone()));
            }
        }
        if !direct.is_empty() {
            return Ok(direct);
        }
        let mut result = Vec::new();
        for base in self.base_types(view, &owner, ty, depth + 1)? {
            result.extend(self.members(view, &base, name, project, depth + 1)?);
        }
        Ok(result)
    }
    fn member_value(
        &mut self,
        view: &View<'_>,
        members: Vec<(Symbol, Type)>,
        explicit: Vec<WrittenType>,
    ) -> Result<Bound> {
        if members.is_empty() {
            return Ok(Bound::default());
        }
        if members.iter().all(|(s, _)| s.declaration().callable()) {
            return Ok(Bound {
                symbols: members.iter().map(|(s, _)| s.clone()).collect(),
                category: Category::Methods {
                    receiver: Some(members[0].1.clone()),
                    explicit,
                },
                ..Default::default()
            });
        }
        if members.len() != 1 {
            return Ok(Bound {
                symbols: members.into_iter().map(|(s, _)| s).collect(),
                uncertain: true,
                ..Default::default()
            });
        }
        let (symbol, receiver) = &members[0];
        let ty = self.resolve_type(view, &symbol.header().ty, symbol, 0)?;
        let ty =
            substitute(&ty, &Self::substitutions(receiver), &mut 4096).unwrap_or(Type::Unsupported);
        Ok(Bound {
            ty: Some(ty),
            symbols: vec![symbol.clone()],
            constant: symbol.header().constant.clone(),
            ..Default::default()
        })
    }

    fn accessible(&mut self, view: &View<'_>, member: &Symbol, context: &Symbol) -> Result<bool> {
        let access = member.declaration().access.as_str();
        if access == "public" {
            return Ok(true);
        }
        let same_assembly = member.id.context == context.id.context;
        if access == "internal" {
            return Ok(same_assembly);
        }
        let mut enclosing = Some(context.clone());
        let mut protected = false;
        while let Some(owner) = enclosing {
            if owner.declaration().qualified == member.declaration().owner && same_assembly {
                return Ok(true);
            }
            if owner.declaration().named_type()
                && access.contains("protected")
                && let Some(base) = self.owner(view, member)?
            {
                let from = self.open_type(view, &owner)?;
                let to = self.open_type(view, &base)?;
                protected |= matches!(
                    self.classify(view, &from, &to, 0)?,
                    Conversion::Identity | Conversion::Implicit
                );
            }
            enclosing = self.owner(view, &owner)?;
        }
        Ok(match access {
            "protected" => protected,
            "protected internal" => protected || same_assembly,
            "private protected" => protected && same_assembly,
            _ => false,
        })
    }

    fn accessible_members(
        &mut self,
        view: &View<'_>,
        context: &Symbol,
        members: Vec<(Symbol, Type)>,
        static_receiver: Option<bool>,
    ) -> Result<Vec<(Symbol, Type)>> {
        let mut result = Vec::new();
        for (symbol, receiver) in members {
            let is_static = symbol
                .declaration()
                .modifiers
                .iter()
                .any(|m| m == "static" || m == "const");
            if static_receiver.is_some_and(|required| required != is_static) {
                continue;
            }
            if self.accessible(view, &symbol, context)? {
                result.push((symbol, receiver));
            }
        }
        Ok(result)
    }

    pub fn resolve(
        &mut self,
        view: &View<'_>,
        file: &str,
        project: usize,
        position: usize,
        construction: bool,
        occurrence_name: &str,
    ) -> Result<Vec<(Symbol, bool)>> {
        self.observed.clear();
        self.catalog.check(view)?;
        let key = *blake3::hash(&postcard::to_allocvec(&(
            view.manifest.environment,
            &view.manifest.root,
            file,
            &view.manifest.files[file].stamp,
            project,
            position,
            construction,
            occurrence_name,
        ))?)
        .as_bytes();
        if let Some(targets) = super::cache::get(&key) {
            let mut result = Vec::new();
            for target in targets {
                if let Some(symbol) = self
                    .lookup(view, &target.name, project)?
                    .into_iter()
                    .find(|s| s.id == target.id && s.file == target.file)
                {
                    result.push((symbol, target.uncertain));
                }
            }
            #[cfg(test)]
            {
                self.cache_hits += 1;
            }
            return Ok(result);
        }
        let headers = self.catalog.headers(view, file)?;
        let Some((index, _)) = headers
            .declarations
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.local() && d.span.contains(&position))
            .min_by_key(|(_, d)| d.span.len())
        else {
            return Ok(vec![]);
        };
        let context = self.catalog.symbol(view, file, project, index as u32)?;
        let context = self.remember(context);
        let body = self.catalog.body(view, file, position)?;
        let Some((mut id, _)) = body
            .expressions
            .iter()
            .enumerate()
            .filter(|(_, e)| e.span.contains(&position))
            .min_by_key(|(_, e)| e.span.len())
        else {
            return Ok(vec![]);
        };
        if construction
            && let Some((new, _)) = body
                .expressions
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    matches!(e.kind, ExpressionKind::New { .. }) && e.span.contains(&position)
                })
                .min_by_key(|(_, e)| e.span.len())
        {
            id = new;
        }
        if let Some((implicit, _)) = body.expressions.iter().enumerate().find(|(_, expression)| expression.span.contains(&position) && matches!(&expression.kind, ExpressionKind::ImplicitCall { name, .. } if name == occurrence_name)) { id = implicit; }
        // Navigate the occurrence's member/call, not a containing argument expression.
        loop {
            let parent = body
                .expressions
                .iter()
                .enumerate()
                .find(|(_, e)| match &e.kind {
                    ExpressionKind::Member { name, .. } => *name as usize == id,
                    ExpressionKind::Call { function, .. } => *function as usize == id,
                    ExpressionKind::Name { .. } => {
                        e.span.start == body.expressions[id].span.start
                            && e.span.end > body.expressions[id].span.end
                    }
                    _ => false,
                });
            match parent {
                Some((next, _)) => id = next,
                None => break,
            }
        }
        let target = id as ExprId;
        // Contextual expressions obtain their types from the enclosing invocation.
        // Each candidate records observations privately; call selection publishes
        // only observations belonging to the selected candidate.
        if body
            .expressions
            .iter()
            .any(|e| matches!(e.kind, ExpressionKind::Lambda { .. }) && e.span.contains(&position))
            && let Some((outer, _)) = body
                .expressions
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    matches!(e.kind, ExpressionKind::Call { .. }) && e.span.contains(&position)
                })
                .max_by_key(|(_, e)| e.span.len())
        {
            id = outer;
        }
        let mut bound = self.expression(
            view,
            &context,
            &body,
            id as ExprId,
            None,
            &HashMap::new(),
            0,
        )?;
        if target != id as ExprId {
            bound = self
                .observed
                .iter()
                .rev()
                .find(|(site, _)| *site == target)
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
        }
        let uncertain = bound.uncertain || bound.symbols.len() > 1;
        #[cfg(test)]
        {
            self.trace_type = bound.ty.clone();
        }
        if bound.symbols.iter().all(|s| !s.header().local) {
            super::cache::put(
                key,
                bound
                    .symbols
                    .iter()
                    .map(|s| super::cache::Target {
                        id: s.id.clone(),
                        file: s.file.clone(),
                        name: s.declaration().name.clone(),
                        uncertain,
                    })
                    .collect(),
            );
        }
        Ok(bound.symbols.into_iter().map(|s| (s, uncertain)).collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn expression(
        &mut self,
        view: &View<'_>,
        context: &Symbol,
        body: &BodyFile,
        id: ExprId,
        expected: Option<&Type>,
        locals: &HashMap<String, Type>,
        depth: usize,
    ) -> Result<Bound> {
        let result = self.expression_inner(view, context, body, id, expected, locals, depth)?;
        self.observed.push((id, result.clone()));
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn expression_inner(
        &mut self,
        view: &View<'_>,
        context: &Symbol,
        body: &BodyFile,
        id: ExprId,
        expected: Option<&Type>,
        locals: &HashMap<String, Type>,
        depth: usize,
    ) -> Result<Bound> {
        if depth > 48 {
            return Ok(Bound::default());
        }
        self.catalog.check(view)?;
        let expression = &body.expressions[id as usize];
        Ok(match &expression.kind {
            ExpressionKind::ImplicitCall { name, receiver } => {
                let Some(receiver) = receiver else {
                    return Ok(Bound::default());
                };
                let receiver =
                    self.expression(view, context, body, *receiver, None, locals, depth + 1)?;
                let Some(ty) = receiver.ty else {
                    return Ok(Bound::default());
                };
                let members = self.members(view, &ty, name, context.project, depth + 1)?;
                let members = self.accessible_members(view, context, members, Some(false))?;
                let group = self.member_value(view, members, vec![])?;
                if matches!(group.category, Category::Methods { .. }) {
                    self.call(view, context, body, id, group, &[], locals, depth + 1)?
                } else {
                    group
                }
            }
            ExpressionKind::OutVariable { ty, .. } => Bound {
                ty: Some(if matches!(ty, WrittenType::Inferred) {
                    expected.cloned().unwrap_or(Type::Unsupported)
                } else {
                    self.resolve_type(view, ty, context, depth + 1)?
                }),
                ..Default::default()
            },
            ExpressionKind::Name { name, arguments } => {
                if let Some(ty) = locals.get(name) {
                    return Ok(Bound {
                        ty: Some(ty.clone()),
                        ..Default::default()
                    });
                }
                if let Some((index, _)) = context
                    .facts
                    .declarations
                    .iter()
                    .enumerate()
                    .filter(|(i, d)| {
                        context.facts.headers[*i].local
                            && d.callable()
                            && d.name == *name
                            && d.scope.contains(&expression.span.start)
                    })
                    .min_by_key(|(_, d)| d.scope.len())
                {
                    let symbol =
                        self.catalog
                            .symbol(view, &context.file, context.project, index as u32)?;
                    let symbol = self.remember(symbol);
                    return Ok(Bound {
                        symbols: vec![symbol],
                        category: Category::Methods {
                            receiver: None,
                            explicit: arguments.clone(),
                        },
                        ..Default::default()
                    });
                }
                if let Some(local) = body
                    .locals
                    .iter()
                    .filter(|l| {
                        &l.name == name
                            && l.span.start < expression.span.start
                            && l.scope.contains(&expression.span.start)
                    })
                    .min_by_key(|l| (l.scope.len(), usize::MAX - l.span.start))
                {
                    let mut ty = self.resolve_type(view, &local.ty, context, depth + 1)?;
                    if matches!(local.ty, WrittenType::Inferred)
                        && let Some(value) = local.value
                    {
                        ty = self
                            .expression(view, context, body, value, None, locals, depth + 1)?
                            .ty
                            .unwrap_or(Type::Unsupported);
                        if local.iteration {
                            ty = self.element_type(view, &ty, context.project, depth + 1)?;
                        }
                        if let Some(argument) = local.out_argument {
                            ty = self
                                .observed
                                .iter()
                                .rev()
                                .find(|(id, _)| *id == argument)
                                .and_then(|(_, value)| value.ty.clone())
                                .unwrap_or(Type::Unsupported);
                        }
                    }
                    return Ok(Bound {
                        ty: Some(ty),
                        ..Default::default()
                    });
                }
                if let Some((index, _)) = context
                    .facts
                    .declarations
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| {
                        d.local() && d.name == *name && d.scope.contains(&expression.span.start)
                    })
                    .min_by_key(|(_, d)| d.scope.len())
                {
                    let symbol =
                        self.catalog
                            .symbol(view, &context.file, context.project, index as u32)?;
                    let ty = self.resolve_type(view, &symbol.header().ty, &symbol, depth + 1)?;
                    return Ok(Bound {
                        ty: Some(ty),
                        symbols: vec![symbol],
                        ..Default::default()
                    });
                }
                let mut owner = Some(context.clone());
                while let Some(current) = owner {
                    if current.declaration().named_type() {
                        let ty = self.open_type(view, &current)?;
                        if name == "this" {
                            return Ok(Bound {
                                ty: Some(ty),
                                ..Default::default()
                            });
                        }
                        let members = self.members(view, &ty, name, context.project, depth + 1)?;
                        let members = self.accessible_members(
                            view,
                            context,
                            members,
                            context
                                .declaration()
                                .modifiers
                                .iter()
                                .any(|m| m == "static")
                                .then_some(true),
                        )?;
                        if !members.is_empty() {
                            return self.member_value(view, members, arguments.clone());
                        }
                    }
                    owner = self.owner(view, &current)?;
                }
                let imports = self.catalog.imports(view, context)?;
                let mut imported_members = Vec::new();
                for import in imports
                    .iter()
                    .filter(|i| matches!(i.kind, ImportKind::Static))
                {
                    let ty = self.resolve_type(view, &import.ty, context, depth + 1)?;
                    let members = self.members(view, &ty, name, context.project, depth + 1)?;
                    imported_members.extend(self.accessible_members(
                        view,
                        context,
                        members,
                        Some(true),
                    )?);
                }
                if !imported_members.is_empty() {
                    return self.member_value(view, imported_members, arguments.clone());
                }
                let ty = self.resolve_type(
                    view,
                    &WrittenType::Name {
                        alias: None,
                        parts: vec![NamePart {
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }],
                    },
                    context,
                    depth + 1,
                )?;
                if let Type::Named { definition, .. } = &ty {
                    Bound {
                        symbols: self
                            .definitions
                            .get(definition)
                            .cloned()
                            .into_iter()
                            .collect(),
                        ty: Some(ty),
                        category: Category::Type,
                        ..Default::default()
                    }
                } else {
                    Bound {
                        category: Category::Namespace(name.clone()),
                        ..Default::default()
                    }
                }
            }
            ExpressionKind::Member {
                receiver,
                name,
                conditional,
            } => {
                let receiver =
                    self.expression(view, context, body, *receiver, None, locals, depth + 1)?;
                let ExpressionKind::Name { name, arguments } =
                    &body.expressions[*name as usize].kind
                else {
                    return Ok(Bound::default());
                };
                if let Category::Namespace(namespace) = receiver.category {
                    let mut parts: Vec<_> = namespace
                        .split('.')
                        .map(|n| NamePart {
                            name: n.into(),
                            arguments: vec![],
                        })
                        .collect();
                    parts.push(NamePart {
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    let ty = self.resolve_type(
                        view,
                        &WrittenType::Name { alias: None, parts },
                        context,
                        depth + 1,
                    )?;
                    if let Type::Named { definition, .. } = &ty {
                        return Ok(Bound {
                            symbols: self
                                .definitions
                                .get(definition)
                                .cloned()
                                .into_iter()
                                .collect(),
                            ty: Some(ty),
                            category: Category::Type,
                            ..Default::default()
                        });
                    }
                    return Ok(Bound {
                        category: Category::Namespace(format!("{namespace}.{name}")),
                        ..Default::default()
                    });
                }
                let Some(mut ty) = receiver.ty else {
                    return Ok(Bound::default());
                };
                if matches!(ty, Type::Dynamic) {
                    return Ok(Bound::default());
                }
                if *conditional && let Type::Nullable(inner) = ty {
                    ty = *inner;
                }
                let members = self.members(view, &ty, name, context.project, depth + 1)?;
                let members = self.accessible_members(
                    view,
                    context,
                    members,
                    Some(matches!(receiver.category, Category::Type)),
                )?;
                let mut value = if !members.is_empty() {
                    self.member_value(view, members, arguments.clone())?
                } else {
                    Bound {
                        category: Category::Methods {
                            receiver: Some(ty),
                            explicit: arguments.clone(),
                        },
                        ..Default::default()
                    }
                };
                value.conditional = *conditional;
                if *conditional && !matches!(value.category, Category::Methods { .. }) {
                    value.ty = value.ty.map(|ty| self.lift_conditional(ty));
                }
                value
            }
            ExpressionKind::Call {
                function,
                arguments,
            } => {
                let group =
                    self.expression(view, context, body, *function, None, locals, depth + 1)?;
                self.call(
                    view,
                    context,
                    body,
                    *function,
                    group,
                    arguments,
                    locals,
                    depth + 1,
                )?
            }
            ExpressionKind::New { ty, arguments } => {
                let ty = if matches!(ty, WrittenType::Inferred) {
                    expected.cloned().unwrap_or(Type::Unsupported)
                } else {
                    self.resolve_type(view, ty, context, depth + 1)?
                };
                let mut value = Bound {
                    ty: Some(ty.clone()),
                    ..Default::default()
                };
                if let Type::Named { definition, .. } = &ty
                    && let Some(owner) = self.definitions.get(definition).cloned()
                {
                    let members = self.members(
                        view,
                        &ty,
                        &owner.declaration().name,
                        context.project,
                        depth + 1,
                    )?;
                    let group = self.member_value(view, members, vec![])?;
                    let call =
                        self.call(view, context, body, id, group, arguments, locals, depth + 1)?;
                    value.symbols = if call.symbols.is_empty() {
                        vec![owner]
                    } else {
                        call.symbols
                    };
                    value.uncertain = call.uncertain;
                }
                value
            }
            ExpressionKind::Literal { kind, value } => {
                let primitive = match kind.as_str() {
                    "integer_literal" => {
                        let number = integer_literal(value).unwrap_or(i128::MAX);
                        let suffix = value.to_ascii_lowercase();
                        if suffix.ends_with("ul") || suffix.ends_with("lu") {
                            "ulong"
                        } else if suffix.ends_with('u') {
                            if u32::try_from(number).is_ok() {
                                "uint"
                            } else {
                                "ulong"
                            }
                        } else if suffix.ends_with('l') {
                            if i64::try_from(number).is_ok() {
                                "long"
                            } else {
                                "ulong"
                            }
                        } else if i32::try_from(number).is_ok() {
                            "int"
                        } else if u32::try_from(number).is_ok() {
                            "uint"
                        } else if i64::try_from(number).is_ok() {
                            "long"
                        } else {
                            "ulong"
                        }
                    }
                    "real_literal" => {
                        if value.ends_with(['f', 'F']) {
                            "float"
                        } else if value.ends_with(['m', 'M']) {
                            "decimal"
                        } else {
                            "double"
                        }
                    }
                    "boolean_literal" => "bool",
                    "character_literal" => "char",
                    "null_literal" => {
                        return Ok(Bound {
                            ty: Some(Type::Null),
                            ..Default::default()
                        });
                    }
                    "string_literal" | "verbatim_string_literal" | "raw_string_literal" => "string",
                    _ => return Ok(Bound::default()),
                };
                Bound {
                    ty: Some(self.resolve_type(view, &named(primitive), context, depth + 1)?),
                    constant: if kind == "integer_literal" {
                        integer_literal(value).map(Constant::Integer)
                    } else {
                        None
                    },
                    ..Default::default()
                }
            }
            ExpressionKind::Cast { ty, .. } => Bound {
                ty: Some(self.resolve_type(view, ty, context, depth + 1)?),
                ..Default::default()
            },
            ExpressionKind::Wrapped(value) => {
                self.expression(view, context, body, *value, expected, locals, depth + 1)?
            }
            ExpressionKind::Assign { left, right } => {
                let left = self.expression(view, context, body, *left, None, locals, depth + 1)?;
                self.expression(
                    view,
                    context,
                    body,
                    *right,
                    left.ty.as_ref(),
                    locals,
                    depth + 1,
                )?
            }
            ExpressionKind::Lambda { .. } => Bound {
                category: Category::Lambda(id),
                ..Default::default()
            },
            ExpressionKind::Index {
                receiver,
                arguments,
            } => {
                let receiver =
                    self.expression(view, context, body, *receiver, None, locals, depth + 1)?;
                match receiver.ty {
                    Some(Type::Array(element, _)) => Bound {
                        ty: Some(*element),
                        ..Default::default()
                    },
                    Some(ty) => {
                        let members =
                            self.members(view, &ty, "Item", context.project, depth + 1)?;
                        let members =
                            self.accessible_members(view, context, members, Some(false))?;
                        let group = Bound {
                            symbols: members.into_iter().map(|(symbol, _)| symbol).collect(),
                            category: Category::Methods {
                                receiver: Some(ty),
                                explicit: vec![],
                            },
                            ..Default::default()
                        };
                        self.call(view, context, body, id, group, arguments, locals, depth + 1)?
                    }
                    None => Bound::default(),
                }
            }
            ExpressionKind::Await(value) => {
                let value =
                    self.expression(view, context, body, *value, None, locals, depth + 1)?;
                let ty = if let Some(ty) = value.ty {
                    let members =
                        self.members(view, &ty, "GetAwaiter", context.project, depth + 1)?;
                    let awaiter = self.member_value(view, members, vec![])?;
                    let awaiter =
                        self.call(view, context, body, id, awaiter, &[], locals, depth + 1)?;
                    if let Some(ty) = awaiter.ty {
                        let members =
                            self.members(view, &ty, "GetResult", context.project, depth + 1)?;
                        let group = self.member_value(view, members, vec![])?;
                        self.call(view, context, body, id, group, &[], locals, depth + 1)?
                            .ty
                    } else {
                        None
                    }
                } else {
                    None
                };
                Bound {
                    ty,
                    ..Default::default()
                }
            }
            ExpressionKind::Unsupported(_) => Bound::default(),
        })
    }

    fn element_type(
        &mut self,
        view: &View<'_>,
        ty: &Type,
        project: usize,
        depth: usize,
    ) -> Result<Type> {
        if let Type::Array(element, _) = ty {
            return Ok(*element.clone());
        }
        let members = self.members(view, ty, "GetEnumerator", project, depth + 1)?;
        if members.len() == 1 {
            let (method, receiver) = &members[0];
            let result = self.resolve_type(view, &method.header().ty, method, depth + 1)?;
            let result = substitute(&result, &Self::substitutions(receiver), &mut 4096)
                .unwrap_or(Type::Unsupported);
            let members = self.members(view, &result, "Current", project, depth + 1)?;
            return Ok(self
                .member_value(view, members, vec![])?
                .ty
                .unwrap_or(Type::Unsupported));
        }
        Ok(Type::Unsupported)
    }

    #[allow(clippy::too_many_arguments)]
    fn infer(
        &mut self,
        view: &View<'_>,
        parameter: &Type,
        argument: &Type,
        owner: &DefinitionId,
        kind: BoundKind,
        inference: &mut Inference,
        depth: usize,
    ) -> Result<()> {
        self.catalog.check(view)?;
        if depth > 24 {
            return Ok(());
        }
        match (parameter, argument) {
            (Type::Parameter(id), _) if &id.owner == owner => inference.add(id, kind, argument),
            (Type::Array(p, pr), Type::Array(a, ar)) if pr == ar => {
                self.infer(view, p, a, owner, kind, inference, depth + 1)?;
            }
            (Type::Nullable(p), Type::Nullable(a)) => {
                self.infer(view, p, a, owner, kind, inference, depth + 1)?;
            }
            (Type::Tuple(ps), Type::Tuple(args)) if ps.len() == args.len() => {
                for ((p, _), (a, _)) in ps.iter().zip(args) {
                    self.infer(view, p, a, owner, kind, inference, depth + 1)?;
                }
            }
            (
                Type::Named {
                    definition,
                    arguments,
                    containing,
                },
                _,
            ) => {
                let projections = self.projections(view, argument, definition, depth + 1)?;
                let mut unique = Vec::new();
                for projection in projections {
                    if !unique.contains(&projection) {
                        unique.push(projection);
                    }
                }
                // Multiple constructions of the same interface do not supply a
                // unique inference. Keep that uncertainty in the candidate.
                if unique.len() > 1 {
                    for p in arguments {
                        self.infer(
                            view,
                            p,
                            &Type::Unsupported,
                            owner,
                            BoundKind::Exact,
                            inference,
                            depth + 1,
                        )?;
                    }
                } else if let Some(Type::Named {
                    arguments: args,
                    containing: actual_owner,
                    ..
                }) = unique.pop()
                {
                    let generics = self
                        .definitions
                        .get(definition)
                        .map(|s| s.header().generics.clone())
                        .unwrap_or_default();
                    if let (Some(p), Some(a)) = (containing, actual_owner) {
                        self.infer(view, p, &a, owner, BoundKind::Exact, inference, depth + 1)?;
                    }
                    for (index, (p, a)) in arguments.iter().zip(args).enumerate() {
                        let nested = match (kind, generics.get(index).map(|p| p.variance)) {
                            (BoundKind::Lower, Some(Variance::Out)) => BoundKind::Lower,
                            (BoundKind::Lower, Some(Variance::In)) => BoundKind::Upper,
                            (BoundKind::Upper, Some(Variance::Out)) => BoundKind::Upper,
                            (BoundKind::Upper, Some(Variance::In)) => BoundKind::Lower,
                            _ => BoundKind::Exact,
                        };
                        self.infer(view, p, &a, owner, nested, inference, depth + 1)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn fix_inference(
        &mut self,
        view: &View<'_>,
        inference: &Inference,
        substitutions: &mut Vec<(ParameterId, Type)>,
    ) -> Result<()> {
        for (id, bounds) in &inference.bounds {
            let mut possible = Vec::new();
            for (_, candidate) in bounds {
                if possible.contains(candidate) {
                    continue;
                }
                let mut valid = true;
                for (kind, bound) in bounds {
                    valid &= match kind {
                        BoundKind::Exact => bound == candidate,
                        BoundKind::Lower => matches!(
                            self.classify(view, bound, candidate, 0)?,
                            Conversion::Identity | Conversion::Implicit
                        ),
                        BoundKind::Upper => matches!(
                            self.classify(view, candidate, bound, 0)?,
                            Conversion::Identity | Conversion::Implicit
                        ),
                    };
                }
                if valid {
                    possible.push(candidate.clone());
                }
            }
            let mut best = Vec::new();
            for candidate in &possible {
                let mut valid = true;
                for other in &possible {
                    valid &= matches!(
                        self.classify(view, other, candidate, 0)?,
                        Conversion::Identity | Conversion::Implicit
                    );
                }
                if valid {
                    best.push(candidate.clone());
                }
            }
            let ty = if best.len() == 1 {
                best.pop().unwrap()
            } else {
                Type::Unsupported
            };
            if let Some((_, previous)) = substitutions
                .iter_mut()
                .find(|(parameter, _)| parameter == id)
            {
                *previous = ty;
            } else {
                substitutions.push((id.clone(), ty));
            }
        }
        Ok(())
    }

    fn classify(
        &mut self,
        view: &View<'_>,
        from: &Type,
        to: &Type,
        depth: usize,
    ) -> Result<Conversion> {
        if depth > 24 {
            return Ok(Conversion::Unknown);
        }
        let basic = conversion(from, to);
        if basic != Conversion::Rejected {
            return Ok(basic);
        }
        if let (Type::Array(a, ar), Type::Array(b, br)) = (from, to)
            && ar == br
            && self.reference_type(a)
            && self.reference_type(b)
        {
            return Ok(match self.classify(view, a, b, depth + 1)? {
                Conversion::Identity | Conversion::Implicit => Conversion::Implicit,
                other => other,
            });
        }
        if matches!(from, Type::Null) && self.reference_type(to) {
            return Ok(Conversion::Implicit);
        }
        if let Type::Named {
            definition,
            arguments,
            containing,
        } = to
        {
            let projections = self.projections(view, from, definition, depth + 1)?;
            let generics = self
                .definitions
                .get(definition)
                .map(|s| s.header().generics.clone())
                .unwrap_or_default();
            for projection in projections {
                if &projection == to {
                    return Ok(Conversion::Implicit);
                }
                let Type::Named {
                    arguments: actual,
                    containing: actual_owner,
                    ..
                } = projection
                else {
                    continue;
                };
                if containing != &actual_owner || actual.len() != arguments.len() {
                    continue;
                }
                let mut valid = true;
                let mut unknown = false;
                for (i, (a, p)) in actual.iter().zip(arguments).enumerate() {
                    if a == p {
                        continue;
                    }
                    if !self.reference_type(a) || !self.reference_type(p) {
                        valid = false;
                        break;
                    }
                    let conversion = match generics.get(i).map(|g| g.variance) {
                        Some(Variance::Out) => self.classify(view, a, p, depth + 1)?,
                        Some(Variance::In) => self.classify(view, p, a, depth + 1)?,
                        _ => Conversion::Rejected,
                    };
                    valid &= conversion != Conversion::Rejected;
                    unknown |= conversion == Conversion::Unknown;
                }
                if valid {
                    return Ok(if unknown {
                        Conversion::Unknown
                    } else {
                        Conversion::Implicit
                    });
                }
            }
        }
        let Type::Named { definition, .. } = from else {
            return Ok(Conversion::Rejected);
        };
        let Some(symbol) = self.definitions.get(definition).cloned() else {
            return Ok(Conversion::Unknown);
        };
        let mut unknown = false;
        for base in self.base_types(view, &symbol, from, depth + 1)? {
            match self.classify(view, &base, to, depth + 1)? {
                Conversion::Identity | Conversion::Implicit => return Ok(Conversion::Implicit),
                Conversion::Unknown => unknown = true,
                Conversion::Rejected => {}
            }
        }
        Ok(if unknown {
            Conversion::Unknown
        } else {
            Conversion::Rejected
        })
    }

    fn reference_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Primitive(Primitive::String | Primitive::Object) | Type::Array(..) => true,
            Type::Named { definition, .. } => self.definitions.get(definition).is_some_and(|s| {
                matches!(
                    s.declaration().kind.as_str(),
                    "class" | "interface" | "delegate"
                )
            }),
            _ => false,
        }
    }

    fn lift_conditional(&self, ty: Type) -> Type {
        if self.reference_type(&ty)
            || matches!(
                ty,
                Type::Nullable(_)
                    | Type::Primitive(Primitive::Void)
                    | Type::Unsupported
                    | Type::Unresolved { .. }
            )
        {
            ty
        } else {
            Type::Nullable(Box::new(ty))
        }
    }

    fn constructible(&mut self, view: &View<'_>, ty: &Type) -> Result<Conversion> {
        match ty {
            Type::Primitive(Primitive::String | Primitive::Void)
            | Type::Array(..)
            | Type::Nullable(_) => Ok(Conversion::Rejected),
            Type::Primitive(_) => Ok(Conversion::Implicit),
            Type::Named { definition, .. } => {
                let Some(symbol) = self.definitions.get(definition).cloned() else {
                    return Ok(Conversion::Unknown);
                };
                if matches!(symbol.declaration().kind.as_str(), "struct" | "enum") {
                    return Ok(Conversion::Implicit);
                }
                if symbol.declaration().kind != "class"
                    || symbol
                        .declaration()
                        .modifiers
                        .iter()
                        .any(|m| m == "abstract")
                {
                    return Ok(Conversion::Rejected);
                }
                let constructors: Vec<_> = self
                    .members(view, ty, &symbol.declaration().name, symbol.project, 0)?
                    .into_iter()
                    .filter(|(s, _)| {
                        s.declaration().kind == "constructor"
                            && !s.declaration().modifiers.iter().any(|m| m == "static")
                    })
                    .collect();
                Ok(
                    if constructors.iter().any(|(s, _)| {
                        s.declaration().kind == "constructor"
                            && s.declaration().access == "public"
                            && s.header().parameters.is_empty()
                    }) || constructors.is_empty() && !view.manifest.files[&symbol.file].metadata
                    {
                        Conversion::Implicit
                    } else {
                        Conversion::Rejected
                    },
                )
            }
            _ => Ok(Conversion::Unknown),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        view: &View<'_>,
        context: &Symbol,
        body: &BodyFile,
        function: ExprId,
        group: Bound,
        arguments: &[Argument],
        locals: &HashMap<String, Type>,
        depth: usize,
    ) -> Result<Bound> {
        let conditional = group.conditional;
        let Category::Methods { receiver, explicit } = group.category else {
            return Ok(Bound::default());
        };
        let candidates = group.symbols;
        let mut applicable = self.applicable(
            view,
            context,
            body,
            &candidates,
            receiver.as_ref(),
            &explicit,
            arguments,
            locals,
            false,
            depth + 1,
        )?;
        if applicable.is_empty() && receiver.is_some() {
            let name = match &body.expressions[function as usize].kind {
                ExpressionKind::Member { name, .. } => match &body.expressions[*name as usize].kind
                {
                    ExpressionKind::Name { name, .. } => Some(name.clone()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(name) = name {
                let imports = self.catalog.imports(view, context)?;
                let mut scopes: std::collections::BTreeMap<usize, Vec<Symbol>> = Default::default();
                for candidate in self.lookup(view, &name, context.project)? {
                    if !candidate
                        .header()
                        .parameters
                        .first()
                        .is_some_and(|p| p.receiver)
                        || !self.accessible(view, &candidate, context)?
                    {
                        continue;
                    }
                    let namespace = &candidate.declaration().namespace;
                    let enclosing = &context.declaration().namespace;
                    let mut rank = (namespace.is_empty()
                        || enclosing == namespace
                        || enclosing
                            .strip_prefix(namespace)
                            .is_some_and(|tail| tail.starts_with('.')))
                    .then(|| {
                        if namespace.is_empty() {
                            1
                        } else {
                            namespace.split('.').count() * 2 + 1
                        }
                    });
                    for import in &imports {
                        let matches = match import.kind {
                            ImportKind::Namespace => {
                                written_name(&import.ty).as_deref() == Some(namespace)
                            }
                            ImportKind::Static => {
                                written_name(&import.ty).as_deref()
                                    == Some(&candidate.declaration().owner)
                            }
                            ImportKind::Alias(_) => false,
                        };
                        if matches {
                            let depth = if import.global {
                                0
                            } else {
                                context
                                    .facts
                                    .declarations
                                    .iter()
                                    .filter(|d| {
                                        d.kind == "namespace"
                                            && d.span.start <= import.scope.start
                                            && d.span.end >= import.scope.end
                                    })
                                    .map(|d| d.qualified.split('.').count())
                                    .max()
                                    .unwrap_or(0)
                            };
                            rank = Some(rank.unwrap_or(0).max(depth * 2));
                        }
                    }
                    if let Some(rank) = rank {
                        scopes.entry(rank).or_default().push(candidate);
                    }
                }
                for (_, candidates) in scopes.into_iter().rev() {
                    applicable = self.applicable(
                        view,
                        context,
                        body,
                        &candidates,
                        receiver.as_ref(),
                        &explicit,
                        arguments,
                        locals,
                        true,
                        depth + 1,
                    )?;
                    if !applicable.is_empty() {
                        break;
                    }
                }
            }
        }
        let mut survivors = Vec::new();
        for (i, candidate) in applicable.iter().enumerate() {
            let mut beaten = false;
            for (j, other) in applicable.iter().enumerate() {
                if i != j && self.better(view, other, candidate)? {
                    beaten = true;
                    break;
                }
            }
            if !beaten {
                survivors.push(candidate);
            }
        }
        let uncertain = survivors.len() != 1 || survivors.iter().any(|c| c.unknown);
        if !uncertain {
            self.observed
                .extend(survivors[0].observations.iter().cloned());
        }
        let ty = if uncertain {
            None
        } else {
            survivors.first().map(|s| s.return_type.clone())
        };
        Ok(Bound {
            symbols: survivors.iter().map(|c| c.symbol.clone()).collect(),
            ty: ty.map(|ty| {
                if conditional {
                    self.lift_conditional(ty)
                } else {
                    ty
                }
            }),
            uncertain,
            ..Default::default()
        })
    }

    fn better(&mut self, view: &View<'_>, a: &Applicable, b: &Applicable) -> Result<bool> {
        if a.unknown || b.unknown {
            return Ok(false);
        }
        let mut better = false;
        for ((ac, bc), (at, bt)) in a
            .conversions
            .iter()
            .zip(&b.conversions)
            .zip(a.types.iter().zip(&b.types))
        {
            if ac == bc && at == bt {
                continue;
            }
            if *bc == Conversion::Identity {
                return Ok(false);
            }
            if *ac == Conversion::Identity {
                better = true;
                continue;
            }
            if self.classify(view, at, bt, 0)? == Conversion::Implicit
                && self.classify(view, bt, at, 0)? == Conversion::Rejected
            {
                better = true;
            } else {
                return Ok(false);
            }
        }
        Ok(better
            || (!a.expanded && b.expanded)
            || (a.types == b.types
                && a.symbol.header().generics.is_empty()
                && !b.symbol.header().generics.is_empty()))
    }

    #[allow(clippy::too_many_arguments)]
    fn applicable(
        &mut self,
        view: &View<'_>,
        context: &Symbol,
        body: &BodyFile,
        candidates: &[Symbol],
        receiver: Option<&Type>,
        explicit: &[WrittenType],
        arguments: &[Argument],
        locals: &HashMap<String, Type>,
        extension: bool,
        depth: usize,
    ) -> Result<Vec<Applicable>> {
        if depth > 48 {
            return Ok(vec![]);
        }
        let mut result = Vec::new();
        for (candidate, expanded) in candidates
            .iter()
            .flat_map(|candidate| [(candidate, false), (candidate, true)])
        {
            if expanded
                && (!candidate
                    .header()
                    .parameters
                    .last()
                    .is_some_and(|p| p.variadic)
                    || result.iter().any(|previous: &Applicable| {
                        previous.symbol.id == candidate.id && !previous.unknown
                    }))
            {
                continue;
            }
            let observation_start = self.observed.len();
            self.catalog.check(view)?;
            let header = candidate.header();
            if !explicit.is_empty() && explicit.len() != header.generics.len() {
                continue;
            }
            let mut substitutions = receiver.map(Self::substitutions).unwrap_or_default();
            let mut inference = Inference::default();
            for (ordinal, ty) in explicit.iter().enumerate() {
                substitutions.push((
                    ParameterId {
                        owner: candidate.id.clone(),
                        ordinal: ordinal as u32,
                    },
                    self.resolve_type(view, ty, context, depth + 1)?,
                ));
            }
            let mut mapped = Vec::new();
            let mut used = vec![false; header.parameters.len()];
            let mut cursor = usize::from(extension);
            if extension && !used.is_empty() {
                used[0] = true;
            }
            let mut valid = true;
            let mut out_of_order_name = false;
            for argument in arguments {
                let index = if let Some(name) = &argument.name {
                    let index = header.parameters.iter().position(|p| &p.name == name);
                    if index == Some(cursor) {
                        cursor += 1;
                    } else {
                        out_of_order_name = true;
                    }
                    index
                } else if !out_of_order_name && cursor < header.parameters.len() {
                    let index = cursor;
                    if !expanded || !header.parameters[index].variadic {
                        cursor += 1;
                    }
                    Some(index)
                } else {
                    None
                };
                let Some(index) = index else {
                    valid = false;
                    break;
                };
                let parameter = &header.parameters[index];
                if used[index] && !(expanded && parameter.variadic && argument.name.is_none())
                    || expanded && parameter.variadic && argument.name.is_some()
                    || parameter.mode != argument.mode
                        && !(parameter.mode == PassingMode::In
                            && argument.mode == PassingMode::Value)
                {
                    valid = false;
                    break;
                }
                used[index] = true;
                let mut ty = self.resolve_type(view, &parameter.ty, candidate, depth + 1)?;
                if expanded
                    && parameter.variadic
                    && let Type::Array(element, _) = ty
                {
                    ty = *element;
                }
                mapped.push((argument, ty));
            }
            if !valid
                || header
                    .parameters
                    .iter()
                    .enumerate()
                    .any(|(i, p)| !used[i] && p.default.is_none() && !(expanded && p.variadic))
            {
                continue;
            }
            if extension
                && let (Some(parameter), Some(receiver)) = (header.parameters.first(), receiver)
            {
                let ty = self.resolve_type(view, &parameter.ty, candidate, depth + 1)?;
                if explicit.is_empty() {
                    self.infer(
                        view,
                        &ty,
                        receiver,
                        &candidate.id,
                        BoundKind::Lower,
                        &mut inference,
                        0,
                    )?;
                    self.fix_inference(view, &inference, &mut substitutions)?;
                }
            }
            let mut values = Vec::new();
            for (argument, ty) in &mapped {
                let expected =
                    substitute(ty, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                let value = self.expression(
                    view,
                    context,
                    body,
                    argument.value,
                    Some(&expected),
                    locals,
                    depth + 1,
                )?;
                if explicit.is_empty()
                    && let Some(actual) = &value.ty
                {
                    self.infer(
                        view,
                        ty,
                        actual,
                        &candidate.id,
                        if argument.mode == PassingMode::Value {
                            BoundKind::Lower
                        } else {
                            BoundKind::Exact
                        },
                        &mut inference,
                        0,
                    )?;
                }
                values.push(value);
            }
            self.fix_inference(view, &inference, &mut substitutions)?;
            let mut lambda_types = HashMap::new();
            for ((argument, parameter), value) in mapped.iter().zip(&values) {
                if let Category::Lambda(lambda) = value.category {
                    let delegate = substitute(parameter, &substitutions, &mut 4096)
                        .unwrap_or(Type::Unsupported);
                    if let Some((inputs, output)) =
                        self.delegate_signature(view, &delegate, depth + 1)?
                    {
                        let ExpressionKind::Lambda {
                            parameters,
                            body: lambda_body,
                        } = &body.expressions[lambda as usize].kind
                        else {
                            unreachable!()
                        };
                        if parameters.len() != inputs.len() {
                            valid = false;
                            break;
                        }
                        if explicit.is_empty() {
                            for (parameter, input) in parameters.iter().zip(&inputs) {
                                if !matches!(parameter.ty, WrittenType::Inferred) {
                                    let actual =
                                        self.resolve_type(view, &parameter.ty, context, depth + 1)?;
                                    self.infer(
                                        view,
                                        input,
                                        &actual,
                                        &candidate.id,
                                        BoundKind::Exact,
                                        &mut inference,
                                        0,
                                    )?;
                                }
                            }
                            self.fix_inference(view, &inference, &mut substitutions)?;
                        }
                        let mut lambda_locals = locals.clone();
                        for (parameter, ty) in parameters.iter().zip(inputs) {
                            let ty = substitute(&ty, &substitutions, &mut 4096)
                                .unwrap_or(Type::Unsupported);
                            if !matches!(parameter.ty, WrittenType::Inferred)
                                && self.resolve_type(view, &parameter.ty, context, depth + 1)? != ty
                            {
                                valid = false;
                                break;
                            }
                            lambda_locals.insert(parameter.name.clone(), ty);
                        }
                        let result = self.expression(
                            view,
                            context,
                            body,
                            *lambda_body,
                            Some(&output),
                            &lambda_locals,
                            depth + 1,
                        )?;
                        if let Some(ty) = result.ty {
                            if explicit.is_empty() {
                                self.infer(
                                    view,
                                    &output,
                                    &ty,
                                    &candidate.id,
                                    BoundKind::Lower,
                                    &mut inference,
                                    0,
                                )?;
                                self.fix_inference(view, &inference, &mut substitutions)?;
                            }
                            lambda_types.insert(lambda, (ty, output));
                        }
                    }
                } else if matches!(value.category, Category::Methods { .. }) {
                    let delegate = substitute(parameter, &substitutions, &mut 4096)
                        .unwrap_or(Type::Unsupported);
                    if let Some((inputs, output)) =
                        self.delegate_signature(view, &delegate, depth + 1)?
                        && let Some((symbol, ty)) =
                            self.method_group(view, value, &inputs, depth + 1)?
                    {
                        if explicit.is_empty() {
                            self.infer(
                                view,
                                &output,
                                &ty,
                                &candidate.id,
                                BoundKind::Lower,
                                &mut inference,
                                0,
                            )?;
                            self.fix_inference(view, &inference, &mut substitutions)?;
                        }
                        lambda_types.insert(argument.value, (ty.clone(), output));
                        self.observed.push((
                            argument.value,
                            Bound {
                                symbols: vec![symbol],
                                ty: Some(ty),
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
            if !valid {
                self.observed.truncate(observation_start);
                continue;
            }
            let missing_inference = (0..header.generics.len()).any(|ordinal| {
                !substitutions
                    .iter()
                    .any(|(id, _)| id.owner == candidate.id && id.ordinal == ordinal as u32)
            });
            let unresolved_argument = values.iter().any(|value| {
                value.uncertain
                    || value.ty.as_ref().is_none_or(|ty| {
                        matches!(
                            ty,
                            Type::Unsupported | Type::Unresolved { .. } | Type::Dynamic
                        )
                    })
            });
            if missing_inference && !unresolved_argument {
                self.observed.truncate(observation_start);
                continue;
            }
            let mut unknown = missing_inference;
            if extension
                && let (Some(receiver), Some(parameter)) = (receiver, header.parameters.first())
            {
                let ty = self.resolve_type(view, &parameter.ty, candidate, depth + 1)?;
                let ty = substitute(&ty, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                match self.classify(view, receiver, &ty, 0)? {
                    Conversion::Rejected => {
                        self.observed.truncate(observation_start);
                        continue;
                    }
                    Conversion::Unknown => unknown = true,
                    _ => {}
                }
            }
            for (ordinal, generic) in header.generics.iter().enumerate() {
                let parameter = Type::Parameter(ParameterId {
                    owner: candidate.id.clone(),
                    ordinal: ordinal as u32,
                });
                let actual =
                    substitute(&parameter, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                for bound in &generic.constraints {
                    let bound = self.resolve_type(view, bound, candidate, depth + 1)?;
                    let bound =
                        substitute(&bound, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                    match self.classify(view, &actual, &bound, 0)? {
                        Conversion::Rejected => valid = false,
                        Conversion::Unknown => unknown = true,
                        _ => {}
                    }
                }
                for special in &generic.special_constraints {
                    match special.as_str() {
                        "class" | "class?" if !self.reference_type(&actual) => {
                            if matches!(
                                actual,
                                Type::Unsupported | Type::Unresolved { .. } | Type::Parameter(_)
                            ) {
                                unknown = true;
                            } else {
                                valid = false;
                            }
                        }
                        "struct" | "unmanaged"
                            if self.reference_type(&actual)
                                || matches!(actual, Type::Nullable(_)) =>
                        {
                            valid = false
                        }
                        "new()" => match self.constructible(view, &actual)? {
                            Conversion::Rejected => valid = false,
                            Conversion::Unknown => unknown = true,
                            _ => {}
                        },
                        "notnull" | "unmanaged" => unknown = true,
                        _ => {}
                    }
                }
            }
            let mut conversions = Vec::new();
            let mut types = Vec::new();
            for ((argument, ty), value) in mapped.iter().zip(&values) {
                let expected =
                    substitute(ty, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                let conversion = if matches!(
                    value.category,
                    Category::Lambda(_) | Category::Methods { .. }
                ) {
                    if let Some((actual, output)) = lambda_types.get(&argument.value) {
                        let output = substitute(output, &substitutions, &mut 4096)
                            .unwrap_or(Type::Unsupported);
                        self.classify(view, actual, &output, 0)?
                    } else {
                        Conversion::Unknown
                    }
                } else {
                    if let Some(actual) = &value.ty {
                        if argument.mode != PassingMode::Value {
                            if actual == &expected {
                                Conversion::Identity
                            } else if matches!(actual, Type::Unsupported | Type::Unresolved { .. })
                                || matches!(
                                    expected,
                                    Type::Unsupported
                                        | Type::Unresolved { .. }
                                        | Type::Parameter(_)
                                )
                            {
                                Conversion::Unknown
                            } else {
                                Conversion::Rejected
                            }
                        } else {
                            match &value.constant {
                                Some(Constant::Unsupported) => Conversion::Unknown,
                                Some(Constant::Integer(value))
                                    if matches!(
                                        actual,
                                        Type::Primitive(Primitive::I32 | Primitive::I64)
                                    ) && (matches!(actual, Type::Primitive(Primitive::I32))
                                        || matches!(expected, Type::Primitive(Primitive::U64)))
                                        && constant_conversion(*value, &expected) =>
                                {
                                    Conversion::Implicit
                                }
                                _ => self.classify(view, actual, &expected, 0)?,
                            }
                        }
                    } else {
                        Conversion::Unknown
                    }
                };
                if conversion == Conversion::Rejected {
                    valid = false;
                    break;
                }
                unknown |= conversion == Conversion::Unknown;
                conversions.push(conversion);
                types.push(expected);
            }
            if !valid {
                self.observed.truncate(observation_start);
                continue;
            }
            let return_type = self.resolve_type(view, &header.ty, candidate, depth + 1)?;
            let return_type =
                substitute(&return_type, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
            result.push(Applicable {
                observations: self.observed.split_off(observation_start),
                symbol: candidate.clone(),
                return_type,
                types,
                conversions,
                expanded,
                unknown,
            });
        }
        Ok(result)
    }

    fn delegate_signature(
        &mut self,
        view: &View<'_>,
        ty: &Type,
        depth: usize,
    ) -> Result<Option<(Vec<Type>, Type)>> {
        let Type::Named { definition, .. } = ty else {
            return Ok(None);
        };
        let Some(symbol) = self.definitions.get(definition).cloned() else {
            return Ok(None);
        };
        let (signature, receiver) = if symbol.declaration().kind == "delegate" {
            (symbol, ty.clone())
        } else {
            let members = self.members(view, ty, "Invoke", symbol.project, depth + 1)?;
            if members.len() != 1 {
                return Ok(None);
            }
            members[0].clone()
        };
        let substitutions = Self::substitutions(&receiver);
        let mut inputs = Vec::new();
        for parameter in &signature.header().parameters {
            let ty = self.resolve_type(view, &parameter.ty, &signature, depth + 1)?;
            inputs.push(substitute(&ty, &substitutions, &mut 4096).unwrap_or(Type::Unsupported));
        }
        let output = self.resolve_type(view, &signature.header().ty, &signature, depth + 1)?;
        Ok(Some((
            inputs,
            substitute(&output, &substitutions, &mut 4096).unwrap_or(Type::Unsupported),
        )))
    }

    fn method_group(
        &mut self,
        view: &View<'_>,
        group: &Bound,
        inputs: &[Type],
        depth: usize,
    ) -> Result<Option<(Symbol, Type)>> {
        let Category::Methods { receiver, .. } = &group.category else {
            return Ok(None);
        };
        let mut applicable = Vec::new();
        let mut incomplete = false;
        for method in &group.symbols {
            if method.header().parameters.len() != inputs.len() {
                continue;
            }
            let mut substitutions = receiver
                .as_ref()
                .map(Self::substitutions)
                .unwrap_or_default();
            let mut parameters = Vec::new();
            let mut inference = Inference::default();
            for (parameter, input) in method.header().parameters.iter().zip(inputs) {
                let parameter = self.resolve_type(view, &parameter.ty, method, depth + 1)?;
                self.infer(
                    view,
                    &parameter,
                    input,
                    &method.id,
                    BoundKind::Lower,
                    &mut inference,
                    0,
                )?;
                parameters.push(parameter);
            }
            self.fix_inference(view, &inference, &mut substitutions)?;
            let mut valid = true;
            for (parameter, input) in parameters.iter().zip(inputs) {
                let parameter =
                    substitute(parameter, &substitutions, &mut 4096).unwrap_or(Type::Unsupported);
                match self.classify(view, input, &parameter, 0)? {
                    Conversion::Rejected => valid = false,
                    Conversion::Unknown => incomplete = true,
                    _ => {}
                }
            }
            if valid {
                let output = self.resolve_type(view, &method.header().ty, method, depth + 1)?;
                applicable.push((
                    method.clone(),
                    substitute(&output, &substitutions, &mut 4096).unwrap_or(Type::Unsupported),
                ));
            }
        }
        Ok(if applicable.len() == 1 && !incomplete {
            applicable.pop()
        } else {
            None
        })
    }
}

struct Applicable {
    observations: Vec<(ExprId, Bound)>,
    symbol: Symbol,
    return_type: Type,
    types: Vec<Type>,
    conversions: Vec<Conversion>,
    expanded: bool,
    unknown: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conversion {
    Identity,
    Implicit,
    Rejected,
    Unknown,
}
fn constant_conversion(value: i128, to: &Type) -> bool {
    let Type::Primitive(to) = to else {
        return false;
    };
    match to {
        Primitive::I8 => i8::try_from(value).is_ok(),
        Primitive::U8 => u8::try_from(value).is_ok(),
        Primitive::I16 => i16::try_from(value).is_ok(),
        Primitive::U16 => u16::try_from(value).is_ok(),
        Primitive::U32 => u32::try_from(value).is_ok(),
        Primitive::U64 => u64::try_from(value).is_ok(),
        _ => false,
    }
}
fn conversion(from: &Type, to: &Type) -> Conversion {
    if from == to
        && !matches!(
            from,
            Type::Unsupported | Type::Unresolved { .. } | Type::Dynamic
        )
    {
        return Conversion::Identity;
    }
    if matches!(
        from,
        Type::Unsupported | Type::Unresolved { .. } | Type::Dynamic | Type::Parameter(_)
    ) || matches!(
        to,
        Type::Unsupported | Type::Unresolved { .. } | Type::Dynamic | Type::Parameter(_)
    ) {
        return Conversion::Unknown;
    }
    if matches!(to, Type::Primitive(Primitive::Object)) {
        return Conversion::Implicit;
    }
    if let (Type::Primitive(from), Type::Primitive(to)) = (from, to) {
        return if from.widens_to(*to) {
            Conversion::Implicit
        } else {
            Conversion::Rejected
        };
    }
    if matches!(from, Type::Null)
        && matches!(to, Type::Primitive(Primitive::String | Primitive::Object))
    {
        return Conversion::Implicit;
    }
    if let Type::Nullable(to) = to {
        let from = if let Type::Nullable(from) = from {
            from.as_ref()
        } else {
            from
        };
        return match conversion(from, to) {
            Conversion::Identity | Conversion::Implicit => Conversion::Implicit,
            other => other,
        };
    }
    if matches!(from, Type::Null) && matches!(to, Type::Array(..) | Type::Nullable(_)) {
        return Conversion::Implicit;
    }
    Conversion::Rejected
}
fn named(name: &str) -> WrittenType {
    WrittenType::Name {
        alias: None,
        parts: vec![NamePart {
            name: name.into(),
            arguments: vec![],
        }],
    }
}
fn written_name(ty: &WrittenType) -> Option<String> {
    match ty {
        WrittenType::Name { parts, .. } => Some(
            parts
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}
fn strip_arity(name: &str) -> String {
    name.split('.')
        .map(|part| part.split('`').next().unwrap_or(part))
        .collect::<Vec<_>>()
        .join(".")
}
