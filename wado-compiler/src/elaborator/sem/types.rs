//! [`TypeAnnotations`] — per-[`crate::ast::AstId`] type annotations and
//! dispatch decisions recorded during the body walk.
//!
//! # Membership rule
//!
//! Add a field here when it stores a fact keyed by an [`crate::ast::AstId`]
//! (or [`crate::symbol::SymbolKey`]) produced as a *decision* by the
//! body-level elaborator: the resolved type of an expression, the chosen
//! method dispatch target, the chosen coercion, the desugar kind of a
//! TIR-direct rewrite. This is what [`super::super::reify`] (Stage 5) will
//! read in lieu of re-running inference.
//!
//! Facts that derive purely from the AST (spans, position lookup) belong
//! on [`crate::ast_index::AstIndex`], not here. Decl-level facts (function
//! return types, generic-parameter tables) belong on
//! [`super::decls::ModuleDecls`].
//!
//! Stage 3 of [`wep-2026-05-26-elaborator-rearchitecture.md`] populates
//! `local_types` (the only field that exists on
//! [`super::super::Elaborator`] today and feeds LSP inlay hints). The
//! per-expression `TypeId` map, dispatch targets, coercion choices, and
//! desugar-kind annotations land in Stage 4.

use crate::hashmap::IndexMap;
use crate::symbol::SymbolKey;
use crate::tir::TypeId;

/// Per-`AstId` type annotations recorded by the body walk.
#[derive(Default, Clone)]
pub(crate) struct TypeAnnotations {
    /// Resolved [`TypeId`] for each local binding, keyed by the binding's
    /// defining [`SymbolKey`]. Populated alongside
    /// [`super::bindings::ModuleBindings::local_symbols`] at every
    /// `record_local_symbol` call. Consumed by LSP inlay hints via
    /// [`crate::semantics::Semantics::local_type_name`] so `let x = 1` can
    /// render the inferred `: i32` annotation without reaching into TIR.
    pub(crate) local_types: IndexMap<SymbolKey, TypeId>,
}
