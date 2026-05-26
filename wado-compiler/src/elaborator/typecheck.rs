//! Centralized type compatibility checking.
//!
//! This module is the single source of truth for "can type A be used where
//! type B is expected?". All type mismatch checks route through here.

use crate::compiler_host::CompilerHost;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;

/// Result of checking whether `actual` is assignable to `expected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeCheckResult {
    /// Types are compatible (identical, or actual is a subtype/coercible).
    Compatible,
    /// Check was deferred because types involve unresolved generics,
    /// UNKNOWN, or other conditions where we can't decide yet.
    Deferred,
    /// Types are definitely incompatible.
    Incompatible,
}

/// Pure type compatibility check. Does NOT emit errors.
///
/// Rules (in order):
/// 1. Identity: same `TypeId` -> Compatible
/// 2. Bottom/Top/Error: UNKNOWN, ERROR -> Deferred; NEVER -> Compatible
/// 3. Type params: unresolved generics -> Deferred
/// 4. References: &T->non-ref, &T->&mut T -> Incompatible; &mut T->&T -> Compatible
/// 5. Newtype/flags: distinct from base type and each other
/// 6. Option: T vs Option<T>, Option<X> vs Option<Y>
/// 7. Function types: structural comparison of params + return type
/// 8. Generic instances: compare name + type args recursively
/// 9. Catch-all: different concrete types -> Incompatible
pub(super) fn check_assignable(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
) -> TypeCheckResult {
    // Rule 1: Identity
    if actual == expected {
        return TypeCheckResult::Compatible;
    }

    // Rule 2: Bottom/Top/Error
    if actual == TypeTable::UNKNOWN
        || expected == TypeTable::UNKNOWN
        || actual == TypeTable::ERROR
        || expected == TypeTable::ERROR
    {
        return TypeCheckResult::Deferred;
    }
    if actual == TypeTable::NEVER {
        return TypeCheckResult::Compatible;
    }

    // Rule 3: Type params -- defer until monomorphization.
    // Note: We can't reject TypeParam-vs-concrete here because trait impls
    // legitimately pass concrete types where type params are expected
    // (e.g., `I` -> `ArrayIter<T>` in Iterator blanket impls).
    if type_table.contains_type_param(actual) || type_table.contains_type_param(expected) {
        return TypeCheckResult::Deferred;
    }

    // Unwrap references for inner type comparison
    let (actual_inner, actual_is_ref) = unwrap_ref(actual, type_table);
    let (expected_inner, expected_is_ref) = unwrap_ref(expected, type_table);

    // Rule 4: Reference compatibility
    if actual_is_ref && !expected_is_ref {
        // &T -> non-ref: always incompatible (auto-deref is only for method calls)
        return TypeCheckResult::Incompatible;
    }
    if !actual_is_ref && expected_is_ref {
        // non-ref -> &T: always incompatible. The caller must take the
        // reference explicitly with `&` (or `&mut`); the type checker
        // never inserts an implicit ref at a call/assignment boundary.
        return TypeCheckResult::Incompatible;
    }
    // Both refs: check inner compatibility
    if actual_is_ref && expected_is_ref {
        let actual_is_mut = matches!(type_table.get(actual), ResolvedType::MutRef(_));
        let expected_is_mut = matches!(type_table.get(expected), ResolvedType::MutRef(_));
        // &mut T -> &T is ok; &T -> &mut T is not
        if !actual_is_mut && expected_is_mut {
            return TypeCheckResult::Incompatible;
        }
        if actual_inner != expected_inner {
            return check_assignable(actual_inner, expected_inner, type_table);
        }
        return TypeCheckResult::Compatible;
    }

    // Rule 5: Newtype/flags distinctness
    let actual_is_newtype = matches!(
        type_table.get(actual_inner),
        ResolvedType::Newtype { .. } | ResolvedType::Flags { .. }
    );
    let expected_is_newtype = matches!(
        type_table.get(expected_inner),
        ResolvedType::Newtype { .. } | ResolvedType::Flags { .. }
    );
    if (actual_is_newtype || expected_is_newtype) && actual_inner != expected_inner {
        return TypeCheckResult::Incompatible;
    }
    if let ResolvedType::Newtype { base_type, .. } = type_table.get(actual_inner)
        && *base_type == expected_inner
    {
        return TypeCheckResult::Incompatible;
    }
    if let ResolvedType::Newtype { base_type, .. } = type_table.get(expected_inner)
        && *base_type == actual_inner
    {
        return TypeCheckResult::Incompatible;
    }

    // Rule 6: Option compatibility
    let actual_option = type_table.as_option(actual);
    let expected_option = type_table.as_option(expected);
    match (actual_option, expected_option) {
        (Some(actual_t), Some(expected_t)) => {
            if actual_t == TypeTable::UNKNOWN || expected_t == TypeTable::UNKNOWN {
                return TypeCheckResult::Compatible;
            }
            return check_assignable(actual_t, expected_t, type_table);
        }
        (Some(_), None) | (None, Some(_)) => {
            return TypeCheckResult::Incompatible;
        }
        (None, None) => {}
    }

    // Rule 7: Function types -- structural comparison of params + return type.
    // Sub-typing: `fn <: fn mut` -- a read-only closure is assignable to a
    // `fn mut`-typed slot, but not the reverse.
    if let ResolvedType::Function {
        is_mut: actual_is_mut,
        params: actual_params,
        return_type: actual_ret,
        ..
    } = type_table.get(actual_inner)
        && let ResolvedType::Function {
            is_mut: expected_is_mut,
            params: expected_params,
            return_type: expected_ret,
            ..
        } = type_table.get(expected_inner)
    {
        if *actual_is_mut && !*expected_is_mut {
            // `fn mut` cannot widen to `fn`.
            return TypeCheckResult::Incompatible;
        }
        if actual_params.len() != expected_params.len() {
            return TypeCheckResult::Incompatible;
        }
        for (a, e) in actual_params.iter().zip(expected_params.iter()) {
            match check_assignable(*a, *e, type_table) {
                TypeCheckResult::Incompatible => return TypeCheckResult::Incompatible,
                TypeCheckResult::Deferred => return TypeCheckResult::Deferred,
                TypeCheckResult::Compatible => {}
            }
        }
        return check_assignable(*actual_ret, *expected_ret, type_table);
    }
    // One is function, the other isn't -> incompatible
    if matches!(type_table.get(actual_inner), ResolvedType::Function { .. })
        || matches!(
            type_table.get(expected_inner),
            ResolvedType::Function { .. }
        )
    {
        return TypeCheckResult::Incompatible;
    }

    // Rule 8: Generic instances -- compare name + type args
    if let ResolvedType::GenericInstance {
        name: actual_name,
        type_args: actual_args,
        ..
    } = type_table.get(actual_inner)
        && let ResolvedType::GenericInstance {
            name: expected_name,
            type_args: expected_args,
            ..
        } = type_table.get(expected_inner)
    {
        if actual_name != expected_name {
            return TypeCheckResult::Incompatible;
        }
        if actual_args.len() != expected_args.len() {
            return TypeCheckResult::Incompatible;
        }
        for (a, e) in actual_args.iter().zip(expected_args.iter()) {
            match check_assignable(*a, *e, type_table) {
                TypeCheckResult::Incompatible => return TypeCheckResult::Incompatible,
                TypeCheckResult::Deferred => return TypeCheckResult::Deferred,
                TypeCheckResult::Compatible => {}
            }
        }
        return TypeCheckResult::Compatible;
    }

    // Rule 9: General catch-all -- different concrete types
    if actual_inner != expected_inner {
        return TypeCheckResult::Incompatible;
    }

    TypeCheckResult::Compatible
}

/// Unwrap one layer of Ref/MutRef, returning (`inner_type`, `was_ref`).
fn unwrap_ref(type_id: TypeId, type_table: &TypeTable) -> (TypeId, bool) {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => (*inner, true),
        _ => (type_id, false),
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check type mismatch and emit error if incompatible.
    pub(super) fn typecheck(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        let type_table = self.type_table.borrow();
        let result = check_assignable(actual, expected, &type_table);
        if result == TypeCheckResult::Incompatible {
            let expected_name = type_table.type_name(expected);
            let found_name = type_table.type_name(actual);
            drop(type_table);
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: expected_name,
                found: found_name,
                span,
            });
        }
    }

    /// Check return type mismatch and emit error if incompatible.
    ///
    /// Additional allowance: UNIT expected is always compatible (void returns).
    pub(super) fn typecheck_return(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        if expected == TypeTable::UNIT {
            return;
        }
        self.typecheck(actual, expected, span);
    }
}
