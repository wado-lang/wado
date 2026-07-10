//! Optimization passes for Wado NIR.
//!
//! The optimizer rewrites the [`NirPackage`] in place. The full pass list and
//! ordering rationale lives in [`docs/optimizer.md`](../../../docs/optimizer.md);
//! the inventory below is just an index of the modules under `optimize/`.
//!
//! Fixed-point loop ([`run_optimization_passes`], in execution order):
//! 1.  `container_sroa` — `AoS` → `SoA` for `List<Tuple<...>>` / `List<Struct>`.
//! 2.  `peephole` (pre-inline) — unified engine session: `MatchToSwitchRule`
//!     (dense `Match` → `Switch`), `ValueCopyElideRule` (strip `$value_copy$T<id>`
//!     wrappers on read-only bindings), `string_push` (`buf.push_str("short")` →
//!     per-byte `push`), `elide_local` (write-only local elimination), env-free
//!     `const_fold` (literal arithmetic + pure CTFE), and `const_branch_prune`
//!     (trivial-block / dead-statement cleanup). See `optimize/peephole.rs`.
//! 3.  `value_copy_demote` — demote deep `$value_copy$T` to a shallow spine
//!     copy when elements are provably immutable through the binding. Runs
//!     *after* the pre-inline `peephole` so `ValueCopyElideRule` strips
//!     fully-read-only copies first; demoting first would hide them behind a
//!     shallow `array_clone` shape elide can no longer match.
//! 4.  `sroa_param` — single-field (`Box<T>`) parameter SROA.
//! 5.  `inline` — function inlining.
//! 6.  `peephole` (post-inline) — unified engine session: `RefElimRule` (drop
//!     reference bindings inlining exposes), `ElideBoxLocalRule` (collapse
//!     `let x = Box{value: inner}; … x.value …` shells),
//!     `LabeledBlockFusionRule` (collapse inlined-helper `Option<T>` /
//!     `Result<T, E>` allocations into the consumer's `if-let` / `match` site),
//!     `array_literal` (materialize `ArrayLiteral` from the `array_new + push`
//!     window), `elide_local`, env-free `const_fold`, and `const_branch_prune`.
//! 7.  `sroa` — Scalar Replacement of Aggregates.
//! 8.  `copy_prop` — copy propagation.
//! 9.  `dae` — Dead Argument Elimination.
//! 10. `drve` — Dead Return Value Elimination.
//! 11. `cse` — Loop-level Common Subexpression Elimination.
//! 12. `store_load_forward` — store-to-load forwarding.
//! 13. `const_folding` — partial evaluation via [`crate::niri`] (also drives
//!     alias-aware field-knowledge tracking; see `alias`). The flow-sensitive
//!     half; the env-free folds and trivial-block pruning run in `peephole`.
//! 14. `licm` — Loop-Invariant Code Motion.
//! 15. `condition_implication` — eliminate conditions implied by dominators.
//! 16. `tmpl_hoist` — hoist template-string backing buffers out of loops.
//!
//! Dense `Match` → `Switch` on global initializer bodies runs once before the
//! loop (`match_to_switch_globals`); `-O0` skips the loop and lowers everything
//! via `match_to_switch_all`.
//!
//! Once after the loop converges: `field_scalarize` (Hot Field Scalarization),
//! then `promote_fields`, the post-promote structural BCE rerun, and
//! `loop_version_bce` (loop-versioned BCE: statically-unprovable in-loop
//! bounds asserts get a runtime residual guard and a checkless fast clone;
//! a constant-fill fast loop collapses to `array.fill`). Last, after every
//! scalarization / globalization recognizer has matched its shape,
//! `scalar_forward` folds the inliner's leftover single-use pure-scalar
//! value-parameter temps into their uses.
//!
//! Outside the loop ([`optimize`]): Dead Code Elimination (`dce`, around the
//! loop) plus the always-on post-optimization rewrites the Wasm backend
//! depends on — `select_lowering` and `multi_value_return` classification.
//!
//! The `$value_copy$T` insertion + synthesis steps that materialize Wado's
//! value-copy semantics live in the lower phase (`lower::plan::value_copy`) —
//! by the time NIR reaches the optimizer, every defensive deep-copy is
//! explicit. The optimizer only *removes* redundant copies, via
//! `value_copy_elide` (full strip) and `value_copy_demote` (deep → shallow).

