//! The `unused_variables` lint: a variable or parameter nothing reads, whether
//! nothing names it at all or only assignments do. See
//! `docs/spec-diagnostics.md`.

use crate::Lints;
use crate::ast::{
    AstId, AstVisitor, Attribute, Expr, Function, Item, Pattern, SelfKind, attrs_allow,
    inner_attrs_allow, lint, walk_expr, walk_function, walk_item, walk_pattern,
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
            sem,
            module: src,
            waived: inner_attrs_allow(&module.inner_attributes, lint::UNUSED_VARIABLES),
            signatures_imposed: false,
            found: Vec::new(),
            shorthand: IndexSet::default(),
            writes: IndexSet::default(),
        };
        for item in &module.items {
            walk.visit_item(item);
        }
        binders.extend(walk.found);
        writes.extend(walk.writes);
    }
    // A binder the elaborator did not bind as a local is a constant or a case
    // a pattern tests against, not a variable.
    binders.retain(|b| {
        b.ids.iter().any(|id| {
            matches!(
                sem.symbol_at(*id).map(|sym| &sym.kind),
                Some(SymbolKind::Variable(_))
            )
        })
    });
    let candidates: IndexSet<AstId> = binders.iter().flat_map(|b| b.ids.iter().copied()).collect();
    let mut reads = IndexSet::default();
    let mut written = IndexSet::default();
    for (site, target) in sem.iter_references() {
        if !candidates.contains(&target) {
            continue;
        }
        if writes.contains(&site) {
            written.insert(target);
        } else {
            reads.insert(target);
        }
    }
    binders
        .into_iter()
        .filter(|b| !b.ids.iter().any(|id| reads.contains(id)))
        .map(|b| {
            let only_written = b.ids.iter().any(|id| written.contains(id));
            b.diagnostic(only_written)
        })
        .collect()
}

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

/// The binders of one module, and the names it assigns to.
struct Binders<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    waived: bool,
    /// Inside a trait, an interface, or an implementation of one. Each names
    /// the parameters of every implementation, so one leaving a parameter
    /// unread is no defect.
    signatures_imposed: bool,
    found: Vec<Binder<'a>>,
    /// The pattern ids a struct pattern binds by its field's own name.
    shorthand: IndexSet<AstId>,
    /// The names an assignment writes into, by `=` or a compound assignment,
    /// directly or through the fields and indexes of a value they hold. None
    /// reads anything back.
    writes: IndexSet<AstId>,
}

impl<'a> Binders<'a> {
    fn bind_parameter(&mut self, attrs: &'a [Attribute], id: AstId, name: &'a str, span: Span) {
        self.visit_attributed(attrs, |s| s.bind(BinderKind::Parameter, id, name, span));
    }

    /// Records the name a write to `place` lands in. Through a projection, that
    /// name must hold nothing that writes elsewhere: an index assignment on a
    /// value holding a `&mut` may write through it.
    fn write(&mut self, place: &Expr) {
        let written = self.sem.written_place(place);
        if let Expr::Ident(ident) = written
            && (matches!(place, Expr::Ident(_)) || !self.sem.expr_reaches_mut_ref(written))
        {
            self.writes.insert(ident.id);
        }
    }

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

impl<'a> AstVisitor<'a> for Binders<'a> {
    fn visit_attributed(&mut self, attrs: &'a [Attribute], body: impl FnOnce(&mut Self)) {
        let saved = self.waived;
        self.waived |= attrs_allow(attrs, lint::UNUSED_VARIABLES);
        body(self);
        self.waived = saved;
    }

    fn visit_item(&mut self, item: &'a Item) {
        let saved = self.signatures_imposed;
        self.signatures_imposed = match item {
            Item::Impl(i) => i.trait_type.is_some(),
            Item::Trait(_) | Item::Interface(_) => true,
            Item::Use(_)
            | Item::Function(_)
            | Item::Struct(_)
            | Item::Enum(_)
            | Item::Variant(_)
            | Item::Flags(_)
            | Item::Newtype(_)
            | Item::TupleTypeDecl(_)
            | Item::BuiltinTypeDecl(_)
            | Item::Resource(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Global(_)
            | Item::Error(_) => false,
        };
        walk_item(self, item);
        self.signatures_imposed = saved;
    }

    /// A parameter of a function without a body has nothing to read it.
    fn visit_function(&mut self, func: &'a Function) {
        if func.body.is_some() && !self.signatures_imposed {
            for param in &func.params {
                if param.self_kind == SelfKind::None {
                    self.bind_parameter(&param.attrs, param.id, &param.name, param.name_span);
                }
            }
        }
        walk_function(self, func);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        for place in expr.assigned_places() {
            self.write(place);
        }
        if let Expr::Closure(closure) = expr {
            for param in &closure.params {
                self.bind_parameter(&param.attrs, param.id, &param.name, param.name_span);
            }
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
