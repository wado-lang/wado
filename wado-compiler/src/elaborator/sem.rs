//! [`ModuleSemantics`] — per-module semantic facts produced by the elaborator.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. The four sub-structs
//! are empty at this stage; the migration plan populates them across
//! stages 3-5 by moving fields off [`super::Elaborator`] and
//! [`super::orchestration::AnnotateState`].
//!
//! # Future ownership (per the WEP)
//!
//! One instance per loaded module, owned by [`crate::semantics::Semantics`]
//! in an `IndexMap<ModuleSource, ModuleSemantics>`. `annotate_bodies` takes
//! `&mut ModuleSemantics` for the module it is processing; every other phase
//! (including `reify`) takes `&ModuleSemantics`.
//!
//! # Decomposition
//!
//! Membership is split into four sub-structs with explicit responsibility
//! rules. Each sub-struct lives in its own file because the
//! "does this fit the sub-struct's responsibility?" question is what gates
//! adding a new field; a single file per rule keeps the question reviewable.
//!
//! - [`bindings::ModuleBindings`] — `use → def` edges and locally defined
//!   symbols (the data the LSP reads to answer go-to-definition,
//!   find-references, and hover-on-local).
//! - [`imports::ModuleImports`] — per-module name resolution context derived
//!   from `use` declarations.
//! - [`types::TypeAnnotations`] — per-[`crate::ast::AstId`] type annotations
//!   and dispatch decisions recorded during the body walk; consumed by
//!   `reify`.
//! - [`decls::ModuleDecls`] — module-internal declarations confirmed by
//!   elaboration.

pub(crate) mod bindings;
pub(crate) mod decls;
pub(crate) mod imports;
pub(crate) mod types;

pub(crate) use bindings::ModuleBindings;
pub(crate) use decls::ModuleDecls;
pub(crate) use imports::ModuleImports;
pub(crate) use types::TypeAnnotations;

#[expect(
    dead_code,
    reason = "Stage 1 skeleton; populated by stages 3-5 of the elaborator re-architecture."
)]
pub(crate) struct ModuleSemantics {
    pub(crate) bindings: ModuleBindings,
    pub(crate) imports: ModuleImports,
    pub(crate) types: TypeAnnotations,
    pub(crate) decls: ModuleDecls,
}
