//! Optimization pass for Wado TIR
//!
//! This module coordinates the following optimization passes:
//! - Dead Code Elimination (DCE) via `dce` module
//! - Function inlining via `inline` module
//! - Labeled block fusion via `labeled_block_fusion` module
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
//!
//! The `$value_copy$T` insertion + synthesis steps that materialize Wado's
//! value-copy semantics live in the lower phase (`lower::value_copy`) — by
//! the time TIR reaches the optimizer, every defensive deep-copy is
//! explicit. The optimizer only *removes* redundant copies via
//! `value_copy_elide`, which runs as a regular pass in the fixed-point loop.

mod condition_implication;
mod const_branch_prune;
mod const_folding;
mod const_global_promotion;
mod container_sroa;
mod copy_prop;
mod cse;
pub mod dce;
mod field_forward;
mod field_scalarize;
mod inline;
mod labeled_block_fusion;
mod licm;
mod ref_elim;
mod select_lowering;
mod sroa;
mod store_load_forward;
mod tmpl_hoist;
mod value_copy_elide;

use condition_implication::eliminate_implied_conditions;
use const_branch_prune::prune_constant_branches;
use const_folding::fold_constants;
use const_global_promotion::promote_constant_globals;
use container_sroa::scalarize_containers;
use copy_prop::propagate_copies;
use cse::eliminate_common_subexprs;
use dce::{
    analyze_project, filter_bytes_literals, remove_unreachable_closure_functors,
    remove_unreachable_functions, remove_unreachable_globals, remove_unreachable_types,
};
use field_forward::forward_struct_field_constants;
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use labeled_block_fusion::fuse_labeled_blocks;
use licm::apply_licm;
use ref_elim::eliminate_unnecessary_refs;
use sroa::scalar_replace_aggregates;
use store_load_forward::forward_stores_to_loads;
use tmpl_hoist::hoist_template_buffers;
use value_copy_elide::elide_synthesized_value_copies;

use crate::compiler_host::SpanEmitter;
use crate::flat_package::FlatPackage;

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
/// | O1    | Yes | 2          | 5                |
/// | O2    | Yes | 10         | 12               |
/// | O3    | Yes | 100        | 20               |
/// | Os    | Yes | 10         | 12               |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimization passes. DCE only.
    O0,
    /// Development optimizations. All passes with fast iteration count.
    /// Iterations: 2, Inline threshold: 5.
    O1,
    /// Production optimizations. All passes including DCE.
    /// Iterations: 10, Inline threshold: 12.
    #[default]
    O2,
    /// Aggressive production optimizations. All passes including DCE.
    /// Iterations: 100, Inline threshold: 20.
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
    mut project: FlatPackage,
    opt_level: OptLevel,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    profiler: &dyn SpanEmitter,
) -> FlatPackage {
    match opt_level {
        OptLevel::O0 => {
            // No optimizations, but still run DCE to reduce codegen work
            run_dce(&mut project, profiler);
        }
        OptLevel::O1 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(2),
                inline_threshold: inline_threshold.unwrap_or(5),
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
                // Threshold 12: allows index_assign (11 expressions) to be inlined
                inline_threshold: inline_threshold.unwrap_or(12),
            };
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            run_dce(&mut project, profiler);
            if opt_level == OptLevel::Os {
                project.strip_names = true;
            }
        }
        OptLevel::O3 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(100),
                inline_threshold: inline_threshold.unwrap_or(30),
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

    project
}

fn run_dce(project: &mut FlatPackage, profiler: &dyn SpanEmitter) {
    profiler.span_start("tir/dce");
    // Iterate to fixed point: `remove_unreachable_globals` rewrites
    // function bodies (drops `GlobalVarSet` for dead globals), which can
    // turn previously-reachable callees into dead code. A single pass
    // would leave them in the WIR; iterating until the function set
    // stops shrinking removes the transitively-dead helpers.
    loop {
        let reachable = analyze_project(project);
        let before = project.functions.len();
        remove_unreachable_functions(project, &reachable);
        remove_unreachable_globals(project);
        if project.functions.len() == before {
            break;
        }
    }
    remove_unreachable_types(project);
    filter_bytes_literals(project);
    remove_unreachable_closure_functors(project);
    project.rebuild_variant_indices();
    profiler.span_end("tir/dce");
}

