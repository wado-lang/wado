//! Unified type-argument inference engine for generic functions, methods,
//! structs, and variants.
//!
//! The resolver has historically carried several near-duplicate inference
//! routines — one each for function calls, static-method calls, struct
//! literals, variant constructors, and instance method calls — plus a
//! weaker re-inference pass in the monomorphizer. Each copy made slightly
//! different choices about literal-number handling, expected-type driven
//! back-inference, and phantom type-parameter preservation, so bugs were
//! fixed in one place but not the others.
//!
//! This module is the single home of the inference logic those callers
//! share. Phase 0 of the refactor only moves the core structural
//! unifier here — later phases add a richer [`InferCtx`] that owns the
//! whole solve (two-phase literal deferral, expected-type back-inference,
//! phantom parameter preservation) so every caller just builds a set of
//! constraints and asks this module to resolve them.

use std::cell::RefCell;

use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Recursively unify an `expected` type (which may contain `TypeParam` /
/// `TypePack` holes) with an `actual` concrete type, extending `bindings`
/// with `TypeParam TypeId -> concrete TypeId` mappings discovered along
/// the way.
///
/// This is a best-effort, non-failing unifier: mismatches that cannot be
/// bridged are silently ignored so that the caller can try a different
/// argument or fall through to a fallback. The `or_insert` policy ensures
/// that an earlier (higher-confidence) binding is never overwritten by a
/// later weaker one.
///
/// Supported cases:
/// * Direct `TypeParam` on the expected side (binds the type parameter)
/// * Tuple types containing a `TypePack` element (variadic splice)
/// * Same-named `GenericInstance` recursively on type arguments
/// * `Array<K>` matched against a homogeneous tuple literal type
/// * `BuiltinArray<T>` matched against `BuiltinArray<U>`
/// * `Ref<T>` / `MutRef<T>` matched against the same shape
/// * `Function` types matched parameter-by-parameter plus return type
pub(super) fn unify(
    type_table: &RefCell<TypeTable>,
    expected: TypeId,
    actual: TypeId,
    bindings: &mut IndexMap<TypeId, TypeId>,
) {
    let array_name = type_table
        .borrow()
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::Array)
        .to_string();
    let expected_type = type_table.borrow().get(expected).clone();
    let actual_type = type_table.borrow().get(actual).clone();

    match (&expected_type, &actual_type) {
        // Direct type parameter mapping.
        //
        // `or_insert` prevents later fields with self-referential types
        // (like `Array<&Node<K>>`) from overwriting earlier correct
        // mappings (like `K -> String`) with incorrect ones (`K -> K`).
        (ResolvedType::TypeParam { .. }, _) => {
            bindings.entry(expected).or_insert(actual);
        }
        // Tuple types with a type pack: e.g., `[A, ..T, B]` matched
        // against `[i32, String, f64, bool]` binds `A = i32`, `B = bool`,
        // and the pack `T = [String, f64]`.
        //
        // Must come before the generic `GenericInstance` arm so that it
        // matches first.
        (
            ResolvedType::GenericInstance {
                name: expected_name,
                module_source: expected_module_source,
                type_args: expected_elems,
            },
            ResolvedType::GenericInstance {
                name: actual_name,
                module_source: actual_module_source,
                type_args: actual_elems,
            },
        ) if TypeTable::is_tuple_type(expected_name, expected_module_source)
            && TypeTable::is_tuple_type(actual_name, actual_module_source)
            && expected_elems
                .iter()
                .any(|e| matches!(type_table.borrow().get(*e), ResolvedType::TypePack { .. })) =>
        {
            let pack_idx = expected_elems
                .iter()
                .position(|e| matches!(type_table.borrow().get(*e), ResolvedType::TypePack { .. }))
                .unwrap();

            let fixed_before = pack_idx;
            let fixed_after = expected_elems.len() - pack_idx - 1;
            let total_fixed = fixed_before + fixed_after;

            if actual_elems.len() >= total_fixed {
                for i in 0..fixed_before {
                    unify(type_table, expected_elems[i], actual_elems[i], bindings);
                }
                for i in 0..fixed_after {
                    unify(
                        type_table,
                        expected_elems[pack_idx + 1 + i],
                        actual_elems[actual_elems.len() - fixed_after + i],
                        bindings,
                    );
                }
                let pack_elements: Vec<TypeId> =
                    actual_elems[fixed_before..actual_elems.len() - fixed_after].to_vec();
                let pack_tuple = type_table.borrow_mut().make_tuple(pack_elements);
                bindings
                    .entry(expected_elems[pack_idx])
                    .or_insert(pack_tuple);
            }
        }
        // Same-named generic instance (including tuples): unify type
        // arguments positionally.
        (
            ResolvedType::GenericInstance {
                name: expected_name,
                type_args: expected_args,
                ..
            },
            ResolvedType::GenericInstance {
                name: actual_name,
                type_args: actual_args,
                ..
            },
        ) if expected_name == actual_name && expected_args.len() == actual_args.len() => {
            for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                unify(type_table, exp_arg, act_arg, bindings);
            }
        }
        // `Array<K>` (generic instance) matched against a homogeneous
        // tuple literal: infer `K` from the tuple element type.
        (
            ResolvedType::GenericInstance {
                name,
                type_args: expected_args,
                ..
            },
            ResolvedType::GenericInstance {
                name: actual_name,
                module_source: actual_module_source,
                type_args: actual_elems,
            },
        ) if name == &array_name
            && TypeTable::is_tuple_type(actual_name, actual_module_source)
            && expected_args.len() == 1
            && !actual_elems.is_empty() =>
        {
            let first_elem_type = actual_elems[0];
            let all_same = actual_elems.iter().all(|&e| e == first_elem_type);
            if all_same {
                unify(type_table, expected_args[0], first_elem_type, bindings);
            }
        }
        // Raw builtin array (`builtin::array<T>`).
        (ResolvedType::BuiltinArray(expected_elem), ResolvedType::BuiltinArray(actual_elem)) => {
            unify(type_table, *expected_elem, *actual_elem, bindings);
        }
        // References: unify through.
        (ResolvedType::Ref(expected_inner), ResolvedType::Ref(actual_inner))
        | (ResolvedType::MutRef(expected_inner), ResolvedType::MutRef(actual_inner)) => {
            unify(type_table, *expected_inner, *actual_inner, bindings);
        }
        // Function types: unify params and return type.
        (
            ResolvedType::Function {
                params: expected_params,
                return_type: expected_ret,
                ..
            },
            ResolvedType::Function {
                params: actual_params,
                return_type: actual_ret,
                ..
            },
        ) if expected_params.len() == actual_params.len() => {
            for (&exp, &act) in expected_params.iter().zip(actual_params.iter()) {
                unify(type_table, exp, act, bindings);
            }
            unify(type_table, *expected_ret, *actual_ret, bindings);
        }
        // Other cases: no type parameters to extract.
        _ => {}
    }
}

