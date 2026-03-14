//! Optimization pass for Wado TIR
//!
//! This module coordinates various optimization passes:
//! - Dead Code Elimination (DCE) via `dce` module
//! - Function inlining via `inline` module
//! - Reference elimination via `ref_elim` module
//! - Scalar Replacement of Aggregates (SROA) via `sroa` module
//! - Copy propagation via `copy_prop` module
//! - Constant propagation via `const_prop` module
//! - Constant folding via `const_fold` module
//! - Loop-Invariant Code Motion (LICM) via `licm` module
//! - Post-optimization rewrites (select lowering) via `rewrite` module

mod const_fold;
mod const_global_promotion;
mod const_prop;
mod copy_prop;
pub mod dce;
mod field_scalarize;
mod inline;
mod labeled_block_fusion;
mod licm;
mod ref_elim;
mod rewrite;
mod sroa;
mod store_load_forward;
mod tmpl_hoist;

use const_fold::fold_constants;
use const_global_promotion::promote_constant_globals;
use const_prop::propagate_constants;
use copy_prop::propagate_copies;
use dce::{
    analyze_project, prune_constant_branches, remove_unreachable_functions,
    remove_unreachable_globals, remove_unreachable_types,
};
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use labeled_block_fusion::fuse_labeled_blocks;
use licm::apply_licm;
use ref_elim::eliminate_unnecessary_refs;
use sroa::scalar_replace_aggregates;
use store_load_forward::forward_stores_to_loads;
use tmpl_hoist::hoist_template_buffers;

use crate::project::Project;

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
/// | O2    | Yes | 10         | 10               |
/// | O3    | Yes | 100        | 19               |
/// | Os    | Yes | 10         | 10               |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimization passes. DCE only.
    O0,
    /// Development optimizations. All passes with fast iteration count.
    /// Iterations: 2, Inline threshold: 5.
    O1,
    /// Production optimizations. All passes including DCE.
    /// Iterations: 10, Inline threshold: 10.
    #[default]
    O2,
    /// Aggressive production optimizations. All passes including DCE.
    /// Iterations: 100, Inline threshold: 19 (20 degrades fts benchmark performance).
    O3,
    /// Size optimizations. Same as O2 plus name section stripping.
    /// Intended for frontend/browser deployment.
    Os,
}

/// Optimize a Project by analyzing and populating its usage fields.
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
    mut project: Project,
    opt_level: OptLevel,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
) -> Project {
    match opt_level {
        OptLevel::O0 => {
            // No optimizations, but still run DCE to reduce codegen work
            run_dce(&mut project);
        }
        OptLevel::O1 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(2),
                inline_threshold: inline_threshold.unwrap_or(5),
            };
            // Early DCE: remove unreachable functions/types before optimization
            // to reduce the working set for subsequent passes
            run_dce(&mut project);
            run_optimization_passes(&mut project, &config);
            // Final DCE: clean up code made dead by optimizations
            run_dce(&mut project);
        }
        OptLevel::O2 | OptLevel::Os => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(10),
                // Threshold 12: allows index_assign (11 expressions) to be inlined
                inline_threshold: inline_threshold.unwrap_or(12),
            };
            run_dce(&mut project);
            run_optimization_passes(&mut project, &config);
            run_dce(&mut project);
            if opt_level == OptLevel::Os {
                project.strip_names = true;
            }
        }
        OptLevel::O3 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(100),
                inline_threshold: inline_threshold.unwrap_or(20),
            };
            run_dce(&mut project);
            run_optimization_passes(&mut project, &config);
            run_dce(&mut project);
        }
    }

    // Post-optimization rewrites: simplify labeled blocks and insert moves in a single pass.
    rewrite::rewrite(&mut project);

    project
}

fn run_dce(project: &mut Project) {
    analyze_project(project);
    remove_unreachable_functions(project);
    remove_unreachable_globals(project);
    remove_unreachable_types(project);
}

/// Run optimization passes with a fixed-point iteration strategy.
///
/// Each iteration runs the full optimization pipeline:
/// - Function inlining
/// - Reference elimination
/// - Scalar Replacement of Aggregates (SROA)
/// - Copy propagation
/// - Constant propagation (global constants → literals)
/// - Constant folding
/// - Constant branch pruning (dead branch elimination)
/// - Loop-invariant code motion (LICM)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
///
/// Hot Field Scalarization (HFS) runs once after the loop converges; see
/// `optimize` for the rationale.
fn run_optimization_passes(project: &mut Project, config: &OptConfig) {
    for _ in 0..config.iterations {
        let mut changed = false;
        changed |= inline_functions(project, config.inline_threshold);
        changed |= fuse_labeled_blocks(project);
        changed |= eliminate_unnecessary_refs(project);
        changed |= scalar_replace_aggregates(project);
        changed |= propagate_copies(project);
        changed |= forward_stores_to_loads(project);
        changed |= propagate_constants(project);
        changed |= fold_constants(project);
        changed |= promote_constant_globals(project);
        changed |= prune_constant_branches(project);
        changed |= apply_licm(project);
        changed |= hoist_template_buffers(project);
        if !changed {
            break;
        }
    }
    // Hot Field Scalarization runs once after the main loop converges.
    // Running inside the loop would cause the write-back/re-read stmts it
    // inserts to be counted as new field accesses on the next iteration,
    // triggering spurious re-scalarization of the same fields.
    scalarize_hot_fields(project);
}
