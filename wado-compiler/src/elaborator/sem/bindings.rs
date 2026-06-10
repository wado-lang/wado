//! [`ModuleBindings`] — `use → def` edges and locally defined symbols.
//!
//! Populated by the body walk (Stage 3 of
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]) — every call site that
//! resolves an identifier to its defining symbol writes here, and every
//! site that introduces a user-visible local binding registers it here.
//!
//! # Membership rule
//!
//! Add a field here when it answers a question the LSP poses for
//! go-to-definition, find-references, or hover-on-local — and only when
//! that question depends on a use-site / def-site pair within a single
//! module. Per-`AstId` type facts go to [`super::types::TypeAnnotations`],
//! not here.
//!
//! # Ownership
//!
//! One instance per loaded module, owned by [`super::ModuleSemantics`].
//! Plain owned [`crate::hashmap::IndexMap`]s replace the previous
//! `Rc<RefCell<…>>` plumbing the elaborator shared with
//! [`super::super::orchestration::AnnotateState`]: each module's body walk
//! has exclusive `&mut` access to its own [`ModuleBindings`] for the
//! duration of [`super::super::Elaborator::resolve_module`], and the
//! driver re-installs the populated instance back into
//! `state.module_semantics` afterwards.

use crate::ast::AstId;
use crate::hashmap::IndexMap;
use crate::symbol::Symbol;

/// `use → def` edges and locally defined symbols for one module.
///
/// Keys are bare [`AstId`]s — globally unique, so an edge recorded while a
/// walk visits foreign AST (e.g. under
/// [`super::super::Elaborator::with_module_perspective`]) still names its node
/// exactly, whichever module's `ModuleBindings` it lands in; the sole consumer
/// ([`crate::semantics::semantics_with_logger`]) flattens them into single
/// `Semantics` maps. Def-side values are bare `AstId`s too; navigation
/// recovers a def's module from its space (`Semantics::module_of_id`).
#[derive(Default, Clone)]
pub(crate) struct ModuleBindings {
    /// `IdentExpr.id → defining AstId`.
    pub(crate) references: IndexMap<AstId, AstId>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters) keyed by the binding's defining [`AstId`].
    pub(crate) local_symbols: IndexMap<AstId, Symbol>,
}
