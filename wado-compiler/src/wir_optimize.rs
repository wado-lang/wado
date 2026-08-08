//! WIR optimization — structural and peephole passes on `WirPackage`.
//! Runs after `wir_build`, before `codegen::emit`.
//!
//! ## Pass inventory
//!
//! `nullable_ref` is a mandatory representation lowering, not an optimization:
//! it runs at every `-O`. The rest are optimizations, skipped at `-O0`.
//!
//! | Module            | Pass                                       |
//! |-------------------|--------------------------------------------|
//! | `nullable_ref`        | Null-niche variant representation (mandatory) |
//! | `sroa_variant_return` | Nested result-slot flattening               |
//! | `elide_struct`        | Box local elimination + seq-assign flattening |
//! | `array`               | Data promotion / splitting / zero-fill elision |
//! | `const_forward`       | Struct field constant forwarding            |
//! | `peephole`            | Constant folding, copy elision              |
//! | `elide_local`     | Write-only local elim for WIR-only locals  |
//! | `cleanup`         | Nop/dead-code removal, normalization        |
//! | `branch_hint`     | `br_if` selection + trap-based hint inference |
//! | `prune_dead_data` | Unreferenced passive data segment removal   |
//! | `dce`             | Dead code / type / global elimination       |
//!
//! Related passes live elsewhere: dead-arg/-return elim and single-field param
//! SROA moved to NIR (`optimize::{dae,drve,sroa_param,elide_box_local}`) to join
//! its fixed-point loop, and variant-return widening followed them
//! (`optimize::sroa_variant_return`), leaving only the slot flattening that
//! needs post-lowering shapes. Write-only-local elim is split —
//! `optimize::elide_local` for TIR locals, `elide_local` here for
//! `wir_build`-synthesised locals TIR can't see.
//!
//! A `#![wasm_module(...)]` core module — the allocator — runs this same list as
//! a package of its own ([`optimize_wasm_modules`]), under its own pass names.

pub(crate) mod array;
mod branch_hint;
mod cleanup;
mod const_forward;
mod const_global;
mod dce;
mod dedupe_const_globals;
mod elide_local;
mod elide_struct;
mod nullability;
mod nullable_ref;
mod peephole;
mod prune_dead_data;
mod sroa_variant_return;
mod util;

use crate::codegen_flags::CodegenFlags;
use crate::compiler_host::SpanEmitter;
use crate::optimize::OptLevel;
use crate::wir::WirPackage;

pub use dce::{compact_dead_items, dce_unreachable_types, mark_unreachable_defined_functions};

use array::{
    elide_zero_fill_of_fresh_arrays, promote_constant_arrays_to_data, split_large_array_literals,
};
use branch_hint::{infer_branch_hints, select_br_ifs};
use cleanup::{cleanup, cleanup_global_inits};
use const_forward::forward_struct_field_constants;
use const_global::promote_const_global_inits;
use dedupe_const_globals::dedupe_const_globals;
use elide_local::elide_write_only_locals;
use elide_struct::{elide_adjacent_box_locals, flatten_seq_assignments};
use nullable_ref::lower_nullable_refs;
use peephole::run_peephole;
use prune_dead_data::prune_dead_data;
use sroa_variant_return::flatten_variant_slots;

/// What a pass run's passes are called: `wir/<pass>` for the main package,
/// `wir/<module>:<pass>` for a `#![wasm_module]` package's.
///
/// `WADO_SKIP_PASS` and `WADO_DUMP_PASS_*` match on those names and count
/// occurrences per name, so sharing them across the two runs would make the
/// allocator occurrence #1 of every pass and bisect the wrong module.
#[derive(Clone, Copy)]
struct PassScope<'a>(Option<&'a str>);

impl PassScope<'_> {
    fn name(self, pass: &str) -> String {
        match self.0 {
            None => format!("wir/{pass}"),
            Some(module) => format!("wir/{module}:{pass}"),
        }
    }
}

/// Run a single WIR optimization pass with profiling.
///
/// Honours `WADO_LIST_PASSES`, `WADO_DUMP_PASS_BEFORE`, and
/// `WADO_DUMP_PASS_AFTER` — see `crate::optimize::pass_dump`.
fn wir_pass(
    scope: PassScope,
    pass: &str,
    module: &mut WirPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut WirPackage),
) {
    use crate::optimize::pass_dump::{self, Phase};
    let name = scope.name(pass);
    pass_dump::list_pass(&name);
    if pass_dump::should_skip_pass(&name) {
        return;
    }
    pass_dump::dump_wir(&name, module, Phase::Before);
    profiler.span_start(&name);
    f(module);
    profiler.span_end(&name);
    pass_dump::dump_wir(&name, module, Phase::After);
}

