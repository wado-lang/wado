//! Optimization pass for Wado TIR
//!
//! This module coordinates the following optimization passes:
//! - Dead Code Elimination (DCE) via `dce` module
//! - Function inlining via `inline` module
//! - Labeled block fusion via `labeled_block_fusion` module
//! - Match → Switch rewrite for dense int/enum scrutinees via `match_to_switch` module
//! - Reference elimination via `ref_elim` module
//! - Container SROA (AoS→SoA for `Array<Tuple<...>>`) via `container_sroa` module
//! - Scalar Replacement of Aggregates (SROA) via `sroa` module
//! - Copy propagation via `copy_prop` module
//! - Common Subexpression Elimination (CSE) via `cse` module
//! - Store-to-load forwarding via `store_load_forward` module
//! - Constant optimizations (propagation, folding, global promotion, branch pruning) via `const_*` modules
//! - Loop-Invariant Code Motion (LICM) via `licm` module
//! - Condition implication elimination via `condition_implication` module
//! - Template buffer hoisting via `tmpl_hoist` module
//! - Hot Field Scalarization (HFS) via `field_scalarize` module
//! - Select lowering via `select_lowering` module
//! - Value-copy elision via `value_copy_elide` module
//! - Short `push_str` simplification via `string_push` module
//!
//! The `$value_copy$T` insertion + synthesis steps that materialize Wado's
//! value-copy semantics live in the lower phase (`lower::plan::value_copy`) — by
//! the time TIR reaches the optimizer, every defensive deep-copy is
//! explicit. The optimizer only *removes* redundant copies via
//! `value_copy_elide`, which runs as a regular pass in the fixed-point loop.

mod alias;
mod condition_implication;
mod const_branch_prune;
mod const_folding;
mod const_global_promotion;
mod container_sroa;
mod copy_prop;
mod cse;
mod dae;
pub mod dce;
mod drve;
mod elide_local;
mod field_scalarize;
mod inline;
mod labeled_block_fusion;
mod licm;
mod match_to_switch;
mod multi_value_return;
mod ref_elim;
mod select_lowering;
mod sroa;
mod store_load_forward;
mod string_push;
mod tmpl_hoist;
mod value_copy_demote;
mod value_copy_elide;

