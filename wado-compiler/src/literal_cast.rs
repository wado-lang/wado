//! The `literal_cast` lint: a cast that types a numeric literal, which a
//! suffix writes. See `docs/wep-2026-10-01-numeric-literal-suffixes.md`.

use crate::ast::{
    AstVisitor, Attribute, CastExpr, Expr, Literal, NumericSuffix, Type, UnaryOp, attrs_allow,
    inner_attrs_allow, lint, spell_number, walk_expr,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::elaborator::liveness::is_user_authored;
use crate::elaborator::util::is_float_only_literal;
use crate::semantics::Semantics;
use crate::token::Span;

/// Source-level `LiteralCast` warnings: every `lit as T` whose literal a
/// suffix types the same way, `255 as u8` for `255_u8`.
pub fn literal_cast_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !is_user_authored(src) || inner_attrs_allow(&module.inner_attributes, lint::LITERAL_CAST)
        {
            continue;
        }
        let mut casts = LiteralCasts::default();
        for item in &module.items {
            casts.visit_item(item);
        }
        out.extend(casts.found.into_iter().map(|found| Diagnostic {
            severity: Severity::Warning,
            code: Code::LiteralCast,
            message: format!(
                "write `{}` for `{}`: the suffix types the literal as the cast does; \
                 `#[allow(literal_cast)]` waives this",
                found.suffixed, found.written
            ),
            span: Some(DiagnosticSpan::from_span(
                &found.span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

/// A cast as written, and as its suffix writes it.
struct LiteralCast {
    span: Span,
    written: String,
    suffixed: String,
}

/// The casts outside the code that waives the lint.
#[derive(Default)]
struct LiteralCasts {
    found: Vec<LiteralCast>,
}

impl AstVisitor for LiteralCasts {
    fn visit_attributed(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        if !attrs_allow(attrs, lint::LITERAL_CAST) {
            body(self);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Cast(cast) = expr
            && let Some(found) = suffixed_spelling(cast)
        {
            self.found.push(found);
        }
        walk_expr(self, expr);
    }
}

/// `cast` with its suffixed spelling, where its operand is an unsuffixed
/// literal that the suffix of its target types as the cast does. A float
/// literal cast to an integer converts rather than types, so it has none.
fn suffixed_spelling(cast: &CastExpr) -> Option<LiteralCast> {
    let (sign, literal) = match &cast.expr {
        Expr::Literal(literal) => ("", literal),
        Expr::Unary(unary) if unary.op == UnaryOp::Neg => match &unary.expr {
            Expr::Literal(literal) => ("-", literal),
            _ => return None,
        },
        _ => return None,
    };
    let Literal::Number(repr, None) = &literal.value else {
        return None;
    };
    let Type::Named(target) = &cast.target_type else {
        return None;
    };
    let suffix = NumericSuffix::from_name(&target.name)?;
    if !suffix.suits(repr) || (!suffix.is_float() && is_float_only_literal(repr)) {
        return None;
    }
    let digits = format!("{sign}{repr}");
    Some(LiteralCast {
        span: cast.span,
        written: format!("{digits} as {}", target.name),
        suffixed: spell_number(&digits, Some(suffix)),
    })
}
