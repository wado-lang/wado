//! Optimization pass for Wado TIR
//!
//! This module coordinates various optimization passes:
//! - Dead Code Elimination (DCE) via `optimize_dce` module
//! - Function inlining via `optimize_inline` module
//! - Reference elimination via `optimize_ref_elim` module
//! - Copy propagation via `optimize_copy_prop` module
//! - Loop-Invariant Code Motion (LICM) via `optimize_licm` module
//! - Move insertion via `optimize_move` module

use crate::optimize_copy_prop::propagate_copies;
use crate::optimize_dce::{
    analyze_project, populate_all_features, remove_unreachable_functions,
    remove_unreachable_structs,
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
use crate::optimize_move::{collect_value_copy_types, insert_moves};
use crate::optimize_ref_elim::eliminate_unnecessary_refs;
use crate::project::Project;

/// Canonical builtin functions imported from wasi or env namespace
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonBuiltin {
    // Stream intrinsics (wasi namespace)
    StreamNew,
    StreamWrite,
    StreamDropWritable,
    StreamDropReadable,
    // Future intrinsics (wasi namespace)
    FutureNew,
    FutureWrite,
    FutureDropWritable,
    FutureDropReadable,
    // Async/task intrinsics (wasi namespace)
    TaskReturn,
    WaitableSetNew,
    WaitableJoin,
    WaitableSetWait,
    SubtaskDrop,
    // Env intrinsics (env namespace)
    Realloc,
    F64ToBuffer,
    F32ToBuffer,
}

impl CanonBuiltin {
    /// Parse canonical name from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stream-new" => Some(Self::StreamNew),
            "stream-write" => Some(Self::StreamWrite),
            "stream-drop-writable" => Some(Self::StreamDropWritable),
            "stream-drop-readable" => Some(Self::StreamDropReadable),
            "future-new" => Some(Self::FutureNew),
            "future-write" => Some(Self::FutureWrite),
            "future-drop-writable" => Some(Self::FutureDropWritable),
            "future-drop-readable" => Some(Self::FutureDropReadable),
            "task-return" => Some(Self::TaskReturn),
            "waitable-set-new" => Some(Self::WaitableSetNew),
            "waitable-join" => Some(Self::WaitableJoin),
            "waitable-set-wait" => Some(Self::WaitableSetWait),
            "subtask-drop" => Some(Self::SubtaskDrop),
            "realloc" => Some(Self::Realloc),
            "f64_to_buffer" => Some(Self::F64ToBuffer),
            "f32_to_buffer" => Some(Self::F32ToBuffer),
            _ => None,
        }
    }

    /// Get the canonical name (for wasm imports)
    pub fn canonical_name(&self) -> &'static str {
        match self {
            Self::StreamNew => "stream-new",
            Self::StreamWrite => "stream-write",
            Self::StreamDropWritable => "stream-drop-writable",
            Self::StreamDropReadable => "stream-drop-readable",
            Self::FutureNew => "future-new",
            Self::FutureWrite => "future-write",
            Self::FutureDropWritable => "future-drop-writable",
            Self::FutureDropReadable => "future-drop-readable",
            Self::TaskReturn => "task-return",
            Self::WaitableSetNew => "waitable-set-new",
            Self::WaitableJoin => "waitable-join",
            Self::WaitableSetWait => "waitable-set-wait",
            Self::SubtaskDrop => "subtask-drop",
            Self::Realloc => "realloc",
            Self::F64ToBuffer => "f64_to_buffer",
            Self::F32ToBuffer => "f32_to_buffer",
        }
    }

    /// Check if this is a float-to-string conversion builtin
    pub fn is_float_to_string(&self) -> bool {
        matches!(self, Self::F64ToBuffer | Self::F32ToBuffer)
    }

    /// All importable builtins (for Command world / standard CLI programs)
    /// Note: Future intrinsics are NOT included here as they're Service-world-specific
    pub const ALL: &'static [CanonBuiltin] = &[
        CanonBuiltin::StreamNew,
        CanonBuiltin::StreamWrite,
        CanonBuiltin::StreamDropWritable,
        CanonBuiltin::StreamDropReadable,
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
        CanonBuiltin::Realloc,
        CanonBuiltin::F64ToBuffer,
        CanonBuiltin::F32ToBuffer,
    ];

    /// Future intrinsics (only available in Service world for HTTP trailers)
    pub const FUTURE: &'static [CanonBuiltin] = &[
        CanonBuiltin::FutureNew,
        CanonBuiltin::FutureWrite,
        CanonBuiltin::FutureDropWritable,
        CanonBuiltin::FutureDropReadable,
    ];

    /// Async/task-related builtins
    pub const ASYNC: &'static [CanonBuiltin] = &[
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];

    /// Waitable-set builtins (only needed when `effect_wait` is called)
    pub const WAITABLE_SET: &'static [CanonBuiltin] = &[
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];
}

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
            // DCE: analyze and remove unreachable functions/structs
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_structs(&mut project);
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
            // DCE: analyze and remove unreachable functions/structs
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            remove_unreachable_structs(&mut project);
        }
    }

    // Insert move optimization for all optimization levels (after inlining)
    // This eliminates unnecessary copies for fresh values
    insert_moves(&mut project);

    // Collect value copy types for all functions
    // This populates needed_copy_types for codegen to pre-allocate scratch locals
    collect_value_copy_types(&mut project);

    project
}

/// Run optimization passes with a fixed-point iteration strategy.
///
/// Each iteration runs the full optimization pipeline:
/// - Function inlining
/// - Reference elimination
/// - Copy propagation
/// - Loop-invariant code motion (LICM)
///
/// The `config` parameter controls the number of iterations and inline threshold.
/// More iterations can find more optimization opportunities but take longer.
fn run_optimization_passes(project: &mut Project, config: &OptConfig) {
    for _ in 0..config.iterations {
        inline_functions(project, config.inline_threshold);
        eliminate_unnecessary_refs(project);
        propagate_copies(project);
        apply_licm(project);
    }
}
