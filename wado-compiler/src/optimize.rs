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
use crate::optimize_dce::{analyze_project, populate_all_features, remove_unreachable_functions};
use crate::optimize_inline::inline_functions;
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

    /// All importable builtins
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    #[default]
    None,
    /// Baseline optimizations including DCE. Intended for development.
    Basic,
    /// All optimizations including inlining, decomposition, etc. (TBD).
    /// Intended for production (server-side).
    Full,
    /// Full optimizations plus name section stripping. Intended for frontend.
    Size,
}

/// Optimize a Project by analyzing and populating its usage fields.
///
/// This is the main entry point for the optimizer. Based on the optimization
/// level, it either performs DCE analysis or enables all features.
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::None => {
            populate_all_features(&mut project);
        }
        OptLevel::Basic => {
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Full => {
            inline_functions(&mut project);
            eliminate_unnecessary_refs(&mut project);
            propagate_copies(&mut project);
            apply_licm(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Size => {
            inline_functions(&mut project);
            eliminate_unnecessary_refs(&mut project);
            propagate_copies(&mut project);
            apply_licm(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            project.strip_names = true;
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