mod alias;
mod arena_query;
mod array_literal;
mod clone_forward;
mod condition_implication;
mod const_branch_prune;
mod const_folding;
mod const_object_globalization;
mod container_sroa;
mod copy_prop;
mod dae;
pub mod dce;
mod drve;
mod elide_box_local;
mod elide_local;
mod escape;
mod extract;
mod field_scalarize;
mod gate;
mod inline;
mod labeled_block_fusion;
mod licm;
mod loop_version_bce;
mod match_to_switch;
mod mod_ref;
mod multi_value_return;
mod peephole;
mod ref_elim;
mod scalar_forward;
mod select_lowering;
mod sroa;
mod sroa_param;
mod store_load_forward;
mod string_push;
mod tmpl_hoist;
mod value_copy;
mod value_copy_demote;
mod value_copy_elide;

use const_branch_prune::{prune_constant_branches, prune_template_block_wrappers};
use const_folding::{fold_constants, fold_constants_all};
use const_object_globalization::globalize_const_objects;
use container_sroa::scalarize_containers;
use copy_prop::propagate_copies;
use dae::eliminate_dead_arguments;
use dce::{
    analyze_dce, filter_bytes_literals, filter_string_literals,
    remove_unreachable_closure_functors, remove_unreachable_functions, remove_unreachable_globals,
    remove_unreachable_types,
};
use drve::eliminate_dead_return_values;
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use licm::apply_licm;
use match_to_switch::{match_to_switch_all, match_to_switch_globals};
use scalar_forward::forward_scalar_temps;
use sroa::scalar_replace_aggregates;
use sroa_param::sroa_single_field_parameters;
use store_load_forward::forward_stores_to_loads_all;
use tmpl_hoist::hoist_template_buffers;
use value_copy_demote::demote_value_copies;

use crate::compiler_host::SpanEmitter;
use crate::nir_package::NirPackage;

/// Configuration for optimization passes
struct OptConfig {
    /// Number of fixed-point iterations
    iterations: u32,
    /// Maximum statement count for inlining
    inline_threshold: usize,
}

/// Optimization level for the compiler.
///
/// All levels run DCE (Dead Code Elimination) to remove unreachable code.
/// The levels differ in what optimization passes are run:
/// - O0: DCE only - no optimization passes
/// - O1: Development - fast compilation with basic optimization passes
/// - O2: Production - full optimization passes (default)
/// - O3: Production - aggressive optimization passes
/// - Os: Frontend - O2 + name section stripping for smaller binaries
///
/// Configuration for each level:
/// | Level | DCE | Iterations | Inline Threshold |
/// |-------|-----|------------|------------------|
/// | O0    | Yes | 0          | N/A              |
/// | O1    | Yes | 2          | 4                |
/// | O2    | Yes | 10         | 13               |
/// | O3    | Yes | 30         | 32               |
/// | Os    | Yes | 10         | 13               |
///
/// "Iterations" / "Inline Threshold" describe the fixed-point
/// optimization loop. Post-loop rewrites that the Wasm backend depends
/// on (`select_lowering`, multi-value-return classification) always
/// run, including at `O0`; only the fixed-point loop itself is skipped
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimization loop. DCE plus the always-on post-optimization
    /// rewrites required by the Wasm backend.
    O0,
    /// Development optimizations. All passes with fast iteration count.
    /// Iterations: 2, Inline threshold: 4.
    O1,
    /// Production optimizations. All passes including DCE.
    /// Iterations: 10, Inline threshold: 13.
    #[default]
    O2,
    /// Aggressive production optimizations. All passes including DCE.
    /// Iterations: 30, Inline threshold: 32.
    O3,
    /// Size optimizations. Same as O2 plus name section stripping.
    /// Intended for frontend/browser deployment.
    Os,
}

