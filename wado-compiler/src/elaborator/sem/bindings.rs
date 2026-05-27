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
#[derive(Default, Clone)]
pub(crate) struct ModuleBindings {
    /// `(module, IdentExpr.id) → (module, defining AstId)`. The use-site key
    /// always lives in this module's `ModuleSource`; the def-site key may
    /// point at a different module (e.g. an imported function).
    pub(crate) references: IndexMap<SymbolKey, SymbolKey>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters) keyed by the binding's defining [`SymbolKey`]. The key's
    /// `module` field always equals this `ModuleBindings`'s owning module.
    pub(crate) local_symbols: IndexMap<SymbolKey, Symbol>,
}
