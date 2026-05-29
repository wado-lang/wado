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

use crate::hashmap::IndexMap;
use crate::symbol::{Symbol, SymbolKey};

/// `use → def` edges and locally defined symbols for one module.
///
/// # Caveat: transient cross-module entries
///
/// The keys *almost always* belong to the owning module: every record site
/// (`Elaborator::record_reference` etc.) builds the use-side key from
/// `self.current_module_source`. The exception is
/// [`super::super::Elaborator::with_module_perspective`], which swaps
/// `current_module_source` to a foreign module while leaving `self.sem`
/// pointing at the outer module's `ModuleSemantics`. Any record calls inside
/// that scope tag their use-key with the foreign module but still write into
/// the outer module's bindings.
///
/// Today's only consumer ([`crate::semantics::semantics_with_logger`]) flattens
/// every module's bindings into a single `Semantics::references` map keyed by
/// the full `SymbolKey`, so the cross-module entries land in the right place
/// regardless of which `ModuleBindings` they passed through. The future
/// per-module reify pass (Stage 5) will need to either teach
/// `with_module_perspective` to swap `bindings` / `types` too, or accept that
/// these maps are a flat-store-by-construction.
#[derive(Default, Clone)]
pub(crate) struct ModuleBindings {
    /// `(module, IdentExpr.id) → (module, defining AstId)`. See the
    /// struct-level "Caveat: transient cross-module entries" note for when
    /// the use-side key's module does not match the owning module.
    pub(crate) references: IndexMap<SymbolKey, SymbolKey>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters) keyed by the binding's defining [`SymbolKey`]. See the
    /// struct-level "Caveat: transient cross-module entries" note.
    pub(crate) local_symbols: IndexMap<SymbolKey, Symbol>,
}
