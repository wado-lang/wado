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

impl ModuleImports {
    /// Canonicalize a `<ns>::<member>` reference (single `::`, prefix is a
    /// namespace import alias) to the bare `<member>` form. Returns `None`
    /// when the name isn't of that shape — including multi-segment cases like
    /// `<ns>::<Type>::<case>`, which route through dedicated namespace paths
    /// (see `resolve_ident` / `resolve_call` / `reify_ident`).
    ///
    /// This is the single source of truth shared by both the annotate phase
    /// ([`super::super::Elaborator`]) and the reify phase
    /// ([`super::super::reify::Reify`]): the AST keeps the user-written
    /// `ns::member` (so LSP cursors land on it as typed) while every
    /// registry lookup sees the canonical bare name the registries were
    /// populated with.
    pub(crate) fn strip_ns_prefix<'s>(&self, name: &'s str) -> Option<&'s str> {
        strip_ns_prefix(&self.namespace_imports, name)
    }
}

/// Canonicalize a `<ns>::<member>` reference against a namespace-alias map.
///
/// Shared by [`ModuleImports::strip_ns_prefix`] and the type resolver's
/// [`super::super::types::TypeLookup`], which carries only the alias map
/// rather than the full [`ModuleImports`]. Returns the bare `<member>` for a
/// single-`::` reference whose prefix is a known namespace alias, and `None`
/// otherwise (including multi-segment `<ns>::<Type>::<case>` forms).
pub(crate) fn strip_ns_prefix<'s>(
    namespace_imports: &IndexMap<String, ModuleSource>,
    name: &'s str,
) -> Option<&'s str> {
    let pos = name.find("::")?;
    let prefix = &name[..pos];
    let suffix = &name[pos + 2..];
    if suffix.contains("::") {
        return None;
    }
    if !namespace_imports.contains_key(prefix) {
        return None;
    }
    Some(suffix)
}
