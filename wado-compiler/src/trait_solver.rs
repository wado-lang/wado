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

pub use candidates::{Candidates, candidates};
pub use coherence::{CoherenceError, coherence_errors};
pub use derive::derive;
pub use holds::{Holds, holds};
pub use program::{
    ArgDefault, AssocId, Declaration, DerivationRequest, Env, Fact, ImplDef, ImplId, ImplOrigin,
    MethodId, ModuleId, ModuleScope, ParamDef, Pin, Program, RefRule, SolverType, TraitDeclId,
    TraitDef, TypeDeclId, TypeDef,
};
pub use rank::{Candidate, Selection, rank};
