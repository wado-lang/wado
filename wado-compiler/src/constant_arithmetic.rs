//! The `arithmetic_overflow` and `unconditional_trap` lints: integer arithmetic
//! whose literal operands already say it wraps or traps. See
//! `docs/spec-diagnostics.md`.

use crate::ast::{
    AstVisitor, Attribute, BinaryExpr, BinaryOp, Expr, Function, Item, Literal, UnaryOp,
    attrs_allow, inner_attrs_allow, lint, walk_expr, walk_function, walk_item,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::const_eval::{int_bit_width, is_signed_int};
use crate::elaborator::liveness::is_user_authored;
use crate::elaborator::util::{parse_i128_literal, parse_u128_literal};
use crate::semantics::Semantics;
use crate::tir::TypeId;
use crate::token::Span;
use crate::unparse::unparse_expr_source;

/// Source-level `ArithmeticOverflow` and `UnconditionalTrap` warnings.
pub fn constant_arithmetic_diagnostics(sem: &Semantics) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !is_user_authored(src) {
            continue;
        }
        let waived = |name| inner_attrs_allow(&module.inner_attributes, name);
        let mut walk = ConstantArithmetic {
            sem,
            overflow: !waived(lint::ARITHMETIC_OVERFLOW),
            trap: !waived(lint::UNCONDITIONAL_TRAP),
            found: Vec::new(),
        };
        for item in &module.items {
            walk.visit_item(item);
        }
        out.extend(walk.found.into_iter().map(|found| Diagnostic {
            severity: Severity::Warning,
            code: found.code,
            message: found.message,
            span: Some(DiagnosticSpan::from_span(
                &found.span,
                Some(src.source_path().as_str()),
            )),
        }));
    }
    out
}

struct Finding {
    code: Code,
    message: String,
    span: Span,
}

/// An integer type as the lints read it: its width and whether it is signed.
#[derive(Clone, Copy)]
struct IntType {
    bits: u32,
    signed: bool,
}

impl IntType {
    fn of(sem: &Semantics, ty: TypeId) -> Option<Self> {
        if let Some(prim) = sem.types.primitive_head(ty) {
            return sem.types.is_integer(ty).then(|| IntType {
                bits: int_bit_width(prim),
                signed: is_signed_int(prim),
            });
        }
        sem.types.wide_int_item(ty).map(|item| IntType {
            bits: 128,
            signed: item == CompilerItem::I128,
        })
    }

    fn min(self) -> i128 {
        if !self.signed {
            0
        } else if self.bits == 128 {
            i128::MIN
        } else {
            -(1_i128 << (self.bits - 1))
        }
    }

    /// Whether `value` lies in the type's range.
    fn holds(self, value: Value) -> bool {
        match value {
            Value::Negative(v) => v >= self.min(),
            Value::NonNegative(v) => {
                let width = if self.signed {
                    self.bits - 1
                } else {
                    self.bits
                };
                width == 128 || v >> width == 0
            }
        }
    }

    /// `value` wrapped into the type's range, as the operator computes it.
    fn wrap(self, value: Value) -> Value {
        let bits = value.bits();
        let low = if self.bits == 128 {
            bits
        } else {
            bits & ((1_u128 << self.bits) - 1)
        };
        if self.signed && low >> (self.bits - 1) & 1 == 1 {
            let shift = 128 - self.bits;
            Value::Negative(((low << shift) as i128) >> shift)
        } else {
            Value::NonNegative(low)
        }
    }
}

/// An integer's exact value. Every operand of a well-typed operator fits in
/// one of the two halves, and so does each exact result the lints compute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    Negative(i128),
    NonNegative(u128),
}

impl Value {
    fn from_i128(v: i128) -> Self {
        u128::try_from(v).map_or(Value::Negative(v), Value::NonNegative)
    }

    fn as_i128(self) -> Option<i128> {
        match self {
            Value::Negative(v) => Some(v),
            Value::NonNegative(v) => i128::try_from(v).ok(),
        }
    }

