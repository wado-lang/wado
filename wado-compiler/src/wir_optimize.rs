//! WIR optimization — structural and peephole optimizations on `WirPackage`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! ## Pass inventory
//!
//! | Module            | Pass                                       |
//! |-------------------|--------------------------------------------|
//! | `nullable_ref`    | Null-niche variant representation           |
//! | `sroa_return`     | Multi-value return SROA (structs + variants)|
//! | `sroa_param`      | Single-field parameter SROA                |
//! | `elide_struct`    | Struct local elimination (single + multi)   |
//! | `array`           | Push collapse / data promotion / splitting  |
//! | `const_forward`   | Struct field constant forwarding            |
//! | `string`          | Short string push_str simplification        |
//! | `peephole`        | Constant folding, copy elision, MV elision  |
//! | `cleanup`         | Nop/dead-code removal, normalization        |
//! | `dae`             | Dead argument elimination                   |
//! | `drve`            | Dead return value elimination               |
//! | `elide_local`     | Write-only local elimination                |
//! | `init_guard`      | Trivial init-guard global removal           |
//! | `dce`             | Dead code / type / global elimination       |

mod array;
mod cleanup;
mod const_forward;
mod dae;
mod dce;
mod drve;
mod elide_local;
mod elide_struct;
mod init_guard;
mod nullable_ref;
mod peephole;
mod sroa_param;
mod sroa_return;
mod string;
mod util;

use crate::compiler_host::SpanEmitter;
use crate::optimize::OptLevel;
use crate::wir::WirPackage;

pub use dce::{dce_unreachable_functions, dce_unreachable_types};

use array::{
    collapse_array_push_sequences, promote_constant_arrays_to_data, split_large_array_literals,
};
use cleanup::cleanup;
use const_forward::forward_struct_field_constants;
use dae::eliminate_dead_arguments;
use drve::eliminate_dead_return_values;
use elide_local::elide_write_only_locals;
use elide_struct::{
    elide_multi_field_struct_locals, elide_single_field_struct_locals, flatten_seq_assignments,
};
use init_guard::remove_trivial_init_globals;
use nullable_ref::optimize_nullable_refs;
use peephole::run_peephole;
use sroa_param::sroa_single_field_parameters;
use sroa_return::sroa_multi_value_returns;
use string::simplify_short_string_pushes;

/// Run a single WIR optimization pass with profiling.
fn wir_pass(
    name: &str,
    module: &mut WirPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut WirPackage),
) {
    profiler.span_start(name);
    f(module);
    profiler.span_end(name);
}

/// Run all WIR-level optimizations on the module (in-place).
///
/// Optimization passes are skipped at `-O0`, but dead-item compaction always runs
/// so the emitter receives a clean module with no dead_*_indices to filter.
pub fn optimize_wir(module: &mut WirPackage, opt_level: OptLevel, profiler: &dyn SpanEmitter) {
    if opt_level == OptLevel::O0 {
        dce::compact_dead_items(module);
        return;
    }

    // Phase 1: Type representation
    //
    // Rewrite type-level representations before any value-level passes see them.
    profiler.span_start("wir/phase1_type_repr");
    optimize_nullable_refs(module);
    // Pre-SROA copy propagation: inline trivial copies like `alias = source`
    // so that SROA can see direct variant access patterns (RefTest/RefCast on source).
    peephole::propagate_trivial_copies(module);
    sroa_multi_value_returns(module);
    sroa_single_field_parameters(module);
    profiler.span_end("wir/phase1_type_repr");

    // Phase 2: Struct local elimination (round 1)
    //
    // After parameter SROA, call sites may hold `LocalSet(x, StructNew { [inner] })`
    // where every use of `x` is via StructGet. Substitute `inner` directly.
    wir_pass("wir/elide_single_field_struct", module, profiler, |m| {
        elide_single_field_struct_locals(m);
    });

    // Phase 3: Data flow
    //
    // Collapse inlined push sequences and forward constants.
    // Order matters: push collapse exposes StructNew nodes that field
    // forwarding then uses for constant index bounds check elimination.
    // Loop-guarded bounds checks are eliminated at TIR level by the
    // condition_implication pass.
    profiler.span_start("wir/phase3_data_flow");
    collapse_array_push_sequences(module);
    // Struct-field constant forwarding. Recovers the
    // bounds-check-elimination path that ran on `array_push_collapse`'s
    // output (a fresh `StructNew Array<T> { used: N, ... }` literal).
    // The TIR-level `field_forward` pass cannot see through the
    // `__seq_lit:` block + push() chain, so this WIR-level pass remains
    // for that pattern.
    forward_struct_field_constants(module);
    profiler.span_end("wir/phase3_data_flow");

    // Phase 4: Library-specific rewrites
    //
    // Rewrite library call patterns into more efficient instruction sequences.
    profiler.span_start("wir/phase4_lib_rewrites");
    simplify_short_string_pushes(module);
    promote_constant_arrays_to_data(module);
    split_large_array_literals(module);
    profiler.span_end("wir/phase4_lib_rewrites");

    // Phase 5: Peephole + struct local elimination (round 2)
    //
    // Run peephole optimizations (constant folding, copy elision, multi-value
    // struct elision), then flatten seq assignments to expose multi-field struct
    // locals for elimination.
    profiler.span_start("wir/phase5_peephole");
    let types = &module.types;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            run_peephole(body, types);
        }
    }
    cleanup(module);
    flatten_seq_assignments(module);
    elide_multi_field_struct_locals(module);
    cleanup(module);
    profiler.span_end("wir/phase5_peephole");

    // Phase 6: Dead value elimination
    //
    // Eliminate dead arguments, dead return values, and write-only locals.
    profiler.span_start("wir/phase6_dead_value_elim");
    eliminate_dead_arguments(module);
    eliminate_dead_return_values(module);
    elide_write_only_locals(module);
    cleanup(module);
    profiler.span_end("wir/phase6_dead_value_elim");

    // Phase 7: Global cleanup
    //
    // Remove trivial module-init guard globals that serve no purpose after DCE.
    profiler.span_start("wir/phase7_global_cleanup");
    remove_trivial_init_globals(module);
    cleanup(module);
    profiler.span_end("wir/phase7_global_cleanup");

    // Phase 8: Final DCE & compaction
    //
    // Mark unreachable types as dead and compact all dead items out of the module.
    profiler.span_start("wir/phase8_dce_compact");
    dce_unreachable_types(module);
    dce::compact_dead_items(module);
    profiler.span_end("wir/phase8_dce_compact");
}
