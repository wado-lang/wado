//! TIR Interpreter (tiri).
//!
//! Compile-time partial evaluator for Wado TIR. The public entry point is
//! [`Interpreter::reduce`], which takes a [`TirExpr`] and returns the most
//! reduced form possible (a literal node when the expression is fully
//! known, the original tree otherwise). Constant folding is the first
//! consumer; future passes (branch pruning, constant propagation,
//! compile-time function evaluation) will reuse the same engine.
//!
//! ```text
//! Interpreter::new(type_table).reduce(&expr) -> TirExpr
//! ```
//!
//! `reduce` is **idempotent** — `reduce(reduce(e))` is structurally equal
//! to `reduce(e)` — and **monotone** — it only moves expressions toward
//! literal form, never the reverse. Literal leaves are preserved as-is so
//! the original lexical repr (e.g. `0xFF`) survives a no-op pass.
//!
//! Today the engine handles:
//!
//! - Integer arithmetic: Add, Sub, Mul, Div, Mod
//! - Integer comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Integer bitwise: `BitAnd`, `BitOr`, `BitXor`, Shl, Shr
//! - Integer unary: Neg, `BitNot`
//! - Integer types: i8, i16, i32, i64, u8, u16, u32, u64
//! - Integer cast: truncation/extension between integer types
//! - Float arithmetic: Add, Sub, Mul, Div (skipped when result is NaN)
//! - Float comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Float unary: Neg (via sign-bit flip, safe for all values including NaN)
//! - Float types: f32, f64
//! - Boolean logical: And, Or (including identity rules `false || X → X`,
//!   `true && X → X`, `X || false → X`, `X && true → X`)
//! - Boolean equality: Eq, `NotEq`
//! - Boolean unary: Not
//!
//! Float arithmetic uses native Rust IEEE 754 ops (same as Wasm), following
//! cranelift's approach: fold the result, but skip if it is NaN since NaN
//! bit patterns are nondeterministic across architectures.
//!
//! Integer division/modulo by zero and signed `MIN / -1` are left
//! unfolded so the runtime trap is preserved.
//!
//! See `docs/wep-2026-04-27-tir-interpreter.md` for the planned trajectory
//! (local-variable environment, `if` / `match` reduction, bounded loop
//! unrolling, pure function inlining, and a complementary wasm-CTFE
//! backend).

use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};

/// A typed compile-time value produced by the interpreter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// Integer value. `prim` carries the integer type (i8..i64, u8..u64);
    /// `value` is the raw bit pattern, sign-extended for signed types.
    Int { value: u64, prim: PrimitiveType },
    /// Floating-point value. `prim` is `F32` or `F64`. For `F32`, `value`
    /// holds the f32 result widened to f64.
    Float { value: f64, prim: PrimitiveType },
    /// Boolean value.
    Bool(bool),
}