/// Optimize a Package by analyzing and populating its usage fields.
///
/// This is the main entry point for the optimizer. Based on the optimization
/// level, it applies different optimization strategies:
///
/// - O0: DCE only (no optimization passes)
/// - O1: Basic optimization passes + DCE
/// - O2: Full optimization passes + DCE (default)
/// - O3: Aggressive optimization passes + DCE
/// - Os: Same as O2 plus name section stripping
///
/// All levels run DCE to remove unreachable functions and types, which
/// significantly reduces codegen work.
///
/// For O1+, DCE runs twice: once before the optimization loop to reduce
/// the working set (eliminating stdlib functions/types the program doesn't
/// use), and once after to clean up code made dead by optimizations
/// (e.g., functions inlined away).
///
/// The `inline_threshold` and `opt_iterations` parameters override the
/// defaults for the given `opt_level` when provided.
/// Maximum UTF-8 byte length for a string literal to be materialized with a
/// constant `array.new_fixed<u8>` repr instead of a passive `array.new_data`
/// data segment. Below it, a constant string global promotes to an eager Wasm
/// constant; above it the compact data-segment repr is kept (and the global
/// stays lazy). `-O3` trades a little code size for more eager string globals;
/// the other levels (and `-Os`, which targets size) stay conservative.
///
/// This threshold applies to every *ordinary* string literal, including one
/// with nothing to do with `const_object_globalization` (e.g. an `assert`
/// diagnostic template, which `codegen_flags` tests require stay byte-scannable
/// below this threshold). A global `const_object_globalization` hoists from an
/// `InlineRef` candidate gets a separate, size-bounded override — see
/// [`crate::nir::NirGlobal::prefer_fixed_string_repr`] — since such a hoist
/// rewrites the literal's construction in place rather than moving it to
/// `__initialize_module`, so only an eager repr lets the redundant re-assignment
/// be recognized and dropped.
fn string_inline_max_bytes(opt_level: OptLevel) -> usize {
    match opt_level {
        OptLevel::O3 => 8,
        _ => NirPackage::DEFAULT_STRING_INLINE_MAX_BYTES,
    }
}

pub fn optimize(
    mut project: NirPackage,
    opt_level: OptLevel,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    profiler: &dyn SpanEmitter,
) -> NirPackage {
    // Decide the short-string inline threshold once, from the opt level. Read
    // by `wir_build` (`translate_packed_array` / `register_literal_data`) to
    // pick a constant `array.new_fixed<u8>` repr for strings at or below it —
    // which lets a constant string global promote to an eager Wasm constant.
    project.string_inline_max_bytes = string_inline_max_bytes(opt_level);
    match opt_level {
        OptLevel::O0 => {
            // No optimizations, but still run DCE to reduce codegen work
            run_dce(&mut project, profiler);
            // Dense-int / dense-enum `Match` → `Switch` is a codegen-
            // friendly late lowering. The translator emits a canonical
            // `Match` (see WEP 2026-05-11). Materialising `Switch` here
            // — even at -O0 — keeps wir_build's `br_table` path live
            // for dense matches when the optimizer loop is skipped.
            // The synthesised default-arm call resolves to
            // `builtin::unreachable`, which lowers to Wasm
            // `unreachable` directly and is never DCE'd, so ordering
            // around DCE is irrelevant.
            run_pass("nir/match_to_switch", &mut project, profiler, |p| {
                // O0 skips the loop/gate entirely; lower every function.
                match_to_switch_all(p)
            });
        }
        OptLevel::O1 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(2),
                inline_threshold: inline_threshold.unwrap_or(4),
            };
            // Early DCE: remove unreachable functions/types before optimization
            // to reduce the working set for subsequent passes
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            // Final DCE: clean up code made dead by optimizations
            run_dce(&mut project, profiler);
        }
        OptLevel::O2 | OptLevel::Os => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(10),
                // Threshold 13 is the sweet spot for -O2/-Os: on
                // syntax-highlight (Gale-generated SQLite highlighter)
                // throughput is ~30% better than 14 because at 14 a
                // specific Gale parser action function chain-inlines
                // and the resulting code regresses (13.5ms/iter -> 18ms).
                // Sizes at 13 and 14 differ by only ~4KB.
                inline_threshold: inline_threshold.unwrap_or(13),
            };
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            run_dce(&mut project, profiler);
            if opt_level == OptLevel::Os {
                project.strip_names = true;
            }
        }
        OptLevel::O3 => {
            // The iteration cap is purely defensive. Since
            // `field_forward` was merged into `const_fold` (issue
            // #1009), straight-line constant chains produced by
            // inlined `List::push` and similar patterns fold in a
            // single iteration rather than one statement per round,
            // so even Gale parsers reach a true fixed point in well
            // under 10 iterations. 30 leaves comfortable headroom for
            // whatever gradient new fixtures expose.
            //
            // Threshold 32 sits just under a discrete size cliff at
            // 33 on syntax-highlight (859KB -> 1049KB, crossing 1MB)
            // where additional Gale action functions become inline
            // candidates with no measurable speed payoff.
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(30),
                inline_threshold: inline_threshold.unwrap_or(32),
            };
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            run_dce(&mut project, profiler);
        }
    }

    // FieldAccess promotion (WEP P2): scalar fields over stable receivers freeze
    // to operands (born-as-operands), so store-load forwarding / cse become
    // operand reads. Runs after the optimization loop (so the struct shape is
    // post-SROA) but BEFORE select_lowering / multi_value_return, which rewrite
    // the body into WIR-shaped forms the value-graph build would misread. The
    // two soundness gates (scalar-field + receiver-availability) and the WIR
    // const-local propagation keep it default-safe.
    if opt_level != OptLevel::O0 {
        run_pass("nir/promote_fields", &mut project, profiler, |p| {
            extract::freeze_pure_arith(p, /* include_fields */ true, /* early */ false)
        });
        // Re-run the structural BCE matcher now that `promote_fields` froze
        // invariant bounds (`arr.used`) into constant operands the in-loop
        // cond-impl could not see; pair with `const_branch_prune` so the
        // now-`false` checks' panic blocks are removed. Must precede
        // `select_lowering`, which reshapes conditions out of matcher form.
        run_pass("nir/cond_impl_post_promote", &mut project, profiler, |p| {
            let mut changed = false;
            while condition_implication::eliminate_post_promote(p) | prune_constant_branches(p) {
                changed = true;
            }
            changed
        });
        // Loop-versioned BCE: a check whose bound is loop-invariant but not
        // statically related to the loop guard (the relation lives at the
        // call site) is deleted in a fast clone guarded by the runtime
        // residual `H < B`; the original loop is kept as the slow arm. Runs
        // after `cond_impl_post_promote` so only statically-unprovable
        // checks are versioned, and before `select_lowering`, which
        // reshapes conditions out of matcher form.
        run_pass("nir/loop_version_bce", &mut project, profiler, |p| {
            loop_version_bce::version_loops(p)
        });
    }

    // Post-optimization rewrites: select lowering for branchless Wasm
    run_pass("nir/select_lowering", &mut project, profiler, |p| {
        select_lowering::select_lowering(p)
    });

    // Multi-value return ABI classification: marks tuple- or
    // user-struct-returning functions whose every return site is a fresh
    // `TupleLiteral` / `StructLiteral` and whose every call site
    // destructures via `FieldAccess` on the bound temp. WIR build
    // (`wir_build::translate::try_emit_multi_value_let`) reads the marker
    // to emit the multi-value Wasm signature on the function definition
    // and to rewrite call-site `let __tmp = Call(f)` into
    // `MultiValueLocalBind [__tmp_0, …] = Call(f)` with subsequent
    // `FieldAccess` reads going to the split locals directly. Runs after
    // every other transformation so the analysis sees the final NIR
    // shape.
    run_pass("nir/multi_value_return", &mut project, profiler, |p| {
        multi_value_return::classify_multi_value_returns(p)
    });

    // Freeze re-emittable pure arithmetic (constants / local reads composed by
    // Binary/Unary/Cast) into operand values, materialised by the WIR
    // extractor. Runs last so no binary-walking pass sees the promoted form;
    // orphaned arith / local-read nodes become unreachable from the skeleton
    // root and are simply not emitted. (Early arith promotion already ran before
    // the loop; `FieldAccess` promotion ran above, after SROA.)
    run_pass("nir/freeze_pure_arith", &mut project, profiler, |p| {
        extract::freeze_pure_arith(p, /* include_fields */ false, /* early */ false)
    });

    // The born-resolved invariant is now enforced by the type system: a call
    // node's `func_id` is a non-optional `FuncId`, stamped at its synthesis site.

    project
}

