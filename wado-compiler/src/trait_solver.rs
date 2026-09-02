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
//! [`coherence_errors`] is the first of the four questions the WEP names.
//! `holds`, `candidates` and `rank` follow it onto this representation.

mod coherence;
mod program;

pub use coherence::{CoherenceError, coherence_errors};
pub use program::{ImplDef, ImplId, ParamDef, Program, SolverType, TraitDeclId, TypeDeclId};