impl Value {
    /// Returns the raw integer bit pattern, or `None` if not an int.
    #[must_use]
    pub fn as_int(&self) -> Option<(u64, PrimitiveType)> {
        match self {
            Self::Int { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the raw float value and width, or `None` if not a float.
    #[must_use]
    pub fn as_float(&self) -> Option<(f64, PrimitiveType)> {
        match self {
            Self::Float { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the boolean value, or `None` if not a bool.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Render the value as a TIR-compatible literal repr string.
    #[must_use]
    pub fn format_repr(&self) -> String {
        match self {
            Self::Int { value, prim } => format_int_repr(*value, *prim),
            Self::Float { value, .. } => format_float_repr(*value),
            Self::Bool(b) => b.to_string(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over [`TirExpr`].
///
/// Holds the type table needed to resolve operand widths. Future
/// extensions (local-variable environment, step budget for loops,
/// pure-call inlining via the `FlatPackage`) will live on this struct.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self { type_table }
    }

    /// Reduce `expr` as far as possible.
    ///
    /// Always returns a (possibly structurally-identical) [`TirExpr`].
    /// Literal leaves are preserved verbatim so their lexical repr
    /// (e.g. `0xFF`) survives a no-op pass.
    pub fn reduce(&mut self, expr: &TirExpr) -> TirExpr {
        let mut owned = expr.clone();
        self.reduce_in_place(&mut owned);
        owned
    }

    /// Recursively reduce `expr` in place over the subtree the engine
    /// currently understands (Binary / Unary / Cast). Returns `true`
    /// when anything changed.
    ///
    /// Internal: the only public entry points are [`reduce`] and
    /// [`reduce_local`]. `reduce` clones into `reduce_in_place`; visitor
    /// drivers that already walk every TIR kind via
    /// `tir_visitor::opt_walk_expr` should call `reduce_local` directly.
    ///
    /// [`reduce`]: Self::reduce
    /// [`reduce_local`]: Self::reduce_local
    fn reduce_in_place(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: recurse into children first so the local rewrite
        // step at this node sees fully-reduced operands.
        let mut changed = match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                let l = self.reduce_in_place(left);
                let r = self.reduce_in_place(right);
                l || r
            }
            TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
                self.reduce_in_place(inner)
            }
            _ => false,
        };

        changed |= self.reduce_local(expr);
        changed
    }

    /// Apply the engine's rewrite rules to `expr` only — without recursing
    /// into children. Returns `true` when `expr` was rewritten.
    ///
    /// This is the right entry point when the caller is already driving a
    /// TIR walk (for example via `tir_visitor::opt_walk_expr`) and wants
    /// to slot tiri's local rewrites into each visited node. Today the
    /// rules are constant folding for Binary / Unary / Cast and the
    /// short-circuit identity simplifications for `&&` / `||`.
    pub fn reduce_local(&mut self, expr: &mut TirExpr) -> bool {
        if let Some(folded) = self.try_fold(expr) {
            expr.kind = value_to_expr_kind(folded);
            return true;
        }
        rewrite_short_circuit(expr)
    }

    /// Convenience: reduce `expr` and, if the result is a literal, return
    /// its [`Value`]. Useful for unit-testing primitive-op semantics
    /// without inspecting [`TirExprKind`].
    pub fn reduce_to_value(&mut self, expr: &TirExpr) -> Option<Value> {
        let reduced = self.reduce(expr);
        self.literal_value(&reduced)
    }

    /// Extract a [`Value`] from a leaf literal node, if `expr` is one.
    fn literal_value(&self, expr: &TirExpr) -> Option<Value> {
        match &expr.kind {
            TirExprKind::BoolLiteral(b) => Some(Value::Bool(*b)),
            TirExprKind::IntLiteral { value, .. } => {
                let prim = int_primitive_of(expr.type_id, self.type_table)?;
                Some(Value::Int {
                    value: *value,
                    prim,
                })
            }
            TirExprKind::FloatLiteral { value, .. } => {
                let prim = if is_f32_type(expr.type_id, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Some(Value::Float {
                    value: *value,
                    prim,
                })
            }
            _ => None,
        }
    }

    /// Try to fold a Binary / Unary / Cast node whose operands are all
    /// already literals. Returns `None` for everything else (leaves,
    /// unsupported kinds, runtime traps).
    fn try_fold(&self, expr: &TirExpr) -> Option<Value> {
        match &expr.kind {
            TirExprKind::Binary { left, op, right } => {
                let l = self.literal_value(left)?;
                let r = self.literal_value(right)?;
                eval_binary(l, *op, r)
            }
            TirExprKind::Unary { op, expr: inner } => {
                let v = self.literal_value(inner)?;
                eval_unary(*op, v)
            }
            TirExprKind::Cast { expr: inner, .. } => {
                let target = int_primitive_of(expr.type_id, self.type_table)?;
                let raw = match &inner.kind {
                    TirExprKind::IntLiteral { value, .. } => *value,
                    TirExprKind::CharLiteral(c) => *c as u64,
                    _ => return None,
                };
                Some(cast_int(raw, target))
            }
            _ => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TirExpr <-> Value bridge
// ──────────────────────────────────────────────────────────────────────────────

fn value_to_expr_kind(v: Value) -> TirExprKind {
    match v {
        Value::Int { value, prim } => TirExprKind::IntLiteral {
            repr: format_int_repr(value, prim),
            value,
        },
        Value::Float { value, .. } => TirExprKind::FloatLiteral {
            repr: format_float_repr(value),
            value,
        },
        Value::Bool(b) => TirExprKind::BoolLiteral(b),
    }
}

/// Identity simplifications for short-circuit operators that *preserve*
/// every subexpression. `false || X → X`, `true && X → X`, and the RHS
/// counterparts (`X || false → X`, `X && true → X`). Returns `true`
/// when `expr` was rewritten.
///
/// The reverse direction (`true || X → true`, `false && X → false`)
/// would drop `X`. Even though Wado's `||`/`&&` short-circuit at runtime
/// — so dropping a side that wouldn't have been evaluated is
/// semantically defensible — this engine stays conservative and leaves
/// those rewrites to a future side-effect-aware pass. Mirrors the
/// previous in-visitor behaviour.
fn rewrite_short_circuit(expr: &mut TirExpr) -> bool {
    enum Pick {
        Left,
        Right,
    }
    let pick = match &expr.kind {
        TirExprKind::Binary { left, op, right } => match (&left.kind, *op, &right.kind) {
            (TirExprKind::BoolLiteral(false), TirBinaryOp::Or, _)
            | (TirExprKind::BoolLiteral(true), TirBinaryOp::And, _) => Pick::Right,
            (_, TirBinaryOp::Or, TirExprKind::BoolLiteral(false))
            | (_, TirBinaryOp::And, TirExprKind::BoolLiteral(true)) => Pick::Left,
            _ => return false,
        },
        _ => return false,
    };
    // Take ownership of the Binary by swapping its `kind` out. The
    // placeholder is local to this function and overwritten before we
    // return, so no caller observes a partially-updated `expr`.
    let TirExprKind::Binary { left, right, .. } =
        std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        unreachable!("matched Binary above");
    };
    *expr = match pick {
        Pick::Left => *left,
        Pick::Right => *right,
    };
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure value evaluation (Bool / Int / Float)
// ──────────────────────────────────────────────────────────────────────────────

/// Evaluate a binary op on two compile-time values.
fn eval_binary(left: Value, op: TirBinaryOp, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Bool(l), Value::Bool(r)) => eval_bool_binary(l, op, r),
        (Value::Float { value: l, prim: lp }, Value::Float { value: r, prim: rp }) if lp == rp => {
            eval_float_binary(l, op, r, lp)
        }
        (Value::Int { value: l, prim: lp }, Value::Int { value: r, prim: rp }) if lp == rp => {
            eval_int_binary(l, op, r, lp)
        }
        _ => None,
    }
}

/// Evaluate a unary op on a compile-time value.
fn eval_unary(op: TirUnaryOp, operand: Value) -> Option<Value> {
    match op {
        TirUnaryOp::Neg => match operand {
            Value::Int { value, prim } => {
                eval_int_neg(value, prim).map(|v| Value::Int { value: v, prim })
            }
            Value::Float { value, prim } => {
                let negated = f64::from_bits(value.to_bits() ^ (1u64 << 63));
                Some(Value::Float {
                    value: negated,
                    prim,
                })
            }
            Value::Bool(_) => None,
        },
        TirUnaryOp::Not => match operand {
            Value::Bool(b) => Some(Value::Bool(!b)),
            _ => None,
        },
        TirUnaryOp::BitNot => match operand {
            Value::Int { value, prim } => Some(Value::Int {
                value: truncate_int(!value, prim),
                prim,
            }),
            _ => None,
        },
        TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => None,
    }
}

/// Cast an integer bit pattern to a target integer primitive.
fn cast_int(value: u64, target: PrimitiveType) -> Value {
    Value::Int {
        value: truncate_int(value, target),
        prim: target,
    }
}

fn eval_bool_binary(l: bool, op: TirBinaryOp, r: bool) -> Option<Value> {
    match op {
        TirBinaryOp::And => Some(Value::Bool(l && r)),
        TirBinaryOp::Or => Some(Value::Bool(l || r)),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        _ => None,
    }
}

fn eval_int_binary(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> Option<Value> {
    match op {
        TirBinaryOp::Add => Some(Value::Int {
            value: truncate_int(lval.wrapping_add(rval), prim),
            prim,
        }),
        TirBinaryOp::Sub => Some(Value::Int {
            value: truncate_int(lval.wrapping_sub(rval), prim),
            prim,
        }),
        TirBinaryOp::Mul => Some(Value::Int {
            value: truncate_int(lval.wrapping_mul(rval), prim),
            prim,
        }),
        TirBinaryOp::Div => eval_int_div(lval, rval, prim).map(|value| Value::Int { value, prim }),
        TirBinaryOp::Mod => eval_int_mod(lval, rval, prim).map(|value| Value::Int { value, prim }),

        TirBinaryOp::Eq
        | TirBinaryOp::NotEq
        | TirBinaryOp::Lt
        | TirBinaryOp::LtEq
        | TirBinaryOp::Gt
        | TirBinaryOp::GtEq => Some(Value::Bool(eval_int_cmp(lval, op, rval, prim))),

        TirBinaryOp::BitAnd => Some(Value::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        TirBinaryOp::BitOr => Some(Value::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        TirBinaryOp::BitXor => Some(Value::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        TirBinaryOp::Shl => Some(Value::Int {
            value: eval_int_shl(lval, rval, prim),
            prim,
        }),
        TirBinaryOp::Shr => Some(Value::Int {
            value: eval_int_shr(lval, rval, prim),
            prim,
        }),

        TirBinaryOp::And | TirBinaryOp::Or | TirBinaryOp::RefEq | TirBinaryOp::RefNotEq => None,
    }
}

fn eval_int_cmp(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    if is_signed_int(prim) {
        let l = lval as i64;
        let r = rval as i64;
        match op {
            TirBinaryOp::Eq => l == r,
            TirBinaryOp::NotEq => l != r,
            TirBinaryOp::Lt => l < r,
            TirBinaryOp::LtEq => l <= r,
            TirBinaryOp::Gt => l > r,
            TirBinaryOp::GtEq => l >= r,
            _ => unreachable!(),
        }
    } else {
        match op {
            TirBinaryOp::Eq => lval == rval,
            TirBinaryOp::NotEq => lval != rval,
            TirBinaryOp::Lt => lval < rval,
            TirBinaryOp::LtEq => lval <= rval,
            TirBinaryOp::Gt => lval > rval,
            TirBinaryOp::GtEq => lval >= rval,
            _ => unreachable!(),
        }
    }
}

fn eval_int_shl(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    truncate_int(lval.wrapping_shl(shift), prim)
}

fn eval_int_shr(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    if is_signed_int(prim) {
        let result = (lval as i64).wrapping_shr(shift);
        truncate_int(result as u64, prim)
    } else {
        truncate_int(lval.wrapping_shr(shift), prim)
    }
}

fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval / rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_div(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_div(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval % rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_rem(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_rem(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_rem(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_rem(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_neg(value: u64, prim: PrimitiveType) -> Option<u64> {
    match prim {
        PrimitiveType::I8 => {
            let result = (value as i8).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (value as i16).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (value as i32).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            let result = (value as i64).wrapping_neg();
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_float_binary(lval: f64, op: TirBinaryOp, rval: f64, prim: PrimitiveType) -> Option<Value> {
    match prim {
        PrimitiveType::F32 => eval_f32_binary(lval, op, rval),
        PrimitiveType::F64 => eval_f64_binary(lval, op, rval),
        _ => None,
    }
}

fn eval_f64_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Add => non_nan_float(lval + rval, PrimitiveType::F64),
        TirBinaryOp::Sub => non_nan_float(lval - rval, PrimitiveType::F64),
        TirBinaryOp::Mul => non_nan_float(lval * rval, PrimitiveType::F64),
        TirBinaryOp::Div => non_nan_float(lval / rval, PrimitiveType::F64),
        _ => eval_float_comparison(lval, op, rval),
    }
}

fn eval_f32_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        TirBinaryOp::Add => non_nan_float(f64::from(l + r), PrimitiveType::F32),
        TirBinaryOp::Sub => non_nan_float(f64::from(l - r), PrimitiveType::F32),
        TirBinaryOp::Mul => non_nan_float(f64::from(l * r), PrimitiveType::F32),
        TirBinaryOp::Div => non_nan_float(f64::from(l / r), PrimitiveType::F32),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        TirBinaryOp::Lt => Some(Value::Bool(l < r)),
        TirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        TirBinaryOp::Gt => Some(Value::Bool(l > r)),
        TirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_float_comparison(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Eq => Some(Value::Bool(lval == rval)),
        TirBinaryOp::NotEq => Some(Value::Bool(lval != rval)),
        TirBinaryOp::Lt => Some(Value::Bool(lval < rval)),
        TirBinaryOp::LtEq => Some(Value::Bool(lval <= rval)),
        TirBinaryOp::Gt => Some(Value::Bool(lval > rval)),
        TirBinaryOp::GtEq => Some(Value::Bool(lval >= rval)),
        _ => None,
    }
}

fn non_nan_float(value: f64, prim: PrimitiveType) -> Option<Value> {
    if value.is_nan() {
        return None;
    }
    Some(Value::Float { value, prim })
}

// ──────────────────────────────────────────────────────────────────────────────
// Type queries, truncation, formatting
// ──────────────────────────────────────────────────────────────────────────────

fn is_signed_int(prim: PrimitiveType) -> bool {
    matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    )
}

fn int_bit_width(prim: PrimitiveType) -> u32 {
    match prim {
        PrimitiveType::I8 | PrimitiveType::U8 => 8,
        PrimitiveType::I16 | PrimitiveType::U16 => 16,
        PrimitiveType::I32 | PrimitiveType::U32 => 32,
        PrimitiveType::I64 | PrimitiveType::U64 => 64,
        _ => 32,
    }
}

fn is_f32_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(PrimitiveType::F32)
    )
}

fn int_primitive_of(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(
            p @ (PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64),
        ) => Some(*p),
        _ => None,
    }
}

/// Truncate / sign-extend an integer bit pattern to fit the target prim.
#[must_use]
pub(crate) fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
    match prim {
        PrimitiveType::U8 => value & 0xFF,
        PrimitiveType::U16 => value & 0xFFFF,
        PrimitiveType::U32 => value & 0xFFFF_FFFF,
        PrimitiveType::U64 => value,
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value,
        _ => value,
    }
}

/// Render an integer bit pattern as decimal text, signed when the prim
/// is signed.
#[must_use]
pub(crate) fn format_int_repr(value: u64, prim: PrimitiveType) -> String {
    if is_signed_int(prim) {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Render a float as a Wado-friendly literal repr (`3.25`, `0.0`,
/// `Infinity`, `-Infinity`, …). Trailing `.0` is appended to integral
/// values so the result parses back as a float literal.
#[must_use]
pub(crate) fn format_float_repr(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    let s = value.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}
