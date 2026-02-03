//! Project - Compilation context for Wado programs
//!
//! This module provides the `Project` struct which encapsulates all
//! compilation context needed for code generation.
//!
//! The compilation flow is:
//! 1. Parse/analyze -> Project (unoptimized)
//! 2. Optimize -> Project (optimized, with usage analysis)
//! 3. Codegen takes Project and generates Wasm

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::name::{FunctionId, ModuleSource};
use crate::symbol::SymbolTable;
use crate::tir::{PrimitiveType, TirModule};
use crate::world_registry::WorldRegistry;
use indexmap::IndexMap;
use std::collections::HashSet;

/// A Wado project ready for code generation.
///
/// A Project is a representation of a WebAssembly Component Model component.
/// It contains all the information needed to compile a Wado program,
/// including the results of optimization analysis.
#[derive(Debug)]
pub struct Project {
    // ========================================
    // Source artifacts
    // ========================================
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules indexed by module source
    pub tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Symbol table from analysis phase
    pub symbols: SymbolTable,
    /// Implicitly imported modules (e.g., core:prelude)
    pub implicit_modules: HashSet<ModuleSource>,
    /// Module name for the output (derived from filename)
    pub module_name: String,

    // ========================================
    // Registries (built once, shared across phases)
    // ========================================
    /// Registry of WASI imports from lib/wasi/*.wado
    pub wasi_registry: WasiRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: WorldRegistry,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    pub builtin_registry: BuiltinRegistry,

    // ========================================
    // Usage analysis results (what the project contains)
    // ========================================
    /// Set of reachable functions (from DCE analysis)
    pub reachable_functions: HashSet<FunctionId>,
    /// When true, all functions are considered reachable (DCE disabled)
    pub all_reachable: bool,
    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: HashSet<String>,
    /// Primitive types that need box types (for references like &i32, &mut f64)
    pub used_box_primitives: HashSet<PrimitiveType>,
    /// When true, generic `ref_box` type is needed (for `&mut T` where T is non-primitive)
    pub needs_ref_box: bool,

    // ========================================
    // Codegen options
    // ========================================
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// Target world for Component Model export (e.g., "Command", "Service")
    /// Defaults to "Command" (wasi:cli/command)
    pub target_world: String,
    /// When true, apply DCE to bundled Wasm module (enabled for -O1+, disabled for -O0)
    pub wasm_dce_enabled: bool,

    // ========================================
    // CM export characteristics (derived from target_world)
    // ========================================
    /// When true, the target world exports an HTTP handler (returns Result<Response, `ErrorCode`>).
    /// This determines whether HTTP-related glue code is needed.
    pub has_http_handler_export: bool,
}

impl Project {
    /// Create a new Project from compilation artifacts (before optimization).
    pub fn new(
        entry_module_source: ModuleSource,
        tir_modules: IndexMap<ModuleSource, TirModule>,
        symbols: SymbolTable,
        implicit_modules: HashSet<ModuleSource>,
        module_name: String,
        wasi_registry: WasiRegistry,
        world_registry: WorldRegistry,
        builtin_registry: BuiltinRegistry,
    ) -> Self {
        Self {
            entry_module_source,
            tir_modules,
            symbols,
            implicit_modules,
            module_name,
            wasi_registry,
            world_registry,
            builtin_registry,
            // Usage analysis fields default to empty/false
            reachable_functions: HashSet::new(),
            all_reachable: false,
            used_wasi_functions: HashSet::new(),
            used_box_primitives: HashSet::new(),
            needs_ref_box: false,
            // Codegen options
            strip_names: false,
            target_world: "Command".to_string(),
            wasm_dce_enabled: true, // Enabled by default, disabled for -O0
            // CM export characteristics
            has_http_handler_export: false,
        }
    }

    /// Get the entry module TIR.
    pub fn entry_module(&self) -> &TirModule {
        self.tir_modules
            .get(&self.entry_module_source)
            .expect("entry module should exist in TIR modules")
    }

    /// Check if a function is reachable (should be included in the binary)
    pub fn is_reachable(&self, func_id: &FunctionId) -> bool {
        self.all_reachable || self.reachable_functions.contains(func_id)
    }

    /// Check if any function from the given WASI effect is used.
    /// Effect names are like "Stdout", "Stderr", "Environment", etc.
    pub fn has_effect(&self, effect_name: &str) -> bool {
        let prefix = format!("{effect_name}::");
        self.used_wasi_functions
            .iter()
            .any(|f| f.starts_with(&prefix))
    }
}
