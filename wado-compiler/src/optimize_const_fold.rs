//! Constant folding optimization for Wado TIR
//!
//! This module folds compile-time-known integer arithmetic expressions into
//! literal values. For example, `2 + 3` becomes `5`.
//!
//! Currently supported:
//! - Integer binary operations: Add, Sub, Mul, Div, Mod
//! - Integer types: i8, i16, i32, i64, u8, u16, u32, u64
//! - Unary negation on integer literals

use crate::project::Project;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt,
    TirStmtKind, TirUnaryOp, TypeTable,
};

/// Apply constant folding to all functions in the project.
pub fn fold_constants(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= fold_constants_in_function(&mut func, &type_table);
        }
    }
    changed
}

fn fold_constants_in_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    fold_constants_in_block(body, type_table)
}

fn fold_constants_in_block(block: &mut TirBlock, type_table: &TypeTable) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= fold_constants_in_stmt(stmt, type_table);
    }
    changed
}

fn fold_constants_in_stmt(stmt: &mut TirStmt, type_table: &TypeTable) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => fold_constants_in_expr(value, type_table),
        TirStmtKind::Expr(expr) => fold_constants_in_expr(expr, type_table),
        TirStmtKind::Return { value } => value
            .as_mut()
            .is_some_and(|v| fold_constants_in_expr(v, type_table)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = fold_constants_in_expr(condition, type_table);
            changed |= fold_constants_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                changed |= fold_constants_in_block(eb, type_table);
            }
            changed
        }
        TirStmtKind::Loop { body } => fold_constants_in_block(body, type_table),
        TirStmtKind::LabeledBlock { block, .. } => fold_constants_in_block(block, type_table),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = fold_constants_in_expr(scrutinee, type_table);
            changed |= fold_constants_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                changed |= fold_constants_in_block(eb, type_table);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => value
            .as_mut()
            .is_some_and(|v| fold_constants_in_expr(v, type_table)),
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => fold_constants_in_expr(value, type_table),
    }
}