fn run_dce(project: &mut NirPackage, profiler: &dyn SpanEmitter) {
    profiler.span_start("nir/dce");
    // Compute every reachability set up front, then apply mutations
    // in order. `remove_unreachable_globals` rewrites function bodies
    // (it drops `GlobalVarSet` for dead globals), which can orphan
    // calls inside dropped initializers; the *final* `run_dce` after
    // the optimization loop cleans those up — the savings of an extra
    // mid-loop call-graph rebuild aren't worth its cost.
    let analysis = analyze_dce(project);
    remove_unreachable_functions(project, &analysis.functions);
    remove_unreachable_globals(project, &analysis.globals);
    filter_string_literals(project);
    remove_unreachable_types(project, &analysis);
    filter_bytes_literals(project);
    remove_unreachable_closure_functors(project);
    project.rebuild_variant_indices();
    profiler.span_end("nir/dce");
}

/// Run a single optimization pass with profiling, returning whether it changed anything.
///
/// Honours the `WADO_LIST_PASSES`, `WADO_DUMP_PASS_BEFORE`, and
/// `WADO_DUMP_PASS_AFTER` developer-debug env vars (see `pass_dump`).
fn run_pass(
    name: &str,
    project: &mut NirPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut NirPackage) -> bool,
) -> bool {
    pass_dump::list_pass(name);
    if pass_dump::should_skip_pass(name) {
        return false;
    }
    pass_dump::dump_nir(name, project, pass_dump::Phase::Before);
    profiler.span_start(name);
    let changed = f(project);
    profiler.span_end(name);
    pass_dump::dump_nir(name, project, pass_dump::Phase::After);
    changed
}

