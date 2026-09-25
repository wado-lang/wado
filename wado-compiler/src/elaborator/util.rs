//! Utility functions for the elaborator phase.

use crate::ast::Pattern;
use crate::elaborator::stmt::primitive_assoc_const_to_i128;
use crate::escape::{unescape_byte, unescape_char};
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Why the integer literal `repr` of `magnitude`, negated where `negated`, is no
/// value of `target_type`. Every format checks the numeric range: `0xFF as i8` reinterprets.
pub(super) fn int_literal_range_error(
    magnitude: u128,
    negated: bool,
    repr: &str,
    target_type: TypeId,
    type_table: &TypeTable,
) -> Option<String> {
    // Past `i128` is out of range for every type `int_range` answers for.
    let value = match i128::try_from(magnitude) {
        Ok(v) if negated => -v,
        Ok(v) => v,
        Err(_) if negated => i128::MIN,
        Err(_) => i128::MAX,
    };
    let shown = if negated {
        format!("-{repr}")
    } else {
        repr.to_string()
    };
    int_value_range_error(value, &shown, target_type, type_table)
}

/// Why `value`, written `shown`, is no value of the integer `target_type`.
pub(super) fn int_value_range_error(
    value: i128,
    shown: &str,
    target_type: TypeId,
    type_table: &TypeTable,
) -> Option<String> {
    let prim = type_table.primitive_head(target_type)?;
    let (min, max) = prim.int_range()?;
    (value < min || value > max)
        .then(|| format!("literal out of range for `{}`: {shown}", prim.as_str()))
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
pub(super) fn range_endpoint_to_i128(pattern: &Pattern, is_unsigned: bool) -> Option<i128> {
    use crate::ast::{Literal, Pattern};
    match pattern {
        Pattern::Literal(Literal::Number(repr)) => parse_int_bits(repr, is_unsigned).ok(),
        Pattern::Literal(Literal::Char(raw)) => {
            unescape_char(raw).ok().map(|c| i128::from(c as u32))
        }
        Pattern::Literal(Literal::Byte(raw)) => unescape_byte(raw).ok().map(i128::from),
        // An associated constant (`i32::MAX`) resolved by value; a user constant
        // needs the reify-side lookup its caller adds.
        Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } if bindings.is_empty() => {
            primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
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

/// Parse a float literal string into an f64 value.
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
