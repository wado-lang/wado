//! The `hex_suffix_lookalike` lint: a hex literal ending in `_f16`, `_bf16`,
//! `_f32` or `_f64`, whose last digits read as a float type. See
//! `docs/spec-diagnostics.md`.

use crate::ast::{
    AstVisitor, Attribute, Expr, Literal, attrs_allow, inner_attrs_allow, lint, walk_expr,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::elaborator::liveness::is_user_authored;
use crate::elaborator::util::is_hex_literal;
use crate::semantics::Semantics;
use crate::token::Span;

/// The float type names whose spelling is also hex digits.
const LOOKALIKES: [&str; 4] = ["f16", "bf16", "f32", "f64"];

/// Source-level `HexSuffixLookalike` warnings: every hex literal expression
/// whose last `_`-separated group spells a float type.
pub fn hex_suffix_lookalike_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !is_user_authored(src)
            || inner_attrs_allow(&module.inner_attributes, lint::HEX_SUFFIX_LOOKALIKE)
        {
            continue;
        }
        let mut walk = Lookalikes(Vec::new());
        for item in &module.items {
            walk.visit_item(item);
        }
        out.extend(walk.0.into_iter().map(|(repr, span)| Diagnostic {
            severity: Severity::Warning,
            code: Code::HexSuffixLookalike,
            message: format!(
                "`{repr}` is the hex integer `{}`, not a float; \
                 `#[allow(hex_suffix_lookalike)]` waives this",
                repr.replace('_', ""),
            ),
            span: Some(DiagnosticSpan::from_span(
                &span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

/// The lookalike literals of one module outside the code that waives the lint.
struct Lookalikes(Vec<(String, Span)>);

impl AstVisitor for Lookalikes {
    fn visit_attributed(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        if !attrs_allow(attrs, lint::HEX_SUFFIX_LOOKALIKE) {
            body(self);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Literal(lit) = expr
            && let Literal::Number(repr) = &lit.value
            && is_lookalike(repr)
        {
            self.0.push((repr.clone(), lit.span));
        }
        walk_expr(self, expr);
    }
}

fn is_lookalike(repr: &str) -> bool {
    is_hex_literal(repr)
        && repr.rsplit_once('_').is_some_and(|(_, last)| {
            LOOKALIKES
                .iter()
                .any(|name| last.eq_ignore_ascii_case(name))
        })
}
