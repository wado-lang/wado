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

/// Per-(impl_block, trait_default_method) snapshot of the body-walk facts
/// recorded while the combined walk synthesises one trait default method on
/// behalf of one specific impl. The body walk writes to `types` and
/// `bindings`; both maps are keyed by `(trait_module, ast_id)` because
/// `ann_module_override` selects the trait module during the walk (the
/// trait body's AST nodes belong to the trait module, not the impl).
///
/// `decls` and `imports` are NOT snapshotted: the body walk reads them (for
/// name resolution / import context / decl indices) but every mutating
/// write that may occur is supposed to flow into the surrounding impl
/// module's `ModuleSemantics` (e.g. `pending_anonymous_structs` from an
/// anon literal inside a default body must reach the impl's
/// `TirModule`). Keeping `decls` / `imports` shared with the surrounding
/// `ModuleSemantics` preserves that flow.
///
/// Reify reads these facts from
/// [`ModuleSemantics::default_method_facts`] keyed by
/// `(impl_block.id, default_method.ast_id)` to synthesise the impl's
/// default-method `TirFunction`s without re-running annotate.
#[derive(Default, Clone)]
#[allow(dead_code)]
pub(crate) struct DefaultMethodFacts {
    pub(crate) types: TypeAnnotations,
    pub(crate) bindings: ModuleBindings,
}

/// Per-module semantic facts. See the module-level documentation for the
/// membership rules and ownership story.
#[derive(Default, Clone)]
pub(crate) struct ModuleSemantics {
    pub(crate) bindings: ModuleBindings,
    pub(crate) imports: ModuleImports,
    pub(crate) types: TypeAnnotations,
    pub(crate) decls: ModuleDecls,
    /// Per-impl trait default-method facts. Keyed by
    /// `(impl_block.id, trait_default_method.ast_id)` — `impl_block.id`
    /// comes from the impl module's parse tree, `default_method.id` from
    /// the trait module's parse tree. The pair is unique per synthesis
    /// site even when the same default body is synthesised across many
    /// impls of the same trait, so the per-walk fact maps stay isolated
    /// (no `(trait_module, ast_id)` collision).
    ///
    /// Populated by the combined walk's `Item::Impl` default-method loop
    /// (`elaborator.rs`'s trait-default synthesis branch) and consumed by
    /// reify's `reify_impl_default_methods` to produce the same
    /// `TirFunction`s the combined walk used to push onto
    /// `ModuleDecls::pending_default_methods`.
    #[allow(dead_code)]
    pub(crate) default_method_facts:
        IndexMap<(AstId, AstId), DefaultMethodFacts>,
}
