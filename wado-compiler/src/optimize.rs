//! Optimization pass for Wado TIR
//!
//! This module coordinates various optimization passes:
//! - Dead Code Elimination (DCE) via `optimize_dce` module
//! - Function inlining via `optimize_inline` module
//! - Reference elimination via `optimize_ref_elim` module
//! - Copy propagation via `optimize_copy_prop` module
//! - Constant folding via `optimize_const_fold` module
//! - Loop-Invariant Code Motion (LICM) via `optimize_licm` module
//! - Post-optimization rewrites (select lowering, move insertion) via `optimize_rewrite` module

use crate::optimize_const_fold::fold_constants;
use crate::optimize_copy_prop::propagate_copies;
use crate::optimize_dce::{
    analyze_project, populate_all_features, remove_unreachable_functions, remove_unreachable_types,
};
use crate::optimize_inline::inline_functions;

/// Configuration for optimization passes
struct OptConfig {
    /// Number of fixed-point iterations
    iterations: u32,
    /// Maximum statement count for inlining
    inline_threshold: usize,
}
use crate::optimize_licm::apply_licm;
use crate::optimize_ref_elim::eliminate_unnecessary_refs;
use crate::optimize_rewrite::{collect_value_copy_types, insert_moves};
use crate::project::Project;

/// Optimization level for the compiler.
///
/// The levels are designed for different use cases:
/// - O0: Debugging - no optimizations
/// - O1: Development - fast compilation, all optimizations except DCE
/// - O2: Production - full optimizations with moderate iteration count (default)
/// - O3: Production - full optimizations with aggressive iteration count
/// - Os: Frontend - O2 + name section stripping for smaller binaries
///
/// Configuration for each level:
/// | Level | DCE | Iterations | Inline Threshold |
/// |-------|-----|------------|------------------|
/// | O0    | No  | 0          | N/A              |
/// | O1    | No  | 2          | 10               |
/// | O2    | Yes | 10         | 10               |
/// | O3    | Yes | 100        | 20               |
/// | Os    | Yes | 10         | 10               |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    O0,
    /// Development optimizations. All passes except DCE.
    /// Keeps dead code for debugging while improving runtime performance.
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
/// - O0: No optimizations, just populate all features for codegen
/// - O1: All optimizations except DCE (keeps dead code for debugging)
/// - O2: Full optimizations including DCE (default)
/// - O3: Full optimizations with aggressive iteration count
/// - Os: Same as O2 plus name section stripping
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::O0 => {
            // No optimizations - enable all standard features
            populate_all_features(&mut project);
            // Note: O0 mode only enables standard WASI functions from the stdlib.
            // Non-standard functions like sockets require O2+ to be detected via DCE analysis.
            // Disable Wasm-level DCE for bundled module (for faster compilation)
            project.wasm_dce_enabled = false;
        }
        OptLevel::O1 => {
            // Development mode: all optimizations except DCE
            // This keeps dead code visible for debugging while improving runtime
            let config = OptConfig {
                iterations: 2,
                inline_threshold: 10,
            };
            run_optimization_passes(&mut project, &config);
            // Still need to populate features without removing unreachable code
            populate_all_features(&mut project);
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

    // Insert move optimization for all optimization levels (after inlining)
    // This eliminates unnecessary copies for fresh values
    insert_moves(&mut project);

    // Collect value copy types for all functions
    // This populates needed_copy_types for codegen to pre-allocate scratch locals
    collect_value_copy_types(&mut project);

    // Expand copy source types for all functions
    // This creates the expanded set of types that need copy source locals,
    // including nested types (e.g., Option<Variant> expands to include the Variant type)
    expand_copy_source_types(&mut project);

    // Analyze scratch local requirements for all functions
    // This must run AFTER inlining since the function body may change.
    // Populates scratch_locals, indirect_call_counts, match_scrutinee_types, let_pattern_types.
    crate::lower::analyze_scratch_locals_project(&mut project);

    project
}

/// Run optimization passes with a fixed-point iteration strategy.
///
/// Each iteration runs the full optimization pipeline:
/// - Function inlining
/// - Reference elimination
/// - Copy propagation
/// - Constant folding
/// - Loop-invariant code motion (LICM)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
fn run_optimization_passes(project: &mut Project, config: &OptConfig) {
    for _ in 0..config.iterations {
        inline_functions(project, config.inline_threshold);
        eliminate_unnecessary_refs(project);
        propagate_copies(project);
        fold_constants(project);
        apply_licm(project);
    }
}

/// Expand copy source types for all functions in the project.
///
/// This takes the `needed_copy_types` set (computed by `collect_value_copy_types`)
/// and expands it to include all nested types that also need copy source locals.
/// For example, `Option<Variant>` expands to include both Option and Variant types.
fn expand_copy_source_types(project: &mut Project) {
    use crate::copy_context::CopyContext;

    for module in project.tir_modules.values() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if !func.needed_copy_types.is_empty() {
                func.copy_source_types =
                    CopyContext::expand_copy_types(&func.needed_copy_types, &type_table);
            }
        }
    }
}
