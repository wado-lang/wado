//! Project - Compilation context for Wado programs
//!
//! This module provides the `Project` struct which encapsulates all
//! compilation context needed for code generation.
//!
//! The compilation flow is:
//! 1. Parse/analyze -> Project (unoptimized)
//! 2. Optimize -> Project (optimized, with usage analysis)
//! 3. Codegen takes Project and generates Wasm

use crate::name::FunctionId;
use crate::optimize::{CanonBuiltin, WasiEffect};
use crate::symbol::SymbolTable;
use crate::tir::{PrimitiveType, TirModule};
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
    /// Path to the entry module (e.g., ["example", "hello"])
    pub entry_path: Vec<String>,
    /// All TIR modules indexed by path
    pub tir_modules: IndexMap<Vec<String>, TirModule>,
    /// Symbol table from analysis phase
    pub symbols: SymbolTable,
    /// Implicitly imported modules (e.g., ["core", "prelude"])
    pub implicit_modules: HashSet<Vec<String>>,
    /// Module name for the output (derived from filename)
    pub module_name: String,

    // ========================================
    // Usage analysis results (what the project contains)
    // ========================================
    /// Set of reachable functions (from DCE analysis)
    pub reachable_functions: HashSet<FunctionId>,
    /// When true, all functions are considered reachable (DCE disabled)
    pub all_reachable: bool,
    /// Set of used WASI effects
    pub used_effects: HashSet<WasiEffect>,
    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: HashSet<String>,
    /// Set of used builtin functions
    pub used_builtins: HashSet<CanonBuiltin>,
    /// Primitive types that need box types (for references like &i32, &mut f64)
    pub used_box_primitives: HashSet<PrimitiveType>,

    // ========================================
    // Codegen options
    // ========================================
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
}

impl Project {
    /// Create a new Project from compilation artifacts (before optimization).
    pub fn new(
        entry_path: Vec<String>,
        tir_modules: IndexMap<Vec<String>, TirModule>,
        symbols: SymbolTable,
        implicit_modules: HashSet<Vec<String>>,
        module_name: String,
    ) -> Self {
        Self {
            entry_path,
            tir_modules,
            symbols,
            implicit_modules,
            module_name,
            // Usage analysis fields default to empty/false
            reachable_functions: HashSet::new(),
            all_reachable: false,
            used_effects: HashSet::new(),
            used_wasi_functions: HashSet::new(),
            used_builtins: HashSet::new(),
            used_box_primitives: HashSet::new(),
            // Codegen options
            strip_names: false,
        }
    }

    /// Get the entry module TIR.
    pub fn entry_module(&self) -> &TirModule {
        self.tir_modules
            .get(&self.entry_path)
            .expect("entry module should exist in TIR modules")
    }

    /// Check if a function is reachable (should be included in the binary)
    pub fn is_reachable(&self, func_id: &FunctionId) -> bool {
        self.all_reachable || self.reachable_functions.contains(func_id)
    }

    /// Check if float-to-string conversion is needed
    pub fn needs_float_to_string(&self) -> bool {
        self.used_builtins.contains(&CanonBuiltin::F64ToBuffer)
            || self.used_builtins.contains(&CanonBuiltin::F32ToBuffer)
    }
}
