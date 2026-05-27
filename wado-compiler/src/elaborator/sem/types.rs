//! [`TypeAnnotations`] — per-[`crate::ast::AstId`] type annotations and
//! dispatch decisions recorded during the body walk.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. Empty at this stage.
//!
//! # Membership rule
//!
//! Add a field here when it stores a fact keyed by an [`crate::ast::AstId`]
//! (or [`crate::symbol::SymbolKey`]) produced as a *decision* by the
//! body-level elaborator: the resolved type of an expression, the chosen
//! method dispatch target, the chosen coercion, the desugar kind of a
//! TIR-direct rewrite. This is what [`super::super::reify`] (Stage 5) reads
//! in lieu of re-running inference.
//!
//! Facts that derive purely from the AST (spans, position lookup) belong
//! on [`crate::ast_index::AstIndex`], not here. Decl-level facts (function
//! return types, generic-parameter tables) belong on
//! [`super::decls::ModuleDecls`].
//!
//! # Planned contents
//!
//! New facts introduced by Stage 4 — none of these exist on the current
//! [`super::super::Elaborator`] yet:
//!
//! - The [`crate::tir::TypeId`] of every typed expression.
//! - The resolved target of each method call (impl block + method name).
//! - The chosen coercion at each conversion site.
//! - The desugar kind for each TIR-direct rewrite (`assert`, `matches`,
//!   comparison chain, for-of, while, compound assignment).
//!
//! Existing fields migrating in from [`super::super::Elaborator`]:
//!
//! - **`local_types`** — [`crate::tir::TypeId`] for each local binding,
//!   keyed by the binding's defining [`crate::symbol::SymbolKey`]. Today
//!   on [`super::super::Elaborator::local_types`] (shared via
//!   `Rc<RefCell<…>>` with
//!   [`super::super::orchestration::AnnotateState::local_types`]). Consumed
//!   by LSP inlay hints via [`crate::semantics::Semantics::local_type_name`]
//!   — the consumer API layer hides the placement, so moving this here
//!   does not affect LSP callers.
//!
//! # Planned API
//!
//! - `set(ast_id, type_id)` / `get(ast_id) -> Option<TypeId>`
//! - `dispatch_target(ast_id) -> Option<MethodTarget>`
//! - `coercion_at(ast_id) -> Option<Coercion>`
//! - `desugar_kind(ast_id) -> Option<DesugarKind>`

pub(crate) struct TypeAnnotations {}
