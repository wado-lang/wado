//! Trait resolution as functions of a self-contained [`Program`], which
//! `elaborator::solver_bridge` lowers the compiler's tables into.

pub mod candidates;
pub mod coherence;
pub mod derive;
pub mod holds;
pub mod program;
pub mod rank;
#[cfg(test)]
pub mod testing;

pub use candidates::{Candidates, bound_candidates, candidates};
pub use coherence::{CoherenceError, coherence_errors};
pub use derive::derive;
pub use holds::{Holds, holds, holds_with_args};
pub use program::{
    ArgDefault, AssocId, Declaration, DerivationRequest, Env, Fact, ImplDef, ImplId, ImplOrigin,
    MethodId, ModuleId, ModuleScope, ParamBound, ParamDef, Pin, Program, RefRule, SolverType,
    TraitDeclId, TraitDef, TypeDeclId, TypeDef,
};
pub use rank::{Candidate, Generality, Selection, rank};
