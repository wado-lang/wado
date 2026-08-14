//! Unified type-argument inference for generic functions, methods, structs, and
//! variants — the single home of logic that was once duplicated per caller, each
//! copy diverging on literal-number handling, expected-type back-inference, and
//! phantom parameter preservation. A caller builds constraints and asks
//! [`InferCtx`] to solve them.

use std::cell::RefCell;

use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Recursively unify an `expected` type, possibly holding `TypeParam` /
/// `TypePack` holes, against a concrete `actual`, extending `bindings` with what
/// it learns. Best-effort and non-failing: an unbridgeable mismatch is ignored
/// so the caller can try another argument, and `or_insert` keeps an earlier,
/// higher-confidence binding from being overwritten by a weaker one.
pub(super) fn unify(
    type_table: &RefCell<TypeTable>,
    expected: TypeId,
    actual: TypeId,
    bindings: &mut IndexMap<TypeId, TypeId>,
) {
    let list_name = type_table
        .borrow()
        .compiler_struct_name(crate::compiler_item::CompilerItem::List)
        .to_string();
    let expected_type = type_table.borrow().get(expected).clone();
    let actual_type = type_table.borrow().get(actual).clone();

    match (&expected_type, &actual_type) {
        // Direct type parameter mapping.
        //
        // `or_insert` prevents later fields with self-referential types
        // (like `List<&Node<K>>`) from overwriting earlier correct
        // mappings (like `K -> String`) with incorrect ones (`K -> K`).
        (ResolvedType::TypeParam { .. } | ResolvedType::InferVar(_), _) => {
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
                def: expected_def,
                type_args: expected_elems,
            },
            ResolvedType::GenericInstance {
                def: actual_def,
                type_args: actual_elems,
            },
        ) if TypeTable::is_tuple_type(type_table.borrow().def_name(*expected_def))
            && TypeTable::is_tuple_type(type_table.borrow().def_name(*actual_def))
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
                def: expected_def,
                type_args: expected_args,
            },
            ResolvedType::GenericInstance {
                def: actual_def,
                type_args: actual_args,
            },
        ) if expected_def == actual_def && expected_args.len() == actual_args.len() => {
            for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                unify(type_table, exp_arg, act_arg, bindings);
            }
        }
        // `List<K>` (generic instance) matched against a homogeneous
        // tuple literal: infer `K` from the tuple element type.
        (
            ResolvedType::GenericInstance {
                def,
                type_args: expected_args,
            },
            ResolvedType::GenericInstance {
                def: actual_def,
                type_args: actual_elems,
            },
        ) if type_table.borrow().def_name(*def) == list_name
            && TypeTable::is_tuple_type(type_table.borrow().def_name(*actual_def))
            && expected_args.len() == 1
            && !actual_elems.is_empty() =>
        {
            let first_elem_type = actual_elems[0];
            let all_same = actual_elems.iter().all(|&e| e == first_elem_type);
            if all_same {
                unify(type_table, expected_args[0], first_elem_type, bindings);
            }
        }
        // Raw builtin array (`Array<T>`).
        (ResolvedType::BuiltinArray(expected_elem), ResolvedType::BuiltinArray(actual_elem)) => {
            unify(type_table, *expected_elem, *actual_elem, bindings);
        }
        // References: unify through. `&mut T` where `&T` is expected is the
        // one direction assignability allows, so it must bind the same way —
        // otherwise `array_len(self)` inside a `&mut self` method leaves the
        // element type unbound.
        (
            ResolvedType::Ref(expected_inner),
            ResolvedType::Ref(actual_inner) | ResolvedType::MutRef(actual_inner),
        )
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

/// A reusable solver for one generic-inference problem, collecting constraints
/// in three confidence tiers and resolving them in order: argument-derived
/// ([`add`]), the declared return against the caller's expected type
/// ([`add_expected_return`]), then queued numeric literals ([`add_deferred`]).
/// `or_insert` keeps a literal from clobbering a typed neighbour.
///
/// [`solve`](Self::solve) returns the type arguments in declaration order. An
/// unbound slot comes back as the inference variable the use site registered, so
/// "unsolved" is `answer == var`.
pub(super) struct InferCtx<'a> {
    type_table: &'a RefCell<TypeTable>,
    /// What this solve is trying to bind, in declaration order: a use site's
    /// inference variables, or the declaration's own slots where instantiation
    /// was declined. Empty means "no type parameters" — the caller generally
    /// short-circuits before constructing the context.
    params: Vec<TypeId>,
    /// Strong bindings accumulated so far (variable / slot -> answer).
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
}
