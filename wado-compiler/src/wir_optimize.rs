//! WIR optimization — peephole and structural optimizations on `WirModule`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! Current passes:
//! - **Multi-value return SROA**: rewrites functions that return small scalar structs
//!   to use Wasm multi-value returns, eliminating GC struct allocation.
//! - **Single-field parameter SROA**: rewrites functions that take `ref null S` parameters
//!   (where S is any single-field struct, including Box<T>) to take the scalar field
//!   value directly, eliminating GC struct allocation at every call site.
//! - **Dead single-field struct local elimination**: after parameter SROA, call sites
//!   may hold `LocalSet(x, StructNew { [inner] })` where every use of `x` is
//!   `StructGet(LocalGet(x), field)`. Substitutes `inner` directly and nops the
//!   dead allocation, eliminating the GC heap object entirely.
//! - **Dead multi-field struct local elimination**: same as above but for structs with
//!   N > 1 fields where each field is accessed exactly once via `StructGet`.
//! - **Multi-value tuple elision**: replaces `MultiValueStructNew` + `StructGet`
//!   sequences with `MultiValueLocalBind` to skip intermediate struct allocation.
//! - **Constant array data promotion**: replaces `ArrayNewFixed` of constant primitive
//!   values with `ArrayNewData` backed by a passive data segment.
//! - **Dead return value elimination (DRVE)**: converts functions whose return value is
//!   always immediately dropped at every call site to void return, eliminating the GC
//!   struct allocation in the return and the `drop` at call sites.
//! - **Write-only local elimination**: converts `LocalSet(x, expr)` to `Drop(expr)` or
//!   `Nop` when local `x` is never read, cleaning up temporaries left by other passes.
//! - **Trivial init-guard removal**: detects compiler-generated `if global { break; };
//!   global = 1;` guard blocks with no actual init work and removes them along with
//!   the dead global, eliminating the module-init overhead.
//! - **Dead type elimination**: marks GC types not referenced by any live function,
//!   import, or global as dead so the emitter can skip them.
//! - **Cleanup**: removes redundant `RefAsNonNull`, `Nop`, and dead code after
//!   `Unreachable`, normalizing WIR so that codegen can emit it as-is.

mod array;
mod dce;
mod drve;
mod elide;
mod forward;
mod init_guard;
mod peephole;
mod sroa;
mod string;

use crate::hashmap::IndexSet;
use crate::optimize::OptLevel;
use crate::wir::{WirExportDesc, WirInstr, WirModule};

pub use dce::{dce_unreachable_functions, dce_unreachable_types};

use array::{
    collapse_array_append_sequences, promote_constant_arrays_to_data, split_large_array_literals,
};
use drve::drve_dead_return_values;
use elide::{
    elide_dead_multi_field_struct_locals, elide_dead_single_field_struct_locals,
    elide_write_only_locals, flatten_seq_assignments,
};
use forward::{eliminate_loop_guarded_bounds_checks, forward_struct_field_constants};
use init_guard::remove_trivial_init_globals;
use peephole::{cleanup_wir, optimize_instrs};
use sroa::{sroa_multi_value_returns, sroa_single_field_parameters};
use string::simplify_short_string_appends;