pub mod pass_dump {
    use std::sync::{Mutex, OnceLock};

    use super::NirPackage;
    use crate::hashmap::IndexMap;
    use crate::wir::WirPackage;

    #[derive(Copy, Clone)]
    pub enum Phase {
        Before,
        After,
    }

    impl Phase {
        fn env_var(self) -> &'static str {
            match self {
                Self::Before => "WADO_DUMP_PASS_BEFORE",
                Self::After => "WADO_DUMP_PASS_AFTER",
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Before => "before",
                Self::After => "after",
            }
        }
    }

    fn dump_before_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var(Phase::Before.env_var()).ok().as_deref())
        })
    }

    fn dump_after_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var(Phase::After.env_var()).ok().as_deref())
        })
    }

    fn list_passes_enabled() -> bool {
        static FLAG: OnceLock<bool> = OnceLock::new();
        *FLAG.get_or_init(|| std::env::var("WADO_LIST_PASSES").is_ok())
    }

    fn skip_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var("WADO_SKIP_PASS").ok().as_deref())
        })
    }

    /// Returns true if `name` matches one of the comma-separated entries in
    /// the `WADO_SKIP_PASS` env var. Each entry is matched against the bare
    /// pass name (e.g., `nir/ref_elim`) and against `<pass>@<n>` where `<n>`
    /// is the 1-based occurrence number — letting bisection target a
    /// specific iteration (e.g., `nir/ref_elim@2`).
    pub fn should_skip_pass(name: &str) -> bool {
        static COUNTS: OnceLock<Mutex<IndexMap<String, u32>>> = OnceLock::new();
        let list = skip_list();
        if list.is_empty() {
            return false;
        }
        let mut counts = COUNTS
            .get_or_init(|| Mutex::new(IndexMap::default()))
            .lock()
            .unwrap();
        let n = counts.entry(name.to_string()).or_insert(0);
        *n += 1;
        let scoped = format!("{name}@{n}");
        list.iter().any(|s| s == name || s == &scoped)
    }

    fn matches(name: &str, phase: Phase) -> bool {
        let list = match phase {
            Phase::Before => dump_before_list(),
            Phase::After => dump_after_list(),
        };
        list.iter().any(|n| n == name)
    }

    pub fn list_pass(name: &str) {
        if list_passes_enabled() {
            eprintln!("[pass] {name}");
        }
    }

    pub fn dump_nir(name: &str, project: &NirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== NIR {label} {name} ===");
            eprintln!("{}", crate::nir_unparse::unparse_nir_package(project));
            eprintln!("=== end NIR {label} {name} ===");
        }
    }

    pub fn dump_wir(name: &str, module: &WirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== WIR {label} {name} ===");
            eprintln!("{}", crate::wir_unparse::unparse_wir(module));
            eprintln!("=== end WIR {label} {name} ===");
        }
    }
}

