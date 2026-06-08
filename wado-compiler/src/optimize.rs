//! Optimization passes for Wado NIR.
//!
//! The optimizer rewrites the [`NirPackage`] in place. The full pass list and
//! ordering rationale lives in [`docs/optimizer.md`](../../../docs/optimizer.md);
//! the inventory below is just an index of the modules under `optimize/`.
//!
//! Fixed-point loop ([`run_optimization_passes`], in order):
//! 1.  `match_to_switch` — dense `Match` → `Switch` lowering.
//! 2.  `container_sroa` — `AoS` → `SoA` for `List<Tuple<...>>` / `List<Struct>`.
//! 3.  `value_copy_elide` — strip `$value_copy$T<id>` wrappers on read-only
//!     bindings.
//! 4.  `value_copy_demote` — demote deep `$value_copy$T` to a shallow spine
//!     copy when elements are provably immutable through the binding.
//! 5.  `peephole` (pre-inline) — unified engine pass: `string_push`
//!     (`buf.push_str("short")` → per-byte `push`), `elide_local` (write-only
//!     local elimination), env-free `const_fold` (literal arithmetic + pure
//!     CTFE), and `const_branch_prune` (trivial-block / dead-statement cleanup;
//!     pre-inline only — see its module docs). See `optimize/peephole.rs`.
//! 6.  `inline` — function inlining.
//! 7.  `peephole` (post-inline) — unified engine pass: `array_literal`
//!     (materialize `ArrayLiteral` from the `array_new + push` window),
//!     `elide_local`, and env-free `const_fold`.
//! 8.  `labeled_block_fusion` — collapse inlined-helper `Option<T>` allocations.
//! 9.  `ref_elim` — drop unnecessary reference bindings exposed by inlining.
//! 10. `sroa` — Scalar Replacement of Aggregates.
//! 11. `copy_prop` — copy propagation.
//! 12. `dae` — Dead Argument Elimination.
//! 13. `drve` — Dead Return Value Elimination.
//! 14. `cse` — Loop-level Common Subexpression Elimination.
//! 15. `store_load_forward` — store-to-load forwarding.
//! 16. `const_folding` — partial evaluation via [`crate::niri`] (also drives
//!     alias-aware field-knowledge tracking; see `alias`). The flow-sensitive
//!     half; the env-free folds and trivial-block pruning run in `peephole`
//!     (`const_branch_prune` in the pre-inline run).
//! 19. `licm` — Loop-Invariant Code Motion.
//! 20. `condition_implication` — eliminate conditions implied by dominators.
//! 21. `tmpl_hoist` — hoist template-string backing buffers out of loops.
//!
//! Once after the loop converges: `field_scalarize` (Hot Field Scalarization).
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
mod condition_implication;
mod const_branch_prune;
mod const_folding;
mod const_object_globalization;
mod container_sroa;
mod copy_prop;
mod cse;
mod dae;
pub mod dce;
mod drve;
mod elide_box_local;
mod elide_local;
mod field_scalarize;
mod gate;
mod inline;
mod labeled_block_fusion;
mod licm;
mod match_to_switch;
mod mod_ref;
mod multi_value_return;
mod peephole;
mod ref_elim;
mod select_lowering;
mod sroa;
mod sroa_param;
mod store_load_forward;
mod string_push;
mod tmpl_hoist;
mod value_copy_demote;
mod value_copy_elide;