    /// The value's two's-complement bits, 128 wide.
    fn bits(self) -> u128 {
        match self {
            Value::Negative(v) => v as u128,
            Value::NonNegative(v) => v,
        }
    }

    fn is_zero(self) -> bool {
        self == Value::NonNegative(0)
    }

    fn render(self) -> String {
        match self {
            Value::Negative(v) => v.to_string(),
            Value::NonNegative(v) => v.to_string(),
        }
    }
}

/// Exact `+`, `-` or `*`; `None` where the exact result leaves both halves,
/// which no 128-bit type holds either.
fn exact(op: BinaryOp, left: Value, right: Value) -> Option<Value> {
    if let (Value::NonNegative(l), Value::NonNegative(r)) = (left, right) {
        let unsigned = match op {
            BinaryOp::Add => l.checked_add(r),
            BinaryOp::Mul => l.checked_mul(r),
            BinaryOp::Sub if l >= r => Some(l - r),
            _ => None,
        };
        if let Some(v) = unsigned {
            return Some(Value::NonNegative(v));
        }
    }
    let (l, r) = (left.as_i128()?, right.as_i128()?);
    let signed = match op {
        BinaryOp::Add => l.checked_add(r),
        BinaryOp::Sub => l.checked_sub(r),
        BinaryOp::Mul => l.checked_mul(r),
        _ => unreachable!("`{op:?}` is not exact arithmetic"),
    }?;
    Some(Value::from_i128(signed))
}

/// `left op right` wrapped to `ty`, by two's complement on the low bits.
fn wrapping(op: BinaryOp, left: Value, right: Value, ty: IntType) -> Value {
    let (l, r) = (left.bits(), right.bits());
    let low = match op {
        BinaryOp::Add => l.wrapping_add(r),
        BinaryOp::Sub => l.wrapping_sub(r),
        BinaryOp::Mul => l.wrapping_mul(r),
        _ => unreachable!("`{op:?}` does not wrap"),
    };
    ty.wrap(Value::NonNegative(low))
}

/// The lints' walk over one module. A constant subexpression is evaluated
/// once, by the outermost operator holding it, which reports the innermost
/// failure and gives the operators above it no value.
struct ConstantArithmetic<'a> {
    sem: &'a Semantics,
    overflow: bool,
    trap: bool,
    found: Vec<Finding>,
}

