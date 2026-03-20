//! Dead Return Value Elimination (DRVE) pass for WIR.
//!
//! Converts functions whose return value is always immediately dropped at every
//! call site to void return, eliminating the GC struct allocation in the return
//! and the `drop` at call sites.

use crate::hashmap::IndexSet;
use crate::wir::{WirFuncType, WirInstr, WirModule, WirType, WirTypeDef, WirTypeId};

use super::util::{collect_pinned_func_ids, is_side_effect_free};

/// Dead Return Value Elimination (DRVE).
///
/// Finds non-pinned functions that return a GC struct ref where:
/// 1. Every return in the body is `Return { value: StructNew { pure_fields } }`.
/// 2. Every call site in the module is `Drop(Call(f, args))`.
///
/// Those functions are converted to void return, the `StructNew` is removed from
/// returns, and the Drop wrapper is removed at each call site.
pub(super) fn eliminate_dead_return_values(module: &mut WirModule) {
    let pinned = collect_pinned_func_ids(module);

    let candidates = find_drve_candidates(module, &pinned);
    if candidates.is_empty() {
        return;
    }

    let confirmed = validate_drve_call_sites(module, &candidates);
    if confirmed.is_empty() {
        return;
    }

    apply_drve(module, &confirmed);
}

struct DrveCandidate {
    func_array_idx: usize,
}

fn find_drve_candidates(module: &WirModule, pinned: &IndexSet<u32>) -> Vec<(u32, DrveCandidate)> {
    let mut candidates = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();

        if pinned.contains(&func_id_index) {
            continue;
        }
        if func.body.is_none() {
            continue;
        }

        let type_idx = func.type_id.index() as usize;
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx) else {
            continue;
        };

        // Must return exactly one Ref to a struct.
        if func_type.results.len() != 1 {
            continue;
        }
        let WirType::Ref {
            type_id: ret_type_id,
            ..
        } = &func_type.results[0]
        else {
            continue;
        };
        let ret_type_idx = ret_type_id.index() as usize;
        let Some(WirTypeDef::Struct(_)) = module.types.get(ret_type_idx) else {
            continue;
        };

        // All returns must be StructNew with all-pure field expressions.
        let body = func.body.as_ref().unwrap();
        if !all_returns_are_pure_struct_new(body) {
            continue;
        }

        candidates.push((func_id_index, DrveCandidate { func_array_idx: i }));
    }

    candidates
}

/// Returns true if every `Return { value: Some(...) }` in the tree is a `StructNew`
/// with all side-effect-free field expressions.
fn all_returns_are_pure_struct_new(instrs: &[WirInstr]) -> bool {
    for instr in instrs {
        match instr {
            WirInstr::Return { value: Some(v) } => {
                let WirInstr::StructNew { fields, .. } = v.as_ref() else {
                    return false;
                };
                if !fields.iter().all(is_side_effect_free) {
                    return false;
                }
            }
            WirInstr::Return { value: None } => {
                return false;
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                if !all_returns_are_pure_struct_new(body) {
                    return false;
                }
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                if !all_returns_are_pure_struct_new(then_body) {
                    return false;
                }
                if let Some(eb) = else_body
                    && !all_returns_are_pure_struct_new(eb)
                {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn validate_drve_call_sites(
    module: &WirModule,
    candidates: &[(u32, DrveCandidate)],
) -> Vec<(u32, DrveCandidate)> {
    let candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();
    let mut invalid: IndexSet<u32> = IndexSet::default();
    let mut has_drop_call: IndexSet<u32> = IndexSet::default();

    for func in &module.functions {
        if let Some(body) = &func.body {
            check_drve_call_uses_in_body(body, &candidate_ids, &mut invalid, &mut has_drop_call);
        }
    }

    candidates
        .iter()
        .filter(|(id, _)| has_drop_call.contains(id) && !invalid.contains(id))
        .map(|(id, c)| {
            (
                *id,
                DrveCandidate {
                    func_array_idx: c.func_array_idx,
                },
            )
        })
        .collect()
}

fn check_drve_call_uses_in_body(
    instrs: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
    has_drop_call: &mut IndexSet<u32>,
) {
    for instr in instrs {
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                check_drve_call_uses_in_body(body, candidate_ids, invalid, has_drop_call);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                find_drve_candidate_calls(condition, candidate_ids, invalid);
                check_drve_call_uses_in_body(then_body, candidate_ids, invalid, has_drop_call);
                if let Some(eb) = else_body {
                    check_drve_call_uses_in_body(eb, candidate_ids, invalid, has_drop_call);
                }
            }
            WirInstr::Seq(body) => {
                check_drve_call_uses_in_body(body, candidate_ids, invalid, has_drop_call);
            }
            WirInstr::Drop(inner) => {
                if let WirInstr::Call { func_id, args } = inner.as_ref()
                    && candidate_ids.contains(&func_id.index())
                {
                    has_drop_call.insert(func_id.index());
                    for arg in args {
                        find_drve_candidate_calls(arg, candidate_ids, invalid);
                    }
                    continue;
                }
                find_drve_candidate_calls(inner, candidate_ids, invalid);
            }
            _ => {
                find_drve_candidate_calls(instr, candidate_ids, invalid);
            }
        }
    }
}

fn find_drve_candidate_calls(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Call { func_id, .. } = instr
        && candidate_ids.contains(&func_id.index())
    {
        invalid.insert(func_id.index());
        return;
    }
    instr.for_each_child(&mut |child| {
        find_drve_candidate_calls(child, candidate_ids, invalid);
    });
}

fn apply_drve(module: &mut WirModule, confirmed: &[(u32, DrveCandidate)]) {
    let candidate_set: IndexSet<u32> = confirmed.iter().map(|(id, _)| *id).collect();

    // Step A: Change function return types to void and rewrite returns.
    for (_, candidate) in confirmed {
        let func = &mut module.functions[candidate.func_array_idx];

        let old_type_idx = func.type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: old_func_type.params.clone(),
            results: vec![],
        };

        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));

        let new_type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
        func.type_id = new_type_id;

        if let Some(body) = &mut func.body {
            rewrite_drve_returns(body);
        }
    }

    // Step B: Remove Drop wrappers at call sites.
    for i in 0..module.functions.len() {
        if module.functions[i].body.is_some() {
            let body = module.functions[i].body.as_mut().unwrap();
            rewrite_drve_call_sites(body, &candidate_set);
        }
    }
}

fn rewrite_drve_returns(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) }
                if matches!(v.as_ref(), WirInstr::StructNew { .. }) =>
            {
                *instr = WirInstr::Return { value: None };
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                rewrite_drve_returns(body);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                rewrite_drve_returns(then_body);
                if let Some(eb) = else_body {
                    rewrite_drve_returns(eb);
                }
            }
            _ => {}
        }
    }
}

fn rewrite_drve_call_sites(instrs: &mut [WirInstr], candidate_set: &IndexSet<u32>) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Drop(inner)
                if matches!(inner.as_ref(), WirInstr::Call { func_id, .. }
                    if candidate_set.contains(&func_id.index())) =>
            {
                let call = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                *instr = call;
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                rewrite_drve_call_sites(body, candidate_set);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                rewrite_drve_call_sites(then_body, candidate_set);
                if let Some(eb) = else_body {
                    rewrite_drve_call_sites(eb, candidate_set);
                }
            }
            _ => {}
        }
    }
}
