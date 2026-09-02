//! Trait resolution as a function of a self-contained value.
//!
//! `docs/wep-2026-09-01-trait-resolution.md` states the order; this module is
//! where it is checkable. Nothing here reaches a `TypeId`, a `DefId`, an AST
//! node, or the annotate-time scope, so a test states a [`Program`] and asks a
//! question, rather than compiling Wado source and reading what came out.
//!
//! `elaborator::trait_env` lowers the compiler's tables into a [`Program`] and
//! turns each answer into a diagnostic. A diagnostic is returned here, never
//! emitted: only the caller knows what a declaration is called.
//!
//! [`coherence_errors`], [`holds`] and [`rank`] are three of the four questions
//! the WEP names. `candidates` — which impls a call has, and at which level of
//! the receiver's newtype chain — follows them onto this representation.

mod coherence;
mod holds;
mod program;
mod rank;

pub use coherence::{CoherenceError, coherence_errors};
pub use holds::{Holds, holds};
pub use program::{
    DerivationRequest, Env, Fact, ImplDef, ImplId, ModuleId, ParamDef, Program, SolverType,
    TraitDeclId, TraitDef, TypeDeclId,
};
pub use rank::{Candidate, Selection, rank};
