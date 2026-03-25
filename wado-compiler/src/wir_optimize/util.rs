//! Shared utility functions for WIR optimization passes.

use crate::hashmap::IndexSet;
use crate::wir::{WirExportDesc, WirInstr, WirModule};

/// Collect all `func_ids` that must NOT be SROA'd or otherwise transformed
/// (exports, element tables, `RefFunc` references).
pub(super) fn collect_pinned_func_ids(module: &WirModule) -> IndexSet<u32> {
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

    pinned
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

/// Returns true if `instr` has no observable side effects.
/// Calls and memory/global stores are not side-effect-free.
/// Memory loads are treated as side-effect-free (pure read with no mutation).
pub(super) fn is_side_effect_free(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => false,
        WirInstr::LocalSet { .. } | WirInstr::LocalTee { .. } | WirInstr::GlobalSet { .. } => false,
        WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. } => false,
        WirInstr::Unreachable => false,
        _ => {
            let mut ok = true;
            instr.for_each_child(&mut |child| {
                if ok && !is_side_effect_free(child) {
                    ok = false;
                }
            });
            ok
        }
    }
}