/// Run all WIR-level optimizations on the module (in-place).
///
/// Optimization passes are skipped at `-O0`, but dead-item compaction always runs
/// so the emitter receives a clean module with no dead_*_indices to filter.
pub fn optimize_wir(module: &mut WirModule, opt_level: OptLevel) {
    if opt_level == OptLevel::O0 {
        dce::compact_dead_items(module);
        return;
    }
    // Whole-module pass: rewrite struct-returning functions to multi-value.
    sroa_multi_value_returns(module);

    // Whole-module pass: rewrite single-field struct parameters (including Box<T>)
    // from `ref null S` to scalar `T`, eliminating GC allocation at call sites.
    sroa_single_field_parameters(module);

    // Whole-module pass: after parameter SROA, call sites may still hold
    // `LocalSet(x, StructNew { [inner] })` where every use of `x` is via StructGet.
    // Substitute `inner` directly and nop the dead allocation.
    elide_dead_single_field_struct_locals(module);

    // Whole-module pass: collapse inlined Array::append sequences back to ArrayNewFixed.
    // Runs before promote/split so that recovered ArrayNewFixed nodes are eligible
    // for data segment promotion and large-literal splitting.
    collapse_array_append_sequences(module);

    // Per-function pass: forward known struct field constants through StructGet,
    // fold constant comparisons, and eliminate dead branches.
    // Runs after array append collapse so that recovered StructNew nodes (with
    // correct `used` field values) enable bounds check elimination.
    forward_struct_field_constants(module);

    // Per-function pass: eliminate bounds checks inside loops when the loop guard
    // already guarantees the index is in-bounds (e.g., `i < arr.len()` dominates
    // `arr[i]`). Runs after field forwarding and LICM so that hoisted `used`
    // values and copy chains are visible.
    eliminate_loop_guarded_bounds_checks(module);

    // Whole-module pass: rewrite String::append of short constant strings to
    // sequences of String::append_char calls, eliminating GC allocations.
    simplify_short_string_appends(module);

    // Whole-module pass: promote constant primitive arrays to data segments.
    // Runs before split_large_array_literals so promoted arrays don't get split.
    promote_constant_arrays_to_data(module);

    // Whole-module pass: split large array.new_fixed into array.new_default + array.set.
    split_large_array_literals(module);

    let types = &module.types;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            optimize_instrs(body, types);
        }
    }

    // Final cleanup pass: normalize the WIR so that codegen can emit it as-is.
    // Removes nops, redundant ref.as_non_null, and dead code after unreachable.
    cleanup_wir(module);

    // Flatten `LocalSet { name, value: Seq([preamble..., final]) }` into
    // `[preamble..., LocalSet { name, value: final }]` so the multi-field elision
    // pass can see bare `LocalSet(name, StructNew { N fields })` patterns.
    flatten_seq_assignments(module);

    // Whole-module pass: same as single-field but for multi-field structs (N > 1).
    // Must run after flatten_seq_assignments so that the StructNew is directly
    // visible as the value of LocalSet (not buried inside a Seq).
    elide_dead_multi_field_struct_locals(module);

    // Run cleanup again to remove nops left by elision.
    cleanup_wir(module);

    // Whole-module pass: eliminate functions whose return value is always
    // immediately dropped at every call site, converting them to void return.
    // Also removes dead writes to locals that only held the now-gone return value.
    drve_dead_return_values(module);
    elide_write_only_locals(module);
    cleanup_wir(module);

    // Remove trivial module-init guard globals: globals that are only ever
    // checked and set in an `if global { break; }; global = 1;` guard with
    // no actual initialization work inside.
    remove_trivial_init_globals(module);
    cleanup_wir(module);

    // Dead type elimination: mark types not referenced by any live function,
    // import, or global as dead, then compact the module to remove them.
    dce_unreachable_types(module);
    dce::compact_dead_items(module);
}

/// Collect all `func_ids` that must NOT be SROA'd or otherwise transformed
/// (exports, element tables, `RefFunc` references).
fn collect_pinned_func_ids(module: &WirModule) -> IndexSet<u32> {
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
fn collect_local_gets_deep(instr: &WirInstr, names: &mut IndexSet<String>) {
    if let WirInstr::LocalGet { name } = instr {
        names.insert(name.clone());
    }
    instr.for_each_child(&mut |child| {
        collect_local_gets_deep(child, names);
    });
}

/// Returns true if `instr` has no observable side effects.
/// Calls and memory/global stores are not side-effect-free.
/// Memory loads are treated as side-effect-free (pure read with no mutation).
fn is_side_effect_free(instr: &WirInstr) -> bool {
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
