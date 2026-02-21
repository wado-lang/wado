//! Optimization pass for Wado TIR
//!
//! This module coordinates various optimization passes:
//! - Dead Code Elimination (DCE) via `dce` module
//! - Function inlining via `inline` module
//! - Reference elimination via `ref_elim` module
//! - Scalar Replacement of Aggregates (SROA) via `sroa` module
//! - Copy propagation via `copy_prop` module
//! - Constant folding via `const_fold` module
//! - Loop-Invariant Code Motion (LICM) via `licm` module
//! - Post-optimization rewrites (select lowering, move insertion) via `rewrite` module

mod const_fold;
mod copy_prop;
pub mod dce;
mod inline;
mod licm;
mod ref_elim;
mod rewrite;
mod sroa;

use const_fold::fold_constants;
use copy_prop::propagate_copies;
use dce::{analyze_project, remove_unreachable_functions, remove_unreachable_types};
use inline::inline_functions;
use licm::apply_licm;
use ref_elim::eliminate_unnecessary_refs;
use sroa::scalar_replace_aggregates;

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
/// | O1    | Yes | 2          | 10               |
/// | O2    | Yes | 10         | 10               |
/// | O3    | Yes | 100        | 20               |
/// | Os    | Yes | 10         | 10               |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimization passes. DCE only.
    O0,
    /// Development optimizations. All passes with fast iteration count.
    /// Iterations: 2, Inline threshold: 10.
    O1,
    /// Production optimizations. All passes including DCE.
    /// Iterations: 10, Inline threshold: 10.
    #[default]
    O2,
    /// Aggressive production optimizations. All passes including DCE.
    /// Iterations: 100, Inline threshold: 20.
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
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::O0 => {
            // No optimizations, but still run DCE to reduce codegen work
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_types(&mut project);
        }
        OptLevel::O1 => {
            // Development mode: all optimizations including DCE
            let config = OptConfig {
                iterations: 2,
                inline_threshold: 10,
            };
            run_optimization_passes(&mut project, &config);
            // DCE: analyze and remove unreachable functions and types
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_types(&mut project);
        }
        OptLevel::O2 | OptLevel::Os => {
            // Production mode: full optimizations with DCE
            let config = OptConfig {
                iterations: 10,
                inline_threshold: 10,
            };
            run_optimization_passes(&mut project, &config);
            // DCE: analyze and remove unreachable functions and types
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_types(&mut project);
            if opt_level == OptLevel::Os {
                project.strip_names = true;
            }
        }
        OptLevel::O3 => {
            // Aggressive production mode: more fixed-point iterations
            let config = OptConfig {
                iterations: 100,
                inline_threshold: 20,
            };
            run_optimization_passes(&mut project, &config);
            // DCE: analyze and remove unreachable functions and types
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_types(&mut project);
        }
    }

    // Post-optimization rewrites: simplify labeled blocks and insert moves in a single pass.
    rewrite::rewrite(&mut project);

    project
}

/// Run optimization passes with a fixed-point iteration strategy.
///
/// Each iteration runs the full optimization pipeline:
/// - Function inlining
/// - Reference elimination
/// - Scalar Replacement of Aggregates (SROA)
/// - Copy propagation
/// - Constant folding
/// - Loop-invariant code motion (LICM)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
fn run_optimization_passes(project: &mut Project, config: &OptConfig) {
    for _ in 0..config.iterations {
        let mut changed = false;
        changed |= inline_functions(project, config.inline_threshold);
        changed |= eliminate_unnecessary_refs(project);
        changed |= scalar_replace_aggregates(project);
        changed |= propagate_copies(project);
        changed |= fold_constants(project);
        changed |= apply_licm(project);
        if !changed {
            break;
        }
    }
}
