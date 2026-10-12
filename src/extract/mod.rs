mod csharp;
pub(crate) mod preprocess;
mod rust;
pub(crate) use csharp::HistoricalPresence;
pub(crate) use rust::modules as rust_modules;

use crate::model::{Declaration, Facts, Language, Occurrence};
use std::ops::Range;

pub fn extract(
    source: &str,
    language: Language,
    defines: &[String],
    edition: &str,
) -> anyhow::Result<Facts> {
    match language {
        Language::CSharp => csharp::extract(source, defines),
        Language::Rust => rust::extract(source, edition),
        Language::Markdown | Language::Text => Ok(Facts::default()),
        language => crate::native::extract(source, language),
    }
}

/// History-only facts. They are neither binding input nor persisted snapshot data.
#[derive(Default)]
pub(crate) struct HistoricalFacts {
    pub declarations: Vec<HistoricalDeclaration>,
    pub occurrences: Vec<Occurrence>,
    pub presence: HistoricalPresence,
    /// A reason absence/uniqueness cannot be proved from the candidate source.
    pub incomplete: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct HistoricalDeclaration {
    pub declaration: Declaration,
    /// Token spelling of the header and lexical owners, excluding trivia.
    /// Same-range alternatives must not disappear merely because their overload
    /// selectors coincide.
    pub written: String,
    /// Feasible environments in these facts' source-local presence arena.
    pub guard: usize,
    pub uncertain: bool,
}
impl HistoricalDeclaration {
    pub fn heap_bytes(&self) -> usize {
        self.written.capacity()
            + [
                &self.name,
                &self.qualified,
                &self.kind,
                &self.namespace,
                &self.owner,
                &self.ty,
                &self.access,
            ]
            .iter()
            .map(|s| s.capacity())
            .sum::<usize>()
            + [
                &self.parameters,
                &self.attributes,
                &self.modifiers,
                &self.bases,
            ]
            .iter()
            .map(|v| {
                v.capacity() * std::mem::size_of::<String>()
                    + v.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>()
    }
}
impl std::ops::Deref for HistoricalDeclaration {
    type Target = Declaration;
    fn deref(&self) -> &Declaration {
        &self.declaration
    }
}

/// Syntax-only committed-source parsing; never discovers a build graph.
pub(crate) fn historical(source: &str, language: Language) -> anyhow::Result<HistoricalFacts> {
    match language {
        Language::CSharp => csharp::historical(source),
        Language::Rust => rust::historical(source),
        _ => Ok(extract(source, language, &[], "2024")?.into()),
    }
}

impl From<Facts> for HistoricalFacts {
    fn from(facts: Facts) -> Self {
        Self {
            declarations: facts
                .declarations
                .into_iter()
                .map(|declaration| HistoricalDeclaration {
                    declaration,
                    written: String::new(),
                    guard: 1,
                    uncertain: facts.errors,
                })
                .collect(),
            occurrences: facts.occurrences,
            incomplete: facts
                .errors
                .then(|| "historical parse recovery left candidate scope incomplete".into()),
            ..Self::default()
        }
    }
}

/// A lexical history region, never a binding declaration. Anonymous labels are
/// deliberately shared by siblings: neither an ordinal nor an offset is identity.
/// The history-only `closure` kind does not promote snapshot `lambda` scopes.
fn callable_region(
    name: &str,
    kind: &str,
    span: Range<usize>,
    body_start: usize,
    owner: Option<&Declaration>,
    separator: &str,
) -> Declaration {
    let owner_name = owner.map_or("", |d| {
        if d.kind == "impl" {
            &d.ty
        } else {
            &d.qualified
        }
    });
    Declaration {
        name: name.into(),
        qualified: if owner_name.is_empty() {
            name.into()
        } else {
            format!("{owner_name}{separator}{name}")
        },
        kind: kind.into(),
        namespace: owner.map_or_else(String::new, |d| d.namespace.clone()),
        owner: owner_name.into(),
        name_span: span.start..span.start,
        header: span.start..body_start,
        scope: span.clone(),
        span,
        parameters: Vec::new(),
        ty: String::new(),
        access: String::new(),
        attributes: Vec::new(),
        modifiers: Vec::new(),
        bases: Vec::new(),
    }
}
