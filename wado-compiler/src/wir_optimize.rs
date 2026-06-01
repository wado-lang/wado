//! WIR optimization — structural and peephole optimizations on `WirPackage`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! ## Pass inventory
//!
//! | Module            | Pass                                       |
//! |-------------------|--------------------------------------------|
//! | `nullable_ref`        | Null-niche variant representation           |
//! | `sroa_variant_return` | Multi-value return SROA (variants)          |
//! | `elide_struct`        | Struct local elimination (single + multi)   |
//! | `array`               | Push collapse / data promotion / splitting  |
//! | `const_forward`       | Struct field constant forwarding            |
//! | `peephole`            | Constant folding, copy elision              |
//! | `elide_local`     | Write-only local elim for WIR-only locals  |
//! | `cleanup`         | Nop/dead-code removal, normalization        |
//! | `init_guard`      | Trivial init-guard global removal           |
//! | `dce`             | Dead code / type / global elimination       |
//!
//! Dead-argument / dead-return-value elimination used to live here. They
//! were moved to TIR (`optimize::dae`, `optimize::drve`) so they can
//! interact with `inline` / `copy_prop` / `const_fold` / `dce` inside the
//! same fixed-point loop.
//!
//! Single-field parameter SROA and the adjacent-use Box-local elision
//! used to live here as `sroa_param` and `elide_struct`'s
//! `elide_adjacent_single_use_struct_locals`. Both moved to NIR
//! (`optimize::sroa_param` and `optimize::elide_box_local`) so the
//! rewritten signatures and substituted expressions feed the rest of
//! the optimizer's fix-point loop. The WIR-side `ModRef` summary that
//! powered the adjacent-use pass was retired with it; the NIR
//! equivalent is `optimize::mod_ref`.
//!
//! Write-only-local elimination is split across both layers:
//! `optimize::elide_local` handles TIR locals (user `let`, SROA / variant
//! shadow temps); `wir_optimize::elide_local` handles locals synthesised
//! by `wir_build` (`__match_scrut_N`, multi-value temps, pair temps) that
//! TIR cannot see. Both passes are narrow: they only elide locals that
//! are never read.

mod array;
mod cleanup;
mod const_forward;
mod dce;
mod elide_local;
mod elide_struct;
mod init_guard;
mod nullable_ref;
mod peephole;
mod sroa_variant_return;
mod util;

use crate::compiler_host::SpanEmitter;
use crate::optimize::OptLevel;
use crate::wir::WirPackage;

pub use dce::{compact_dead_items, dce_unreachable_types, mark_unreachable_defined_functions};

use array::{promote_constant_arrays_to_data, split_large_array_literals};
use cleanup::cleanup;
use const_forward::forward_struct_field_constants;
use elide_local::elide_write_only_locals;
use elide_struct::{
    elide_multi_field_struct_locals, elide_single_field_struct_locals, flatten_seq_assignments,
};
use init_guard::remove_trivial_init_globals;
use nullable_ref::optimize_nullable_refs;
use peephole::run_peephole;
use sroa_variant_return::sroa_variant_returns;

/// Run a single WIR optimization pass with profiling.
///
/// Honours `WADO_LIST_PASSES`, `WADO_DUMP_PASS_BEFORE`, and
/// `WADO_DUMP_PASS_AFTER` — see `crate::optimize::pass_dump`.
fn wir_pass(
    name: &str,
    module: &mut WirPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut WirPackage),
) {
    use crate::optimize::pass_dump::{self, Phase};
    pass_dump::list_pass(name);
    if pass_dump::should_skip_pass(name) {
        return;
    }
    pass_dump::dump_wir(name, module, Phase::Before);
    profiler.span_start(name);
    f(module);
    profiler.span_end(name);
    pass_dump::dump_wir(name, module, Phase::After);
}

