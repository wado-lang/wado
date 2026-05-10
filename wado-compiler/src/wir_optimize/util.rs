//! Shared utility functions for WIR optimization passes.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirInstr, WirPackage};

/// Collect all `func_ids` that must NOT be SROA'd or otherwise transformed
/// (exports, element tables, `RefFunc` references, and helpers referenced
/// only by name from `WirInstr::ArrayClone::element_copy_func`).
pub(super) fn collect_pinned_func_ids(module: &WirPackage) -> IndexSet<u32> {
    let mut pinned = IndexSet::default();

    // Exported functions
    for export in &module.exports {
        if let WirExportDesc::Func { func_id } = &export.desc {
            pinned.insert(func_id.index());
        }
    }

    // Element table functions
    for elem in &module.elements {
        for fid in &elem.func_ids {
            pinned.insert(fid.index());
        }
    }

    // RefFunc references in all function bodies
    for func in &module.functions {
        if let Some(body) = &func.body {
            collect_ref_funcs(body, &mut pinned);
        }
    }

    // Also check global initializers for RefFunc
    for global in &module.globals {
        collect_ref_funcs_instr(&global.init, &mut pinned);
    }

    // `WirInstr::ArrayClone::element_copy_func` references its helper by
    // *name* — codegen looks the name up at emit time and emits a plain
    // `Call(func_idx)`. SROA-style return rewrites would change the
    // helper's signature without touching that emit path, leaving the
    // call expecting a single (ref T) while the rewritten helper now
    // returns multi-value. Pin every helper that any ArrayClone refers
    // to so the rewrites skip them.
    let name_to_idx: IndexMap<String, u32> = module
        .functions
        .iter()
        .enumerate()
        .map(|(i, func)| {
            (
                func.name.fq.clone(),
                u32::try_from(i).expect("func index fits u32") + module.defined_func_base,
            )
        })
        .collect();
    for func in &module.functions {
        if let Some(body) = &func.body {
            collect_array_clone_helpers(body, &name_to_idx, &mut pinned);
        }
    }

    pinned
}

fn collect_array_clone_helpers(
    instrs: &[WirInstr],
    name_to_idx: &IndexMap<String, u32>,
    pinned: &mut IndexSet<u32>,
) {
    for instr in instrs {
        collect_array_clone_helpers_instr(instr, name_to_idx, pinned);
    }
}

fn collect_array_clone_helpers_instr(
    instr: &WirInstr,
    name_to_idx: &IndexMap<String, u32>,
    pinned: &mut IndexSet<u32>,
) {
    if let WirInstr::ArrayClone {
        element_copy_func: Some(name_suffix),
        ..
    } = instr
    {
        for (fq, idx) in name_to_idx {
            if fq.ends_with(name_suffix.as_str()) {
                pinned.insert(*idx);
                break;
            }
        }
    }
    instr.for_each_child(&mut |child| {
        collect_array_clone_helpers_instr(child, name_to_idx, pinned);
    });
}

fn collect_ref_funcs(instrs: &[WirInstr], pinned: &mut IndexSet<u32>) {
    for instr in instrs {
        collect_ref_funcs_instr(instr, pinned);
    }
}

fn collect_ref_funcs_instr(instr: &WirInstr, pinned: &mut IndexSet<u32>) {
    if let WirInstr::RefFunc { func_id } = instr {
        pinned.insert(func_id.index());
    }
    instr.for_each_child(&mut |child| collect_ref_funcs_instr(child, pinned));
}

/// Collect all local names referenced by `LocalGet` in an expression tree.
pub(super) fn collect_local_gets_deep(instr: &WirInstr, names: &mut IndexSet<String>) {
    if let WirInstr::LocalGet { name, .. } = instr {
        names.insert(name.clone());
    }
    instr.for_each_child(&mut |child| {
        collect_local_gets_deep(child, names);
    });
}

/// True if no node in `instr`'s sub-tree is observable. Pure loads
/// (`StructGet`, `ArrayGet*`, memory loads, `LocalGet`, `GlobalGet`) and
/// arithmetic / ref ops are treated as side-effect-free.
pub(super) fn is_side_effect_free(instr: &WirInstr) -> bool {
    if is_root_observable(instr) {
        return false;
    }
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !is_side_effect_free(child) {
            ok = false;
        }
    });
    ok
}

/// True if the *root* of `instr` would change observable program behavior on
/// its own. Covers explicit state mutation (heap / global / local / table),
/// calls (potentially I/O), the explicit [`WirInstr::Unreachable`] trap, and
/// control-flow exits that bypass subsequent siblings. Does **not** classify
/// implicit-trap ops (integer divide / remainder, float→int trunc, OOB heap
/// reads / loads, null `ref.as_non_null` / `ref.cast`, etc.) as observable.
///
/// Does not look at children; combine with recursion (see
/// [`is_side_effect_free`]) for tree purity.
pub(super) fn is_root_observable(instr: &WirInstr) -> bool {
    matches!(
        instr,
        // Calls.
        WirInstr::Call { .. }
        | WirInstr::CallIndirect { .. }
        | WirInstr::CallRef { .. }
        // GC / local / global state mutation.
        | WirInstr::LocalSet { .. }
        | WirInstr::LocalTee { .. }
        | WirInstr::GlobalSet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. }
        | WirInstr::ArrayCopy { .. }
        | WirInstr::ArrayFill { .. }
        | WirInstr::TableSet { .. }
        | WirInstr::MultiValueLocalBind { .. }
        // Linear-memory writes.
        | WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. }
        | WirInstr::V128Store { .. }
        | WirInstr::MemoryGrow(_)
        | WirInstr::MemoryFill { .. }
        // Trap.
        | WirInstr::Unreachable
        // Control-flow exits — execution of a sub-expression that contains
        // these is observable because the branch transfers control past
        // siblings that would otherwise execute.
        | WirInstr::Br { .. }
        | WirInstr::BrIf { .. }
        | WirInstr::BrTable { .. }
        | WirInstr::Return { .. }
    )
}
