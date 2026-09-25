//! Utility functions for the elaborator phase.

use crate::ast::Pattern;
use crate::elaborator::stmt::primitive_assoc_const_to_i128;
use crate::escape::{unescape_byte, unescape_char};
use crate::primitive::PrimitiveType;
use crate::resolve::Resolutions;
use crate::tir::{ResolvedType, TypeId, TypeTable};

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
    let base_id = type_table.representation_head(target_type);
    let in_range = match type_table.get(base_id) {
        // i128/u128/f32/f64/bool/char are handled elsewhere.
        ResolvedType::Primitive(prim) => value <= prim.int_max()?,
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
    let base_id = type_table.representation_head(target_type);
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

/// An integer literal's 128 bits, read as signed or unsigned per `is_unsigned`.
/// Only the unsigned read reaches a value above `i128::MAX`.
pub(super) fn parse_int_bits(repr: &str, is_unsigned: bool) -> Result<i128, String> {
    if is_unsigned {
        parse_u128_literal(repr).map(u128::cast_signed)
    } else {
        parse_i128_literal(repr)
    }
}

/// A range-pattern endpoint's value, as the bits of the scrutinee's own type.
/// `None` for an endpoint denoting no integer, which annotate diagnoses.
pub(super) fn range_endpoint_to_i128(
    pattern: &Pattern,
    is_unsigned: bool,
    resolutions: &Resolutions,
) -> Option<i128> {
    use crate::ast::{Literal, Pattern};
    match pattern {
        Pattern::Literal(Literal::Number(repr)) => parse_int_bits(repr, is_unsigned).ok(),
        Pattern::Literal(Literal::Char(raw)) => {
            unescape_char(raw).ok().map(|c| i128::from(c as u32))
        }
        Pattern::Literal(Literal::Byte(raw)) => unescape_byte(raw).ok().map(i128::from),
        // Only a primitive's bound (`i32::MAX`); a user constant is no endpoint.
        Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } if bindings.is_empty() => {
            primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name, resolutions)
        }
        _ => None,
    }
}

/// Order two endpoints [`range_endpoint_to_i128`] returned. An unsigned bound
/// above `i128::MAX` reads negative, so a signed compare would call it reversed.
pub(super) fn range_endpoints_ordered(
    start: i128,
    end: i128,
    is_unsigned: bool,
) -> std::cmp::Ordering {
    if is_unsigned {
        start.cast_unsigned().cmp(&end.cast_unsigned())
    } else {
        start.cmp(&end)
    }
}

/// Parse an unsigned integer literal into a u128 value.
/// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10").
pub(crate) fn parse_u128_literal(repr: &str) -> Result<u128, String> {
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
pub(crate) fn parse_i128_literal(repr: &str) -> Result<i128, String> {
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

/// The name a type carries its trait bounds under, where it carries any.
/// A pack answers with its own name: a pack's bound holds of each member
/// (WEP 2026-03-14), and `type_param_bounds` keys both the same way.
pub(super) fn bound_param_name(resolved: &ResolvedType) -> Option<&String> {
    match resolved {
        ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => Some(name),
        _ => None,
    }
}

/// Unpack i128 into (low, high) pair for codegen.
pub(super) fn unpack_i128(value: i128) -> (u64, i64) {
    (value as u64, (value >> 64) as i64)
}

/// Run `body` with `owner`'s `field` set to `value`, answering its result and
/// what the field then held. The enclosing value returns even on a panic.
pub(super) fn replaced<O, T, R>(
    owner: &mut O,
    field: for<'s> fn(&'s mut O) -> &'s mut T,
    value: T,
    body: impl FnOnce(&mut O) -> R,
) -> (R, T) {
    struct Restore<'r, O, T> {
        owner: &'r mut O,
        field: for<'s> fn(&'s mut O) -> &'s mut T,
        saved: Option<T>,
    }
    impl<O, T> Drop for Restore<'_, O, T> {
        fn drop(&mut self) {
            if let Some(saved) = self.saved.take() {
                *(self.field)(self.owner) = saved;
            }
        }
    }
    let saved = std::mem::replace(field(owner), value);
    let mut guard = Restore {
        owner,
        field,
        saved: Some(saved),
    };
    let result = body(guard.owner);
    let saved = guard.saved.take().expect("enclosing value present");
    let left = std::mem::replace(field(guard.owner), saved);
    (result, left)
}
