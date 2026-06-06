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
    /// Drop a `<ns>::<member>` namespace prefix (single `::`, prefix is a
    /// namespace import alias) to the bare `<member>`. Returns `None` when the
    /// name isn't of that shape.
    ///
    /// Used only for the short-name *comparisons* in pattern matching
    /// (`Pattern::Struct` / `Pattern::Variant`), where the goal is to check
    /// the written type against the scrutinee's short name; the owning module
    /// is verified separately. Name *resolution* uses
    /// [`Self::canonical_ns_ref`] instead, which keeps the namespace so two
    /// namespaces exporting the same member stay distinct.
    pub(crate) fn strip_ns_prefix<'s>(&self, name: &'s str) -> Option<&'s str> {
        strip_ns_prefix(&self.namespace_imports, name)
    }

    /// Canonicalize a namespace-qualified reference `ns::rest` to its internal
    /// single-token alias form `ns$rest` (e.g. `geo::Point` → `geo$Point`,
    /// `geo::Shape::Circle` → `geo$Shape::Circle`). Returns `None` when the
    /// first segment is not a namespace-import alias.
    ///
    /// Unlike [`Self::strip_ns_prefix`] (which drops the prefix to a bare name
    /// and is used only for short-name *comparisons* in pattern matching),
    /// this keeps the namespace baked into the name so that registry lookups
    /// stay disambiguated by source module — two namespaces exporting the same
    /// member resolve to distinct `a$X` / `b$X` aliases.
    pub(crate) fn canonical_ns_ref(&self, name: &str) -> Option<String> {
        canonical_ns_ref(&self.namespace_imports, name)
    }
}

/// Free-function form of [`ModuleImports::canonical_ns_ref`], shared with the
/// type resolver's [`super::super::types::TypeLookup`], which carries only the
/// namespace-alias map.
pub(crate) fn canonical_ns_ref(
    namespace_imports: &IndexMap<String, ModuleSource>,
    name: &str,
) -> Option<String> {
    let pos = name.find("::")?;
    let prefix = &name[..pos];
    if !namespace_imports.contains_key(prefix) {
        return None;
    }
    let rest = &name[pos + 2..];
    Some(crate::name::namespace_member_alias(prefix, rest))
}

/// Free-function form of [`ModuleImports::strip_ns_prefix`] (bare-member
/// stripping for pattern short-name comparisons). Returns the bare `<member>`
/// for a single-`::` reference whose prefix is a known namespace alias, and
/// `None` otherwise (including multi-segment `<ns>::<Type>::<case>` forms).
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
