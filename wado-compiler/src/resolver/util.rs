//! Utility functions for integer literal range checking during type coercion.

use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

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
