//! Owned syntax. IDs refer only to records in this file revision.
use super::types::{Constant, PassingMode, WrittenType};
use serde::{Deserialize, Serialize};
use std::ops::Range;

pub type ExprId = u32;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileSyntax {
    pub forwarders: Vec<(String, String)>,
    pub headers: Vec<Header>,
    pub imports: Vec<Import>,
    pub expressions: Vec<Expression>,
    pub locals: Vec<Local>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeclarationFile {
    pub declarations: Vec<crate::model::Declaration>,
    pub headers: Vec<Header>,
    pub imports: Vec<Import>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyFile {
    pub expressions: Vec<Expression>,
    pub locals: Vec<Local>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub local: bool,
    pub declaration: u32,
    pub owner: Option<u32>,
    pub ty: WrittenType,
    pub parameters: Vec<Parameter>,
    pub generics: Vec<GenericParameter>,
    pub bases: Vec<WrittenType>,
    pub explicit_interface: Option<WrittenType>,
    pub implementations: Vec<WrittenMember>,
    pub constant: Option<Constant>,
    pub accessors: Vec<Accessor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Accessor {
    pub role: String,
    pub access: String,
    pub metadata_method: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WrittenMember {
    pub owner: WrittenType,
    pub name: String,
    pub parameters: Vec<WrittenType>,
    pub generic_arity: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenericParameter {
    pub name: String,
    pub variance: Variance,
    pub constraints: Vec<WrittenType>,
    pub special_constraints: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub enum Variance {
    #[default]
    Invariant,
    In,
    Out,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub ty: WrittenType,
    pub mode: PassingMode,
    pub default: Option<String>,
    pub variadic: bool,
    pub receiver: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ImportKind {
    Namespace,
    Static,
    Alias(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub kind: ImportKind,
    pub ty: WrittenType,
    pub scope: Range<usize>,
    pub global: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Expression {
    pub span: Range<usize>,
    pub kind: ExpressionKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ExpressionKind {
    Name {
        name: String,
        arguments: Vec<WrittenType>,
    },
    Member {
        receiver: ExprId,
        name: ExprId,
        conditional: bool,
    },
    Call {
        function: ExprId,
        arguments: Vec<Argument>,
    },
    New {
        ty: WrittenType,
        arguments: Vec<Argument>,
    },
    Literal {
        kind: String,
        value: String,
    },
    Cast {
        ty: WrittenType,
        value: ExprId,
    },
    Index {
        receiver: ExprId,
        arguments: Vec<Argument>,
    },
    Assign {
        left: ExprId,
        right: ExprId,
    },
    Lambda {
        parameters: Vec<Parameter>,
        body: ExprId,
    },
    Await(ExprId),
    Wrapped(ExprId),
    Unsupported(String),
    OutVariable {
        name: String,
        ty: WrittenType,
    },
    ImplicitCall {
        name: String,
        receiver: Option<ExprId>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Argument {
    pub name: Option<String>,
    pub mode: PassingMode,
    pub value: ExprId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Local {
    pub name: String,
    pub span: Range<usize>,
    pub scope: Range<usize>,
    pub ty: WrittenType,
    pub value: Option<ExprId>,
    pub iteration: bool,
    pub out_argument: Option<ExprId>,
}

/// Partition syntax by outer callable. Local functions and lambdas stay with
/// their enclosing callable so lexical dependencies never cross records.
pub fn split_bodies(
    syntax: &FileSyntax,
    declarations: &[crate::model::Declaration],
    length: usize,
) -> anyhow::Result<Vec<(Range<usize>, BodyFile)>> {
    let mut callables: Vec<_> = declarations
        .iter()
        .filter(|d| d.callable())
        .map(|d| d.span.clone())
        .collect();
    callables.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for span in callables {
        if ranges
            .last()
            .is_none_or(|previous| previous.end <= span.start)
        {
            ranges.push(span);
        }
    }
    let group = |span: &Range<usize>| {
        let position = ranges.partition_point(|range| range.start <= span.start);
        if position > 0 && ranges[position - 1].end >= span.end {
            position
        } else {
            0
        }
    };
    let mut groups: Vec<_> = std::iter::once(0..length)
        .chain(ranges.iter().cloned())
        .map(|range| {
            (
                range,
                BodyFile {
                    expressions: vec![],
                    locals: vec![],
                },
            )
        })
        .collect();
    let mut remap = Vec::with_capacity(syntax.expressions.len());
    let mut owners = Vec::with_capacity(syntax.expressions.len());
    for expression in &syntax.expressions {
        let owner = group(&expression.span);
        let mut expression = expression.clone();
        let map = |id: &mut ExprId| -> anyhow::Result<()> {
            anyhow::ensure!(
                owners.get(*id as usize) == Some(&owner),
                "Expression crosses a body record boundary"
            );
            *id = remap[*id as usize];
            Ok(())
        };
        match &mut expression.kind {
            ExpressionKind::Member { receiver, name, .. } => {
                map(receiver)?;
                map(name)?;
            }
            ExpressionKind::Call {
                function,
                arguments,
            } => {
                map(function)?;
                for argument in arguments {
                    map(&mut argument.value)?;
                }
            }
            ExpressionKind::New { arguments, .. } => {
                for argument in arguments {
                    map(&mut argument.value)?;
                }
            }
            ExpressionKind::Cast { value, .. }
            | ExpressionKind::Await(value)
            | ExpressionKind::Wrapped(value) => map(value)?,
            ExpressionKind::Index {
                receiver,
                arguments,
            } => {
                map(receiver)?;
                for argument in arguments {
                    map(&mut argument.value)?;
                }
            }
            ExpressionKind::Assign { left, right } => {
                map(left)?;
                map(right)?;
            }
            ExpressionKind::Lambda { body, .. } => map(body)?,
            ExpressionKind::ImplicitCall {
                receiver: Some(receiver),
                ..
            } => map(receiver)?,
            _ => {}
        }
        remap.push(groups[owner].1.expressions.len() as ExprId);
        owners.push(owner);
        groups[owner].1.expressions.push(expression);
    }
    for local in &syntax.locals {
        let owner = group(&local.span);
        let mut local = local.clone();
        for id in [&mut local.value, &mut local.out_argument]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                owners.get(*id as usize) == Some(&owner),
                "Local crosses a body record boundary"
            );
            *id = remap[*id as usize];
        }
        groups[owner].1.locals.push(local);
    }
    Ok(groups)
}