impl ConstantArithmetic<'_> {
    fn int_type(&self, expr: &Expr) -> Option<IntType> {
        IntType::of(self.sem, self.sem.expression_type(expr.id())?)
    }

    /// `expr`'s value where its literals alone decide it. Any other
    /// expression is walked for the operators inside it.
    fn value(&mut self, expr: &Expr) -> Option<Value> {
        match expr {
            Expr::Literal(literal) => {
                let Literal::Number(repr, _) = &literal.value else {
                    return None;
                };
                let ty = self.int_type(expr)?;
                let value = parse_u128_literal(repr).ok().map(Value::NonNegative)?;
                ty.holds(value).then_some(value)
            }
            Expr::Unary(unary) if unary.op == UnaryOp::Neg => {
                if let Expr::Literal(literal) = &unary.expr
                    && let Literal::Number(repr, _) = &literal.value
                {
                    // `-128_i8` is one literal, never `-(128_i8)`.
                    let ty = self.int_type(expr)?;
                    let value = Value::from_i128(parse_i128_literal(&format!("-{repr}")).ok()?);
                    return ty.holds(value).then_some(value);
                }
                let operand = self.value(&unary.expr);
                let ty = self.int_type(expr)?;
                let negated = exact(BinaryOp::Sub, Value::NonNegative(0), operand?)?;
                if ty.holds(negated) {
                    return Some(negated);
                }
                self.report_overflow(expr, ty, ty.wrap(negated));
                None
            }
            Expr::Binary(binary) => self.binary(expr, binary),
            _ => {
                walk_expr(self, expr);
                None
            }
        }
    }

    fn binary(&mut self, expr: &Expr, binary: &BinaryExpr) -> Option<Value> {
        let left = self.value(&binary.left);
        let right = self.value(&binary.right);
        let ty = self.int_type(expr)?;
        match binary.op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
                let (l, r) = (left?, right?);
                let result = exact(binary.op, l, r).filter(|v| ty.holds(*v));
                if result.is_none() {
                    self.report_overflow(expr, ty, wrapping(binary.op, l, r, ty));
                }
                result
            }
            BinaryOp::Div | BinaryOp::Mod => {
                if right.is_some_and(Value::is_zero) {
                    self.report_trap(expr, "divides by zero".to_string());
                    return None;
                }
                let (l, r) = (left?.as_i128(), right?.as_i128());
                if binary.op == BinaryOp::Div && ty.signed && l == Some(ty.min()) && r == Some(-1) {
                    let name = self.type_name(expr);
                    self.report_trap(expr, format!("overflows `{name}`"));
                }
                None
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let amount = right?;
                let width = u128::from(ty.bits);
                if matches!(amount, Value::NonNegative(n) if n < width) {
                    return None;
                }
                // The width is a power of two, so the low bits are the residue.
                let message = format!(
                    "`{}` shifts by {}, which `{}` takes modulo {width} as {}",
                    unparse_expr_source(expr),
                    amount.render(),
                    self.type_name(expr),
                    amount.bits() % width,
                );
                self.report_overflow_message(expr, message);
                None
            }
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => None,
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => unreachable!(
                "`{:?}` answers a `bool`, which `int_type` refused",
                binary.op
            ),
        }
    }

    /// The name of `expr`'s type, which `int_type` has already read.
    fn type_name(&self, expr: &Expr) -> String {
        let ty = self
            .sem
            .expression_type(expr.id())
            .expect("an operation the lints report has an integer type");
        self.sem.types.type_name(ty)
    }

    fn report_overflow(&mut self, expr: &Expr, ty: IntType, wrapped: Value) {
        assert!(
            ty.holds(wrapped),
            "a wrapped value lies in its type's range"
        );
        let message = format!(
            "`{}` overflows `{}`, so it wraps to {}",
            unparse_expr_source(expr),
            self.type_name(expr),
            wrapped.render(),
        );
        self.report_overflow_message(expr, message);
    }

    fn report_overflow_message(&mut self, expr: &Expr, message: String) {
        if self.overflow {
            self.found.push(Finding {
                code: Code::ArithmeticOverflow,
                message: format!("{message}; `#[allow(arithmetic_overflow)]` waives this"),
                span: expr.span(),
            });
        }
    }

    fn report_trap(&mut self, expr: &Expr, why: String) {
        if self.trap {
            self.found.push(Finding {
                code: Code::UnconditionalTrap,
                message: format!(
                    "`{}` {why}, so it always traps; `#[allow(unconditional_trap)]` waives this",
                    unparse_expr_source(expr),
                ),
                span: expr.span(),
            });
        }
    }

    /// Run `body` with each lint the attributes `#[allow]` turned off.
    fn waiving(&mut self, attrs: &[Attribute], body: impl FnOnce(&mut Self)) {
        let saved = (self.overflow, self.trap);
        self.overflow &= !attrs_allow(attrs, lint::ARITHMETIC_OVERFLOW);
        self.trap &= !attrs_allow(attrs, lint::UNCONDITIONAL_TRAP);
        if self.overflow || self.trap {
            body(self);
        }
        (self.overflow, self.trap) = saved;
    }
}

impl AstVisitor for ConstantArithmetic<'_> {
    fn visit_item(&mut self, item: &Item) {
        self.waiving(item.attrs(), |walk| walk_item(walk, item));
    }

    fn visit_function(&mut self, func: &Function) {
        self.waiving(&func.attrs, |walk| walk_function(walk, func));
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Binary(_) | Expr::Unary(_) => {
                self.value(expr);
            }
            _ => walk_expr(self, expr),
        }
    }
}
