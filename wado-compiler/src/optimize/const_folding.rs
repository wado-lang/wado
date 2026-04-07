//! Constant folding optimization for Wado TIR
//!
//! This module folds compile-time-known expressions into literal values.
//! For example, `2 + 3` becomes `5`, `10 > 5` becomes `true`.
//!
//! Supported operations:
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
//! - Boolean logical: And, Or
//! - Boolean equality: Eq, `NotEq`
//! - Boolean unary: Not
//!
//! Float arithmetic uses native Rust IEEE 754 ops (same as Wasm), following
//! cranelift's approach: fold the result, but skip if it is NaN since NaN
//! bit patterns are nondeterministic across architectures.
//! Float negation is a pure bit flip (XOR sign bit), always deterministic.
//!
//! Integer division/modulo by zero and signed MIN / -1 are not folded —
//! they must remain runtime traps.

use crate::package::Package;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};

use crate::tir_visitor::{TirOptVisitor, opt_walk_expr};

enum FoldedExpr {
    Int { value: u64, prim: PrimitiveType },
    Float { value: f64, repr: String },
    Bool(bool),
}

impl FoldedExpr {
    fn into_expr_kind(self) -> TirExprKind {
        match self {
            Self::Int { value, prim } => TirExprKind::IntLiteral {
                repr: format_int_value(value, prim),
                value,
            },
            Self::Float { value, repr } => TirExprKind::FloatLiteral { value, repr },
            Self::Bool(b) => TirExprKind::BoolLiteral(b),
        }
    }
}

/// Apply constant folding to all functions in the project.
pub fn fold_constants(project: &mut Package) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        let mut visitor = ConstFoldVisitor {
            type_table: &type_table,
        };
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(ref mut body) = func.body {
                changed |= visitor.visit_block(body);
            }
        }
    }
    changed
}

struct ConstFoldVisitor<'a> {
    type_table: &'a TypeTable,
}

impl TirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: recurse into children first
        let mut changed = opt_walk_expr(self, expr);
        // Then try to fold this expression
        if let Some(folded) = try_fold_expr(expr, self.type_table) {
            expr.kind = folded.into_expr_kind();
            changed = true;
        }
        changed
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Top-level fold dispatch
// ──────────────────────────────────────────────────────────────────────────────