fn fold_constants_in_expr(expr: &mut TirExpr, type_table: &TypeTable) -> bool {
    let mut changed = false;

    // First, recurse into sub-expressions (bottom-up folding)
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= fold_constants_in_expr(left, type_table);
            changed |= fold_constants_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            changed |= fold_constants_in_expr(inner, type_table);
        }
        TirExprKind::Assign { target, value } => {
            changed |= fold_constants_in_expr(target, type_table);
            changed |= fold_constants_in_expr(value, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= fold_constants_in_expr(receiver, type_table);
            for arg in args {
                changed |= fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= fold_constants_in_expr(callee, type_table);
            for arg in args {
                changed |= fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= fold_constants_in_expr(functor, type_table);
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            changed |= fold_constants_in_expr(inner, type_table);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= fold_constants_in_expr(inner, type_table);
            changed |= fold_constants_in_expr(index, type_table);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= fold_constants_in_block(block, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= fold_constants_in_expr(condition, type_table);
            changed |= fold_constants_in_block(then_branch, type_table);
            if let Some(eb) = else_branch {
                changed |= fold_constants_in_block(eb, type_table);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= fold_constants_in_expr(inner, type_table);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= fold_constants_in_expr(guard, type_table);
                }
                changed |= fold_constants_in_expr(&mut arm.body, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= fold_constants_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= fold_constants_in_expr(elem, type_table);
            }
        }
        TirExprKind::OptionSome { value } => {
            changed |= fold_constants_in_expr(value, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                changed |= fold_constants_in_expr(payload_expr, type_table);
            }
        }
        TirExprKind::Move { expr } => {
            changed |= fold_constants_in_expr(expr, type_table);
        }
        TirExprKind::Closure { body, .. } => {
            changed |= fold_constants_in_expr(body, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= fold_constants_in_expr(value, type_table);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            changed |= fold_constants_in_expr(expr, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= fold_constants_in_expr(scrutinee, type_table);
            for arm in arms {
                changed |= fold_constants_in_block(arm, type_table);
            }
            changed |= fold_constants_in_block(default, type_table);
        }
        // Leaf nodes - nothing to recurse into
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
    }

    // Now try to fold this expression
    if let Some((value, prim)) = try_fold_expr(expr, type_table) {
        expr.kind = TirExprKind::IntLiteral {
            value,
            repr: format_int_value(value, prim),
        };
        changed = true;
    }

    changed
}

/// Try to fold a single expression node into a constant.
/// Returns the folded value and its primitive type, or `None` if not foldable.
fn try_fold_expr(expr: &TirExpr, type_table: &TypeTable) -> Option<(u64, PrimitiveType)> {
    let prim = get_int_primitive(expr.type_id, type_table)?;
    match &expr.kind {
        TirExprKind::Binary { left, op, right } => {
            try_fold_int_binary(left, *op, right, prim).map(|v| (v, prim))
        }
        TirExprKind::Unary {
            op: TirUnaryOp::Neg,
            expr: inner,
        } => {
            let TirExprKind::IntLiteral { value, .. } = &inner.kind else {
                return None;
            };
            eval_int_neg(*value, prim).map(|v| (v, prim))
        }
        TirExprKind::Cast { expr: inner, .. } => {
            let TirExprKind::IntLiteral { value, .. } = &inner.kind else {
                return None;
            };
            Some((truncate_int(*value, prim), prim))
        }
        _ => None,
    }
}

/// Format an integer value as a string appropriate for its type.
/// Signed types display as signed (e.g., -128), unsigned as unsigned.
fn format_int_value(value: u64, prim: PrimitiveType) -> String {
    match prim {
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64 => {
            (value as i64).to_string()
        }
        _ => value.to_string(),
    }
}

/// Try to fold an integer binary operation with two literal operands.
/// Returns the folded value if both operands are integer literals.
fn try_fold_int_binary(
    left: &TirExpr,
    op: TirBinaryOp,
    right: &TirExpr,
    prim: PrimitiveType,
) -> Option<u64> {
    let TirExprKind::IntLiteral { value: lval, .. } = &left.kind else {
        return None;
    };
    let TirExprKind::IntLiteral { value: rval, .. } = &right.kind else {
        return None;
    };

    match op {
        TirBinaryOp::Add => eval_int_add(*lval, *rval, prim),
        TirBinaryOp::Sub => eval_int_sub(*lval, *rval, prim),
        TirBinaryOp::Mul => eval_int_mul(*lval, *rval, prim),
        TirBinaryOp::Div => eval_int_div(*lval, *rval, prim),
        TirBinaryOp::Mod => eval_int_mod(*lval, *rval, prim),
        _ => None,
    }
}

/// Get the integer `PrimitiveType` for a `TypeId`, following newtypes.
/// Returns `None` for non-integer types and i128/u128 (not yet supported).
fn get_int_primitive(type_id: crate::tir::TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    let base = type_table.get_ultimate_base_type(type_id);
    match type_table.get(base) {
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

/// Truncate a u64 value to the width of the given integer type.
///
/// For unsigned types, zero-extends (masks to width).
/// For signed types, sign-extends back to 64 bits,
/// so that WIR emission's `*value as i32` / `*value as i64` produces the correct signed value.
#[allow(clippy::cast_sign_loss)]
fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
    match prim {
        PrimitiveType::U8 => value & 0xFF,
        PrimitiveType::U16 => value & 0xFFFF,
        PrimitiveType::U32 => value & 0xFFFF_FFFF,
        PrimitiveType::U64 => value,
        // Signed: truncate then sign-extend
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value,
        _ => value,
    }
}

fn eval_int_add(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_add(rval), prim))
}

fn eval_int_sub(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_sub(rval), prim))
}

fn eval_int_mul(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(truncate_int(lval.wrapping_mul(rval), prim))
}

#[allow(clippy::cast_sign_loss, clippy::invalid_upcast_comparisons)]
fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None; // division by zero traps at runtime
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval / rval, prim))
        }
        // i8/i16: executed as i32 instructions in Wasm, so MIN / -1 doesn't trap
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        // i32/i64: Wasm's div_s traps on MIN / -1, so don't fold that case
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

