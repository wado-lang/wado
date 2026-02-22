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

// Literal parsing functions

/// Parse an integer literal string into a u64 value.
/// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10").
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
pub(super) fn parse_int_literal(repr: &str) -> Result<u64, String> {
    // Remove underscores for parsing
    let clean: String = repr.chars().filter(|&c| c != '_').collect();

    // Handle negative numbers by parsing as i64 and reinterpreting bits
    if clean.starts_with('-') {
        // Check for scientific notation in negative numbers
        if clean.to_lowercase().contains('e') {
            let value: f64 = clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))?;
            if value.fract() != 0.0 {
                return Err(format!("integer literal has fractional part: {repr}"));
            }
            if value < i64::MIN as f64 || value > i64::MAX as f64 {
                return Err(format!("integer literal out of range: {repr}"));
            }
            return Ok((value as i64) as u64);
        }
        let value: i64 = clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))?;
        // Reinterpret i64 bits as u64 for storage
        return Ok(value as u64);
    }

    if clean.starts_with("0x") || clean.starts_with("0X") {
        u64::from_str_radix(&clean[2..], 16).map_err(|_| format!("invalid hex literal: {repr}"))
    } else if clean.starts_with("0b") || clean.starts_with("0B") {
        u64::from_str_radix(&clean[2..], 2).map_err(|_| format!("invalid binary literal: {repr}"))
    } else if clean.starts_with("0o") || clean.starts_with("0O") {
        u64::from_str_radix(&clean[2..], 8).map_err(|_| format!("invalid octal literal: {repr}"))
    } else if clean.to_lowercase().contains('e') {
        // Scientific notation: parse as f64 first, then convert to u64
        let value: f64 = clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))?;
        if value.fract() != 0.0 {
            return Err(format!("integer literal has fractional part: {repr}"));
        }
        if value < 0.0 || value > u64::MAX as f64 {
            return Err(format!("integer literal out of range: {repr}"));
        }
        Ok(value as u64)
    } else {
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Parse a float literal string into an f64 value.
#[allow(clippy::cast_precision_loss)]
pub(super) fn parse_float_literal(repr: &str) -> Result<f64, String> {
    // Remove underscores for parsing
    let clean: String = repr.chars().filter(|&c| c != '_').collect();

    // Handle hex/binary/octal literals as float values (not bit patterns)
    if clean.starts_with("0x") || clean.starts_with("0X") {
        let value = u64::from_str_radix(&clean[2..], 16)
            .map_err(|_| format!("invalid hex literal: {repr}"))?;
        return Ok(value as f64);
    } else if clean.starts_with("0b") || clean.starts_with("0B") {
        let value = u64::from_str_radix(&clean[2..], 2)
            .map_err(|_| format!("invalid binary literal: {repr}"))?;
        return Ok(value as f64);
    } else if clean.starts_with("0o") || clean.starts_with("0O") {
        let value = u64::from_str_radix(&clean[2..], 8)
            .map_err(|_| format!("invalid octal literal: {repr}"))?;
        return Ok(value as f64);
    }

    clean
        .parse()
        .map_err(|_| format!("invalid float literal: {repr}"))
}

/// Check if a number literal can only be a float (has decimal point or negative exponent).
pub(super) fn is_float_only_literal(repr: &str) -> bool {
    // Has decimal point → float only
    if repr.contains('.') {
        return true;
    }

    // Check for negative exponent (e.g., "1e-5")
    let lower = repr.to_lowercase();
    if let Some(e_pos) = lower.find('e') {
        let after_e = &repr[e_pos + 1..];
        if after_e.starts_with('-') {
            return true;
        }
    }

    false
}

/// Parse an unsigned integer literal into a u128 value.
/// Supports decimal, hex, binary, and octal formats.
pub(super) fn parse_u128_literal(repr: &str) -> Result<u128, String> {
    let clean: String = repr.chars().filter(|&c| c != '_').collect();

    if clean.starts_with("0x") || clean.starts_with("0X") {
        u128::from_str_radix(&clean[2..], 16).map_err(|_| format!("invalid hex literal: {repr}"))
    } else if clean.starts_with("0b") || clean.starts_with("0B") {
        u128::from_str_radix(&clean[2..], 2).map_err(|_| format!("invalid binary literal: {repr}"))
    } else if clean.starts_with("0o") || clean.starts_with("0O") {
        u128::from_str_radix(&clean[2..], 8).map_err(|_| format!("invalid octal literal: {repr}"))
    } else {
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Parse a signed integer literal into an i128 value.
/// Supports decimal, hex, binary, and octal formats (negative decimals supported).
pub(super) fn parse_i128_literal(repr: &str) -> Result<i128, String> {
    let clean: String = repr.chars().filter(|&c| c != '_').collect();

    if clean.starts_with("0x") || clean.starts_with("0X") {
        // Hex literals are always positive, parse as u128 then convert
        let unsigned = u128::from_str_radix(&clean[2..], 16)
            .map_err(|_| format!("invalid hex literal: {repr}"))?;
        Ok(unsigned as i128)
    } else if clean.starts_with("0b") || clean.starts_with("0B") {
        let unsigned = u128::from_str_radix(&clean[2..], 2)
            .map_err(|_| format!("invalid binary literal: {repr}"))?;
        Ok(unsigned as i128)
    } else if clean.starts_with("0o") || clean.starts_with("0O") {
        let unsigned = u128::from_str_radix(&clean[2..], 8)
            .map_err(|_| format!("invalid octal literal: {repr}"))?;
        Ok(unsigned as i128)
    } else {
        // Decimal - may be negative
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Unpack u128 into (low, high) pair for codegen.
pub(super) fn unpack_u128(value: u128) -> (u64, u64) {
    (value as u64, (value >> 64) as u64)
}

/// Unpack i128 into (low, high) pair for codegen.
#[allow(clippy::cast_sign_loss)]
pub(super) fn unpack_i128(value: i128) -> (u64, i64) {
    (value as u64, (value >> 64) as i64)
}

/// Get the clean representation of a literal (without underscores).
pub(super) fn clean_literal_repr(repr: &str) -> String {
    repr.chars().filter(|&c| c != '_').collect()
}
