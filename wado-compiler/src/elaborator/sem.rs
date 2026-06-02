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

use crate::ast::AstId;
use crate::hashmap::IndexMap;

/// Per-module semantic facts. See the module-level documentation for the
/// membership rules and ownership story.
#[derive(Default, Clone)]
pub(crate) struct ModuleSemantics {
    pub(crate) bindings: ModuleBindings,
    pub(crate) imports: ModuleImports,
    pub(crate) types: TypeAnnotations,
    pub(crate) decls: ModuleDecls,
    /// Per-impl trait default-method `ModuleSemantics` snapshots. Keyed by
    /// `(impl_block.id, trait_default_method.ast_id)` — `impl_block.id`
    /// from the impl module's parse tree, `default_method.id` from the
    /// trait module's parse tree. The pair is unique per synthesis site
    /// even when the same default body is synthesised across many impls of
    /// the same trait, so the per-walk fact maps stay isolated (no
    /// `(trait_module, ast_id)` collision).
    ///
    /// Each value is a full `ModuleSemantics` with:
    /// - `types` / `bindings`: freshly produced by the combined walk's body
    ///   walk for that one `(impl, default_method)` pair, keyed under the
    ///   trait module via `ann_module_override`.
    /// - `decls` / `imports`: cloned from the surrounding impl module's
    ///   `ModuleSemantics` so name resolution + decl indices work inside
    ///   the default body walk.
    /// - `default_method_semantics`: empty (no recursive synthesis).
    ///
    /// Reify reads each entry by doing the same `self.sem` /
    /// `self.current_module_source` swap that
    /// [`super::reify::Reify::with_const_module_perspective`] uses for
    /// cross-module AST: the entry has lifetime `'a` because it lives
    /// inside the impl module's `ModuleSemantics` (which reify borrows
    /// at `'a`), so the swap is a pointer swap (no lifetime extension).
    ///
    /// Populated by the combined walk's `Item::Impl` default-method loop
    /// (`elaborator.rs`'s trait-default synthesis branch) and consumed by
    /// reify's `reify_impl_default_methods` to produce the same
    /// `TirFunction`s the combined walk used to push onto
    /// `ModuleDecls::pending_default_methods` (now gone — reify is the
    /// sole producer of default-method TIR).
    pub(crate) default_method_semantics: IndexMap<(AstId, AstId), ModuleSemantics>,
}