/// Run all WIR-level optimizations on the module (in-place).
///
/// Optimization passes are skipped at `-O0`, but dead-item compaction always runs
/// so the emitter receives a clean module with no dead_*_indices to filter.
///
/// `optimize_nullable_refs` is the exception: it runs unconditionally (even at
/// `-O0`) because it picks a *representation* for eligible variants (e.g.
/// `Option<&T>` → `(ref null T)`), and that representation has to be
/// consistent between the storage (the Wasm initializer can only encode
/// `None` as `ref.null` for these variants) and the consumers (pattern
/// match emits `ref.test`/`ref.cast` against the chosen repr). Skipping
/// it at `-O0` produces a module where a `null`-initialised
/// `Option<&T>` global stores `ref.null` but `if let Some(_) = ...`
/// expects a `struct.new`-shaped subtype-hierarchy value, trapping in
/// `ref.as_non_null` on the first read.
pub fn optimize_wir(module: &mut WirPackage, opt_level: OptLevel, profiler: &dyn SpanEmitter) {
    // Always run NullableRef before anything else — see comment above.
    optimize_nullable_refs(module);

    if opt_level == OptLevel::O0 {
        dce::compact_dead_items(module);
        return;
    }

    // Phase 1: Type representation
    //
    // Rewrite type-level representations before any value-level passes see them.
    profiler.span_start("wir/phase1_type_repr");
    // Pre-SROA copy propagation: inline trivial copies like `alias = source`
    // so that SROA can see direct variant access patterns (RefTest/RefCast on source).
    wir_pass("wir/propagate_trivial_copies", module, profiler, |m| {
        peephole::propagate_trivial_copies(m);
    });
    wir_pass("wir/sroa_variant_returns", module, profiler, |m| {
        sroa_variant_returns(m);
    });
    profiler.span_end("wir/phase1_type_repr");

    // Phase 2: Struct local elimination (round 1)
    //
    // After NIR `sroa_param` strips single-field struct parameters down
    // to scalars, call sites may hold `LocalSet(x, StructNew { [inner] })`
    // where every use of `x` is via StructGet. Substitute `inner` directly.
    // The adjacent-use Box-local elision that used to follow this pass
    // now runs at NIR (`optimize::elide_box_local`).
    wir_pass("wir/elide_single_field_struct", module, profiler, |m| {
        elide_single_field_struct_locals(m);
    });

    // Phase 3: Data flow
    //
    // Forward constant struct fields. Array literals already arrive as
    // `StructNew Array<T> { repr: array.new_fixed, used: N }` (the NIR
    // `optimize::array_literal` pass materializes the literal and `wir_build`
    // lowers it), so the bounds-check-elimination path keys on that shape
    // directly. Loop-guarded bounds checks are eliminated at TIR level by the
    // condition_implication pass.
    profiler.span_start("wir/phase3_data_flow");
    forward_struct_field_constants(module);
    profiler.span_end("wir/phase3_data_flow");

    // Phase 4: Library-specific rewrites
    //
    // Rewrite library call patterns into more efficient instruction sequences.
    profiler.span_start("wir/phase4_lib_rewrites");
    promote_constant_arrays_to_data(module);
    split_large_array_literals(module);
    profiler.span_end("wir/phase4_lib_rewrites");

    // Phase 5: Peephole + struct local elimination (round 2)
    //
    // Run peephole optimizations (constant folding, copy elision, multi-value
    // struct elision), then flatten seq assignments to expose multi-field struct
    // locals for elimination. The Nops/dead locals these passes leave behind
    // are picked up by phase 7's final `cleanup` before codegen.
    profiler.span_start("wir/phase5_peephole");
    let types = &module.types;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            run_peephole(body, types);
        }
    }
    flatten_seq_assignments(module);
    elide_multi_field_struct_locals(module);
    // Multi-field struct-local elision rewrites multi-value destructures into
    // fresh `LocalSet alias = LocalGet temp` copies. Re-run copy propagation to
    // fold those away — the phase-1 run happened before these copies existed.
    wir_pass(
        "wir/propagate_trivial_copies_post_sroa",
        module,
        profiler,
        |m| {
            peephole::propagate_trivial_copies(m);
        },
    );
    profiler.span_end("wir/phase5_peephole");

    // Phase 6: Write-only local elimination for WIR-synthesised locals
    //
    // TIR's `optimize::elide_local` only sees TIR locals. The WIR builder
    // synthesises locals during lowering — `__match_scrut_N` for match
    // scrutinee binding, multi-value temps, `__pair_temp_N` for
    // Future/Stream pair returns — that no TIR pass can reach. Once the
    // surrounding lowering shape turns out not to need their value (e.g.
    // every match arm uses wildcard / binding patterns), they sit as
    // dead writes. Strip them here so codegen doesn't emit unreachable
    // locals.
    wir_pass("wir/elide_write_only_locals", module, profiler, |m| {
        elide_write_only_locals(m);
    });

    // Phase 7: Global cleanup
    //
    // Remove trivial module-init guard globals that serve no purpose after DCE,
    // then run the final body cleanup before codegen sees the module: removes
    // Nops, dead `DeclareLocal`s, and dead code after `Unreachable`.
    profiler.span_start("wir/phase7_global_cleanup");
    remove_trivial_init_globals(module);
    cleanup(module);
    profiler.span_end("wir/phase7_global_cleanup");

    // Phase 8: Final DCE & compaction
    //
    // Mark functions orphaned by earlier WIR passes as dead (notably a
    // single-element array literal's `Array<T>::push` instantiation, whose
    // only call site never existed once NIR `optimize::array_literal`
    // materialized the literal), then mark unreachable types as dead and
    // compact all dead items out of the module.
    profiler.span_start("wir/phase8_dce_compact");
    dce::mark_unreachable_defined_functions(module);
    dce_unreachable_types(module);
    dce::compact_dead_items(module);
    profiler.span_end("wir/phase8_dce_compact");
}