/// A reusable solver for one generic-inference problem.
///
/// `InferCtx` collects unification constraints in three confidence tiers
/// and resolves them in order so that higher-confidence information always
/// wins:
///
/// 1. **Strong (argument-derived)** constraints — added via [`add`]. These
///    are unified immediately into the shared bindings map, so the first
///    typed argument to mention a type parameter pins its binding.
/// 2. **Medium (expected-return)** constraints — added via
///    [`add_expected_return`]. The declaration's return type is unified
///    against the caller's expected type after every strong argument
///    constraint has run. An LHS type annotation
///    (`let x: Array<u16> = Array::filled(n, 0)`) is more precise than
///    the default type of a numeric literal argument, so it is processed
///    before the literal-deferred pass and pins type parameters that no
///    typed argument constrains.
/// 3. **Weak (literal-number)** constraints — added via [`add_deferred`].
///    These are queued and only unified after both stronger tiers have
///    run. Because [`unify`] uses `or_insert`, a literal `0` cannot
///    clobber a binding already produced from a neighbouring typed
///    argument or an LHS annotation. This preserves the
///    `fn two<T>(x: T, y: T); two(1, 2 as u8)` ⇒ `T = u8` behaviour
///    historically only supported for plain function calls and only kicks
///    in when nothing stronger pinned the parameter.
///
/// Use [`solve`](Self::solve) to get the final type arguments in
/// declaration order. Unbound parameters fall back to their original
/// `TypeParam TypeId`. Helper [`solve_with_phantoms`](Self::solve_with_phantoms)
/// additionally reports whether every parameter resolved to a concrete
/// type, so callers can tell unresolved (phantom / forwarded) parameters
/// from bound ones.
pub(super) struct InferCtx<'a> {
    type_table: &'a RefCell<TypeTable>,
    /// Declaration-order `TypeParam` / `TypePack` ids that this solve is
    /// trying to bind. Empty entries mean "no type parameters" — the
    /// caller generally short-circuits before constructing the context.
    params: Vec<TypeId>,
    /// Strong bindings accumulated so far (param `TypeId` -> concrete).
    bindings: IndexMap<TypeId, TypeId>,
    /// Queue of `(expected, actual)` constraints to apply after the
    /// strong pass. Used for literal numbers whose default type would
    /// otherwise lock a type parameter prematurely.
    deferred_args: Vec<(TypeId, TypeId)>,
    /// Queue of `(decl_return, expected)` constraints to apply after the
    /// deferred pass. These allow an annotation at the use site
    /// (`let x: i32 = foo()`) to fix type parameters that never appear in
    /// the argument list.
    expected_returns: Vec<(TypeId, TypeId)>,
}

