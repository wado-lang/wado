//! The `unused_imports` lint: a `use` bringing in a name its module never
//! refers to. See `docs/spec-diagnostics.md`.

use crate::ast::{Item, UseDecl, UseItem, attrs_allow, inner_attrs_allow, lint};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::defs::{DefId, DefKind};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::NAMESPACE_MEMBER_SEP;
use crate::resolve::Resolutions;
use crate::semantics::Semantics;
use crate::token::Span;

/// Source-level `UnusedImport` warnings: every name a private `use` brings in
/// that no reference site of its module reaches. A re-export is used by
/// whoever imports it, so it is never reported.
pub fn unused_import_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let Some(resolutions) = sem.resolutions() else {
        return Vec::new();
    };
    let mut dispatched = dispatched_traits(sem, resolutions);
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !src.is_user_authored()
            || module.has_generated()
            || inner_attrs_allow(&module.inner_attributes, lint::UNUSED_IMPORTS)
        {
            continue;
        }
        let uses = Uses {
            resolutions,
            module: src,
            dispatched_traits: dispatched.swap_remove(src).unwrap_or_default(),
        };
        for item in &module.items {
            if let Item::Use(decl) = item
                && !decl.visibility.reaches_beyond_file()
                && !decl.is_export
                && !attrs_allow(&decl.attrs, lint::UNUSED_IMPORTS)
            {
                out.extend(
                    uses.unused_in(decl)
                        .into_iter()
                        .map(|(name, span)| Diagnostic {
                            severity: Severity::Warning,
                            code: Code::UnusedImport,
                            message: format!("import `{name}` is never used"),
                            span: Some(DiagnosticSpan::from_span(
                                &span,
                                Some(src.source_path().as_str()),
                            )),
                        }),
                );
            }
        }
    }
    out
}

/// What one module refers to.
struct Uses<'a> {
    resolutions: &'a Resolutions,
    module: &'a ModuleSource,
    /// The traits whose methods the module's calls dispatched to. A trait's
    /// methods are candidates only where the trait is in scope, so importing
    /// one for its methods uses it without naming it.
    dispatched_traits: IndexSet<DefId>,
}

impl Uses<'_> {
    /// The names `decl` brings in that go unused, each with where it is
    /// written.
    fn unused_in(&self, decl: &UseDecl) -> Vec<(String, Span)> {
        let mut unused = Vec::new();
        for item in &decl.items {
            match item {
                UseItem::Simple {
                    name,
                    alias,
                    local_span,
                    ..
                } => {
                    let local = alias.as_ref().unwrap_or(name);
                    if !self.uses_local(local) {
                        unused.push((local.clone(), *local_span));
                    }
                }
                UseItem::InterfaceFunctions {
                    interface_name,
                    name_span,
                    functions,
                    ..
                } => {
                    let unused_functions: Vec<(String, Span)> = functions
                        .iter()
                        .map(|f| (f.alias.as_ref().unwrap_or(&f.name).clone(), f.local_span))
                        .filter(|(local, _)| !self.uses_local(local))
                        .collect();
                    // Naming the interface is what imports its functions, so it
                    // is unused only with all of them.
                    if unused_functions.len() == functions.len() && !self.uses_local(interface_name)
                    {
                        unused.push((interface_name.clone(), *name_span));
                    } else {
                        unused.extend(unused_functions);
                    }
                }
                UseItem::Namespace { name, name_span } => {
                    let reached = self
                        .resolutions
                        .imports_in(self.module)
                        .filter(|(alias, _)| {
                            alias
                                .strip_prefix(name.as_str())
                                .is_some_and(|rest| rest.starts_with(NAMESPACE_MEMBER_SEP))
                        })
                        .any(|(_, def)| self.resolutions.references_from(self.module, def));
                    if !reached {
                        unused.push((name.clone(), *name_span));
                    }
                }
                UseItem::Wildcard => {}
            }
        }
        unused
    }

    /// Whether the module refers to what it imported as `local`.
    fn uses_local(&self, local: &str) -> bool {
        // An import that reaches nothing has its own error, and the editor
        // lints a module that still carries one.
        let Some(def) = self.resolutions.imported_as(self.module, local) else {
            return true;
        };
        self.resolutions.references_from(self.module, def) || self.dispatched_traits.contains(&def)
    }
}

/// Per user-authored module, the traits declaring or implementing a method
/// that one of its method calls dispatched to.
fn dispatched_traits(
    sem: &Semantics,
    resolutions: &Resolutions,
) -> IndexMap<ModuleSource, IndexSet<DefId>> {
    let defs = resolutions.defs();
    let mut implemented: IndexMap<DefId, Option<DefId>> = IndexMap::default();
    let mut out: IndexMap<ModuleSource, IndexSet<DefId>> = IndexMap::default();
    for (site, target) in sem.iter_references() {
        let Some(module) = sem.module_of_id(site) else {
            continue;
        };
        if !module.is_user_authored() {
            continue;
        }
        let Some(method) = defs.of_ast_id(target) else {
            continue;
        };
        if defs.kind(method) != DefKind::Method {
            continue;
        }
        let owner = defs.parent(method).expect("a method has the item declaring it");
        let owning_trait = match defs.kind(owner) {
            DefKind::Trait => Some(owner),
            DefKind::Impl => *implemented
                .entry(owner)
                .or_insert_with(|| implemented_trait(sem, resolutions, owner)),
            _ => None,
        };
        if let Some(t) = owning_trait {
            out.entry(module.clone()).or_default().insert(t);
        }
    }
    out
}

/// The trait the `impl` block `block` implements; `None` for an inherent one.
fn implemented_trait(sem: &Semantics, resolutions: &Resolutions, block: DefId) -> Option<DefId> {
    let defs = resolutions.defs();
    let id = defs.ast_id(block);
    sem.modules[defs.module(block)]
        .items
        .iter()
        .find_map(|item| match item {
            Item::Impl(b) if b.id == id => resolutions.head_decl(b.trait_type.as_ref()?),
            _ => None,
        })
}
