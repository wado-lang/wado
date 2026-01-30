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
    // Libm intrinsics (env namespace, from wado-bundled)
    LibmSin,
    LibmCos,
    LibmTan,
    LibmAsin,
    LibmAcos,
    LibmAtan,
    LibmAtan2,
    LibmSinh,
    LibmCosh,
    LibmTanh,
    LibmAsinh,
    LibmAcosh,
    LibmAtanh,
    LibmExp,
    LibmExp2,
    LibmExpm1,
    LibmLog,
    LibmLog2,
    LibmLog10,
    LibmLog1p,
    LibmPow,
    LibmCbrt,
    LibmHypot,
    LibmFmod,
    LibmSinf,
    LibmCosf,
    LibmTanf,
    LibmAsinf,
    LibmAcosf,
    LibmAtanf,
    LibmAtan2f,
    LibmSinhf,
    LibmCoshf,
    LibmTanhf,
    LibmAsinhf,
    LibmAcoshf,
    LibmAtanhf,
    LibmExpf,
    LibmExp2f,
    LibmExpm1f,
    LibmLogf,
    LibmLog2f,
    LibmLog10f,
    LibmLog1pf,
    LibmPowf,
    LibmCbrtf,
    LibmHypotf,
    LibmFmodf,
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
            // Libm f64
            "libm_sin" => Some(Self::LibmSin),
            "libm_cos" => Some(Self::LibmCos),
            "libm_tan" => Some(Self::LibmTan),
            "libm_asin" => Some(Self::LibmAsin),
            "libm_acos" => Some(Self::LibmAcos),
            "libm_atan" => Some(Self::LibmAtan),
            "libm_atan2" => Some(Self::LibmAtan2),
            "libm_sinh" => Some(Self::LibmSinh),
            "libm_cosh" => Some(Self::LibmCosh),
            "libm_tanh" => Some(Self::LibmTanh),
            "libm_asinh" => Some(Self::LibmAsinh),
            "libm_acosh" => Some(Self::LibmAcosh),
            "libm_atanh" => Some(Self::LibmAtanh),
            "libm_exp" => Some(Self::LibmExp),
            "libm_exp2" => Some(Self::LibmExp2),
            "libm_expm1" => Some(Self::LibmExpm1),
            "libm_log" => Some(Self::LibmLog),
            "libm_log2" => Some(Self::LibmLog2),
            "libm_log10" => Some(Self::LibmLog10),
            "libm_log1p" => Some(Self::LibmLog1p),
            "libm_pow" => Some(Self::LibmPow),
            "libm_cbrt" => Some(Self::LibmCbrt),
            "libm_hypot" => Some(Self::LibmHypot),
            "libm_fmod" => Some(Self::LibmFmod),
            // Libm f32
            "libm_sinf" => Some(Self::LibmSinf),
            "libm_cosf" => Some(Self::LibmCosf),
            "libm_tanf" => Some(Self::LibmTanf),
            "libm_asinf" => Some(Self::LibmAsinf),
            "libm_acosf" => Some(Self::LibmAcosf),
            "libm_atanf" => Some(Self::LibmAtanf),
            "libm_atan2f" => Some(Self::LibmAtan2f),
            "libm_sinhf" => Some(Self::LibmSinhf),
            "libm_coshf" => Some(Self::LibmCoshf),
            "libm_tanhf" => Some(Self::LibmTanhf),
            "libm_asinhf" => Some(Self::LibmAsinhf),
            "libm_acoshf" => Some(Self::LibmAcoshf),
            "libm_atanhf" => Some(Self::LibmAtanhf),
            "libm_expf" => Some(Self::LibmExpf),
            "libm_exp2f" => Some(Self::LibmExp2f),
            "libm_expm1f" => Some(Self::LibmExpm1f),
            "libm_logf" => Some(Self::LibmLogf),
            "libm_log2f" => Some(Self::LibmLog2f),
            "libm_log10f" => Some(Self::LibmLog10f),
            "libm_log1pf" => Some(Self::LibmLog1pf),
            "libm_powf" => Some(Self::LibmPowf),
            "libm_cbrtf" => Some(Self::LibmCbrtf),
            "libm_hypotf" => Some(Self::LibmHypotf),
            "libm_fmodf" => Some(Self::LibmFmodf),
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
            // Libm f64
            Self::LibmSin => "libm_sin",
            Self::LibmCos => "libm_cos",
            Self::LibmTan => "libm_tan",
            Self::LibmAsin => "libm_asin",
            Self::LibmAcos => "libm_acos",
            Self::LibmAtan => "libm_atan",
            Self::LibmAtan2 => "libm_atan2",
            Self::LibmSinh => "libm_sinh",
            Self::LibmCosh => "libm_cosh",
            Self::LibmTanh => "libm_tanh",
            Self::LibmAsinh => "libm_asinh",
            Self::LibmAcosh => "libm_acosh",
            Self::LibmAtanh => "libm_atanh",
            Self::LibmExp => "libm_exp",
            Self::LibmExp2 => "libm_exp2",
            Self::LibmExpm1 => "libm_expm1",
            Self::LibmLog => "libm_log",
            Self::LibmLog2 => "libm_log2",
            Self::LibmLog10 => "libm_log10",
            Self::LibmLog1p => "libm_log1p",
            Self::LibmPow => "libm_pow",
            Self::LibmCbrt => "libm_cbrt",
            Self::LibmHypot => "libm_hypot",
            Self::LibmFmod => "libm_fmod",
            // Libm f32
            Self::LibmSinf => "libm_sinf",
            Self::LibmCosf => "libm_cosf",
            Self::LibmTanf => "libm_tanf",
            Self::LibmAsinf => "libm_asinf",
            Self::LibmAcosf => "libm_acosf",
            Self::LibmAtanf => "libm_atanf",
            Self::LibmAtan2f => "libm_atan2f",
            Self::LibmSinhf => "libm_sinhf",
            Self::LibmCoshf => "libm_coshf",
            Self::LibmTanhf => "libm_tanhf",
            Self::LibmAsinhf => "libm_asinhf",
            Self::LibmAcoshf => "libm_acoshf",
            Self::LibmAtanhf => "libm_atanhf",
            Self::LibmExpf => "libm_expf",
            Self::LibmExp2f => "libm_exp2f",
            Self::LibmExpm1f => "libm_expm1f",
            Self::LibmLogf => "libm_logf",
            Self::LibmLog2f => "libm_log2f",
            Self::LibmLog10f => "libm_log10f",
            Self::LibmLog1pf => "libm_log1pf",
            Self::LibmPowf => "libm_powf",
            Self::LibmCbrtf => "libm_cbrtf",
            Self::LibmHypotf => "libm_hypotf",
            Self::LibmFmodf => "libm_fmodf",
        }
    }

    /// Check if this is a float-to-string conversion builtin
    pub fn is_float_to_string(&self) -> bool {
        matches!(self, Self::F64ToBuffer | Self::F32ToBuffer)
    }

    /// Check if this is a libm math function
    pub fn is_libm(&self) -> bool {
        matches!(
            self,
            Self::LibmSin
                | Self::LibmCos
                | Self::LibmTan
                | Self::LibmAsin
                | Self::LibmAcos
                | Self::LibmAtan
                | Self::LibmAtan2
                | Self::LibmSinh
                | Self::LibmCosh
                | Self::LibmTanh
                | Self::LibmAsinh
                | Self::LibmAcosh
                | Self::LibmAtanh
                | Self::LibmExp
                | Self::LibmExp2
                | Self::LibmExpm1
                | Self::LibmLog
                | Self::LibmLog2
                | Self::LibmLog10
                | Self::LibmLog1p
                | Self::LibmPow
                | Self::LibmCbrt
                | Self::LibmHypot
                | Self::LibmFmod
                | Self::LibmSinf
                | Self::LibmCosf
                | Self::LibmTanf
                | Self::LibmAsinf
                | Self::LibmAcosf
                | Self::LibmAtanf
                | Self::LibmAtan2f
                | Self::LibmSinhf
                | Self::LibmCoshf
                | Self::LibmTanhf
                | Self::LibmAsinhf
                | Self::LibmAcoshf
                | Self::LibmAtanhf
                | Self::LibmExpf
                | Self::LibmExp2f
                | Self::LibmExpm1f
                | Self::LibmLogf
                | Self::LibmLog2f
                | Self::LibmLog10f
                | Self::LibmLog1pf
                | Self::LibmPowf
                | Self::LibmCbrtf
                | Self::LibmHypotf
                | Self::LibmFmodf
        )
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
        // Libm f64
        CanonBuiltin::LibmSin,
        CanonBuiltin::LibmCos,
        CanonBuiltin::LibmTan,
        CanonBuiltin::LibmAsin,
        CanonBuiltin::LibmAcos,
        CanonBuiltin::LibmAtan,
        CanonBuiltin::LibmAtan2,
        CanonBuiltin::LibmSinh,
        CanonBuiltin::LibmCosh,
        CanonBuiltin::LibmTanh,
        CanonBuiltin::LibmAsinh,
        CanonBuiltin::LibmAcosh,
        CanonBuiltin::LibmAtanh,
        CanonBuiltin::LibmExp,
        CanonBuiltin::LibmExp2,
        CanonBuiltin::LibmExpm1,
        CanonBuiltin::LibmLog,
        CanonBuiltin::LibmLog2,
        CanonBuiltin::LibmLog10,
        CanonBuiltin::LibmLog1p,
        CanonBuiltin::LibmPow,
        CanonBuiltin::LibmCbrt,
        CanonBuiltin::LibmHypot,
        CanonBuiltin::LibmFmod,
        // Libm f32
        CanonBuiltin::LibmSinf,
        CanonBuiltin::LibmCosf,
        CanonBuiltin::LibmTanf,
        CanonBuiltin::LibmAsinf,
        CanonBuiltin::LibmAcosf,
        CanonBuiltin::LibmAtanf,
        CanonBuiltin::LibmAtan2f,
        CanonBuiltin::LibmSinhf,
        CanonBuiltin::LibmCoshf,
        CanonBuiltin::LibmTanhf,
        CanonBuiltin::LibmAsinhf,
        CanonBuiltin::LibmAcoshf,
        CanonBuiltin::LibmAtanhf,
        CanonBuiltin::LibmExpf,
        CanonBuiltin::LibmExp2f,
        CanonBuiltin::LibmExpm1f,
        CanonBuiltin::LibmLogf,
        CanonBuiltin::LibmLog2f,
        CanonBuiltin::LibmLog10f,
        CanonBuiltin::LibmLog1pf,
        CanonBuiltin::LibmPowf,
        CanonBuiltin::LibmCbrtf,
        CanonBuiltin::LibmHypotf,
        CanonBuiltin::LibmFmodf,
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