/// Run the WIR-level optimizations on the module (in-place).
///
/// Passes are skipped at `-O0`; only dead-item compaction runs, so the emitter
/// never sees `dead_*_indices`.
///
/// `lower_nullable_refs` is the exception — a mandatory representation lowering,
/// not an optimization, so it runs before the `-O0` gate. The frontend already
/// emits `None` as `ref.null` (the `?`-desugar yields `TirExprKind::Null`); this
/// picks the matching WIR repr, so skipping it miscompiles rather than just slowing
/// code. (The frontend/WIR repr split is a known layering smell, tracked separately.)
pub fn optimize_wir(
    module: &mut WirPackage,
    opt_level: OptLevel,
    flags: CodegenFlags,
    profiler: &dyn SpanEmitter,
) {
    optimize_scoped(module, opt_level, flags, profiler, PassScope(None));
}

fn optimize_scoped(
    module: &mut WirPackage,
    opt_level: OptLevel,
    flags: CodegenFlags,
    profiler: &dyn SpanEmitter,
    scope: PassScope,
) {
    optimize_wasm_modules(module, opt_level, flags, profiler);

    // Mandatory representation lowering — runs before the `-O0` gate. See above.
    lower_nullable_refs(module);

    if opt_level == OptLevel::O0 {
        // Branch hints are independent of `-O`, like build-time cold-path hints.
        if flags.branch_hinting {
            wir_pass(scope, "infer_branch_hints", module, profiler, |m| {
                infer_branch_hints(m);
            });
        }
        dce::compact_dead_items(module);
        finalize_locals(module);
        return;
    }

    // Phase 1: type representation, before any value-level pass sees it.
    profiler.span_start(&scope.name("phase1_type_repr"));
    // Inline trivial `alias = source` copies so SROA sees RefTest/RefCast on source.
    wir_pass(scope, "propagate_trivial_copies", module, profiler, |m| {
        peephole::propagate_trivial_copies(m);
    });
    profiler.span_end(&scope.name("phase1_type_repr"));

    // Phase 2: box-local elimination — substitute `inner` at the single
    // `StructGet` use of a `Box<T>` local `lower::plan::boxing` minted.
    wir_pass(scope, "elide_adjacent_box_locals", module, profiler, |m| {
        elide_adjacent_box_locals(m);
    });

    // Phase 3: forward constant struct fields. List literals arrive as
    // `StructNew List<T> { repr: array.new_fixed, used: N }`, so bounds-check
    // elimination keys on that shape.
    profiler.span_start(&scope.name("phase3_data_flow"));
    wir_pass(
        scope,
        "forward_struct_field_constants",
        module,
        profiler,
        |m| {
            forward_struct_field_constants(m);
        },
    );
    profiler.span_end(&scope.name("phase3_data_flow"));

    // Phase 4: rewrite library call patterns into tighter instruction sequences.
    profiler.span_start(&scope.name("phase4_lib_rewrites"));
    wir_pass(
        scope,
        "promote_constant_arrays_to_data",
        module,
        profiler,
        |m| {
            promote_constant_arrays_to_data(m);
        },
    );
    wir_pass(scope, "split_large_array_literals", module, profiler, |m| {
        split_large_array_literals(m);
    });
    wir_pass(
        scope,
        "elide_zero_fill_of_fresh_arrays",
        module,
        profiler,
        |m| {
            elide_zero_fill_of_fresh_arrays(m);
        },
    );
    profiler.span_end(&scope.name("phase4_lib_rewrites"));

    // Phase 5: peephole (instruction selection, const fold, copy elision), then
    // flatten seq assignments so the copy propagation below sees the
    // destructures they hide. Leftover Nops/dead locals are cleaned in phase 7.
    profiler.span_start(&scope.name("phase5_peephole"));
    wir_pass(scope, "run_peephole", module, profiler, |m| {
        let types = &m.types;
        for func in &mut m.functions {
            let locals = func.declared_locals();
            if let Some(body) = &mut func.body {
                let null = nullability::Nullability::new(&locals);
                run_peephole(body, &null, types);
            }
        }
    });
    wir_pass(scope, "flatten_seq_assignments", module, profiler, |m| {
        flatten_seq_assignments(m);
    });
    // Re-run copy propagation: `flatten_seq_assignments` exposes fresh
    // `LocalSet alias = LocalGet temp` copies the phase-1 run never saw.
    wir_pass(
        scope,
        "propagate_trivial_copies_post_sroa",
        module,
        profiler,
        |m| {
            peephole::propagate_trivial_copies(m);
        },
    );
    // Variant-slot flattening: now that `flatten_seq_assignments` + copy
    // propagation have exposed the clean `multivalue_bind […] = call;
    // x = if Ok { payload } else { return Err }` shape, split a `ref W` result
    // slot into `W`'s multi-value layout (`compute_variant_layout`, the same
    // engine single-level SROA uses), removing the per-element inner box that
    // single-level variant-return SROA leaves boxed. Runs to a fix-point, so
    // nested-in-nested slots peel one level per round.
    wir_pass(scope, "flatten_variant_slots", module, profiler, |m| {
        flatten_variant_slots(m);
    });
    profiler.span_end(&scope.name("phase5_peephole"));

    // Phase 6: strip write-only WIR-synthesised locals (`__match_scrut_N`,
    // multi-value temps, `__pair_temp_N`) that no TIR pass can reach, so codegen
    // doesn't emit dead locals.
    wir_pass(scope, "elide_write_only_locals", module, profiler, |m| {
        elide_write_only_locals(m);
    });

    // Phase 7: global cleanup, then final body cleanup (Nops, dead
    // `DeclareLocal`s, dead code after `Unreachable`) before codegen.
    profiler.span_start(&scope.name("phase7_global_cleanup"));
    // Promote now-constant global inits to eager Wasm constants first, so the
    // emptied `__initialize_module` and its guard become reclaimable here.
    wir_pass(scope, "promote_const_global_inits", module, profiler, |m| {
        promote_const_global_inits(m);
    });
    wir_pass(scope, "cleanup_global_inits", module, profiler, |m| {
        cleanup_global_inits(m);
    });
    // Merge identical immutable const globals now that they are immutable.
    wir_pass(scope, "dedupe_const_globals", module, profiler, |m| {
        dedupe_const_globals(m);
    });
    wir_pass(scope, "cleanup", module, profiler, |m| {
        cleanup(m);
    });
    // Drop data segments `register_literal_data` registered speculatively
    // but no surviving `array.new_data` ended up reading (a bounded
    // force-eager global promoted to `array.new_fixed` instead).
    wir_pass(scope, "prune_dead_data", module, profiler, |m| {
        prune_dead_data(m);
    });
    // Collapse `if cond { br N }` guards into `br_if`, then infer trap-based hints.
    wir_pass(scope, "select_br_if", module, profiler, |m| {
        select_br_ifs(m);
    });
    if flags.branch_hinting {
        wir_pass(scope, "infer_branch_hints", module, profiler, |m| {
            infer_branch_hints(m);
        });
    }
    profiler.span_end(&scope.name("phase7_global_cleanup"));

    // Phase 8: mark functions/types/globals orphaned by earlier passes dead,
    // then compact. Globals are marked after functions so a global read only by
    // an already-dead function is itself pruned.
    profiler.span_start(&scope.name("phase8_dce_compact"));
    dce::mark_unreachable_defined_functions(module);
    dce::mark_unreferenced_globals(module);
    dce_unreachable_types(module);
    dce::compact_dead_items(module);
    profiler.span_end(&scope.name("phase8_dce_compact"));

    // Phase 9: finalize the declared-local SSoT now that no pass adds or removes
    // a `DeclareLocal`.
    finalize_locals(module);
}

