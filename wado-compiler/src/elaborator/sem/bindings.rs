//! [`ModuleBindings`] — `use → def` edges and locally defined symbols.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. Empty at this stage.
//!
//! # Membership rule
//!
//! Add a field here when it answers a question the LSP poses for
//! go-to-definition, find-references, or hover-on-local — and only when
//! that question depends on a use-site / def-site pair within a single
//! module. Per-`AstId` type facts go to [`super::types::TypeAnnotations`],
//! not here.
//!
//! # Planned contents
//!
//! - **`references`** — `(module, IdentExpr.id) → (module, defining AstId)`.
//!   Populated by `resolve_ident` / `resolve_call` / etc. as the body walk
//!   reaches each identifier. Today lives on
//!   [`super::super::Elaborator::references`] (shared via `Rc<RefCell<…>>`
//!   with [`super::super::orchestration::AnnotateState::references`]).
//! - **`local_symbols`** — locally-defined [`crate::symbol::Symbol`]s
//!   (let bindings, parameters, closure parameters) keyed by the binding's
//!   defining [`crate::symbol::SymbolKey`]. Today lives on
//!   [`super::super::Elaborator::local_symbols`] (shared via
//!   `Rc<RefCell<…>>` with
//!   [`super::super::orchestration::AnnotateState::local_symbols`]).
//!
//! # Planned API
//!
//! - `record_reference(use_id, def_id)`
//! - `record_local_symbol(def_id, symbol)`
//! - Reference-resolution helpers used by `Semantics::referenced_symbol`
//!   and `Semantics::references_to_def`.

pub(crate) struct ModuleBindings {}