fn try_fold_expr(expr: &TirExpr, type_table: &TypeTable) -> Option<FoldedExpr> {
    match &expr.kind {
        TirExprKind::Binary { left, op, right } => {
            try_fold_binary(expr.type_id, left, *op, right, type_table)
        }
        TirExprKind::Unary { op, expr: inner } => {
            try_fold_unary(expr.type_id, *op, inner, type_table)
        }
        TirExprKind::Cast { expr: inner, .. } => {
            let prim = get_int_primitive(expr.type_id, type_table)?;
            let value = match &inner.kind {
                TirExprKind::IntLiteral { value, .. } => *value,
                TirExprKind::CharLiteral(c) => *c as u64,
                _ => return None,
            };
            Some(FoldedExpr::Int {
                value: truncate_int(value, prim),
                prim,
            })
        }
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Binary operation folding
// ──────────────────────────────────────────────────────────────────────────────

fn try_fold_binary(
    result_type: TypeId,
    left: &TirExpr,
    op: TirBinaryOp,
    right: &TirExpr,
    type_table: &TypeTable,
) -> Option<FoldedExpr> {
    if let (TirExprKind::BoolLiteral(lb), TirExprKind::BoolLiteral(rb)) = (&left.kind, &right.kind)
    {
        return match op {
            TirBinaryOp::And => Some(FoldedExpr::Bool(*lb && *rb)),
            TirBinaryOp::Or => Some(FoldedExpr::Bool(*lb || *rb)),
            TirBinaryOp::Eq => Some(FoldedExpr::Bool(*lb == *rb)),
            TirBinaryOp::NotEq => Some(FoldedExpr::Bool(*lb != *rb)),
            _ => None,
        };
    }

    if let (
        TirExprKind::FloatLiteral { value: lv, .. },
        TirExprKind::FloatLiteral { value: rv, .. },
    ) = (&left.kind, &right.kind)
    {
        return if is_f32_type(left.type_id, type_table) {
            try_fold_f32_binary(*lv, op, *rv)
        } else {
            try_fold_f64_binary(*lv, op, *rv)
        };
    }

    if let (TirExprKind::IntLiteral { value: lv, .. }, TirExprKind::IntLiteral { value: rv, .. }) =
        (&left.kind, &right.kind)
    {
        let operand_prim = get_int_primitive(left.type_id, type_table)?;
        return try_fold_int_binary(result_type, *lv, op, *rv, operand_prim, type_table);
    }

    None
}

fn try_fold_int_binary(
    result_type: TypeId,
    lval: u64,
    op: TirBinaryOp,
    rval: u64,
    prim: PrimitiveType,
    type_table: &TypeTable,
) -> Option<FoldedExpr> {
    match op {
        TirBinaryOp::Add => Some(FoldedExpr::Int {
            value: eval_int_add(lval, rval, prim)?,
            prim,
        }),
        TirBinaryOp::Sub => Some(FoldedExpr::Int {
            value: eval_int_sub(lval, rval, prim)?,
            prim,
        }),
        TirBinaryOp::Mul => Some(FoldedExpr::Int {
            value: eval_int_mul(lval, rval, prim)?,
            prim,
        }),
        TirBinaryOp::Div => Some(FoldedExpr::Int {
            value: eval_int_div(lval, rval, prim)?,
            prim,
        }),
        TirBinaryOp::Mod => Some(FoldedExpr::Int {
            value: eval_int_mod(lval, rval, prim)?,
            prim,
        }),

        TirBinaryOp::Eq
        | TirBinaryOp::NotEq
        | TirBinaryOp::Lt
        | TirBinaryOp::LtEq
        | TirBinaryOp::Gt
        | TirBinaryOp::GtEq => Some(FoldedExpr::Bool(eval_int_cmp(lval, op, rval, prim))),

        TirBinaryOp::BitAnd => Some(FoldedExpr::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        TirBinaryOp::BitOr => Some(FoldedExpr::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        TirBinaryOp::BitXor => Some(FoldedExpr::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        TirBinaryOp::Shl => eval_int_shl(lval, rval, prim).map(|value| FoldedExpr::Int {
            value,
            prim: get_int_primitive(result_type, type_table).unwrap_or(prim),
        }),
        TirBinaryOp::Shr => eval_int_shr(lval, rval, prim).map(|value| FoldedExpr::Int {
            value,
            prim: get_int_primitive(result_type, type_table).unwrap_or(prim),
        }),

        TirBinaryOp::And | TirBinaryOp::Or | TirBinaryOp::RefEq | TirBinaryOp::RefNotEq => None,
    }
}

fn try_fold_f64_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<FoldedExpr> {
    match op {
        TirBinaryOp::Add => non_nan_float(lval + rval),
        TirBinaryOp::Sub => non_nan_float(lval - rval),
        TirBinaryOp::Mul => non_nan_float(lval * rval),
        TirBinaryOp::Div => non_nan_float(lval / rval),
        _ => try_fold_float_comparison(lval, op, rval),
    }
}

fn try_fold_f32_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<FoldedExpr> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        TirBinaryOp::Add => non_nan_float(f64::from(l + r)),
        TirBinaryOp::Sub => non_nan_float(f64::from(l - r)),
        TirBinaryOp::Mul => non_nan_float(f64::from(l * r)),
        TirBinaryOp::Div => non_nan_float(f64::from(l / r)),
        TirBinaryOp::Eq => Some(FoldedExpr::Bool(l == r)),
        TirBinaryOp::NotEq => Some(FoldedExpr::Bool(l != r)),
        TirBinaryOp::Lt => Some(FoldedExpr::Bool(l < r)),
        TirBinaryOp::LtEq => Some(FoldedExpr::Bool(l <= r)),
        TirBinaryOp::Gt => Some(FoldedExpr::Bool(l > r)),
        TirBinaryOp::GtEq => Some(FoldedExpr::Bool(l >= r)),
        _ => None,
    }
}

fn try_fold_float_comparison(lval: f64, op: TirBinaryOp, rval: f64) -> Option<FoldedExpr> {
    match op {
        TirBinaryOp::Eq => Some(FoldedExpr::Bool(lval == rval)),
        TirBinaryOp::NotEq => Some(FoldedExpr::Bool(lval != rval)),
        TirBinaryOp::Lt => Some(FoldedExpr::Bool(lval < rval)),
        TirBinaryOp::LtEq => Some(FoldedExpr::Bool(lval <= rval)),
        TirBinaryOp::Gt => Some(FoldedExpr::Bool(lval > rval)),
        TirBinaryOp::GtEq => Some(FoldedExpr::Bool(lval >= rval)),
        _ => None,
    }
}

fn non_nan_float(value: f64) -> Option<FoldedExpr> {
    if value.is_nan() {
        return None;
    }
    Some(FoldedExpr::Float {
        repr: format_float(value),
        value,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Unary operation folding
// ──────────────────────────────────────────────────────────────────────────────

fn try_fold_unary(
    result_type: TypeId,
    op: TirUnaryOp,
    inner: &TirExpr,
    type_table: &TypeTable,
) -> Option<FoldedExpr> {
    match op {
        TirUnaryOp::Neg => match &inner.kind {
            TirExprKind::IntLiteral { value, .. } => {
                let prim = get_int_primitive(result_type, type_table)?;
                eval_int_neg(*value, prim).map(|value| FoldedExpr::Int { value, prim })
            }
            TirExprKind::FloatLiteral { value, .. } => {
                let negated = f64::from_bits(value.to_bits() ^ (1u64 << 63));
                Some(FoldedExpr::Float {
                    repr: format_float(negated),
                    value: negated,
                })
            }
            _ => None,
        },
        TirUnaryOp::Not => {
            let TirExprKind::BoolLiteral(b) = &inner.kind else {
                return None;
            };
            Some(FoldedExpr::Bool(!b))
        }
        TirUnaryOp::BitNot => {
            let TirExprKind::IntLiteral { value, .. } = &inner.kind else {
                return None;
            };
            let prim = get_int_primitive(result_type, type_table)?;
            Some(FoldedExpr::Int {
                value: truncate_int(!value, prim),
                prim,
            })
        }
        TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Integer comparison
// ──────────────────────────────────────────────────────────────────────────────

fn eval_int_cmp(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    let is_signed = matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    );
    if is_signed {
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

// ──────────────────────────────────────────────────────────────────────────────
// Integer shift operations
// ──────────────────────────────────────────────────────────────────────────────

fn eval_int_shl(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    Some(truncate_int(lval.wrapping_shl(shift), prim))
}

fn eval_int_shr(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    let is_signed = matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    );
    if is_signed {
        let result = (lval as i64).wrapping_shr(shift);
        Some(truncate_int(result as u64, prim))
    } else {
        Some(truncate_int(lval.wrapping_shr(shift), prim))
    }
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

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

fn format_int_value(value: u64, prim: PrimitiveType) -> String {
    match prim {
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64 => {
            (value as i64).to_string()
        }
        _ => value.to_string(),
    }
}

fn format_float(value: f64) -> String {
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

fn is_f32_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(PrimitiveType::F32)
    )
}

fn get_int_primitive(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
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

fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
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

// ──────────────────────────────────────────────────────────────────────────────
// Integer arithmetic evaluators
// ──────────────────────────────────────────────────────────────────────────────

fn eval_int_add(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_add(rval), prim))
}

fn eval_int_sub(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_sub(rval), prim))
}

fn eval_int_mul(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_mul(rval), prim))
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

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_int_unsigned() {
        assert_eq!(truncate_int(256, PrimitiveType::U8), 0);
        assert_eq!(truncate_int(255, PrimitiveType::U8), 255);
        assert_eq!(truncate_int(0x1_0000, PrimitiveType::U16), 0);
        assert_eq!(truncate_int(0x1_0000_0000, PrimitiveType::U32), 0);
        assert_eq!(truncate_int(u64::MAX, PrimitiveType::U64), u64::MAX);
    }

    #[test]
    fn test_truncate_int_signed() {
        assert_eq!(truncate_int(128, PrimitiveType::I8) as i64, -128);
        assert_eq!(truncate_int(127, PrimitiveType::I8), 127);
        assert_eq!(truncate_int(0x8000, PrimitiveType::I16) as i64, -32768);
        assert_eq!(
            truncate_int(0x8000_0000, PrimitiveType::I32) as i64,
            -2_147_483_648
        );
    }

    #[test]
    fn test_add_wrapping() {
        assert_eq!(eval_int_add(255, 1, PrimitiveType::U8), Some(0));
        assert_eq!(eval_int_add(21, 21, PrimitiveType::I32), Some(42));
    }

    #[test]
    fn test_sub() {
        assert_eq!(eval_int_sub(10, 3, PrimitiveType::I32), Some(7));
        assert_eq!(eval_int_sub(0, 1, PrimitiveType::U8), Some(255));
    }

    #[test]
    fn test_mul() {
        assert_eq!(eval_int_mul(6, 7, PrimitiveType::I32), Some(42));
        assert_eq!(eval_int_mul(21, 2, PrimitiveType::I32), Some(42));
    }

    #[test]
    fn test_div() {
        assert_eq!(eval_int_div(42, 6, PrimitiveType::I32), Some(7));
        assert_eq!(eval_int_div(42, 0, PrimitiveType::I32), None);
        let neg7 = (-7_i32) as u64;
        let result = eval_int_div(neg7, 2, PrimitiveType::I32);
        assert_eq!(result.map(|v| v as i32), Some(-3));
        let i32_min = i32::MIN as u64;
        let neg1_i32 = (-1_i32) as u64;
        assert_eq!(eval_int_div(i32_min, neg1_i32, PrimitiveType::I32), None);
        let i64_min = i64::MIN as u64;
        let neg1_i64 = (-1_i64) as u64;
        assert_eq!(eval_int_div(i64_min, neg1_i64, PrimitiveType::I64), None);
        let i8_min = (-128_i8) as u64;
        let neg1_i8 = (-1_i8) as u64;
        assert!(eval_int_div(i8_min, neg1_i8, PrimitiveType::I8).is_some());
    }

    #[test]
    fn test_mod() {
        assert_eq!(eval_int_mod(10, 3, PrimitiveType::I32), Some(1));
        assert_eq!(eval_int_mod(10, 0, PrimitiveType::I32), None);
        let i32_min = i32::MIN as u64;
        let neg1 = (-1_i32) as u64;
        assert_eq!(eval_int_mod(i32_min, neg1, PrimitiveType::I32), None);
        let i64_min = i64::MIN as u64;
        let neg1_i64 = (-1_i64) as u64;
        assert_eq!(eval_int_mod(i64_min, neg1_i64, PrimitiveType::I64), None);
    }

    #[test]
    fn test_neg() {
        assert_eq!(
            eval_int_neg(42, PrimitiveType::I32).map(|v| v as i32),
            Some(-42)
        );
        assert_eq!(eval_int_neg(42, PrimitiveType::U32), None);
    }

    #[test]
    fn test_cast_mask() {
        assert_eq!(truncate_int(1_000_000, PrimitiveType::I64), 1_000_000);
        assert_eq!(truncate_int(0x1_0000_0001, PrimitiveType::I32), 1);
        assert_eq!(truncate_int(300, PrimitiveType::U8), 44);
        let neg128 = (-128_i64) as u64;
        assert_eq!(truncate_int(neg128, PrimitiveType::I8) as i64, -128);
    }

    #[test]
    fn test_int_cmp_unsigned() {
        assert!(eval_int_cmp(10, TirBinaryOp::Eq, 10, PrimitiveType::U32));
        assert!(!eval_int_cmp(10, TirBinaryOp::Eq, 20, PrimitiveType::U32));
        assert!(eval_int_cmp(10, TirBinaryOp::NotEq, 20, PrimitiveType::U32));
        assert!(eval_int_cmp(5, TirBinaryOp::Lt, 10, PrimitiveType::U32));
        assert!(!eval_int_cmp(10, TirBinaryOp::Lt, 5, PrimitiveType::U32));
        assert!(eval_int_cmp(5, TirBinaryOp::LtEq, 5, PrimitiveType::U32));
        assert!(eval_int_cmp(10, TirBinaryOp::Gt, 5, PrimitiveType::U32));
        assert!(eval_int_cmp(10, TirBinaryOp::GtEq, 10, PrimitiveType::U32));
    }

    #[test]
    fn test_int_cmp_signed() {
        let neg5 = (-5_i32) as u64;
        let neg10 = (-10_i32) as u64;
        assert!(eval_int_cmp(
            neg5,
            TirBinaryOp::Gt,
            neg10,
            PrimitiveType::I32
        ));
        assert!(eval_int_cmp(
            neg10,
            TirBinaryOp::Lt,
            neg5,
            PrimitiveType::I32
        ));
        assert!(eval_int_cmp(neg5, TirBinaryOp::Lt, 5, PrimitiveType::I32));
    }

    #[test]
    fn test_bitwise_and() {
        assert_eq!(truncate_int(0xFF & 0x0F, PrimitiveType::U8), 0x0F);
    }

    #[test]
    fn test_bitwise_or() {
        assert_eq!(truncate_int(0xF0 | 0x0F, PrimitiveType::U8), 0xFF);
    }

    #[test]
    fn test_bitwise_xor() {
        assert_eq!(truncate_int(0xFF ^ 0x0F, PrimitiveType::U8), 0xF0);
    }

    #[test]
    fn test_bitwise_not() {
        assert_eq!(truncate_int(!0u64, PrimitiveType::U8), 0xFF);
        assert_eq!(truncate_int(!0u64, PrimitiveType::I32) as i64, -1);
    }

    #[test]
    fn test_shift_left() {
        assert_eq!(eval_int_shl(1, 4, PrimitiveType::U32), Some(16));
        assert_eq!(eval_int_shl(1, 32, PrimitiveType::U32), Some(1));
    }

    #[test]
    fn test_shift_right_unsigned() {
        assert_eq!(eval_int_shr(16, 4, PrimitiveType::U32), Some(1));
        assert_eq!(eval_int_shr(0xFF, 4, PrimitiveType::U8), Some(0x0F));
    }

    #[test]
    fn test_shift_right_signed() {
        let neg128 = (-128_i32) as u64;
        let result = eval_int_shr(neg128, 1, PrimitiveType::I32);
        assert_eq!(result.map(|v| v as i32), Some(-64));
    }

    #[test]
    fn test_float_format() {
        assert_eq!(format_float(3.14), "3.14");
        assert_eq!(format_float(0.0), "0.0");
        assert_eq!(format_float(4.0), "4.0");
        assert_eq!(format_float(f64::INFINITY), "Infinity");
        assert_eq!(format_float(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn test_f64_arithmetic() {
        let r = try_fold_f64_binary(1.5, TirBinaryOp::Add, 2.5);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == 4.0));
        let r = try_fold_f64_binary(10.0, TirBinaryOp::Sub, 3.5);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == 6.5));
        let r = try_fold_f64_binary(3.0, TirBinaryOp::Mul, 2.0);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == 6.0));
        let r = try_fold_f64_binary(10.0, TirBinaryOp::Div, 4.0);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == 2.5));
        let r = try_fold_f64_binary(1.0, TirBinaryOp::Div, 0.0);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == f64::INFINITY));
    }

    #[test]
    fn test_f32_arithmetic() {
        let r = try_fold_f32_binary(1.5, TirBinaryOp::Add, 2.5);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == 4.0));
        let r = try_fold_f32_binary(1.0, TirBinaryOp::Div, 3.0);
        let expected = f64::from(1.0f32 / 3.0f32);
        assert!(matches!(r, Some(FoldedExpr::Float { value, .. }) if value == expected));
    }

    #[test]
    fn test_float_nan_skipped() {
        assert!(try_fold_f64_binary(0.0, TirBinaryOp::Div, 0.0).is_none());
        assert!(try_fold_f64_binary(f64::INFINITY, TirBinaryOp::Sub, f64::INFINITY).is_none());
        assert!(try_fold_f64_binary(0.0, TirBinaryOp::Mul, f64::INFINITY).is_none());
        assert!(try_fold_f32_binary(0.0, TirBinaryOp::Div, 0.0).is_none());
    }

    #[test]
    fn test_f32_comparison() {
        let pi_f64 = std::f64::consts::PI;
        let tau_f64 = std::f64::consts::TAU;
        let pi_times_2 = f64::from(pi_f64 as f32 * 2.0_f32);
        assert_ne!(tau_f64, pi_times_2);
        assert!(matches!(
            try_fold_f32_binary(tau_f64, TirBinaryOp::Eq, pi_times_2),
            Some(FoldedExpr::Bool(true))
        ));
        assert!(matches!(
            try_fold_f32_binary(tau_f64, TirBinaryOp::NotEq, pi_times_2),
            Some(FoldedExpr::Bool(false))
        ));
        assert!(matches!(
            try_fold_f32_binary(f64::NAN, TirBinaryOp::Eq, f64::NAN),
            Some(FoldedExpr::Bool(false))
        ));
    }

    #[test]
    fn test_float_comparison() {
        assert!(matches!(
            try_fold_f64_binary(1.0, TirBinaryOp::Lt, 2.0),
            Some(FoldedExpr::Bool(true))
        ));
        assert!(matches!(
            try_fold_f64_binary(2.0, TirBinaryOp::Lt, 1.0),
            Some(FoldedExpr::Bool(false))
        ));
        assert!(matches!(
            try_fold_f64_binary(1.0, TirBinaryOp::Eq, 1.0),
            Some(FoldedExpr::Bool(true))
        ));
        assert!(matches!(
            try_fold_f64_binary(f64::NAN, TirBinaryOp::Eq, f64::NAN),
            Some(FoldedExpr::Bool(false))
        ));
        assert!(matches!(
            try_fold_f64_binary(f64::NAN, TirBinaryOp::NotEq, f64::NAN),
            Some(FoldedExpr::Bool(true))
        ));
        assert!(matches!(
            try_fold_f64_binary(f64::NAN, TirBinaryOp::Lt, 1.0),
            Some(FoldedExpr::Bool(false))
        ));
    }
}
