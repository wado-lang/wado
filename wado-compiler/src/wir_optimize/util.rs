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
    //
    // Build the suffix → id map once: helper names always have the
    // form `<module-prefix>/$value_copy$T<id>` and `element_copy_func`
    // carries the trailing portion (`$value_copy$T<id>`), so the
    // last `/`-segment of `fq` is the lookup key. Linear-scanning
    // `name_to_idx` per `ArrayClone` site would otherwise be O(N²).
    let helper_suffix_to_idx: IndexMap<String, u32> = module
        .functions
        .iter()
        .enumerate()
        .filter_map(|(i, func)| {
            let suffix = func
                .name
                .fq
                .rsplit('/')
                .next()
                .filter(|s| s.starts_with("$value_copy$"))?;
            Some((
                suffix.to_string(),
                u32::try_from(i).expect("func index fits u32") + module.defined_func_base,
            ))
        })
        .collect();
    for func in &module.functions {
        if let Some(body) = &func.body {
            collect_array_clone_helpers(body, &helper_suffix_to_idx, &mut pinned);
        }
    }

    pinned
}

fn collect_array_clone_helpers(
    instrs: &[WirInstr],
    helper_suffix_to_idx: &IndexMap<String, u32>,
    pinned: &mut IndexSet<u32>,
) {
    for instr in instrs {
        collect_array_clone_helpers_instr(instr, helper_suffix_to_idx, pinned);
    }
}

fn collect_array_clone_helpers_instr(
    instr: &WirInstr,
    helper_suffix_to_idx: &IndexMap<String, u32>,
    pinned: &mut IndexSet<u32>,
) {
    if let WirInstr::ArrayClone {
        element_copy_func: Some(name_suffix),
        ..
    } = instr
        && let Some(idx) = helper_suffix_to_idx.get(name_suffix.as_str())
    {
        pinned.insert(*idx);
    }
    instr.for_each_child(&mut |child| {
        collect_array_clone_helpers_instr(child, helper_suffix_to_idx, pinned);
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

/// True if `instr` (or any descendant) can trap at runtime on some inputs.
///
/// Used to keep `Drop(value)` instructions whose `value` is otherwise
/// `is_side_effect_free` but would trap on certain inputs — the Wado
/// language requires those traps to be observable even when the read
/// value is discarded (`let _ = arr[-1]` must trap). Distinct from
/// [`is_root_observable`] which the WIR optimizer deliberately keeps lax
/// to enable CSE / dead-load elimination of trapping operations whose
/// result IS used.
pub(super) fn may_trap(instr: &WirInstr) -> bool {
    // Operand-dependent: `ref.as_non_null(inner)` only traps when `inner`
    // could itself produce null. `is_nonnull_result` recognises
    // `struct.new`/`array.new*`/`ref.func`/`ref.i31`/already-non-null
    // typed locals, so a `ref.as_non_null(ref.func "…")` produced by
    // closure lowering doesn't keep the surrounding `Drop` alive.
    if let WirInstr::RefAsNonNull(inner) = instr {
        return may_trap(inner) || !inner.is_nonnull_result();
    }
    // Operand-dependent: `ref.cast T(struct.new T { … })` is identity
    // (`struct.new` always produces exactly `T`), so the cast can't
    // trap. Only handles the direct-`StructNew`-operand case; tracing
    // through `LocalGet` would need def-use analysis.
    if let WirInstr::RefCast { type_id, expr, .. } = instr
        && let WirInstr::StructNew {
            type_id: src_type, ..
        } = expr.as_ref()
        && src_type == type_id
    {
        return may_trap(expr);
    }
    if matches!(
        instr,
        // GC array reads trap on null / OOB.
        WirInstr::ArrayGet { .. }
        | WirInstr::ArrayGetS { .. }
        | WirInstr::ArrayGetU { .. }
        // `array.len` traps when the array reference is null.
        | WirInstr::ArrayLen(_)
        // `array.new_data` traps when offset + len overruns the data
        // segment.
        | WirInstr::ArrayNewData { .. }
        // GC struct reads trap on null receiver.
        | WirInstr::StructGet { .. }
        // Ref cast / non-null assertion trap on failure. The
        // operand-dependent early returns above peel off the
        // statically-safe shapes; whatever's left here is the
        // conservative case.
        | WirInstr::RefAsNonNull(_)
        | WirInstr::RefCast { .. }
        // Integer divide / remainder trap on zero divisor (and signed
        // div/rem of MIN by -1 overflows).
        | WirInstr::I32DivS(_, _)
        | WirInstr::I32DivU(_, _)
        | WirInstr::I32RemS(_, _)
        | WirInstr::I32RemU(_, _)
        | WirInstr::I64DivS(_, _)
        | WirInstr::I64DivU(_, _)
        | WirInstr::I64RemS(_, _)
        | WirInstr::I64RemU(_, _)
        // Non-saturating float-to-int truncation traps on out-of-range
        // values (the saturating variants — `*TruncSatF*` — don't).
        | WirInstr::I32TruncF32S(_)
        | WirInstr::I32TruncF32U(_)
        | WirInstr::I32TruncF64S(_)
        | WirInstr::I32TruncF64U(_)
        | WirInstr::I64TruncF32S(_)
        | WirInstr::I64TruncF32U(_)
        | WirInstr::I64TruncF64S(_)
        | WirInstr::I64TruncF64U(_)
        // Linear-memory loads trap on out-of-bounds access.
        | WirInstr::I32Load { .. }
        | WirInstr::I32Load8U { .. }
        | WirInstr::I32Load8S { .. }
        | WirInstr::I32Load16U { .. }
        | WirInstr::I32Load16S { .. }
        | WirInstr::I64Load { .. }
        | WirInstr::V128Load { .. }
        // `table.get` traps when the index is out of bounds.
        | WirInstr::TableGet { .. }
        // Explicit trap.
        | WirInstr::Unreachable
    ) {
        return true;
    }
    let mut trap = false;
    instr.for_each_child(&mut |child| {
        if !trap && may_trap(child) {
            trap = true;
        }
    });
    trap
}
