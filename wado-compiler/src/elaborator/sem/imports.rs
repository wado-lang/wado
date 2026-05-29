//! [`ModuleImports`] — per-module name resolution context derived from
//! `use` declarations.
//!
//! Populated by [`super::super::orchestration::Elaborator::build_tir_from_state`]
//! before the body walk starts, and consulted by name-resolution sites in
//! the elaborator (Stage 3 of
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]).
//!
//! # Membership rule
//!
//! Add a field here when it carries a fact derived from this module's
//! `use` declarations (or its local declarations that participate in
//! same-name resolution, like `interface` / `resource`). If the fact is
//! the canonicalisation of an *imported* name to its *declaring* module,
//! it belongs here. If the fact is a *local* declaration's body, it
//! belongs in [`super::decls::ModuleDecls`].

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;

/// Per-module name-resolution context derived from `use` declarations.
#[derive(Default, Clone)]
pub(crate) struct ModuleImports {
    /// `local_name → defining ModuleSource`, populated from
    /// `use { Foo as Bar } from "..."` declarations. `Bar` (the local name)
    /// is the key.
    pub(crate) imported_type_sources: IndexMap<String, ModuleSource>,
    /// `local_name → original_decl_name`. Populated only for aliased
    /// imports (`use { Foo as Bar }`), so [`super::super::Elaborator::canonical_decl_key`]
    /// can canonicalise an aliased name to its original declaration.
    pub(crate) import_original_names: IndexMap<String, String>,
    /// Namespace-alias map. `use helper from "..."` registers `helper →
    /// resolved("...")` so `helper::foo` paths in identifiers resolve
    /// against the namespace's module.
    pub(crate) namespace_imports: IndexMap<String, ModuleSource>,
    /// Effect name → module-source map built from import declarations and
    /// local `interface` / `resource` declarations. Consulted by
    /// [`super::super::Elaborator::resolve_effects`] and
    /// [`super::super::Elaborator::canonical_decl_key`] when resolving
    /// effect references in `with` clauses.
    pub(crate) effect_sources: IndexMap<String, ModuleSource>,
}