impl<'a> InferCtx<'a> {
    /// Create a new solver. `params` should be the declaration's type
    /// parameter `TypeId`s in declaration order (skipping effect params).
    pub(super) fn new(type_table: &'a RefCell<TypeTable>, params: Vec<TypeId>) -> Self {
        Self {
            type_table,
            params,
            bindings: IndexMap::default(),
            deferred_args: Vec::new(),
            expected_returns: Vec::new(),
        }
    }

    /// Add a strong constraint, unifying `expected` (which may contain
    /// type parameters) against the concrete `actual` immediately.
    pub(super) fn add(&mut self, expected: TypeId, actual: TypeId) {
        unify(self.type_table, expected, actual, &mut self.bindings);
    }

    /// Queue a weak constraint for the second pass. Used when the actual
    /// type originates from a numeric literal whose default (i32 / f64)
    /// must not override a stronger binding from a typed neighbour.
    pub(super) fn add_deferred(&mut self, expected: TypeId, actual: TypeId) {
        self.deferred_args.push((expected, actual));
    }

    /// Queue an expected-return constraint for the third pass. The
    /// declaration's return type is unified against the caller's expected
    /// type only after every argument-derived binding has been produced.
    pub(super) fn add_expected_return(&mut self, decl_return: TypeId, expected: TypeId) {
        self.expected_returns.push((decl_return, expected));
    }

    /// Run every queued constraint in order and return the final type
    /// arguments in declaration order. Parameters that remain unbound
    /// fall back to their original `TypeParam TypeId`.
    pub(super) fn solve(mut self) -> Vec<TypeId> {
        self.run_deferred_passes();
        self.params
            .iter()
            .map(|param_id| self.bindings.get(param_id).copied().unwrap_or(*param_id))
            .collect()
    }

    /// Run every queued constraint and return both the final type
    /// arguments and the raw bindings map. Callers use the map to tell
    /// "no inference happened" (the param is missing) from "bound to
    /// itself" (which can happen when a generic function forwards its
    /// own type parameter to another generic function and the interned
    /// `TypeId`s coincide) — the returned `Vec<TypeId>` alone cannot
    /// distinguish these two cases.
    pub(super) fn solve_with_bindings(mut self) -> (Vec<TypeId>, IndexMap<TypeId, TypeId>) {
        self.run_deferred_passes();
        let inferred: Vec<TypeId> = self
            .params
            .iter()
            .map(|param_id| self.bindings.get(param_id).copied().unwrap_or(*param_id))
            .collect();
        (inferred, self.bindings)
    }

    fn run_deferred_passes(&mut self) {
        // Pass 2: expected-return driven back-inference. An LHS annotation
        // is more precise than the default type of a numeric literal
        // argument, so it pins type parameters before the literal-deferred
        // pass clobbers them with i32 / f64.
        for (decl_return, expected) in std::mem::take(&mut self.expected_returns) {
            unify(self.type_table, decl_return, expected, &mut self.bindings);
        }
        // Pass 3: literal-number args. Only fills parameters that no
        // stronger constraint already pinned.
        for (expected, actual) in std::mem::take(&mut self.deferred_args) {
            unify(self.type_table, expected, actual, &mut self.bindings);
        }
    }

    /// Run [`solve`](Self::solve) and return arguments paired with a flag
    /// indicating whether every parameter resolved. Callers that can
    /// tolerate unresolved parameters (e.g. generic struct fields with
    /// phantom parameters, or a generic function forwarding one of its
    /// own type parameters) can still use the partial result; callers
    /// that require fully-concrete type args can check the flag.
    pub(super) fn solve_with_phantoms(self) -> (Vec<TypeId>, bool) {
        let type_table = self.type_table;
        let inferred = self.solve();
        let all_concrete = inferred.iter().all(|&id| {
            !matches!(
                type_table.borrow().get(id),
                ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
            )
        });
        (inferred, all_concrete)
    }
}