use condition_implication::eliminate_implied_conditions;
use const_branch_prune::prune_constant_branches;
use const_folding::fold_constants;
use const_global_promotion::promote_constant_globals;
use container_sroa::scalarize_containers;
use copy_prop::propagate_copies;
use cse::eliminate_common_subexprs;
use dae::eliminate_dead_arguments;
use dce::{
    analyze_project, filter_bytes_literals, filter_string_literals,
    remove_unreachable_closure_functors, remove_unreachable_functions, remove_unreachable_globals,
    remove_unreachable_types,
};
use drve::eliminate_dead_return_values;
use elide_local::elide_write_only_locals;
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use labeled_block_fusion::fuse_labeled_blocks;
use licm::apply_licm;
use match_to_switch::match_to_switch;
use ref_elim::eliminate_unnecessary_refs;
use sroa::scalar_replace_aggregates;
use store_load_forward::forward_stores_to_loads;
use string_push::simplify_short_push_str;
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
pub fn optimize(
    mut project: NirPackage,
    opt_level: OptLevel,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    profiler: &dyn SpanEmitter,
) -> NirPackage {
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
            run_pass("tir/match_to_switch", &mut project, profiler, |p| {
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
            // inlined `Array::push` and similar patterns fold in a
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
    profiler.span_start("tir/select_lowering");
    select_lowering::select_lowering(&mut project);
    profiler.span_end("tir/select_lowering");

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
    profiler.span_start("tir/multi_value_return");
    multi_value_return::classify_multi_value_returns(&mut project);
    profiler.span_end("tir/multi_value_return");

    project
}

fn run_dce(project: &mut NirPackage, profiler: &dyn SpanEmitter) {
    profiler.span_start("tir/dce");
    // Single pass per `run_dce` invocation. `remove_unreachable_globals`
    // can rewrite function bodies (it drops `GlobalVarSet` for dead
    // globals), which may orphan calls inside the dropped initializers,
    // but the *final* `run_dce` after the optimizer cleans those up — the
    // savings here aren't worth the cost of an extra full call-graph
    // rebuild (the dominant cost of this pass).
    let reachable_positions = analyze_project(project);
    remove_unreachable_functions(project, &reachable_positions);
    remove_unreachable_globals(project);
    filter_string_literals(project);
    remove_unreachable_types(project);
    filter_bytes_literals(project);
    remove_unreachable_closure_functors(project);
    project.rebuild_variant_indices();
    profiler.span_end("tir/dce");
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
    use std::sync::OnceLock;

    use super::NirPackage;
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
    /// pass name (e.g., `tir/ref_elim`) and against `<pass>@<n>` where `<n>`
    /// is the 1-based occurrence number — letting bisection target a
    /// specific iteration (e.g., `tir/ref_elim@2`).
    pub fn should_skip_pass(name: &str) -> bool {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static COUNTS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
        let list = skip_list();
        if list.is_empty() {
            return false;
        }
        let mut counts = COUNTS
            .get_or_init(|| Mutex::new(HashMap::new()))
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
/// Each iteration runs the following passes in order. Container SROA runs
/// first because it needs to see `Array<T>` *method calls* (push, `index_value`,
/// `index_assign`, len, ...) before inline expands them into raw field-accesses
/// and `builtin::array_get`/`array_set` pairs. Running it before `inline` in
/// each iteration — rather than only in iteration 0 — also lets the
/// optimization loop re-run container SROA on newly-inlined code that
/// exposes fresh `Array<Tuple<...>>` locals.
///
///  1. Match → Switch (`match_to_switch`)
///  2. Container SROA (`container_sroa`)
///  3. Value-copy elision (`value_copy_elide`)
///  4. Short `push_str` simplification (`string_push`)
///  5. Function inlining (`inline`)
///  6. Labeled block fusion (`labeled_block_fusion`)
///  7. Reference elimination (`ref_elim`)
///  8. Scalar Replacement of Aggregates (`sroa`)
///  9. Copy propagation (`copy_prop`)
/// 10. Dead Argument Elimination (`dae`)
/// 11. Dead Return Value Elimination (`drve`)
/// 12. Write-only local elimination (`elide_local`)
/// 13. Common Subexpression Elimination (`cse`)
/// 14. Store-to-load forwarding (`store_load_forward`)
/// 15. Constant folding (`const_fold`)
/// 16. Constant global promotion (`const_global_promotion`)
/// 17. Constant branch pruning (`branch_prune`)
/// 18. Loop-invariant code motion (`licm`)
/// 19. Condition implication elimination (`condition_implication`)
/// 20. Template buffer hoisting (`tmpl_hoist`)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
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
    for i in 0..config.iterations {
        profiler.span_start(&format!("tir/iteration {}", i + 1));
        let mut changed = false;
        let mut iter_changed: Vec<&'static str> = Vec::new();
        macro_rules! step {
            ($name:expr, $body:expr) => {{
                let c = run_pass($name, project, profiler, $body);
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
        step!("tir/match_to_switch", match_to_switch);
        // Container SROA must run *before* inline in each iteration: inline
        // expands trait methods like `IndexValue::index_value` into raw
        // `builtin::array_get` + field-access pairs, after which the
        // method-call shape container SROA relies on is gone. Running early
        // also means we see the `SequenceLiteralBuilder` desugaring for `[]`
        // while its inner `Constructor` call is still a plain `Call` node,
        // which `recognize_init` can match structurally.
        step!("tir/container_sroa", scalarize_containers);
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
        run_pass("tir/value_copy_elide", project, profiler, |p| {
            elide_synthesized_value_copies(p);
            false
        });
        // Demote deep `$value_copy$T` copies of `Array<E>` to shallow spine
        // copies when the binding's elements are provably never mutated
        // through it. Runs alongside `value_copy_elide`: elide removes a
        // copy whose target is read-only; demote weakens a copy whose target
        // is only spine-mutated. Both before `tir/inline` for the same
        // `$value_copy$T(arg)`-shape-visibility reason.
        run_pass("tir/value_copy_demote", project, profiler, |p| {
            demote_value_copies(p);
            false
        });
        // Run short-`push_str` simplification *before* inline. Once the
        // inliner expands `String::push_str`'s body the `MethodCall` node is
        // gone and the literal-recognising rewrite can no longer match,
        // leaving short-string formatting paths (e.g. `fpfmt.wado`'s
        // `buf.push_str("0.")`) paying full per-call allocation cost.
        step!("tir/string_push", simplify_short_push_str);
        step!("tir/inline", |p| inline_functions(p, threshold));
        step!("tir/labeled_block_fusion", fuse_labeled_blocks);
        step!("tir/ref_elim", eliminate_unnecessary_refs);
        step!("tir/sroa", scalar_replace_aggregates);
        step!("tir/copy_prop", propagate_copies);
        // DAE / DRVE / write-only local elimination after `copy_prop` shrinks
        // signatures and discards unused let-bindings before `cse` /
        // `const_fold` revisit the simplified body. Running here (rather
        // than at WIR level) lets `inline` see the slimmer signatures on
        // the next iteration and lets `dce` clean up the freshly dead
        // computation in the same fixed-point loop.
        step!("tir/dae", eliminate_dead_arguments);
        step!("tir/drve", eliminate_dead_return_values);
        step!("tir/elide_local", elide_write_only_locals);
        step!("tir/cse", eliminate_common_subexprs);
        step!("tir/store_load_forward", forward_stores_to_loads);
        // `field_forward`'s rewrite responsibilities are absorbed by
        // `const_fold` (see `optimize::const_folding::ConstFoldVisitor`).
        // Both passes used to alternate one statement at a time on
        // chained-`Array::push` patterns produced by Gale-generated
        // parsers, leaving the optimizer non-convergent at `-O3`
        // (issue #1009). The merged const-fold walk feeds the
        // interpreter's `field_env` from `Let` / `Assign` /
        // `$value_copy$T(arg)` shapes and forks per branch, so a
        // chain of pushes folds in a single iteration. The alias and
        // value-copy-helper analyses migrated to
        // `optimize::alias`.
        step!("tir/const_fold", fold_constants);
        step!("tir/const_global_promotion", promote_constant_globals);
        step!("tir/branch_prune", prune_constant_branches);
        step!("tir/licm", apply_licm);
        step!("tir/condition_implication", eliminate_implied_conditions);
        step!("tir/tmpl_hoist", hoist_template_buffers);
        profiler.span_end(&format!("tir/iteration {}", i + 1));
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
                "TIR optimizer converged after {} iteration(s)",
                i + 1
            ));
            break;
        }
    }
    // Hot Field Scalarization runs once after the main loop converges.
    // Running inside the loop would cause the write-back/re-read stmts it
    // inserts to be counted as new field accesses on the next iteration,
    // triggering spurious re-scalarization of the same fields.
    run_pass("tir/field_scalarize", project, profiler, |p| {
        scalarize_hot_fields(p);
        true // always runs once, mark as changed for profiling visibility
    });
}
