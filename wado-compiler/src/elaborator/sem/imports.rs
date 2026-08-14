//! [`ModuleImports`] — per-module name resolution context derived from `use`
//! declarations (WEP 2026-05-26), populated before the body walk starts. A field
//! belongs here when it canonicalises an imported name to its declaring module,
//! or comes from a local declaration participating in same-name resolution. A
//! local declaration's *body* goes in [`super::decls::ModuleDecls`].

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;

/// Per-module name-resolution context derived from `use` declarations.
#[derive(Default, Clone)]
pub(crate) struct ModuleImports {
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
    /// Drop a `ns::member` prefix (single `::`, `ns` is a namespace-import
    /// alias) to the bare `member`, or `None` if not that shape. Used for the
    /// short-name comparisons in pattern matching (`Pattern::Struct` /
    /// `Pattern::Variant`); name resolution uses [`Self::canonical_ns_ref`].
    pub(crate) fn strip_ns_prefix<'s>(&self, name: &'s str) -> Option<&'s str> {
        strip_ns_prefix(&self.namespace_imports, name)
    }

    /// Canonicalize a namespace-qualified reference `ns::rest` to its `ns$rest`
    /// alias (`geo::Point` → `geo$Point`, `geo::Shape::Circle` →
    /// `geo$Shape::Circle`), or `None` if `ns` is not a namespace-import alias.
    /// The alias resolves through the per-name import maps to the namespace's
    /// own module.
    pub(crate) fn canonical_ns_ref(&self, name: &str) -> Option<String> {
        canonical_ns_ref(&self.namespace_imports, name)
    }
}

/// Free-function form of [`ModuleImports::canonical_ns_ref`], shared with
/// [`super::super::types::TypeLookup`], which holds only the alias map.
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

/// Free-function form of [`ModuleImports::strip_ns_prefix`]. Returns `None`
/// for multi-segment `ns::Type::case` forms.
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
