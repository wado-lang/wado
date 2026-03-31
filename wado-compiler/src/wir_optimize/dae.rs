//! Dead Argument Elimination (DAE) pass for WIR.
//!
//! Removes unused function parameters and their corresponding arguments at
//! call sites. A parameter is "dead" when it is never referenced by name
//! (`LocalGet`, `LocalSet`, `LocalTee`) anywhere in the function body.
//!
//! Safety requirements:
//! - The function must not be pinned (exported, in element tables, or
//!   referenced via `RefFunc`).
//! - Every argument expression at a dead parameter position must be
//!   side-effect-free at every call site, so removal doesn't change
//!   observable behavior.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirFuncType, WirInstr, WirModule, WirTypeDef, WirTypeId};

use super::util::{collect_pinned_func_ids, is_side_effect_free};

/// Dead Argument Elimination.
///
/// Finds non-pinned functions with unused parameters, verifies all call sites
/// pass side-effect-free arguments at those positions, then removes the dead
/// parameters and arguments.
pub(super) fn eliminate_dead_arguments(module: &mut WirModule) {
    let pinned = collect_pinned_func_ids(module);

    let candidates = find_dae_candidates(module, &pinned);
    if candidates.is_empty() {
        return;
    }

    let confirmed = validate_dae_call_sites(module, &candidates);
    if confirmed.is_empty() {
        return;
    }

    apply_dae(module, &confirmed);
}

/// A candidate function with its dead parameter indices.
struct DaeCandidate {
    func_array_idx: usize,
    /// Bit set of dead parameter positions (position in the param list).
    dead_params: Vec<bool>,
}

/// Scan all defined functions for unused parameters.
fn find_dae_candidates(module: &WirModule, pinned: &IndexSet<u32>) -> Vec<(u32, DaeCandidate)> {
    let mut candidates = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();

        if pinned.contains(&func_id_index) {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        if func.param_names.is_empty() {
            continue;
        }

        // Collect all local names referenced in the body.
        let mut referenced = IndexSet::default();
        collect_referenced_locals(body, &mut referenced);

        // Also check stores: parameters in the stores list are semantically
        // significant even if not directly referenced in the body.
        let mut dead_params = Vec::with_capacity(func.param_names.len());
        let mut has_dead = false;
        for name in &func.param_names {
            let is_dead = !referenced.contains(name.as_str()) && !func.stores.contains(name);
            if is_dead {
                has_dead = true;
            }
            dead_params.push(is_dead);
        }

        if has_dead {
            candidates.push((
                func_id_index,
                DaeCandidate {
                    func_array_idx: i,
                    dead_params,
                },
            ));
        }
    }

    candidates
}

/// Collect all local names that appear in `LocalGet`, `LocalSet`, or `LocalTee`
/// anywhere in the instruction tree.
fn collect_referenced_locals(instrs: &[WirInstr], names: &mut IndexSet<String>) {
    for instr in instrs {
        collect_referenced_locals_instr(instr, names);
    }
}

fn collect_referenced_locals_instr(instr: &WirInstr, names: &mut IndexSet<String>) {
    match instr {
        WirInstr::LocalGet { name, .. }
        | WirInstr::LocalSet { name, .. }
        | WirInstr::LocalTee { name, .. } => {
            names.insert(name.clone());
        }
        WirInstr::MultiValueLocalBind { locals, .. } => {
            for local in locals.iter().flatten() {
                names.insert(local.clone());
            }
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| {
        collect_referenced_locals_instr(child, names);
    });
}

/// Validate that every call site to each candidate passes side-effect-free
/// arguments at dead parameter positions.
fn validate_dae_call_sites(
    module: &WirModule,
    candidates: &[(u32, DaeCandidate)],
) -> Vec<(u32, DaeCandidate)> {
    let candidate_map: IndexMap<u32, &DaeCandidate> =
        candidates.iter().map(|(id, c)| (*id, c)).collect();

    let mut invalid: IndexSet<u32> = IndexSet::default();

    for func in &module.functions {
        if let Some(body) = &func.body {
            check_dae_call_sites(body, &candidate_map, &mut invalid);
        }
    }

    candidates
        .iter()
        .filter(|(id, _)| !invalid.contains(id))
        .map(|(id, c)| {
            (
                *id,
                DaeCandidate {
                    func_array_idx: c.func_array_idx,
                    dead_params: c.dead_params.clone(),
                },
            )
        })
        .collect()
}

/// Walk all instructions looking for Call sites to candidates.
/// If any dead-position argument has side effects, mark the candidate invalid.
fn check_dae_call_sites(
    instrs: &[WirInstr],
    candidates: &IndexMap<u32, &DaeCandidate>,
    invalid: &mut IndexSet<u32>,
) {
    for instr in instrs {
        check_dae_call_sites_instr(instr, candidates, invalid);
    }
}

fn check_dae_call_sites_instr(
    instr: &WirInstr,
    candidates: &IndexMap<u32, &DaeCandidate>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Call { func_id, args } = instr
        && let Some(candidate) = candidates.get(&func_id.index())
    {
        // Check that all arguments at dead positions are side-effect-free.
        for (i, arg) in args.iter().enumerate() {
            if i < candidate.dead_params.len()
                && candidate.dead_params[i]
                && !is_side_effect_free(arg)
            {
                invalid.insert(func_id.index());
                // Still walk the args for nested calls to other candidates.
                break;
            }
        }
    }
    instr.for_each_child(&mut |child| {
        check_dae_call_sites_instr(child, candidates, invalid);
    });
}

/// Apply DAE: remove dead parameters from function types and `param_names`,
/// and remove corresponding arguments from all call sites.
fn apply_dae(module: &mut WirModule, confirmed: &[(u32, DaeCandidate)]) {
    // Build a map from func_id → dead_params for efficient lookup during rewriting.
    let dae_map: IndexMap<u32, &Vec<bool>> = confirmed
        .iter()
        .map(|(id, c)| (*id, &c.dead_params))
        .collect();

    // Step A: Rewrite function signatures and param_names.
    for (_, candidate) in confirmed {
        let func = &mut module.functions[candidate.func_array_idx];

        // Remove dead param names.
        let old_param_names = std::mem::take(&mut func.param_names);
        func.param_names = old_param_names
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !candidate.dead_params[*i])
            .map(|(_, name)| name)
            .collect();

        // Create new function type without dead parameter types.
        let old_type_idx = func.type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let new_params: Vec<_> = old_func_type
            .params
            .iter()
            .enumerate()
            .filter(|(i, _)| !candidate.dead_params[*i])
            .map(|(_, ty)| ty.clone())
            .collect();
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: new_params,
            results: old_func_type.results.clone(),
        };

        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));
        func.type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
    }

    // Step B: Remove dead arguments at all call sites.
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            rewrite_dae_call_sites(body, &dae_map);
        }
    }
}

