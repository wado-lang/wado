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

pub use candidates::{Candidates, bound_candidates, candidates};
pub use coherence::{CoherenceError, coherence_errors};
pub use derive::{derive, pair_comparisons};
pub use holds::{Holds, comparison_row, holds, holds_with_args, owed};
pub use program::{
    ArgDefault, AssocId, Declaration, DerivationRequest, Env, Fact, ImplDef, ImplId, ImplOrigin,
    MethodId, ModuleId, ModuleScope, ParamBound, ParamDef, Pin, Program, RefRule, SolverType,
    TraitDeclId, TraitDef, TypeDeclId, TypeDef, args_per_param,
};
pub use rank::{Candidate, Generality, Selection, rank};
