//! The `unused_imports` lint: a `use` bringing in a name its module never
//! refers to. See `docs/spec-diagnostics.md`.

use crate::Lints;
use crate::ast::{Item, UseDecl, UseItem, attrs_allow, inner_attrs_allow, lint};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::defs::{DefId, DefKind};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::NAMESPACE_MEMBER_SEP;
use crate::resolve::Resolutions;
use crate::semantics::Semantics;
use crate::token::Span;

/// Source-level `UnusedImport` warnings, in the modules `lints` names: every
/// name a private `use` brings in that no reference site of its module
/// reaches. A re-export is used by whoever imports it, so it is never reported.
pub fn unused_import_diagnostics(sem: &Semantics, lints: Lints) -> Vec<Diagnostic> {
    let Some(resolutions) = sem.resolutions() else {
        return Vec::new();
    };
    let reported: IndexSet<ModuleSource> = lints
        .modules(&sem.modules)
        .map(|(src, _)| src.clone())
        .collect();
    let mut dispatched = dispatched_traits(sem, resolutions, &reported);
    let mut out = Vec::new();
    for src in &reported {
        let module = &sem.modules[src];
        if inner_attrs_allow(&module.inner_attributes, lint::UNUSED_IMPORTS) {
            continue;
        }
        let traits = dispatched.swap_remove(src).unwrap_or_default();
        let admitting = traits
            .iter()
            .flat_map(|&t| resolutions.imports_admitting(src, t));
        let uses = Uses {
            resolutions,
            module: src,
            used: resolutions.used_imports_in(src).chain(admitting).collect(),
        };
        for item in &module.items {
            if let Item::Use(decl) = item
                && !decl.visibility.reaches_beyond_file()
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
    /// The local names of the module's imports that answered one of its
    /// reference sites, or put in scope a trait one of its calls dispatched
    /// to: a trait's methods are candidates only where the trait is in scope.
    used: IndexSet<&'a str>,
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
                } => unused.extend(self.unused_binding(name, alias.as_ref(), *local_span)),
                // `I::{f}` binds the functions alone; `I` is a name of its own.
                UseItem::InterfaceFunctions { functions, .. } => {
                    unused.extend(functions.iter().filter_map(|f| {
                        self.unused_binding(&f.name, f.alias.as_ref(), f.local_span)
                    }));
                }
                UseItem::Namespace { name, name_span } => {
                    let reached = self.used.iter().any(|alias| {
                        alias.strip_prefix(name.as_str()).is_some_and(|rest| {
                            rest.is_empty() || rest.starts_with(NAMESPACE_MEMBER_SEP)
                        })
                    });
                    if !reached {
                        unused.push((name.clone(), *name_span));
                    }
                }
                UseItem::Wildcard => {}
            }
        }
        unused
    }

    /// The local name an import of `name` binds (its `alias` where it has
    /// one) and `span`, where it is written, when the module never refers
    /// to it.
    fn unused_binding(
        &self,
        name: &String,
        alias: Option<&String>,
        span: Span,
    ) -> Option<(String, Span)> {
        let local = alias.unwrap_or(name);
        // An import that reaches nothing has its own error, and the editor
        // lints a module that still carries one.
        let unused = self.resolutions.imported_as(self.module, local).is_some()
            && !self.used.contains(local.as_str());
        unused.then(|| (local.clone(), span))
    }
}

/// Per module of `reported`, the traits declaring or implementing a method
/// that one of its method calls dispatched to.
fn dispatched_traits(
    sem: &Semantics,
    resolutions: &Resolutions,
    reported: &IndexSet<ModuleSource>,
) -> IndexMap<ModuleSource, IndexSet<DefId>> {
    let defs = resolutions.defs();
    let mut implemented: IndexMap<DefId, Option<DefId>> = IndexMap::default();
    let mut out: IndexMap<ModuleSource, IndexSet<DefId>> = IndexMap::default();
    for (site, target) in sem.iter_references() {
        let Some(module) = sem.module_of_id(site).filter(|m| reported.contains(*m)) else {
            continue;
        };
        let Some(method) = defs.of_ast_id(target) else {
            continue;
        };
        if defs.kind(method) != DefKind::Method {
            continue;
        }
        let owner = defs
            .parent(method)
            .expect("a method has the item declaring it");
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
