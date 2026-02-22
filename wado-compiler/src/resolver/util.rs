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
/// Only checks primitive integer types (i128/u128 struct types are handled separately).
///
/// All literal formats (decimal, hex, octal, binary) use strict numeric range.
/// To reinterpret a bit pattern, use an explicit cast: `0xFF as i8`.
pub(super) fn check_int_range_positive(
    value: u128,
    target_type: TypeId,
    type_table: &TypeTable,
    repr: &str,
) -> Option<String> {
    let base_id = type_table.get_ultimate_base_type(target_type);
    let in_range = match type_table.get(base_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => value <= i8::MAX as u128,
            PrimitiveType::I16 => value <= i16::MAX as u128,
            PrimitiveType::I32 => value <= i32::MAX as u128,
            PrimitiveType::I64 => value <= i64::MAX as u128,
            PrimitiveType::U8 => value <= u128::from(u8::MAX),
            PrimitiveType::U16 => value <= u128::from(u16::MAX),
            PrimitiveType::U32 => value <= u128::from(u32::MAX),
            PrimitiveType::U64 => value <= u128::from(u64::MAX),
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
/// Only checks primitive integer types (i128/u128 struct types are handled separately).
pub(super) fn check_int_range_negative(
    pos_value: u128,
    target_type: TypeId,
    type_table: &TypeTable,
    repr: &str,
) -> Option<String> {
    let base_id = type_table.get_ultimate_base_type(target_type);
    let in_range = match type_table.get(base_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => pos_value <= u128::from(i8::MIN.unsigned_abs()),
            PrimitiveType::I16 => pos_value <= u128::from(i16::MIN.unsigned_abs()),
            PrimitiveType::I32 => pos_value <= u128::from(i32::MIN.unsigned_abs()),
            PrimitiveType::I64 => pos_value <= u128::from(i64::MIN.unsigned_abs()),
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

/// Normalize a numeric literal representation: remove underscores and lowercase.
/// This produces a canonical form for parsing (e.g., `"0x_FF"` → `"0xff"`, `"1E10"` → `"1e10"`).
pub(super) fn normalize_numeric_literal(repr: &str) -> String {
    repr.chars()
        .filter(|&c| c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Parse an unsigned integer literal into a u128 value.
/// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10").
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
pub(super) fn parse_u128_literal(repr: &str) -> Result<u128, String> {
    let clean = normalize_numeric_literal(repr);

    if let Some(hex) = clean.strip_prefix("0x") {
        u128::from_str_radix(hex, 16).map_err(|_| format!("invalid hex literal: {repr}"))
    } else if let Some(bin) = clean.strip_prefix("0b") {
        u128::from_str_radix(bin, 2).map_err(|_| format!("invalid binary literal: {repr}"))
    } else if let Some(oct) = clean.strip_prefix("0o") {
        u128::from_str_radix(oct, 8).map_err(|_| format!("invalid octal literal: {repr}"))
    } else if clean.contains('e') {
        // Scientific notation: parse as f64 first, then convert
        let value: f64 = clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))?;
        if value.fract() != 0.0 {
            return Err(format!("integer literal has fractional part: {repr}"));
        }
        if value < 0.0 || value > u128::MAX as f64 {
            return Err(format!("integer literal out of range: {repr}"));
        }
        Ok(value as u128)
    } else {
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Parse a signed integer literal into an i128 value.
/// Supports decimal, hex, binary, octal, and scientific notation.
/// For non-negative values, delegates to `parse_u128_literal` with an i128 range check.
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
pub(super) fn parse_i128_literal(repr: &str) -> Result<i128, String> {
    let clean = normalize_numeric_literal(repr);

    if clean.starts_with('-') {
        // Scientific notation in negative numbers
        if clean.contains('e') {
            let value: f64 = clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))?;
            if value.fract() != 0.0 {
                return Err(format!("integer literal has fractional part: {repr}"));
            }
            return Ok(value as i128);
        }
        return clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"));
    }

    // Non-negative: delegate to unsigned parser, then check i128 range
    let unsigned = parse_u128_literal(repr)?;
    i128::try_from(unsigned).map_err(|_| format!("integer literal out of range: {repr}"))
}

/// Parse a float literal string into an f64 value.
#[allow(clippy::cast_precision_loss)]
pub(super) fn parse_float_literal(repr: &str) -> Result<f64, String> {
    let clean = normalize_numeric_literal(repr);

    // Handle hex/binary/octal literals as float values (not bit patterns)
    if let Some(hex) = clean.strip_prefix("0x") {
        let value =
            u64::from_str_radix(hex, 16).map_err(|_| format!("invalid hex literal: {repr}"))?;
        return Ok(value as f64);
    } else if let Some(bin) = clean.strip_prefix("0b") {
        let value =
            u64::from_str_radix(bin, 2).map_err(|_| format!("invalid binary literal: {repr}"))?;
        return Ok(value as f64);
    } else if let Some(oct) = clean.strip_prefix("0o") {
        let value =
            u64::from_str_radix(oct, 8).map_err(|_| format!("invalid octal literal: {repr}"))?;
        return Ok(value as f64);
    }

    clean
        .parse()
        .map_err(|_| format!("invalid float literal: {repr}"))
}

/// Check if a number literal can only be a float (has decimal point or negative exponent).
pub(super) fn is_float_only_literal(repr: &str) -> bool {
    if repr.contains('.') {
        return true;
    }

    // Check for negative exponent (e.g., "1e-5")
    let lower = normalize_numeric_literal(repr);
    if let Some(e_pos) = lower.find('e') {
        let after_e = &lower[e_pos + 1..];
        if after_e.starts_with('-') {
            return true;
        }
    }

    false
}

/// Unpack i128 into (low, high) pair for codegen.
#[allow(clippy::cast_sign_loss)]
pub(super) fn unpack_i128(value: i128) -> (u64, i64) {
    (value as u64, (value >> 64) as i64)
}
