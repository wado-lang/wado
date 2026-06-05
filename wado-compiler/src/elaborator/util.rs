//! Utility functions for the elaborator phase.

use crate::tir::{PrimitiveType, ResolvedType, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::token::Span;

/// Stage 7-B placeholder. The combined walk records facts and returns
/// `TypeId`; reify is the sole TIR producer. The few combined-walk sites that
/// hand an already-resolved operand to a reify-shared builder (which still
/// takes a `TirExpr`) wrap the resolved type with this `Unit` sentinel — only
/// its `type_id` / `span` are ever read.
pub(super) fn placeholder(type_id: TypeId, span: Span) -> TirExpr {
    TirExpr::new(TirExprKind::Unit, type_id, span)
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

/// Interpret the raw content of a string literal (between quotes, without the quotes).
///
/// Processes all escape sequences and handles surrogate pairs (`\uD800\uDC00` style).
/// Returns the resulting Rust `String`, or an error message.
pub(super) fn unescape_string(raw: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    let mut pending_high: Option<u16> = None;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let escaped = unescape_one(&mut chars)?;
            if let Some(code_unit) = as_surrogate_code_unit(escaped) {
                if is_high_surrogate(code_unit) {
                    if pending_high.is_some() {
                        return Err(
                            "invalid surrogate pair: high surrogate not followed by low surrogate"
                                .to_string(),
                        );
                    }
                    pending_high = Some(code_unit);
                    continue;
                } else if is_low_surrogate(code_unit)
                    && let Some(high) = pending_high.take()
                {
                    let combined = decode_surrogate_pair(high, code_unit);
                    let c = char::from_u32(combined)
                        .ok_or_else(|| "invalid surrogate pair".to_string())?;
                    result.push(c);
                    continue;
                }
            }
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(escaped);
        } else {
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(ch);
        }
    }
    if pending_high.is_some() {
        return Err("invalid surrogate pair: high surrogate at end of string".to_string());
    }
    Ok(result)
}

/// Interpret the raw content of a char literal (between quotes, without the quotes).
///
/// Returns the resulting `char`, or an error message.
pub(super) fn unescape_char(raw: &str) -> Result<char, String> {
    let mut chars = raw.chars().peekable();
    let result = match chars.next() {
        Some('\\') => unescape_one(&mut chars)?,
        Some(c) => c,
        None => return Err("empty char literal".to_string()),
    };
    if chars.next().is_some() {
        return Err("char literal contains more than one character".to_string());
    }
    Ok(result)
}

/// Unescape a template string literal part (raw text between backticks).
///
/// Like `unescape_string` but also handles template-specific escapes: `\{` and `\}`.
pub(super) fn unescape_template_string(raw: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    let mut pending_high: Option<u16> = None;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // Handle template-specific escapes first
            if let Some(&next) = chars.peek()
                && (next == '{' || next == '}')
            {
                if pending_high.is_some() {
                    return Err(
                        "invalid surrogate pair: high surrogate not followed by low surrogate"
                            .to_string(),
                    );
                }
                chars.next();
                result.push(next);
                continue;
            }
            let escaped = unescape_one(&mut chars)?;
            if let Some(code_unit) = as_surrogate_code_unit(escaped) {
                if is_high_surrogate(code_unit) {
                    if pending_high.is_some() {
                        return Err(
                            "invalid surrogate pair: high surrogate not followed by low surrogate"
                                .to_string(),
                        );
                    }
                    pending_high = Some(code_unit);
                    continue;
                } else if is_low_surrogate(code_unit)
                    && let Some(high) = pending_high.take()
                {
                    let combined = decode_surrogate_pair(high, code_unit);
                    let c = char::from_u32(combined)
                        .ok_or_else(|| "invalid surrogate pair".to_string())?;
                    result.push(c);
                    continue;
                }
            }
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(escaped);
        } else {
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(ch);
        }
    }
    if pending_high.is_some() {
        return Err("invalid surrogate pair: high surrogate at end of string".to_string());
    }
    Ok(result)
}

/// Parse one escape sequence (after the leading `\` has been consumed).
fn unescape_one<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
) -> Result<char, String> {
    match chars.next() {
        Some('n') => Ok('\n'),
        Some('t') => Ok('\t'),
        Some('r') => Ok('\r'),
        Some('\\') => Ok('\\'),
        Some('"') => Ok('"'),
        Some('\'') => Ok('\''),
        Some('/') => Ok('/'),
        Some('b') => Ok('\x08'),
        Some('f') => Ok('\x0C'),
        Some('0') => Ok('\0'),
        Some('u') => unescape_unicode(chars),
        Some(c) => Err(format!("invalid escape sequence: \\{c}")),
        None => Err("unterminated escape sequence".to_string()),
    }
}

fn unescape_unicode<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
) -> Result<char, String> {
    if chars.peek() == Some(&'{') {
        chars.next(); // consume '{'
        let mut hex = String::new();
        loop {
            match chars.next() {
                Some('}') => break,
                Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                Some(c) => return Err(format!("invalid character in unicode escape: {c}")),
                None => return Err("unterminated unicode escape".to_string()),
            }
        }
        if hex.is_empty() {
            return Err("empty unicode escape".to_string());
        }
        let code_point = u32::from_str_radix(&hex, 16)
            .map_err(|_| format!("invalid unicode escape: \\u{{{hex}}}"))?;
        char::from_u32(code_point)
            .ok_or_else(|| format!("invalid unicode code point: U+{code_point:04X}"))
    } else {
        let mut hex = String::new();
        for _ in 0..4 {
            match chars.next() {
                Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                _ => return Err("expected 4 hex digits after \\u".to_string()),
            }
        }
        let code_unit = u16::from_str_radix(&hex, 16)
            .map_err(|_| format!("invalid unicode escape: \\u{hex}"))?;
        if is_high_surrogate(code_unit) || is_low_surrogate(code_unit) {
            // Encode surrogate as PUA so the caller can detect and combine pairs
            let pua = if is_high_surrogate(code_unit) {
                u32::from(code_unit - 0xD800) + 0xE000
            } else {
                u32::from(code_unit - 0xDC00) + 0xE800
            };
            Ok(char::from_u32(pua).unwrap())
        } else {
            char::from_u32(u32::from(code_unit))
                .ok_or_else(|| format!("invalid unicode code point: U+{code_unit:04X}"))
        }
    }
}

fn as_surrogate_code_unit(c: char) -> Option<u16> {
    let code = c as u32;
    if (0xE000..=0xE7FF).contains(&code) {
        Some((code - 0xE000 + 0xD800) as u16)
    } else if (0xE800..=0xEBFF).contains(&code) {
        Some((code - 0xE800 + 0xDC00) as u16)
    } else {
        None
    }
}

fn is_high_surrogate(code_unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&code_unit)
}

fn is_low_surrogate(code_unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&code_unit)
}

fn decode_surrogate_pair(high: u16, low: u16) -> u32 {
    let high = u32::from(high - 0xD800);
    let low = u32::from(low - 0xDC00);
    0x10000 + (high << 10) + low
}

/// Parse an unsigned integer literal into a u128 value.
/// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10").
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
pub(super) fn unpack_i128(value: i128) -> (u64, i64) {
    (value as u64, (value >> 64) as i64)
}