/// Run a single optimization pass with profiling, returning whether it changed anything.
fn run_pass(
    name: &str,
    project: &mut FlatPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut FlatPackage) -> bool,
) -> bool {
    profiler.span_start(name);
    let changed = f(project);
    profiler.span_end(name);
    changed
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
/// 1. Container SROA (`container_sroa`)
/// 2. Function inlining (`inline`)
/// 3. Labeled block fusion (`labeled_block_fusion`)
/// 4. Reference elimination (`ref_elim`)
/// 5. Scalar Replacement of Aggregates (`sroa`)
/// 6. Copy propagation (`copy_prop`)
/// 7. Common Subexpression Elimination (`cse`)
/// 8. Store-to-load forwarding (`store_load_forward`)
/// 9. Constant propagation (`const_prop`)
/// 10. Constant folding (`const_fold`)
/// 11. Constant global promotion (`const_global_promotion`)
/// 12. Constant branch pruning (`branch_prune`)
/// 13. Loop-invariant code motion (`licm`)
/// 14. Condition implication elimination (`condition_implication`)
/// 15. Template buffer hoisting (`tmpl_hoist`)
/// 16. Value-copy elision (`value_copy_elide`)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
///
/// Hot Field Scalarization (HFS) runs once after the loop converges; see
/// `optimize` for the rationale.
fn run_optimization_passes(
    project: &mut FlatPackage,
    config: &OptConfig,
    profiler: &dyn SpanEmitter,
) {
    let threshold = config.inline_threshold;
    for i in 0..config.iterations {
        profiler.span_start(&format!("tir/iteration {}", i + 1));
        let mut changed = false;
        // Container SROA must run *before* inline in each iteration: inline
        // expands trait methods like `IndexValue::index_value` into raw
        // `builtin::array_get` + field-access pairs, after which the
        // method-call shape container SROA relies on is gone. Running early
        // also means we see the `SequenceLiteralBuilder` desugaring for `[]`
        // while its inner `Constructor` call is still a plain `Call` node,
        // which `recognize_init` can match structurally.
        changed |= run_pass("tir/container_sroa", project, profiler, |p| {
            scalarize_containers(p)
        });
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
        changed |= run_pass("tir/inline", project, profiler, |p| {
            inline_functions(p, threshold)
        });
        changed |= run_pass("tir/labeled_block_fusion", project, profiler, |p| {
            fuse_labeled_blocks(p)
        });
        changed |= run_pass("tir/ref_elim", project, profiler, |p| {
            eliminate_unnecessary_refs(p)
        });
        changed |= run_pass("tir/sroa", project, profiler, |p| {
            scalar_replace_aggregates(p)
        });
        changed |= run_pass("tir/copy_prop", project, profiler, propagate_copies);
        changed |= run_pass("tir/cse", project, profiler, eliminate_common_subexprs);
        changed |= run_pass("tir/store_load_forward", project, profiler, |p| {
            forward_stores_to_loads(p)
        });
        changed |= run_pass("tir/field_forward", project, profiler, |p| {
            forward_struct_field_constants(p)
        });
        changed |= run_pass("tir/const_fold", project, profiler, fold_constants);
        changed |= run_pass("tir/const_global_promotion", project, profiler, |p| {
            promote_constant_globals(p)
        });
        changed |= run_pass("tir/branch_prune", project, profiler, |p| {
            prune_constant_branches(p)
        });
        changed |= run_pass("tir/licm", project, profiler, apply_licm);
        changed |= run_pass("tir/condition_implication", project, profiler, |p| {
            eliminate_implied_conditions(p)
        });
        changed |= run_pass("tir/tmpl_hoist", project, profiler, |p| {
            hoist_template_buffers(p)
        });
        profiler.span_end(&format!("tir/iteration {}", i + 1));
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
