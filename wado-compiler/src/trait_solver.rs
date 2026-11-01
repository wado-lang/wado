//! Trait resolution as functions of a self-contained [`Program`], which
//! `elaborator::solver_bridge` lowers the compiler's tables into.

mod candidates;
mod coherence;
mod derive;
mod holds;
mod program;
mod rank;
#[cfg(test)]
mod testing;

pub use candidates::{
    Candidates, bound_candidates, candidates, cmp_giving_eq, instance_candidates,
};
pub use coherence::{CoherenceError, coherence_errors, mark_duplicates};
pub use derive::{derive, derive_alongside, pair_comparisons, sourced, written_over};
pub use holds::{Holds, comparison_row, holds, holds_with_args, owed, written_comparison};
pub use program::{
    ArgDefault, AssocId, Declaration, DerivationRequest, Env, Fact, ImplDef, ImplId, ImplOrigin,
    MethodId, ModuleId, ModuleScope, ParamBound, ParamDef, Pin, Program, RefRule, SolverType,
    Source, TraitDeclId, TraitDef, TypeDeclId, TypeDef, args_per_param,
};
pub use rank::{Candidate, Generality, Selection, rank};
