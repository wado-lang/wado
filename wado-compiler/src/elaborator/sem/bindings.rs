//! [`ModuleBindings`] — `use → def` edges and locally defined symbols, written
//! by the body walk (WEP 2026-05-26) wherever an identifier resolves or a
//! user-visible local is introduced. A field belongs here when it answers a
//! go-to-definition or find-references question over a use-site / def-site pair;
//! per-`AstId` type facts go to [`super::types::TypeAnnotations`].
//!
//! One instance per loaded module, owned by [`super::ModuleSemantics`] and held
//! `&mut` by that module's body walk for its duration, so plain owned maps
//! replace the `Rc<RefCell<…>>` the elaborator once shared with the driver.

use crate::ast::AstId;
use crate::hashmap::IndexMap;
use crate::symbol::Symbol;

/// `use → def` edges and locally defined symbols for one module.
///
/// Keys are bare [`AstId`]s — globally unique, so an edge recorded while a
/// walk visits foreign AST (e.g. under
/// [`super::super::Elaborator::with_module_perspective_for`]) still names its node
/// exactly, whichever module's `ModuleBindings` it lands in — which is where it
/// stays, [`crate::semantics::Semantics`] routing a query to it rather than
/// holding a copy. Def-side values are bare `AstId`s too; navigation
/// recovers a def's module from its space (`Semantics::module_of_id`).
#[derive(Default, Clone)]
pub(crate) struct ModuleBindings {
    /// `IdentExpr.id → defining AstId`.
    pub(crate) references: IndexMap<AstId, AstId>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters) keyed by the binding's defining [`AstId`].
    pub(crate) local_symbols: IndexMap<AstId, Symbol>,
}
