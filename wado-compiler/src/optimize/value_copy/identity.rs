//! Whether a value shares storage with its source on a shallow copy.

use crate::tir::{ResolvedType, TypeId, TypeTable};

pub(in crate::optimize) use crate::lower::plan::value_copy::needs_value_copy;

/// Whether a value of `type_id` can carry another value's identity: an
/// aggregate that shares GC storage when aliased ([`needs_value_copy`]), or a
/// reference to one (`&T` / `&mut T`, which copies the pointer, not the
/// pointee). Primitives (integers, floats, bools, bare enums, unit) cannot.
pub(in crate::optimize) fn carries_identity(type_id: TypeId, type_table: &TypeTable) -> bool {
    needs_value_copy(type_id, type_table)
        || matches!(
            type_table.get(type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        )
}
