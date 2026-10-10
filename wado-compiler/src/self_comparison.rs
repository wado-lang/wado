//! The `self_comparison` lint: a comparison of an expression with itself, which
//! reflexivity answers the same way on every type. See
//! `docs/wep-2026-09-23-comparison-traits.md`.

use std::cell::OnceCell;
use std::mem;

use crate::ast::{
    AstVisitor, Attribute, BinaryOp, Expr, Literal, SelfKind, UnaryOp, attrs_allow,
    inner_attrs_allow, lint, walk_expr,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::effect_check::EffectProbe;
use crate::elaborator::liveness::is_user_authored;
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::token::Span;
use crate::unparse::{binary_op_str, unparse_expr_source};

/// Source-level `SelfComparison` warnings: every comparison, a chain's adjacent
/// pairs included, whose two operands are one expression that evaluates the
/// same way twice.
pub fn self_comparison_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let probe = OnceCell::new();
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !is_user_authored(src)
            || inner_attrs_allow(&module.inner_attributes, lint::SELF_COMPARISON)
        {
            continue;
        }
        let mut comparisons = SelfComparisons {
            sem,
            probe: &probe,
            module: src,
            found: Vec::new(),
        };
        for item in &module.items {
            comparisons.visit_item(item);
        }
        out.extend(comparisons.found.into_iter().map(|found| Diagnostic {
            severity: Severity::Warning,
            code: Code::SelfComparison,
            message: found.message(),
            span: Some(DiagnosticSpan::from_span(
                &found.span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

/// One comparison of an expression with itself.
struct SelfComparison {
    span: Span,
    operand: String,
    op: BinaryOp,
    is_float: bool,
}

impl SelfComparison {
    fn message(&self) -> String {
        let answer = matches!(self.op, BinaryOp::Eq | BinaryOp::LtEq | BinaryOp::GtEq);
        let nan = if self.is_float {
            "; test a NaN with `is_nan()`"
        } else {
            ""
        };
        format!(
            "`{0} {1} {0}` compares a value with itself, so it is always {answer}{nan}; \
             `#[allow(self_comparison)]` waives this",
            self.operand,
            binary_op_str(self.op),
        )
    }
}

/// The comparisons of one module outside the code that waives the lint.
struct SelfComparisons<'a> {
    sem: &'a Semantics,
    /// Built at the first candidate, since most programs have none. `None`
    /// where the elaborator left no state to answer from.
    probe: &'a OnceCell<Option<EffectProbe<'a>>>,
    module: &'a ModuleSource,
    found: Vec<SelfComparison>,
}

impl SelfComparisons<'_> {
    fn check(&mut self, left: &Expr, op: BinaryOp, right: &Expr) {
        // Unparsing inverts the parse, so two operands of different kinds
        // never unparse alike.
        if mem::discriminant(left) != mem::discriminant(right) {
            return;
        }
        let operand = unparse_expr_source(left);
        // `#line` unparses alike wherever it is written, and is its own line.
        if operand != unparse_expr_source(right)
            || line_literals(left) != line_literals(right)
            || self.may_differ(left)
        {
            return;
        }
        let is_float = self.sem.expression_type(left.id()).is_some_and(|ty| {
            let ty = self.sem.types.peel_refs(ty);
            self.sem.types.is_float(ty) || self.sem.types.is_half(ty)
        });
        self.found.push(SelfComparison {
            span: left.span().merge(&right.span()),
            operand,
            op,
            is_float,
        });
    }

    /// Whether evaluating `expr` twice may give two answers: it performs an
    /// effect, writes through a `&mut`, or calls what cannot be seen into.
    fn may_differ(&self, expr: &Expr) -> bool {
        let mut writes = Writes {
            sem: self.sem,
            found: false,
        };
        writes.visit_expr(expr);
        writes.found
            || self
                .probe
                .get_or_init(|| EffectProbe::new(self.sem))
                .as_ref()
                .is_none_or(|probe| probe.performs_effect(self.module, expr))
    }
}

impl AstVisitor for SelfComparisons<'_> {
    fn visit_attributed(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        if !attrs_allow(attrs, lint::SELF_COMPARISON) {
            body(self);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Binary(binary) if binary.op.is_comparison() => {
                self.check(&binary.left, binary.op, &binary.right);
            }
            Expr::ComparisonChain(chain) => {
                let mut left = &chain.first;
                for comparison in &chain.comparisons {
                    self.check(left, comparison.op, &comparison.right);
                    left = &comparison.right;
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// The line of each `#line` in `expr`, in source order.
fn line_literals(expr: &Expr) -> Vec<usize> {
    struct Lines(Vec<usize>);
    impl AstVisitor for Lines {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Literal(lit) = expr
                && matches!(lit.value, Literal::LocationLine)
            {
                self.0.push(lit.span.line);
            }
            walk_expr(self, expr);
        }
    }
    let mut lines = Lines(Vec::new());
    lines.visit_expr(expr);
    lines.0
}

/// Finds what may change state between two evaluations without performing an
/// effect: an assignment, a `&mut` borrow, a method taking `&mut self`, a call
/// handed a value reaching a `&mut`, and a call of anything but a declared
/// function, such as a `fn mut` closure.
struct Writes<'a> {
    sem: &'a Semantics,
    found: bool,
}

impl Writes<'_> {
    fn reaches_mut_ref<'e>(&self, operands: impl IntoIterator<Item = &'e Expr>) -> bool {
        operands.into_iter().any(|operand| {
            self.sem
                .expression_type(operand.id())
                .is_none_or(|ty| self.sem.reaches_mut_ref(ty))
        })
    }
}

impl AstVisitor for Writes<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        self.found |= match expr {
            Expr::Assign(_) | Expr::CompoundAssign(_) => true,
            Expr::Unary(unary) => unary.op == UnaryOp::MutRef,
            Expr::MethodCall(call) => {
                let mut dispatches = self.sem.method_dispatches_at(call.id).peekable();
                dispatches.peek().is_none()
                    || self.reaches_mut_ref(std::iter::once(&call.receiver).chain(&call.args))
                    || dispatches.any(|dispatch| dispatch.self_kind == SelfKind::MutRef)
            }
            Expr::Call(call) => {
                self.reaches_mut_ref(&call.args)
                    || match &call.callee {
                        Expr::Ident(ident) => self
                            .sem
                            .referenced_symbol(ident.id)
                            .and_then(|def| self.sem.function_at(def))
                            .is_none(),
                        _ => true,
                    }
            }
            _ => false,
        };
        walk_expr(self, expr);
    }
}
