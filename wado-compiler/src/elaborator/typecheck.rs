//! Centralized type compatibility checking.
//!
//! This module is the single source of truth for "can type A be used where
//! type B is expected?". All type mismatch checks route through here.
//!
//! # Layering
//!
//! The check is split across three layers, each more host-aware than the
//! last:
//!
//! 1. [`check_assignable`] — pure function over the type table. Returns
//!    [`TypeCheckResult`] (`Compatible` / `Deferred` / `Incompatible`).
//!    No diagnostics, no host.
//! 2. [`TypeSystem::typecheck`] and friends — type-system operations that
//!    build a [`TypeMismatchPayload`] (the pretty-printed `expected` /
//!    `found` names) on rejection. Still host-agnostic — `TypeSystem` does
//!    not carry `<H: CompilerHost>`.
//! 3. [`Elaborator::typecheck`] and friends — thin wrappers that call (2)
//!    and emit a [`TypeError::TypeMismatch`] diagnostic via `self.logger`
//!    on rejection. This is the layer callers in the body walk use.
//!
//! Layers 2 and 3 each come in three flavours: the plain check, the
//! `_return` one (`UNIT` expected always passes), and the `_opaque` one
//! ([`ParamBinding`]).
//!
//! Keeping the `<H>` plumbing confined to layer 3 means future
//! `TypeSystem` operations that report errors can mirror this split
//! without bleeding the host trait into the type-system API.

use crate::compiler_host::CompilerHost;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;
use super::tysys::TypeSystem;

/// Whether the type parameters inside `expected` can still be substituted.
///
/// The two questions "does a concrete type instantiate this parameter?" and
/// "is this parameter opaque here?" are distinct, and one predicate over the
/// type table cannot answer both — the caller knows which it is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParamBinding {
    /// `expected` names the parameters of a signature being instantiated (a
    /// callee's parameter list, a trait impl's self type). Substitution can
    /// still make the comparison hold, so a concrete actual defers.
    Instantiable,
    /// `expected` was written in the scope that binds its parameters — a
    /// `let` annotation inside the generic body, say. Nothing later
    /// substitutes them, so they compare as opaque leaves.
    Opaque,
}

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
/// 3. Type params: unresolved generics -> Deferred (see [`ParamBinding`])
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
    check_assignable_with(actual, expected, type_table, ParamBinding::Instantiable)
}

/// [`check_assignable`] with the caller stating whether `expected`'s type
/// parameters are still instantiable (see [`ParamBinding`]).
pub(super) fn check_assignable_with(
    actual: TypeId,
    expected: TypeId,
    type_table: &TypeTable,
    binding: ParamBinding,
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

    // Rule 3: Type params -- defer until monomorphization, except where
    // substitution cannot rescue the comparison.
    //
    // A bare parameter is opaque inside the body that declares it: whatever a
    // caller instantiates `T` to, a `T` in hand is not a `String`. Deferring
    // that never revisits it, so a `-> String` body returning its `T` reached
    // monomorphization and emitted a `u32` against the declared signature.
    //
    // Only this direction is decidable from the types alone. The other one --
    // concrete where a parameter is expected -- is instantiation when the
    // parameter belongs to a signature being instantiated (`ListIter<T>` for
    // `I` in the `Iterator` blanket impl), and a plain mismatch when it is
    // opaque here (`let x: T = 5` in the body that declares `T`). Only the
    // caller knows which, so `binding` decides it.
    if matches!(type_table.get(actual), ResolvedType::TypeParam { .. })
        && !type_table.contains_type_param(expected)
    {
        return TypeCheckResult::Incompatible;
    }
    let expected_is_open =
        binding == ParamBinding::Instantiable && type_table.contains_type_param(expected);
    if type_table.contains_type_param(actual) || expected_is_open {
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
            return check_assignable_with(actual_inner, expected_inner, type_table, binding);
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
            return check_assignable_with(actual_t, expected_t, type_table, binding);
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
            match check_assignable_with(*a, *e, type_table, binding) {
                TypeCheckResult::Incompatible => return TypeCheckResult::Incompatible,
                TypeCheckResult::Deferred => return TypeCheckResult::Deferred,
                TypeCheckResult::Compatible => {}
            }
        }
        return check_assignable_with(*actual_ret, *expected_ret, type_table, binding);
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
            match check_assignable_with(*a, *e, type_table, binding) {
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
        self.typecheck_binding(actual, expected, ParamBinding::Instantiable)
    }

    /// [`Self::typecheck`] for an `expected` written in the scope that binds
    /// its type parameters, so they are opaque here — see [`ParamBinding`].
    pub(crate) fn typecheck_opaque(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Result<(), TypeMismatchPayload> {
        self.typecheck_binding(actual, expected, ParamBinding::Opaque)
    }

    fn typecheck_binding(
        &self,
        actual: TypeId,
        expected: TypeId,
        binding: ParamBinding,
    ) -> Result<(), TypeMismatchPayload> {
        let type_table = self.type_table.borrow();
        match check_assignable_with(actual, expected, &type_table, binding) {
            TypeCheckResult::Incompatible => Err(TypeMismatchPayload {
                expected: type_table.type_name(expected),
                found: type_table.type_name(actual),
            }),
            TypeCheckResult::Compatible | TypeCheckResult::Deferred => Ok(()),
        }
    }

    /// Host-agnostic return-type check. `UNIT` expected always succeeds
    /// (void returns); otherwise delegates to [`TypeSystem::typecheck`].
    ///
    /// Not [`Self::typecheck_opaque`], even though a function's declared
    /// return type is written by the item that binds its parameters: a
    /// closure body shares this path, and there the expected return type is
    /// the *callee's* parameter (`Acc` in `fold(0, |acc, x| …)`), which
    /// inference still substitutes.
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

    /// [`Self::typecheck`] for an expected type the current scope wrote
    /// itself — a `let` annotation. Its type parameters are the enclosing
    /// item's own, so a concrete value does not instantiate them
    /// ([`ParamBinding::Opaque`]).
    pub(super) fn typecheck_opaque(&self, actual: TypeId, expected: TypeId, span: Span) {
        if let Err(payload) = self.tysys.typecheck_opaque(actual, expected) {
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