/// Run optimization passes with a fixed-point iteration strategy.
///
/// Container SROA runs early because it needs to see `List<T>` *method
/// calls* (push, `index_value`, `index_assign`, len, ...) before `inline`
/// expands them into raw field-accesses and `builtin::array_get` /
/// `array_set` pairs. Running it before `inline` in each iteration — rather
/// than only in iteration 0 — also lets the optimization loop re-run
/// container SROA on newly-inlined code that exposes fresh
/// `List<Tuple<...>>` locals.
///
/// The exact in-loop pass list and its ordering rationale lives on the
/// module doc above; the `step!` calls below are the canonical source for
/// pass names and order.
///
/// The `config` parameter controls the number of iterations and inline
/// threshold. More iterations can find more optimization opportunities but
/// take longer.
///
/// Hot Field Scalarization (HFS) runs once after the loop converges; see
/// `optimize` for the rationale.
fn run_optimization_passes(
    project: &mut NirPackage,
    config: &OptConfig,
    profiler: &dyn SpanEmitter,
) {
    let threshold = config.inline_threshold;
    let trace_loop = crate::trace::filter().enabled("opt_loop");
    // Per-function dirty-set gate (WEP Phase 6). Every loop pass is gate-aware:
    // a per-function pass (`gated!`) skips functions unchanged since it last ran;
    // an interprocedural pass scans all functions but reports exactly the ones
    // it touched. Both go through `&mut gate`.
    let mut gate = gate::FunctionGate::new(project);
    // Dense `Match` → `Switch` in global initializer bodies. Functions are
    // lowered by `MatchToSwitchRule` inside the unified peephole session; the
    // function-level loop never mutates global initializer bodies, so a single
    // pass over globals here is equivalent to running it each iteration.
    run_pass("nir/match_to_switch_globals", project, profiler, |p| {
        match_to_switch_globals(p)
    });
    // Operand-promotion keystone (WEP: The Live ValueGraph). Pure values are
    // frozen into `Operand::Value` before the value passes, so the passes read
    // operands (`engine.operand_value`) instead of rebuilding the value graph.
    // Arith only here (before the loop); `FieldAccess` promotion runs late (after
    // the SROA passes), since SROA scalarizes the structs a promoted `FieldAccess`
    // would reference. See the late call in `optimize`.
    run_pass("nir/promote_pure_values_early", project, profiler, |p| {
        extract::freeze_pure_arith(p, /* include_fields */ false, /* early */ true)
    });
    for i in 0..config.iterations {
        profiler.span_start(&format!("nir/iteration {}", i + 1));
        let mut changed = false;
        let mut iter_changed: Vec<&'static str> = Vec::new();
        // A gate-aware pass: receives `&mut gate`, skips functions it has
        // already processed at their current revision, and reports its own
        // per-function changes.
        macro_rules! gated {
            ($name:expr, $pass:expr) => {{
                let c = run_pass($name, project, profiler, |p| $pass(p, &mut gate));
                if c {
                    changed = true;
                    if trace_loop {
                        iter_changed.push($name);
                    }
                }
            }};
        }
        // Reports changes to the gate but not the convergence flag — must never
        // keep the loop alive on its own.
        macro_rules! gate_only {
            ($name:expr, $pass:expr) => {{
                run_pass($name, project, profiler, |p| $pass(p, &mut gate));
            }};
        }
        // Dense-int / dense-enum `Match` → `Switch` is a codegen-friendly late
        // lowering (see WEP 2026-05-11). It runs as `MatchToSwitchRule` inside
        // the unified peephole session below: the pre-inline `peephole` run
        // converts every function's `Match` to `Switch` before `inline`, so
        // inline still sees `Switch`-shaped bodies, and the worklist reconverges
        // on any `Match` a later rewrite plants. `container_sroa` /
        // `value_copy_*` run before that first `peephole` and so see `Match`,
        // which is fine — neither keys on the `Switch` shape.
        // Container SROA must run *before* inline in each iteration: inline
        // expands trait methods like `IndexValue::index_value` into raw
        // `builtin::array_get` + field-access pairs, after which the
        // method-call shape container SROA relies on is gone. Running early
        // also means we see the `SequenceLiteralBuilder` desugaring for `[]`
        // while its inner `Constructor` call is still a plain `Call` node,
        // which `recognize_init` can match structurally.
        gated!("nir/container_sroa", scalarize_containers);
        // Value-copy elision (`ValueCopyElideRule`) runs inside the pre-inline
        // `peephole` session below, before `inline` expands every reachable
        // `$value_copy$T<id>` body (after which the `Call($value_copy$T, [arg])`
        // shape it matches is gone). A wrapper the inliner later plants is
        // caught by the next iteration's pre-inline run.
        // Unified peephole engine pass, pre-inline run. Folds short
        // `push_str` literals, elides write-only locals, and (post-inline only)
        // materializes array literals — all three rules over one shared
        // worklist; see `optimize/peephole.rs`. Runs *before* inline so
        // `string_push` still sees the `buf.push_str("0.")` `MethodCall` shape:
        // once the inliner expands `String::push_str`'s body that node is gone
        // and the literal-recognising rewrite can no longer match, leaving
        // short-string formatting paths (e.g. `fpfmt.wado`) paying full
        // per-call allocation cost. `ValueCopyElideRule` runs in this same
        // session, so `string_push`'s duplicable-receiver check sees the
        // value-copy-stripped receiver via the shared worklist. This run
        // also hosts `const_branch_prune` (trivial-block / dead-statement
        // cleanup); it keys only on block structure, so `copy_prop` — not pass
        // ordering — is what folds the inliner's parameter copies. This
        // pre-inline run also hosts `MatchToSwitchRule` (`include_match =
        // true`), lowering every `Match` to `Switch` before `inline`.
        gated!("nir/peephole", |p, g| peephole::run_peephole(p, g, true));
        // Demote deep `$value_copy$T` copies of `List<E>` to shallow spine
        // copies when the binding's elements are provably never mutated through
        // it. Runs *after* the pre-inline `peephole` (which hosts
        // `ValueCopyElideRule`) so elide gets first crack: elide removes a copy
        // whose target is read-only, then demote weakens the remaining
        // spine-mutated copies. Running demote before elide would rewrite a
        // fully-elidable `$value_copy$T(arg)` into a shallow `array_clone` shape
        // that elide's `is_value_copy_call` no longer recognises, leaving the
        // copy un-elided. Still before `nir/inline`, where the
        // `$value_copy$T(arg)` shape both passes match disappears.
        gate_only!("nir/value_copy_demote", demote_value_copies);
        // Single-field parameter SROA: rewrite functions whose parameter type
        // is `&S` for a single-field struct (`Box<T>` being the canonical
        // case) to take the inner scalar directly. Runs before `nir/inline`
        // so the inliner sees post-SROA signatures and can propagate the
        // scalar through call chains. NIR analog of WIR's `sroa_param`; see
        // `optimize/sroa_param.rs`.
        gated!("nir/sroa_param", sroa_single_field_parameters);
        // `inline` self-reports the callers it modified to the gate (no
        // `bump_all`); it only mutates caller bodies, so the gated passes need
        // re-examine just those (and their neighbours).
        {
            let c = run_pass("nir/inline", project, profiler, |p| {
                inline_functions(p, threshold, &mut gate)
            });
            if c {
                changed = true;
                if trace_loop {
                    iter_changed.push("nir/inline");
                }
            }
        }
        // Unified peephole engine pass, post-inline run. Now `array_literal`
        // fires: it materializes `ArrayLiteral` from the `List<T> {
        // array_new(N) } + N × List::push` builder window, which inline must
        // expose first — the `SequenceLiteralBuilder` methods (and, for wrapper
        // builders such as `SeqVec { items: List<T> }`, the `push_literal →
        // self.field.push` delegation) are inlined into the raw `array_new +
        // push` window, direct or field-rooted. Later `cse` / `const_fold` in
        // this same loop then see the normalized literal. `elide_local` runs
        // again here over inline's freshly dead bindings. No `MatchToSwitchRule`
        // here (`include_match = false`): the pre-inline run already lowered
        // every reachable `Match`, and `inline` copies `Switch`-shaped bodies.
        gated!("nir/peephole", |p, g| peephole::run_peephole(p, g, false));
        // `labeled_block_fusion` moved into the post-inline `nir/peephole`
        // session as `LabeledBlockFusionRule`; see `optimize/peephole.rs`.
        gated!("nir/sroa", scalar_replace_aggregates);
        gated!("nir/copy_prop", propagate_copies);
        // DAE / DRVE after `copy_prop` shrinks signatures and discards unused
        // let-bindings before `cse` / `const_fold` revisit the simplified body.
        // Running here (rather than at WIR level) lets `inline` see the slimmer
        // signatures on the next iteration and lets `dce` clean up the freshly
        // dead computation in the same fixed-point loop. (Write-only local
        // elimination moved into the unified `nir/peephole` pass above.)
        gated!("nir/dae", eliminate_dead_arguments);
        gated!("nir/drve", eliminate_dead_return_values);
        // Pure-value CSE is subsumed by hash-consing (identical values already
        // share a ValueId), and store-load forwarding by FieldAccess promoting at
        // its heap version, so no separate `cse` pass runs (WEP: The Live ValueGraph).
        // The flow-sensitive half of constant folding; the env-free half
        // (literal arithmetic + pure CTFE) runs in the `nir/peephole` passes.
        // This walker handles the folds that need per-function dataflow state —
        // env-bound locals, forwarded struct fields, immutable-global reads, and
        // constant-branch collapse.
        //
        // `field_forward`'s rewrite responsibilities are absorbed by
        // `const_fold` (see `optimize::const_folding::ConstFoldVisitor`).
        // Both passes used to alternate one statement at a time on
        // chained-`List::push` patterns produced by Gale-generated
        // parsers, leaving the optimizer non-convergent at `-O3`
        // (issue #1009). The merged const-fold walk feeds the
        // interpreter's `field_env` from `Let` / `Assign` /
        // `$value_copy$T(arg)` shapes and forks per branch, so a
        // chain of pushes folds in a single iteration. The alias and
        // value-copy-helper analyses migrated to
        // `optimize::alias`.
        gated!("nir/const_fold", fold_constants);
        // Trivial-block / dead-statement pruning moved into the pre-inline
        // `nir/peephole` run above; the post-loop `branch_prune_final` and the
        // post-globalization `const_fold_post_global` keep their own engine
        // sessions (`prune_template_block_wrappers` / `prune_constant_branches`).
        gated!("nir/licm", apply_licm);
        gated!("nir/tmpl_hoist", hoist_template_buffers);
        profiler.span_end(&format!("nir/iteration {}", i + 1));
        if trace_loop {
            crate::compiler_trace!(
                "opt_loop",
                "iter {:>3}: changed_by = [{}]",
                i + 1,
                iter_changed.join(", ")
            );
        }
        if !changed {
            profiler.debug(&format!(
                "NIR optimizer converged after {} iteration(s)",
                i + 1
            ));
            break;
        }
    }
    // Hot Field Scalarization runs once after the main loop converges.
    // Running inside the loop would cause the write-back/re-read stmts it
    // inserts to be counted as new field accesses on the next iteration,
    // triggering spurious re-scalarization of the same fields.
    run_pass(
        "nir/field_scalarize",
        project,
        profiler,
        scalarize_hot_fields,
    );
    // Forward the scalarization shadow inits (`__hfs_x = obj.f`) to constants.
    // `field_scalarize` runs after the fixed-point loop, so no in-loop
    // `store_load_forward` sees its shadow reads; this once-over folds an
    // `obj.f` whose field the ValueGraph still knows (`obj` built from a
    // constant literal upstream) into the literal, before globalization. Runs
    // before `const_object_globalization` so it never sees the nullable
    // `GlobalVarGet`s globalization emits.
    run_pass(
        "nir/store_load_forward_post_scalarize",
        project,
        profiler,
        forward_stores_to_loads_all,
    );
    // Final cleanup: flatten any `__tmpl:` labeled blocks the fixpoint
    // preserved as anchors for `tmpl_hoist`. `tmpl_hoist` has finished
    // by now (it runs inside the fixpoint loop), so the wrappers are
    // pure overhead — peel them so codegen emits the inner straight-line
    // body directly. Iterate until convergence because one flatten can
    // expose another (e.g. single-stmt Block collapse on a freshly
    // produced `Block { Expr(tail) }`).
    run_pass("nir/branch_prune_final", project, profiler, |p| {
        let mut any_changed = false;
        while prune_template_block_wrappers(p) {
            any_changed = true;
        }
        any_changed
    });
    // Body globalization: hoist constant, read-only aggregate `let` bindings
    // into shared immutable module globals so they build once at instantiation
    // (WEP-2026-05-31). Runs once after the fixpoint converges, on the stable
    // post-optimization shape, so the read-only gate and the const-aggregate
    // recognizer see fully-inlined / array-literal-materialized bindings. The
    // inline `GlobalVarSet`s it emits are promoted to eager Wasm constants by
    // `wir_optimize::const_global`; the final `run_dce` reclaims the dead
    // binding locals.
    run_pass("nir/const_object_globalization", project, profiler, |p| {
        globalize_const_objects(p)
    });
    // Clean up after globalization: fold the `global:X.used` length reads it
    // exposes (recovered via `const_folding`'s `GlobalFieldEnv`) and prune the
    // now-constant bounds-check branches, so a hoisted constant-index array
    // keeps the bounds-check elimination it had as a local. Only `const_fold` /
    // `branch_prune` run here — re-entering the full loop is unsafe, since the
    // nullable `GlobalVarGet`s globalization emits are not meant to flow back
    // through `value_copy` / `sroa` (which is why globalization runs last).
    run_pass("nir/const_fold_post_global", project, profiler, |p| {
        let mut changed = false;
        while fold_constants_all(p) | prune_constant_branches(p) {
            changed = true;
        }
        changed
    });
    // Forward the inliner's leftover single-use pure-scalar value-parameter
    // temps into their uses. Runs last, after every scalarization / globalization
    // recognizer has matched its shape, so it only strips dead-weight locals.
    run_pass("nir/scalar_forward", project, profiler, |p| {
        forward_scalar_temps(p, &mut gate)
    });
    // Forward read-only clones that inlining + const-object globalization leave
    // behind (`array_clone(&array_clone(&__const_obj))` into a read-only
    // binding). Runs last: `const_object_globalization` — which creates the
    // const global the residual clone reads — is itself a post-loop pass, so the
    // flat `let t = array_clone(&global.field)` shape only exists here, after the
    // pre-inline value-copy elider that cannot reach it has long run.
    run_pass("nir/clone_forward", project, profiler, |p| {
        clone_forward::forward_redundant_clones(p)
    });
}
