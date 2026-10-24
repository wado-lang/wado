//! The `float_literal_pattern` lint: a pattern naming one float a rounding step
//! can miss. The elaborator judges which patterns those are, since that takes
//! the scrutinee's type; this walk reports them where no `allow` waives it. See
//! `docs/spec-diagnostics.md`.

use crate::Lints;
use crate::ast::{
    AstId, AstVisitor, Attribute, Condition, ConditionElement, Stmt, attrs_allow,
    inner_attrs_allow, lint, walk_condition, walk_stmt,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::{IndexMap, IndexSet};
use crate::semantics::Semantics;
use crate::token::Span;

/// Source-level `FloatLiteralPattern` warnings, one per pattern the elaborator
/// noted.
pub fn float_literal_pattern_diagnostics(sem: &Semantics, lints: Lints) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, noted) in sem.inexact_float_patterns() {
        let module = &sem.modules[&src];
        if !lints.reports_in(&src, module) {
            continue;
        }
        let mut walk = Sites {
            noted,
            waived: inner_attrs_allow(&module.inner_attributes, lint::FLOAT_LITERAL_PATTERN),
            found: Vec::new(),
        };
        for item in &module.items {
            walk.visit_item(item);
        }
        assert!(
            walk.noted.is_empty(),
            "every noted pattern sits at a site the walk reaches: {:?}",
            walk.noted.keys().collect::<Vec<_>>()
        );
        out.extend(walk.found.into_iter().map(|(shown, span)| Diagnostic {
            severity: Severity::Warning,
            code: Code::FloatLiteralPattern,
            message: format!(
                "`{shown}` matches one exact float, which a rounding step can miss; \
                 match a range instead; `#[allow(float_literal_pattern)]` waives this"
            ),
            span: Some(DiagnosticSpan::from_span(
                &span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

/// The arms and statements of one module holding the noted patterns, keyed by
/// their spans, and the ones reported so far.
struct Sites {
    noted: IndexMap<Span, IndexSet<String>>,
    waived: bool,
    found: Vec<(String, Span)>,
}

impl Sites {
    /// Take what was noted at the site `span`, reporting it unless waived.
    fn reach(&mut self, span: Span) {
        let Some(shown) = self.noted.shift_remove(&span) else {
            return;
        };
        if !self.waived {
            self.found
                .extend(shown.into_iter().map(|shown| (shown, span)));
        }
    }

    fn waiving(&mut self, waives: bool, body: impl FnOnce(&mut Self)) {
        let saved = self.waived;
        self.waived |= waives;
        body(self);
        self.waived = saved;
    }
}

impl AstVisitor<'_> for Sites {
    fn visit_attributed(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        self.waiving(attrs_allow(attrs, lint::FLOAT_LITERAL_PATTERN), body);
    }

    /// A match arm, a `matches` and a `let … else` are each reached by their id.
    fn visit_id(&mut self, _id: AstId, span: Span) {
        self.reach(span);
    }

    /// A `let`'s own attributes waive its pattern, which the walk reaches
    /// before them.
    fn visit_stmt(&mut self, stmt: &Stmt) {
        let waives =
            matches!(stmt, Stmt::Let(s) if attrs_allow(&s.attrs, lint::FLOAT_LITERAL_PATTERN));
        self.waiving(waives, |walk| walk_stmt(walk, stmt));
    }

    fn visit_condition(&mut self, cond: &Condition) {
        if let Condition::LetChain { elements, .. } = cond {
            for element in elements {
                if let ConditionElement::Let { span, .. } = element {
                    self.reach(*span);
                }
            }
        }
        walk_condition(self, cond);
    }
}
