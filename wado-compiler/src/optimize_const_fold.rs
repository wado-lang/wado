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
pub fn fold_constants(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            fold_constants_in_function(&mut func, &type_table);
        }
    }
}

fn fold_constants_in_function(func: &mut TirFunction, type_table: &TypeTable) {
    let Some(body) = &mut func.body else {
        return;
    };
    fold_constants_in_block(body, type_table);
}

fn fold_constants_in_block(block: &mut TirBlock, type_table: &TypeTable) {
    for stmt in &mut block.stmts {
        fold_constants_in_stmt(stmt, type_table);
    }
}

fn fold_constants_in_stmt(stmt: &mut TirStmt, type_table: &TypeTable) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            fold_constants_in_expr(value, type_table);
        }
        TirStmtKind::Expr(expr) => {
            fold_constants_in_expr(expr, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                fold_constants_in_expr(v, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            fold_constants_in_expr(condition, type_table);
            fold_constants_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                fold_constants_in_block(eb, type_table);
            }
        }
        TirStmtKind::Loop { body } => {
            fold_constants_in_block(body, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            fold_constants_in_block(block, type_table);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            fold_constants_in_expr(scrutinee, type_table);
            fold_constants_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                fold_constants_in_block(eb, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                fold_constants_in_expr(v, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            fold_constants_in_expr(value, type_table);
        }
    }
}

fn fold_constants_in_expr(expr: &mut TirExpr, type_table: &TypeTable) {
    // First, recurse into sub-expressions (bottom-up folding)
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            fold_constants_in_expr(left, type_table);
            fold_constants_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            fold_constants_in_expr(inner, type_table);
        }
        TirExprKind::Assign { target, value } => {
            fold_constants_in_expr(target, type_table);
            fold_constants_in_expr(value, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            fold_constants_in_expr(receiver, type_table);
            for arg in args {
                fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            fold_constants_in_expr(callee, type_table);
            for arg in args {
                fold_constants_in_expr(arg, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            fold_constants_in_expr(functor, type_table);
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            fold_constants_in_expr(inner, type_table);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            fold_constants_in_expr(inner, type_table);
            fold_constants_in_expr(index, type_table);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            fold_constants_in_block(block, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            fold_constants_in_expr(condition, type_table);
            fold_constants_in_block(then_branch, type_table);
            if let Some(eb) = else_branch {
                fold_constants_in_block(eb, type_table);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            fold_constants_in_expr(inner, type_table);
            for arm in arms {
                fold_constants_in_expr(&mut arm.body, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                fold_constants_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                fold_constants_in_expr(elem, type_table);
            }
        }
        TirExprKind::OptionSome { value } => {
            fold_constants_in_expr(value, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                fold_constants_in_expr(payload_expr, type_table);
            }
        }
        TirExprKind::Move { expr } => {
            fold_constants_in_expr(expr, type_table);
        }
        TirExprKind::Closure { body, .. } => {
            fold_constants_in_expr(body, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            fold_constants_in_expr(value, type_table);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            fold_constants_in_expr(expr, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            fold_constants_in_expr(scrutinee, type_table);
            for arm in arms {
                fold_constants_in_block(arm, type_table);
            }
            fold_constants_in_block(default, type_table);
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
    try_fold_expr(expr, type_table);
}

/// Try to fold a single expression node into a constant.
/// Returns the folded result value if folding is possible.
fn try_fold_expr(expr: &mut TirExpr, type_table: &TypeTable) {
    let folded: Option<(u64, PrimitiveType)> = match &expr.kind {
        TirExprKind::Binary { left, op, right } => get_int_primitive(expr.type_id, type_table)
            .and_then(|prim| try_fold_int_binary(left, *op, right, prim).map(|v| (v, prim))),
        TirExprKind::Unary {
            op: TirUnaryOp::Neg,
            expr: inner,
        } => {
            if let TirExprKind::IntLiteral { value, .. } = &inner.kind {
                get_int_primitive(expr.type_id, type_table)
                    .and_then(|prim| eval_int_neg(*value, prim).map(|v| (v, prim)))
            } else {
                None
            }
        }
        // Cast of integer literal → fold to literal with target type width
        TirExprKind::Cast { expr: inner, .. } => {
            if let TirExprKind::IntLiteral { value, .. } = &inner.kind {
                get_int_primitive(expr.type_id, type_table)
                    .map(|prim| (mask_to_width(*value, prim), prim))
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some((value, prim)) = folded {
        expr.kind = TirExprKind::IntLiteral {
            value,
            repr: format_int_value(value, prim),
        };
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

/// Truncate and sign/zero-extend a u64 value to the width of the given integer type.
///
/// For unsigned types, this masks to the appropriate width.
/// For signed types, this masks then sign-extends back to 64 bits,
/// so that codegen's `*value as i32` / `*value as i64` produces the correct signed value.
fn mask_to_width(value: u64, prim: PrimitiveType) -> u64 {
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
    Some(mask_to_width(lval.wrapping_add(rval), prim))
}

fn eval_int_sub(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(mask_to_width(lval.wrapping_sub(rval), prim))
}

fn eval_int_mul(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    Some(mask_to_width(lval.wrapping_mul(rval), prim))
}

fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None; // avoid division by zero at compile time
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(mask_to_width(lval / rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (lval as i32).wrapping_div(rval as i32);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I64 => {
            let result = (lval as i64).wrapping_div(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None; // avoid division by zero at compile time
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(mask_to_width(lval % rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_rem(rval as i8);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_rem(rval as i16);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (lval as i32).wrapping_rem(rval as i32);
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I64 => {
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
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (value as i16).wrapping_neg();
            Some(mask_to_width(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (value as i32).wrapping_neg();
            Some(mask_to_width(result as u64, prim))
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
    fn test_mask_to_width_unsigned() {
        assert_eq!(mask_to_width(256, PrimitiveType::U8), 0);
        assert_eq!(mask_to_width(255, PrimitiveType::U8), 255);
        assert_eq!(mask_to_width(0x1_0000, PrimitiveType::U16), 0);
        assert_eq!(mask_to_width(0x1_0000_0000, PrimitiveType::U32), 0);
        assert_eq!(mask_to_width(u64::MAX, PrimitiveType::U64), u64::MAX);
    }

    #[test]
    fn test_mask_to_width_signed() {
        // i8: 128 (0x80) sign-extends to -128
        assert_eq!(mask_to_width(128, PrimitiveType::I8) as i64, -128);
        // i8: 127 stays 127
        assert_eq!(mask_to_width(127, PrimitiveType::I8), 127);
        // i16: 0x8000 sign-extends to -32768
        assert_eq!(mask_to_width(0x8000, PrimitiveType::I16) as i64, -32768);
        // i32: 0x8000_0000 sign-extends to -2147483648
        assert_eq!(
            mask_to_width(0x8000_0000, PrimitiveType::I32) as i64,
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
    }

    #[test]
    fn test_mod() {
        assert_eq!(eval_int_mod(10, 3, PrimitiveType::I32), Some(1));
        assert_eq!(eval_int_mod(10, 0, PrimitiveType::I32), None);
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
        assert_eq!(mask_to_width(1_000_000, PrimitiveType::I64), 1_000_000);
        // i64 large value cast to i32 truncates + sign-extends
        assert_eq!(mask_to_width(0x1_0000_0001, PrimitiveType::I32), 1);
        // u8 cast truncates
        assert_eq!(mask_to_width(300, PrimitiveType::U8), 44);
        // Signed cast: -128 as i8
        let neg128 = (-128_i64) as u64;
        assert_eq!(mask_to_width(neg128, PrimitiveType::I8) as i64, -128);
    }
}