use condition_implication::eliminate_implied_conditions;
use const_branch_prune::{prune_constant_branches, prune_template_block_wrappers};
use const_folding::{fold_constants, fold_constants_all};
use const_object_globalization::globalize_const_objects;
use container_sroa::scalarize_containers;
use copy_prop::propagate_copies;
use cse::eliminate_common_subexprs;
use dae::eliminate_dead_arguments;
use dce::{
    analyze_dce, filter_bytes_literals, filter_string_literals,
    remove_unreachable_closure_functors, remove_unreachable_functions, remove_unreachable_globals,
    remove_unreachable_types,
};
use drve::eliminate_dead_return_values;
use elide_box_local::elide_adjacent_box_locals;
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use labeled_block_fusion::fuse_labeled_blocks;
use licm::apply_licm;
use match_to_switch::match_to_switch;
use ref_elim::eliminate_unnecessary_refs;
use sroa::scalar_replace_aggregates;
use sroa_param::sroa_single_field_parameters;
use store_load_forward::forward_stores_to_loads;
use tmpl_hoist::hoist_template_buffers;
use value_copy_demote::demote_value_copies;
use value_copy_elide::elide_synthesized_value_copies;

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
    // by `wir_build` (`translate_string_literal` / `register_string_data`) to
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
                match_to_switch(p)
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

    // Post-optimization rewrites: select lowering for branchless Wasm
    profiler.span_start("nir/select_lowering");
    select_lowering::select_lowering(&mut project);
    profiler.span_end("nir/select_lowering");

    // Multi-value return ABI classification: marks tuple- or
    // user-struct-returning functions whose every return site is a fresh
    // `TupleLiteral` / `StructLiteral` and whose every call site
    // destructures via `FieldAccess` on the bound temp. WIR build
    // (`wir_build::translate::try_emit_multi_value_let`) reads the marker
    // to emit the multi-value Wasm signature on the function definition
    // and to rewrite call-site `let __tmp = Call(f)` into
    // `MultiValueLocalBind [__tmp_0, …] = Call(f)` with subsequent
    // `FieldAccess` reads going to the split locals directly. Runs after
    // every other transformation so the analysis sees the final TIR
    // shape.
    profiler.span_start("nir/multi_value_return");
    multi_value_return::classify_multi_value_returns(&mut project);
    profiler.span_end("nir/multi_value_return");

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
    pass_dump::dump_tir(name, project, pass_dump::Phase::Before);
    profiler.span_start(name);
    let changed = f(project);
    profiler.span_end(name);
    pass_dump::dump_tir(name, project, pass_dump::Phase::After);
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

    pub fn dump_tir(name: &str, project: &NirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== TIR {label} {name} ===");
            eprintln!("{}", crate::nir_unparse::unparse_nir_package(project));
            eprintln!("=== end TIR {label} {name} ===");
        }
    }

    pub fn dump_wir(name: &str, module: &WirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== WIR {label} {name} ===");
            eprintln!("{}", crate::wir_unparse::unparse_wir(module, None));
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
    // Per-function dirty-set gate (WEP Phase 6). Gate-aware passes (`gated!`)
    // skip functions unchanged since they last ran; passes that are not yet
    // gate-aware (`step!`) report change at package granularity, so on any
    // change they `bump_all` to keep the gated passes conservatively correct.
    let mut gate = gate::FunctionGate::new(project);
    for i in 0..config.iterations {
        profiler.span_start(&format!("nir/iteration {}", i + 1));
        let mut changed = false;
        let mut iter_changed: Vec<&'static str> = Vec::new();
        // A pass that is not gate-aware: runs over all functions and, on change,
        // dirties every function for the gated passes.
        macro_rules! step {
            ($name:expr, $body:expr) => {{
                let c = run_pass($name, project, profiler, $body);
                if c {
                    changed = true;
                    gate.bump_all();
                    if trace_loop {
                        iter_changed.push($name);
                    }
                }
            }};
        }
        // A gate-aware pass: receives `&mut gate`, skips functions it has
        // already processed at their current revision, and reports per-function
        // change itself (no `bump_all`).
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
        // Dense-int / dense-enum `Match` → `Switch` is a codegen-
        // friendly late lowering the optimizer materialises (see WEP
        // 2026-05-11). Running it first lets every subsequent pass in
        // the iteration see the `Switch` shape its variant-walking arms
        // already handle. Subsequent iterations only see fresh `Match`
        // shapes if `inline` (or a future shape-rewriting pass) plants
        // them, in which case this pass reconverges on iteration N+1.
        step!("nir/match_to_switch", match_to_switch);
        // Container SROA must run *before* inline in each iteration: inline
        // expands trait methods like `IndexValue::index_value` into raw
        // `builtin::array_get` + field-access pairs, after which the
        // method-call shape container SROA relies on is gone. Running early
        // also means we see the `SequenceLiteralBuilder` desugaring for `[]`
        // while its inner `Constructor` call is still a plain `Call` node,
        // which `recognize_init` can match structurally.
        step!("nir/container_sroa", scalarize_containers);
        // Run value-copy elision *before* inlining: the inliner expands
        // every reachable `$value_copy$T<id>` body into a labeled
        // block, after which the `Call($value_copy$T, [arg])` shape the
        // elider matches on no longer exists. Running before inline
        // lets the elider strip wrappers around `match Parser::expect(p,
        // K) { Ok(v) => v, Err(e) => return Err(e) }`-style `?`
        // desugarings, where the match's `Ok` arm produces a value
        // that is observably read-only and the surplus copy can fold
        // away.
        //
        // No post-pass run is needed: the only way a fresh
        // `$value_copy$T(arg)` `Call` shape can appear after lowering
        // is for the inliner to expand a function whose body still
        // contains a wrapper. The next iteration's pre-inline run
        // catches those, and if the loop converges (no pass returned
        // `changed`) the inliner did nothing this round, so no new
        // wrappers were introduced.
        // These two report change only to the gate (so the gated passes
        // re-examine the bodies they rewrote), not to the convergence `changed`
        // flag — keeping the original convergence behaviour where they never
        // kept the loop alive on their own.
        if run_pass("nir/value_copy_elide", project, profiler, |p| {
            elide_synthesized_value_copies(p)
        }) {
            gate.bump_all();
        }
        // Demote deep `$value_copy$T` copies of `List<E>` to shallow spine
        // copies when the binding's elements are provably never mutated
        // through it. Runs alongside `value_copy_elide`: elide removes a
        // copy whose target is read-only; demote weakens a copy whose target
        // is only spine-mutated. Both before `nir/inline` for the same
        // `$value_copy$T(arg)`-shape-visibility reason.
        if run_pass("nir/value_copy_demote", project, profiler, |p| {
            demote_value_copies(p)
        }) {
            gate.bump_all();
        }
        // Unified peephole engine pass, pre-inline run. Folds short
        // `push_str` literals, elides write-only locals, and (post-inline only)
        // materializes array literals — all three rules over one shared
        // worklist; see `optimize/peephole.rs`. Runs *before* inline so
        // `string_push` still sees the `buf.push_str("0.")` `MethodCall` shape:
        // once the inliner expands `String::push_str`'s body that node is gone
        // and the literal-recognising rewrite can no longer match, leaving
        // short-string formatting paths (e.g. `fpfmt.wado`) paying full
        // per-call allocation cost. Also after value-copy elision/demotion so
        // the duplicable-receiver check sees the stripped receiver. This run
        // also hosts `const_branch_prune` (trivial-block / dead-statement
        // cleanup); it keys only on block structure, so `copy_prop` — not pass
        // ordering — is what folds the inliner's parameter copies.
        gated!("nir/peephole", peephole::run_peephole);
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
        // again here over inline's freshly dead bindings.
        gated!("nir/peephole", peephole::run_peephole);
        // Adjacent-use Box-local elision. After `sroa_param` reshapes
        // `Box<T>` parameters into scalars and `inline` propagates the
        // resulting `FieldAccess(Local(x), "value")` shape into call
        // sites, this pass collapses the surrounding `let x = Box{value:
        // inner}; … x.value …` shells. See `optimize/elide_box_local.rs`.
        gated!("nir/elide_box_local", elide_adjacent_box_locals);
        step!("nir/labeled_block_fusion", fuse_labeled_blocks);
        gated!("nir/ref_elim", eliminate_unnecessary_refs);
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
        gated!("nir/cse", eliminate_common_subexprs);
        gated!("nir/store_load_forward", forward_stores_to_loads);
        // The flow-sensitive half of constant folding. The env-free half
        // (literal arithmetic + pure CTFE) already ran on the worklist in the
        // `nir/peephole` passes above; this walker handles the folds that need
        // per-function dataflow state — env-bound locals, forwarded struct
        // fields, immutable-global reads, and constant-branch collapse.
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
        gated!("nir/condition_implication", eliminate_implied_conditions);
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
    run_pass("nir/field_scalarize", project, profiler, |p| {
        scalarize_hot_fields(p);
        true // always runs once, mark as changed for profiling visibility
    });
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
}
