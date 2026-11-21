//! The `binding_free_let_else` lint: a `let … else` whose pattern binds no
//! name only tests its value, which an `if` says plainly. See
//! `docs/spec-diagnostics.md`.

use crate::Lints;
use crate::ast::{
    AstVisitor, Attribute, Pattern, Stmt, attrs_allow, for_each_pattern_name, inner_attrs_allow,
    lint, walk_stmt,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::semantics::Semantics;
use crate::token::Span;
use crate::unparse::{unparse_pattern_source, unparse_type_into};

pub fn binding_free_let_else_diagnostics(sem: &Semantics, lints: Lints) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in lints.modules(&sem.modules) {
        if inner_attrs_allow(&module.inner_attributes, lint::BINDING_FREE_LET_ELSE) {
            continue;
        }
        let mut walk = Tests(Vec::new());
        for item in &module.items {
            walk.visit_item(item);
        }
        out.extend(walk.0.into_iter().map(|(pattern, span)| Diagnostic {
            severity: Severity::Warning,
            code: Code::BindingFreeLetElse,
            message: format!(
                "`let {pattern} = … else` binds no name; write \
                 `if !(… matches {{ {pattern} }}) {{ … }}`, or \
                 `#[allow(binding_free_let_else)]` waives this",
            ),
            span: Some(DiagnosticSpan::from_span(
                &span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

/// The binding-free `let … else` statements of one module outside the code
/// that waives the lint.
struct Tests(Vec<(String, Span)>);

impl AstVisitor<'_> for Tests {
    fn visit_attributed(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        if !attrs_allow(attrs, lint::BINDING_FREE_LET_ELSE) {
            body(self);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Let(l) = stmt
            && l.else_block.is_some()
            && !attrs_allow(&l.attrs, lint::BINDING_FREE_LET_ELSE)
            && !binds_a_name(&l.pattern)
        {
            let mut written = unparse_pattern_source(&l.pattern);
            if let Some(ty) = &l.ty {
                written.push_str(": ");
                unparse_type_into(ty, &mut written);
            }
            self.0.push((written, l.span));
        }
        walk_stmt(self, stmt);
    }
}

fn binds_a_name(pattern: &Pattern) -> bool {
    let mut binds = false;
    for_each_pattern_name(pattern, &mut |_, _| binds = true);
    binds
}