fn rewrite_dae_call_sites(instrs: &mut [WirInstr], dae_map: &IndexMap<u32, &Vec<bool>>) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Call { func_id, args } => {
                if let Some(dead_params) = dae_map.get(&func_id.index()) {
                    let old_args = std::mem::take(args);
                    *args = old_args
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| *i >= dead_params.len() || !dead_params[*i])
                        .map(|(_, arg)| arg)
                        .collect();
                }
                // Recurse into remaining args (they may contain nested calls).
                for arg in args {
                    rewrite_dae_call_sites_instr(arg, dae_map);
                }
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                rewrite_dae_call_sites(body, dae_map);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                rewrite_dae_call_sites_instr(condition, dae_map);
                rewrite_dae_call_sites(then_body, dae_map);
                if let Some(eb) = else_body {
                    rewrite_dae_call_sites(eb, dae_map);
                }
            }
            _ => {
                rewrite_dae_call_sites_instr(instr, dae_map);
            }
        }
    }
}

fn rewrite_dae_call_sites_instr(instr: &mut WirInstr, dae_map: &IndexMap<u32, &Vec<bool>>) {
    match instr {
        WirInstr::Call { func_id, args } => {
            if let Some(dead_params) = dae_map.get(&func_id.index()) {
                let old_args = std::mem::take(args);
                *args = old_args
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| *i >= dead_params.len() || !dead_params[*i])
                    .map(|(_, arg)| arg)
                    .collect();
            }
            for arg in args {
                rewrite_dae_call_sites_instr(arg, dae_map);
            }
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            rewrite_dae_call_sites(body, dae_map);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            rewrite_dae_call_sites_instr(condition, dae_map);
            rewrite_dae_call_sites(then_body, dae_map);
            if let Some(eb) = else_body {
                rewrite_dae_call_sites(eb, dae_map);
            }
        }
        _ => {
            // Walk other instruction variants for nested Calls.
            // Since there's no for_each_child_mut, handle the common cases.
            match instr {
                WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. } => {
                    rewrite_dae_call_sites_instr(value, dae_map);
                }
                WirInstr::Drop(inner)
                | WirInstr::RefAsNonNull(inner)
                | WirInstr::RefIsNull(inner)
                | WirInstr::StructGet { expr: inner, .. }
                | WirInstr::RefCast { expr: inner, .. }
                | WirInstr::RefTest { expr: inner, .. }
                | WirInstr::ValueCopy { expr: inner, .. } => {
                    rewrite_dae_call_sites_instr(inner, dae_map);
                }
                WirInstr::StructNew { fields, .. }
                | WirInstr::ArrayNewFixed {
                    elements: fields, ..
                } => {
                    for f in fields {
                        rewrite_dae_call_sites_instr(f, dae_map);
                    }
                }
                WirInstr::StructSet { expr, value, .. } => {
                    rewrite_dae_call_sites_instr(expr, dae_map);
                    rewrite_dae_call_sites_instr(value, dae_map);
                }
                WirInstr::MultiValueLocalBind { instr: inner, .. }
                | WirInstr::MultiValueStructNew { instr: inner, .. } => {
                    rewrite_dae_call_sites_instr(inner, dae_map);
                }
                WirInstr::Return { value: Some(v) } => {
                    rewrite_dae_call_sites_instr(v, dae_map);
                }
                WirInstr::CallRef { args, func_ref, .. } => {
                    rewrite_dae_call_sites_instr(func_ref, dae_map);
                    for arg in args {
                        rewrite_dae_call_sites_instr(arg, dae_map);
                    }
                }
                _ => {}
            }
        }
    }
}
