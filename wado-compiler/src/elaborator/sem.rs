//! [`ModuleSemantics`] — per-module semantic facts produced by the elaborator.
//!
//! Introduced by [`wep-2026-05-26-elaborator-rearchitecture.md`]. Stage 1
//! placed the empty skeleton; Stage 3 populates the four sub-structs with
//! the per-module state that previously lived as flat fields on
//! [`super::Elaborator`] (and as `Rc<RefCell<…>>`-shared maps on
//! [`super::orchestration::AnnotateState`]).
//!
//! # Ownership
//!
//! One instance per loaded module, owned by
//! [`super::orchestration::AnnotateState::module_semantics`]. The per-module
//! [`super::Elaborator`] takes ownership of the instance for the duration
//! of [`super::Elaborator::resolve_module`] and the driver re-installs it
//! into the map afterwards; every other phase (including the future
//! `reify`) takes `&ModuleSemantics`.
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

/// Per-module semantic facts. See the module-level documentation for the
/// membership rules and ownership story.
#[derive(Default, Clone)]
pub(crate) struct ModuleSemantics {
    pub(crate) bindings: ModuleBindings,
    pub(crate) imports: ModuleImports,
    pub(crate) types: TypeAnnotations,
    pub(crate) decls: ModuleDecls,
}