/// Run this same list over each `#![wasm_module(...)]` core module.
///
/// Codegen emits those packages verbatim, so this is the only WIR-level
/// optimization they get. Reachability is seeded from the module's own exports
/// first, so an unused allocator variant is dropped at every `-O`.
///
/// Recursion terminates at one level: such a package has no nested
/// `wasm_modules` of its own.
fn optimize_wasm_modules(
    module: &mut WirPackage,
    opt_level: OptLevel,
    flags: CodegenFlags,
    profiler: &dyn SpanEmitter,
) {
    let mut wasm_modules = std::mem::take(&mut module.wasm_modules);
    for (name, wasm_module) in &mut wasm_modules {
        dce::mark_unreachable_defined_functions(wasm_module);
        // A wasm module has a 1:1 function/type correspondence — each function
        // owns the func type at its own index — so a dead function's type is
        // dead too.
        for i in wasm_module.dead_func_indices.clone() {
            wasm_module.dead_type_indices.insert(i);
        }
        optimize_scoped(
            wasm_module,
            opt_level,
            flags,
            profiler,
            PassScope(Some(name)),
        );
    }
    module.wasm_modules = wasm_modules;
}

/// Freeze each function's declared locals into `func.locals` for the emitter.
/// Called on every `optimize_wir` exit path, so `-O0` finalizes too.
fn finalize_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        func.locals = func.declared_locals();
    }
}
