//! Bounds collected from arguments, before fixing method type parameters.
use super::types::{ParameterId, Type};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundKind {
    Exact,
    Lower,
    Upper,
}

#[derive(Default)]
pub(super) struct Inference {
    pub bounds: HashMap<ParameterId, Vec<(BoundKind, Type)>>,
}

impl Inference {
    pub fn add(&mut self, id: &ParameterId, kind: BoundKind, ty: &Type) {
        // Neither null nor a lambda without a target supplies a type bound.
        if matches!(ty, Type::Null) {
            return;
        }
        let bounds = self.bounds.entry(id.clone()).or_default();
        if !bounds.iter().any(|bound| bound == &(kind, ty.clone())) {
            bounds.push((kind, ty.clone()));
        }
    }
}
