//! Utility functions for the resolver phase.

use crate::ast::{BinaryOp, UnaryOp};
use crate::tir::{PrimitiveType, ResolvedType, TirBinaryOp, TirUnaryOp, TypeId, TypeTable};

/// Convert AST `BinaryOp` to TIR `BinaryOp`.
pub(super) fn convert_binary_op(op: BinaryOp) -> TirBinaryOp {
    match op {
        BinaryOp::Add => TirBinaryOp::Add,
        BinaryOp::Sub => TirBinaryOp::Sub,
        BinaryOp::Mul => TirBinaryOp::Mul,
        BinaryOp::Div => TirBinaryOp::Div,
        BinaryOp::Mod => TirBinaryOp::Mod,
        BinaryOp::Eq => TirBinaryOp::Eq,
        BinaryOp::NotEq => TirBinaryOp::NotEq,
        BinaryOp::Lt => TirBinaryOp::Lt,
        BinaryOp::LtEq => TirBinaryOp::LtEq,
        BinaryOp::Gt => TirBinaryOp::Gt,
        BinaryOp::GtEq => TirBinaryOp::GtEq,
        BinaryOp::And => TirBinaryOp::And,
        BinaryOp::Or => TirBinaryOp::Or,
        BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        BinaryOp::BitOr => TirBinaryOp::BitOr,
        BinaryOp::BitXor => TirBinaryOp::BitXor,
        BinaryOp::Shl => TirBinaryOp::Shl,
        BinaryOp::Shr => TirBinaryOp::Shr,
    }
}

/// Convert AST `UnaryOp` to TIR `UnaryOp`.
pub(super) fn convert_unary_op(op: UnaryOp) -> TirUnaryOp {
    match op {
        UnaryOp::Neg => TirUnaryOp::Neg,
        UnaryOp::Not => TirUnaryOp::Not,
        UnaryOp::BitNot => TirUnaryOp::BitNot,
        UnaryOp::Ref => TirUnaryOp::Ref,
        UnaryOp::MutRef => TirUnaryOp::MutRef,
        UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Check if a positive integer literal value fits in the target integer type.
/// Returns `Some(error_message)` if out of range, `None` if OK.
/// Only checks primitive integer types (not i128/u128, which are handled separately).
///
/// All literal formats (decimal, hex, octal, binary) use strict numeric range.
/// To reinterpret a bit pattern, use an explicit cast: `0xFF as i8`.
pub(super) fn check_int_range_positive(
    value: u64,
    target_type: TypeId,
    type_table: &TypeTable,
    repr: &str,
) -> Option<String> {
    let base_id = type_table.get_ultimate_base_type(target_type);
    let in_range = match type_table.get(base_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => i8::try_from(value).is_ok(),
            PrimitiveType::I16 => i16::try_from(value).is_ok(),
            PrimitiveType::I32 => i32::try_from(value).is_ok(),
            PrimitiveType::I64 => i64::try_from(value).is_ok(),
            PrimitiveType::U8 => u8::try_from(value).is_ok(),
            PrimitiveType::U16 => u16::try_from(value).is_ok(),
            PrimitiveType::U32 => u32::try_from(value).is_ok(),
            PrimitiveType::U64 => true,
            _ => return None, // i128/u128/f32/f64/bool/char handled elsewhere
        },
        _ => return None,
    };
    if in_range {
        None
    } else {
        let type_name = match type_table.get(base_id) {
            ResolvedType::Primitive(prim) => prim.as_str(),
            _ => "unknown",
        };
        Some(format!("literal out of range for `{type_name}`: {repr}"))
    }
}

/// Check if a negated integer literal `-pos_value` fits in the target integer type.
/// Returns `Some(error_message)` if out of range, `None` if OK.
/// Only checks primitive integer types (not i128/u128, which are handled separately).
pub(super) fn check_int_range_negative(
    pos_value: u64,
    target_type: TypeId,
    type_table: &TypeTable,
    repr: &str,
) -> Option<String> {
    let base_id = type_table.get_ultimate_base_type(target_type);
    let in_range = match type_table.get(base_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => pos_value <= i64::from(i8::MIN).unsigned_abs(),
            PrimitiveType::I16 => pos_value <= i64::from(i16::MIN).unsigned_abs(),
            PrimitiveType::I32 => pos_value <= i64::from(i32::MIN).unsigned_abs(),
            PrimitiveType::I64 => pos_value <= i64::MIN.unsigned_abs(),
            // Unsigned types cannot hold negative values
            PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
                false
            }
            _ => return None, // i128/u128/f32/f64/bool/char handled elsewhere
        },
        _ => return None,
    };
    if in_range {
        None
    } else {
        let type_name = match type_table.get(base_id) {
            ResolvedType::Primitive(prim) => prim.as_str(),
            _ => "unknown",
        };
        Some(format!("literal out of range for `{type_name}`: -{repr}"))
    }
}