#[allow(clippy::cast_sign_loss, clippy::invalid_upcast_comparisons)]
fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None; // division by zero traps at runtime
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
        // i32/i64: Wasm's rem_s traps on MIN % -1, so don't fold that case
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

#[allow(clippy::cast_sign_loss)]
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
        // Negation on unsigned doesn't make sense; skip
        _ => None,
    }
}

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
        // i8: 128 (0x80) sign-extends to -128
        assert_eq!(truncate_int(128, PrimitiveType::I8) as i64, -128);
        // i8: 127 stays 127
        assert_eq!(truncate_int(127, PrimitiveType::I8), 127);
        // i16: 0x8000 sign-extends to -32768
        assert_eq!(truncate_int(0x8000, PrimitiveType::I16) as i64, -32768);
        // i32: 0x8000_0000 sign-extends to -2147483648
        assert_eq!(
            truncate_int(0x8000_0000, PrimitiveType::I32) as i64,
            -2_147_483_648
        );
    }

    #[test]
    fn test_add_wrapping() {
        // u8: 255 + 1 = 0
        assert_eq!(eval_int_add(255, 1, PrimitiveType::U8), Some(0));
        // i32: normal addition
        assert_eq!(eval_int_add(21, 21, PrimitiveType::I32), Some(42));
    }

    #[test]
    fn test_sub() {
        assert_eq!(eval_int_sub(10, 3, PrimitiveType::I32), Some(7));
        // u8: 0 - 1 wraps to 255
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
        // Signed division: -7 / 2 = -3 (truncates toward zero)
        let neg7 = (-7_i32) as u64;
        let result = eval_int_div(neg7, 2, PrimitiveType::I32);
        assert_eq!(result.map(|v| v as i32), Some(-3));
        // i32::MIN / -1 traps in Wasm — must not fold
        let i32_min = i32::MIN as u64;
        let neg1_i32 = (-1_i32) as u64;
        assert_eq!(eval_int_div(i32_min, neg1_i32, PrimitiveType::I32), None);
        // i64::MIN / -1 traps in Wasm — must not fold
        let i64_min = i64::MIN as u64;
        let neg1_i64 = (-1_i64) as u64;
        assert_eq!(eval_int_div(i64_min, neg1_i64, PrimitiveType::I64), None);
        // i8::MIN / -1 is fine (executed as i32 in Wasm, no trap)
        let i8_min = (-128_i8) as u64;
        let neg1_i8 = (-1_i8) as u64;
        assert!(eval_int_div(i8_min, neg1_i8, PrimitiveType::I8).is_some());
    }

    #[test]
    fn test_mod() {
        assert_eq!(eval_int_mod(10, 3, PrimitiveType::I32), Some(1));
        assert_eq!(eval_int_mod(10, 0, PrimitiveType::I32), None);
        // i32::MIN % -1 traps in Wasm — must not fold
        let i32_min = i32::MIN as u64;
        let neg1 = (-1_i32) as u64;
        assert_eq!(eval_int_mod(i32_min, neg1, PrimitiveType::I32), None);
        // i64::MIN % -1 traps in Wasm — must not fold
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
        // Unsigned negation returns None
        assert_eq!(eval_int_neg(42, PrimitiveType::U32), None);
    }

    #[test]
    fn test_cast_mask() {
        // i32 value cast to i64 preserves value
        assert_eq!(truncate_int(1_000_000, PrimitiveType::I64), 1_000_000);
        // i64 large value cast to i32 truncates + sign-extends
        assert_eq!(truncate_int(0x1_0000_0001, PrimitiveType::I32), 1);
        // u8 cast truncates
        assert_eq!(truncate_int(300, PrimitiveType::U8), 44);
        // Signed cast: -128 as i8
        let neg128 = (-128_i64) as u64;
        assert_eq!(truncate_int(neg128, PrimitiveType::I8) as i64, -128);
    }
}
