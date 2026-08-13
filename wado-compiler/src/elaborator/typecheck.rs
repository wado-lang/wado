//! The single source of truth for "can type A be used where type B is
//! expected?", in three layers of increasing host-awareness: the pure
//! [`check_assignable`], then [`TypeSystem::typecheck`] adding a
//! [`TypeMismatchPayload`], then [`Elaborator::typecheck`] emitting the
//! diagnostic. The last two each have a `_return` flavour where `UNIT` passes.

use crate::compiler_host::CompilerHost;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;
use super::tysys::TypeSystem;

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

/// Pure type compatibility check, emitting no errors. Rules apply in order:
/// identity; `NEVER` compatible and `UNKNOWN` / `ERROR` deferred; anything
/// genuinely undecided deferred; reference variance (`&mut T` → `&T` only);
/// newtypes and flags distinct from their base; `Option`; structural comparison
/// for function types and generic instances; anything else incompatible.
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

    // Defer only what is genuinely undecided: an inference variable awaiting its
    // solver, a pack awaiting expansion, a projection awaiting its impl, and
    // `unknown` / `error`. A rigid `TypeParam` is opaque, not undecided — a use
    // of a polymorphic signature instantiates its slots into `InferVar`s first,
    // so nothing but itself is ever assignable to it.
    if type_table.contains_undecided(actual) || type_table.contains_undecided(expected) {
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
        stores: actual_stores,
        ..
    } = type_table.get(actual_inner)
        && let ResolvedType::Function {
            is_mut: expected_is_mut,
            params: expected_params,
            return_type: expected_ret,
            stores: expected_stores,
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
        if actual_stores.iter().any(|p| !expected_stores.contains(p)) {
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

/// Pretty-printed payload carried when [`TypeSystem::typecheck`] /
/// [`TypeSystem::typecheck_return`] rejects an assignment.
///
/// The strings are materialised at the rejection site (while the
/// [`TypeTable`] is still borrowed) so the caller can drop the type
/// table before emitting the diagnostic. Both fields use the same
/// [`TypeTable::type_name`] formatting that the original
/// `TypeError::TypeMismatch` diagnostic used.
#[derive(Debug, Clone)]
pub(crate) struct TypeMismatchPayload {
    pub(crate) expected: String,
    pub(crate) found: String,
}

impl TypeSystem {
    /// Host-agnostic type-mismatch check. Returns `Ok(())` if `actual`
    /// is assignable to `expected` (or if the answer is deferred —
    /// unknown types, unresolved generics — which the body walk
    /// re-checks once the type is known); returns `Err` with the
    /// pretty-printed `expected` / `found` names on a definite reject.
    ///
    /// This is layer 2 of [the typecheck stack](self): callers that
    /// want a diagnostic emitted should go through
    /// [`Elaborator::typecheck`].
    pub(crate) fn typecheck(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Result<(), TypeMismatchPayload> {
        let type_table = self.type_table.borrow();
        match check_assignable(actual, expected, &type_table) {
            TypeCheckResult::Incompatible => Err(TypeMismatchPayload {
                expected: type_table.type_name(expected),
                found: type_table.type_name(actual),
            }),
            TypeCheckResult::Compatible | TypeCheckResult::Deferred => Ok(()),
        }
    }

    /// Host-agnostic return-type check. `UNIT` expected always succeeds
    /// (void returns); otherwise delegates to [`TypeSystem::typecheck`].
    pub(crate) fn typecheck_return(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Result<(), TypeMismatchPayload> {
        if expected == TypeTable::UNIT {
            return Ok(());
        }
        self.typecheck(actual, expected)
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Check type mismatch and emit a [`TypeError::TypeMismatch`]
    /// diagnostic via [`Self::logger`] on rejection.
    ///
    /// Layer 3 wrapper over [`TypeSystem::typecheck`]: confines the
    /// `<H: CompilerHost>` plumbing to the `Elaborator` boundary so
    /// `TypeSystem` itself stays host-agnostic.
    pub(super) fn typecheck(&self, actual: TypeId, expected: TypeId, span: Span) {
        if let Err(payload) = self.tysys.typecheck(actual, expected) {
            let _ = self.emit(TypeError::TypeMismatch {
                expected: payload.expected,
                found: payload.found,
                span,
            });
        }
    }

    /// Check return type mismatch and emit a diagnostic on rejection.
    ///
    /// `UNIT` expected is always compatible (void returns); otherwise
    /// delegates to [`Self::typecheck`]'s emit path.
    pub(super) fn typecheck_return(&self, actual: TypeId, expected: TypeId, span: Span) {
        if let Err(payload) = self.tysys.typecheck_return(actual, expected) {
            let _ = self.emit(TypeError::TypeMismatch {
                expected: payload.expected,
                found: payload.found,
                span,
            });
        }
    }
}
