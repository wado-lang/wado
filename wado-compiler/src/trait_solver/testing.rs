//! What a solver test states a program with.

use super::program::{
    Fact, ImplDef, ImplOrigin, ParamDef, Program, SolverType, TraitDeclId, TraitDef, TypeDeclId,
};

pub(super) fn decl(id: TypeDeclId) -> SolverType {
    SolverType::Decl(id, vec![])
}

pub(super) fn ref_to(inner: SolverType) -> SolverType {
    SolverType::Ref {
        is_mut: false,
        inner: Box::new(inner),
    }
}

pub(super) fn concrete(trait_: TraitDeclId, target: SolverType) -> ImplDef {
    ImplDef {
        trait_: Some(trait_),
        trait_args: vec![],
        target,
        params: vec![],
        origin: ImplOrigin::Written,
    }
}

/// `impl<T: bounds> trait_ for target`, where `target` mentions `T` as
/// parameter 0.
pub(super) fn bounded(
    trait_: TraitDeclId,
    target: SolverType,
    bounds: Vec<TraitDeclId>,
) -> ImplDef {
    ImplDef {
        params: vec![ParamDef::bounded(bounds)],
        ..concrete(trait_, target)
    }
}

#[derive(Default)]
pub(super) struct Builder {
    program: Program,
}

impl Builder {
    pub(super) fn impl_(mut self, def: ImplDef) -> Self {
        self.program.push_impl(def);
        self
    }

    pub(super) fn concrete(self, trait_: TraitDeclId, target: SolverType) -> Self {
        self.impl_(concrete(trait_, target))
    }

    pub(super) fn bounded(
        self,
        trait_: TraitDeclId,
        target: SolverType,
        bounds: Vec<TraitDeclId>,
    ) -> Self {
        self.impl_(bounded(trait_, target, bounds))
    }

    pub(super) fn fact(mut self, head: TypeDeclId, trait_: TraitDeclId, fact: Fact) -> Self {
        self.program.facts.insert((head, trait_), fact);
        self
    }

    pub(super) fn supertrait(mut self, sub: TraitDeclId, super_: TraitDeclId) -> Self {
        self.program.traits.insert(
            sub,
            TraitDef {
                supertraits: vec![super_],
                ..TraitDef::default()
            },
        );
        self
    }

    pub(super) fn build(self) -> Program {
        self.program
    }
}
