//! Trivial module-init guard removal pass for WIR.
//!
//! Detects and removes globals used only as "initialized once" guards
//! (`if global { break; }; global = 1;`) with no actual init work.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirInstr, WirPackage};

pub(super) fn remove_trivial_init_globals(module: &mut WirPackage) {
    // Collect globals that appear only in the trivial-init pattern.
    // First, find all globals that appear in ANY non-trivial-guard context.
    let num_globals = module.globals.len();
    if num_globals == 0 {
        return;
    }

    // Build a set of global fq names that are exported.
    let exported_globals: IndexSet<String> = module
        .exports
        .iter()
        .filter_map(|e| {
            if let WirExportDesc::Global { name } = &e.desc {
                Some(name.fq.clone())
            } else {
                None
            }
        })
        .collect();

    // For each global, count how many times it's used in trivial-guard blocks
    // vs. in any other context. Use an IndexMap for O(1) lookups by fq name.
    let global_idx_map: IndexMap<String, usize> = module
        .globals
        .iter()
        .enumerate()
        .map(|(i, g)| (g.name.fq.clone(), i))
        .collect();

    // For each global, track (trivial_guard_count, other_use_count).
    let mut trivial_guard_blocks: Vec<usize> = vec![0; num_globals]; // functions where this global has a trivial guard block
    let mut other_use_counts: Vec<usize> = vec![0; num_globals];

    for func in &module.functions {
        if let Some(body) = &func.body {
            for instr in body {
                count_global_uses_in_instr(
                    instr,
                    &global_idx_map,
                    &mut trivial_guard_blocks,
                    &mut other_use_counts,
                    false,
                );
            }
        }
    }

    // Also check globals themselves (init expressions).
    for global in &module.globals {
        count_global_uses_in_instr(
            &global.init,
            &global_idx_map,
            &mut trivial_guard_blocks,
            &mut other_use_counts,
            false,
        );
    }

    // Identify globals eligible for removal:
    // - Not exported
    // - No other uses (other_use_counts == 0)
    // - Has at least one trivial guard block (trivial_guard_blocks >= 1)
    let removable: IndexSet<String> = global_idx_map
        .iter()
        .filter(|(fq, i)| {
            !exported_globals.contains(fq.as_str())
                && other_use_counts[**i] == 0
                && trivial_guard_blocks[**i] >= 1
        })
        .map(|(fq, _)| fq.clone())
        .collect();

    if removable.is_empty() {
        return;
    }

    // Mark matching globals as dead.
    for (i, global) in module.globals.iter().enumerate() {
        if removable.contains(&global.name.fq) {
            module.dead_global_indices.insert(i as u32);
        }
    }

    // Remove trivial-guard blocks from all function bodies.
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                nop_trivial_init_blocks(instr, &removable);
            }
        }
    }
}

/// Classify global uses in an instruction tree.
/// - Trivial-guard blocks increment `trivial_guard_blocks[i]`.
/// - All other GlobalGet/GlobalSet uses increment `other_use_counts[i]`.
fn count_global_uses_in_instr(
    instr: &WirInstr,
    global_idx_map: &IndexMap<String, usize>,
    trivial_guard_blocks: &mut Vec<usize>,
    other_use_counts: &mut Vec<usize>,
    in_other_context: bool,
) {
    // Check if this is a trivial-guard block at statement level (not inside another expr).
    if !in_other_context
        && let Some(fq) = is_trivial_init_block(instr)
        && let Some(&idx) = global_idx_map.get(fq)
    {
        trivial_guard_blocks[idx] += 1;
        return; // Don't recurse — all uses are accounted for by this pattern.
    }

    // For GlobalGet/GlobalSet at any depth, count as "other use".
    match instr {
        WirInstr::GlobalGet { name, .. } => {
            if let Some(&idx) = global_idx_map.get(name.fq.as_str()) {
                other_use_counts[idx] += 1;
            }
        }
        WirInstr::GlobalSet { name, value } => {
            if let Some(&idx) = global_idx_map.get(name.fq.as_str()) {
                other_use_counts[idx] += 1;
            }
            count_global_uses_in_instr(
                value,
                global_idx_map,
                trivial_guard_blocks,
                other_use_counts,
                true,
            );
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            for child in body {
                count_global_uses_in_instr(
                    child,
                    global_idx_map,
                    trivial_guard_blocks,
                    other_use_counts,
                    in_other_context,
                );
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            count_global_uses_in_instr(
                condition,
                global_idx_map,
                trivial_guard_blocks,
                other_use_counts,
                true,
            );
            for child in then_body {
                count_global_uses_in_instr(
                    child,
                    global_idx_map,
                    trivial_guard_blocks,
                    other_use_counts,
                    in_other_context,
                );
            }
            if let Some(eb) = else_body {
                for child in eb {
                    count_global_uses_in_instr(
                        child,
                        global_idx_map,
                        trivial_guard_blocks,
                        other_use_counts,
                        in_other_context,
                    );
                }
            }
        }
        WirInstr::Seq(body) => {
            for child in body {
                count_global_uses_in_instr(
                    child,
                    global_idx_map,
                    trivial_guard_blocks,
                    other_use_counts,
                    in_other_context,
                );
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                count_global_uses_in_instr(
                    child,
                    global_idx_map,
                    trivial_guard_blocks,
                    other_use_counts,
                    true,
                );
            });
        }
    }
}

/// Returns the global fq name if `instr` is a trivial init guard block with exactly
/// two instructions (`If { GlobalGet(x), Br 1 }` + `GlobalSet { x, I32Const(1) }`)
/// and no result type.
fn is_trivial_init_block(instr: &WirInstr) -> Option<&str> {
    let WirInstr::Block {
        result: None, body, ..
    } = instr
    else {
        return None;
    };
    if body.len() != 2 {
        return None;
    }
    // First: If { condition: GlobalGet(x), then_body: [Br { depth: 0 }], else_body: None }
    let WirInstr::If {
        condition,
        then_body,
        else_body: None,
        result: None,
    } = &body[0]
    else {
        return None;
    };
    let WirInstr::GlobalGet {
        name: guard_name, ..
    } = condition.peel_hint()
    else {
        return None;
    };
    if then_body.len() != 1 {
        return None;
    }
    // The Br targets the outer block (depth 1 from inside the If's then_body,
    // since the If itself introduces a block scope at depth 0).
    if !matches!(then_body[0], WirInstr::Br { depth: 1 }) {
        return None;
    }
    // Second: GlobalSet { name: x, value: I32Const(1) }
    let WirInstr::GlobalSet {
        name: set_name,
        value,
    } = &body[1]
    else {
        return None;
    };
    if guard_name.fq != set_name.fq {
        return None;
    }
    if !matches!(value.as_ref(), WirInstr::I32Const(1)) {
        return None;
    }
    Some(&guard_name.fq)
}

/// Replace trivial-guard blocks for removable globals with `Nop`.
fn nop_trivial_init_blocks(instr: &mut WirInstr, removable: &IndexSet<String>) {
    if let Some(fq) = is_trivial_init_block(instr)
        && removable.contains(fq)
    {
        *instr = WirInstr::Nop;
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| nop_trivial_init_blocks(child, removable));
}
