//! Structural matching of a type holding parameters or variables against a
//! concrete one. Calls are solved in the inference context, not here.

use std::cell::RefCell;

use crate::hashmap::IndexMap;
use crate::tir::{InferVarId, ResolvedType, StructDef, TypeId};
use crate::type_table::TypeTable;

/// The answers among `bindings` that [`unify`] gave inference variables, keyed
/// for [`TypeTable::substitute_infer_vars`].
pub(super) fn infer_var_answers(
    type_table: &TypeTable,
    bindings: impl IntoIterator<Item = (TypeId, TypeId)>,
) -> IndexMap<InferVarId, TypeId> {
    bindings
        .into_iter()
        .filter_map(|(var, answer)| match *type_table.get(var) {
            ResolvedType::InferVar(id) => Some((id, answer)),
            _ => None,
        })
        .collect()
}

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
    let expected_type = type_table.borrow().get(expected).clone();
    let actual_type = type_table.borrow().get(actual).clone();

    match (&expected_type, &actual_type) {
        // A pack stands for a list of types, so it binds to a tuple and nothing
        // else. Without this arm a turbofish or annotation could settle no pack.
        (
            ResolvedType::TypePack {
                mapped_elem: None, ..
            },
            ResolvedType::Tuple(_),
        ) => {
            bindings.entry(expected).or_insert(actual);
        }
        // A variable met by itself, as `null` typed `Option<$T>` under an
        // expected `Option<$T>` is, answers nothing, and must leave the slot
        // for a later field to settle.
        (ResolvedType::InferVar(_), _) if expected == actual => {}
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
        (ResolvedType::Tuple(expected_elems), ResolvedType::Tuple(actual_elems))
            if expected_elems
                .iter()
                .any(|e| type_table.borrow().is_type_pack(*e)) =>
        {
            let packs: Vec<usize> = {
                let table = type_table.borrow();
                expected_elems
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| table.is_type_pack(**e))
                    .map(|(i, _)| i)
                    .collect()
            };
            assert!(!packs.is_empty(), "the arm's guard found a pack");

            // The elements ahead of the first pack and behind the last keep
            // their positions whatever the packs expand to; what lies between
            // two moves with a boundary the source never wrote.
            let last_pack = packs[packs.len() - 1];
            let fixed_before = packs[0];
            let fixed_after = expected_elems.len() - last_pack - 1;
            let total_fixed = fixed_before + fixed_after;

            if actual_elems.len() >= total_fixed {
                for i in 0..fixed_before {
                    unify(type_table, expected_elems[i], actual_elems[i], bindings);
                }
                for i in 0..fixed_after {
                    unify(
                        type_table,
                        expected_elems[last_pack + 1 + i],
                        actual_elems[actual_elems.len() - fixed_after + i],
                        bindings,
                    );
                }
                // One pack takes what the fixed elements leave over; two divide
                // it at a boundary nothing here can find.
                if packs.len() == 1 {
                    let pack_elements: Vec<TypeId> =
                        actual_elems[fixed_before..actual_elems.len() - fixed_after].to_vec();
                    let pack = expected_elems[packs[0]];
                    let repeated = type_table.borrow_mut().repeated_element(pack);
                    let key = match repeated {
                        // A repeat's pack is only its length: the elements meet
                        // the one type it repeats.
                        Some((identity, element)) => {
                            for &actual_elem in &pack_elements {
                                unify(type_table, element, actual_elem, bindings);
                            }
                            identity
                        }
                        None => pack,
                    };
                    let pack_tuple = type_table.borrow_mut().make_tuple(pack_elements);
                    bindings.entry(key).or_insert(pack_tuple);
                }
            }
        }
        // Same generic declaration (including `Future` and `Stream`), or two
        // tuples of one length: unify type arguments positionally.
        (ResolvedType::Tuple(expected_args), ResolvedType::Tuple(actual_args))
            if expected_args.len() == actual_args.len() =>
        {
            for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                unify(type_table, exp_arg, act_arg, bindings);
            }
        }
        (
            ResolvedType::Struct {
                def: StructDef::Decl(expected_def),
                type_args: expected_args,
            },
            ResolvedType::Struct {
                def: StructDef::Decl(actual_def),
                type_args: actual_args,
            },
        )
        | (
            ResolvedType::Resource {
                def: expected_def,
                type_args: expected_args,
            },
            ResolvedType::Resource {
                def: actual_def,
                type_args: actual_args,
            },
        )
        | (
            ResolvedType::Variant {
                def: expected_def,
                type_args: expected_args,
            },
            ResolvedType::Variant {
                def: actual_def,
                type_args: actual_args,
            },
        )
        | (
            ResolvedType::Newtype {
                def: expected_def,
                type_args: expected_args,
                ..
            },
            ResolvedType::Newtype {
                def: actual_def,
                type_args: actual_args,
                ..
            },
        ) if expected_def == actual_def && expected_args.len() == actual_args.len() => {
            for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                unify(type_table, exp_arg, act_arg, bindings);
            }
        }
        // `List<K>` (generic instance) matched against a homogeneous
        // tuple literal: infer `K` from the tuple element type.
        (
            ResolvedType::Struct {
                type_args: expected_args,
                ..
            },
            ResolvedType::Tuple(actual_elems),
        ) if type_table.borrow().is_list(expected)
            && expected_args.len() == 1
            && !actual_elems.is_empty() =>
        {
            let first_elem_type = actual_elems[0];
            let all_same = actual_elems.iter().all(|&e| e == first_elem_type);
            if all_same {
                unify(type_table, expected_args[0], first_elem_type, bindings);
            }
        }
        // A projection over a rigid base answers nothing yet, so two of one
        // family meet argument by argument, as Rust relates rigid aliases.
        (
            ResolvedType::AssocTypeProjection {
                param_id: expected_base,
                assoc_name: expected_name,
                args: expected_args,
                owning_trait: expected_trait,
                trait_args: expected_trait_args,
                ..
            },
            ResolvedType::AssocTypeProjection {
                param_id: actual_base,
                assoc_name: actual_name,
                args: actual_args,
                owning_trait: actual_trait,
                trait_args: actual_trait_args,
                ..
            },
        ) if expected_base == actual_base
            && expected_name == actual_name
            && expected_trait == actual_trait
            && expected_args.len() == actual_args.len() =>
        {
            let trait_args = expected_trait_args
                .iter()
                .flatten()
                .zip(actual_trait_args.iter().flatten());
            for (&exp_arg, &act_arg) in expected_args
                .iter()
                .zip(actual_args.iter())
                .chain(trait_args)
            {
                unify(type_table, exp_arg, act_arg, bindings);
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
