//! The `unused_variables` lint: a variable or parameter nothing reads, whether
//! nothing names it at all or only assignments do. See
//! `docs/spec-diagnostics.md`.

use crate::Lints;
use crate::ast::{
    AstId, AstVisitor, Attribute, Expr, Function, Pattern, SelfKind, attrs_allow,
    inner_attrs_allow, lint, walk_expr, walk_function, walk_pattern,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::symbol::SymbolKind;
use crate::token::Span;

/// Source-level `UnusedVariable` and `UnusedParameter` warnings, in the
/// modules `lints` names.
pub fn unused_variable_diagnostics(sem: &Semantics, lints: Lints) -> Vec<Diagnostic> {
    let mut binders = Vec::new();
    let mut writes = IndexSet::default();
    for (src, module) in lints.modules(&sem.modules) {
        let mut walk = Binders {
            module: src,
            waived: inner_attrs_allow(&module.inner_attributes, lint::UNUSED_VARIABLES),
            found: Vec::new(),
            shorthand: IndexSet::default(),
            writes: &mut writes,
        };
        for item in &module.items {
            walk.visit_item(item);
        }
        binders.extend(walk.found);
    }
    let mut reads = IndexSet::default();
    let mut written = IndexSet::default();
    for (site, target) in sem.iter_references() {
        if writes.contains(&site) {
            written.insert(target);
        } else {
            reads.insert(target);
        }
    }
    binders
        .into_iter()
        // A binder the elaborator did not bind as a local is a constant or a
        // case a pattern tests against, not a variable.
        .filter(|b| {
            b.ids.iter().any(|id| {
                matches!(
                    sem.symbol_at(*id).map(|sym| &sym.kind),
                    Some(SymbolKind::Variable(_))
                )
            })
        })
        .filter(|b| !b.ids.iter().any(|id| reads.contains(id)))
        .map(|b| {
            let only_written = b.ids.iter().any(|id| written.contains(id));
            b.diagnostic(only_written)
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BinderKind {
    Parameter,
    Variable,
}

/// One name a binder introduces. An or-pattern binds each name once per
/// alternative, and a use reaches only one of them, so they are one binder.
struct Binder<'a> {
    module: &'a ModuleSource,
    kind: BinderKind,
    name: &'a str,
    ids: Vec<AstId>,
    span: Span,
    /// Bound by a struct pattern's `{ name }`, where `_name` would name
    /// another field.
    shorthand: bool,
}

impl Binder<'_> {
    fn diagnostic(&self, only_written: bool) -> Diagnostic {
        let (code, what) = match self.kind {
            BinderKind::Parameter => (Code::UnusedParameter, "parameter"),
            BinderKind::Variable => (Code::UnusedVariable, "variable"),
        };
        let name = self.name;
        let fate = if only_written {
            "is assigned to, but never read"
        } else {
            "is never used"
        };
        let fix = if self.shorthand {
            format!("write `{name}: _`")
        } else {
            format!("name it `_{name}`")
        };
        Diagnostic {
            severity: Severity::Warning,
            code,
            message: format!("{what} `{name}` {fate}; {fix} if that is deliberate"),
            span: Some(DiagnosticSpan::from_span(
                &self.span,
                Some(self.module.source_path().as_str()),
            )),
        }
    }
}

/// The binders of one module, and the sites where it assigns to a name.
struct Binders<'a, 'w> {
    module: &'a ModuleSource,
    waived: bool,
    found: Vec<Binder<'a>>,
    /// The pattern ids a struct pattern binds by its field's own name.
    shorthand: IndexSet<AstId>,
    /// Every module's: the sites written to by `=` or a compound assignment,
    /// which read nothing back.
    writes: &'w mut IndexSet<AstId>,
}

impl<'a> Binders<'a, '_> {
    fn bind(&mut self, kind: BinderKind, id: AstId, name: &'a str, span: Span) {
        if self.waived || name.starts_with('_') {
            return;
        }
        self.found.push(Binder {
            module: self.module,
            kind,
            name,
            ids: vec![id],
            span,
            shorthand: self.shorthand.contains(&id),
        });
    }
}

impl<'a> AstVisitor<'a> for Binders<'a, '_> {
    fn visit_attributed(&mut self, attrs: &'a [Attribute], body: impl FnOnce(&mut Self)) {
        let saved = self.waived;
        self.waived |= attrs_allow(attrs, lint::UNUSED_VARIABLES);
        body(self);
        self.waived = saved;
    }

    /// A parameter of a function without a body has nothing to read it.
    fn visit_function(&mut self, func: &'a Function) {
        if func.body.is_some() {
            for param in &func.params {
                if param.self_kind == SelfKind::None {
                    self.visit_attributed(&param.attrs, |s| {
                        s.bind(
                            BinderKind::Parameter,
                            param.id,
                            &param.name,
                            param.name_span,
                        );
                    });
                }
            }
        }
        walk_function(self, func);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Assign(assign) => {
                for place in assign.places() {
                    if let Expr::Ident(ident) = place {
                        self.writes.insert(ident.id);
                    }
                }
            }
            Expr::CompoundAssign(assign) => {
                if let Expr::Ident(ident) = &assign.target {
                    self.writes.insert(ident.id);
                }
            }
            Expr::Closure(closure) => {
                for param in &closure.params {
                    self.visit_attributed(&param.attrs, |s| {
                        s.bind(
                            BinderKind::Parameter,
                            param.id,
                            &param.name,
                            param.name_span,
                        );
                    });
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }

    fn visit_pattern(&mut self, pat: &'a Pattern) {
        match pat {
            Pattern::Ident { id, name, span } | Pattern::MutIdent { id, name, span } => {
                self.bind(BinderKind::Variable, *id, name, *span);
            }
            Pattern::Struct { fields, .. } => {
                for field in fields {
                    if let Some(bound) = field.pattern.as_name()
                        && bound.name == field.field_name
                    {
                        self.shorthand.insert(bound.id);
                    }
                }
            }
            Pattern::Or(_) => {
                let start = self.found.len();
                walk_pattern(self, pat);
                let mut merged: IndexMap<&str, Binder<'a>> = IndexMap::default();
                for binder in self.found.split_off(start) {
                    match merged.get_mut(binder.name) {
                        Some(first) => first.ids.extend(binder.ids),
                        None => {
                            merged.insert(binder.name, binder);
                        }
                    }
                }
                self.found.extend(merged.into_values());
                return;
            }
            _ => {}
        }
        walk_pattern(self, pat);
    }
}
