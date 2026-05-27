//! [`ModuleDecls`] — module-internal declarations confirmed by elaboration.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. Empty at this stage.
//!
//! # Membership rule
//!
//! Add a field here when it summarises a *declaration* in this module
//! after elaboration has resolved its signature: function return types,
//! generic parameter tables, global variable types, associated constants,
//! synthesised structures. If the fact is a *use-site* decision
//! (annotation on an [`crate::ast::AstId`]) it belongs in
//! [`super::types::TypeAnnotations`]; if it is an *import* fact derived
//! from a `use` declaration it belongs in [`super::imports::ModuleImports`].
//!
//! # Planned contents
//!
//! Function / global tables:
//!
//! - **`function_return_types`** — `func_name → return TypeId` for
//!   functions defined in this module. Today on
//!   [`super::super::Elaborator::function_return_types`].
//! - **`imported_functions`** — names visible via `use`. Today on
//!   [`super::super::Elaborator::imported_functions`].
//! - **`current_module_globals`** — `name → (TypeId, is_mut)` for globals
//!   declared in this module. Today on
//!   [`super::super::Elaborator::current_module_globals`].
//! - **`imported_globals`** — `local_name → (source, original_name, TypeId,
//!   is_mut)` for globals brought in by `use`. Today on
//!   [`super::super::Elaborator::imported_globals`].
//! - **`associated_constants`** — `"Type::CONST" → (ast::Type, ast::Expr)`,
//!   inlined at every use site during resolution. Today on
//!   [`super::super::Elaborator::associated_constants`].
//!
//! Generic-parameter tables (the elaborator's resolution memo for generic
//! functions / methods / structs):
//!
//! - **`generic_struct_names`** — Today on
//!   [`super::super::Elaborator::generic_struct_names`].
//! - **`generic_function_params`**,
//!   **`generic_function_resolved_param_types`**,
//!   **`generic_function_resolved_return_types`** — Today on the
//!   corresponding [`super::super::Elaborator`] fields.
//! - **`generic_method_params`**,
//!   **`generic_method_resolved_param_types`** — Today on the
//!   corresponding [`super::super::Elaborator`] fields.
//!
//! Per-module local additions to the cross-module decl tables (anonymous
//! structs synthesised from struct literals, and any decl the elaborator
//! discovered while walking this module):
//!
//! - **`pending_anonymous_structs`** — Today on
//!   [`super::super::Elaborator::pending_anonymous_structs`]; flushed into
//!   the [`crate::tir::TirModule`] at the end of `resolve_module`.
//! - **`local_struct_fields`**, **`local_newtypes`**,
//!   **`local_generic_newtypes`**, **`local_enum_cases`**,
//!   **`local_flags_cases`**, **`local_variant_cases`** — Today on the
//!   corresponding [`super::super::Elaborator`] fields. Stage 2/3 will
//!   revisit whether these stay here or graduate into
//!   [`super::super::tysys::TypeSystem`] as a per-module view.
//!
//! # Planned API
//!
//! - `function_return_type(name) -> Option<TypeId>`
//! - `generic_function_params(name) -> Option<&[…]>`, …
//! - `imported_global(name) -> Option<&ImportedGlobal>`
//! - `associated_constant(key) -> Option<&(ast::Type, ast::Expr)>`
//! - `pending_anonymous_structs()` drain.

pub(crate) struct ModuleDecls {}
