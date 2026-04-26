//! Package — per-module compilation context for Wado programs
//!
//! `Package` flows through the early compilation phases (resolve → synthesis →
//! monomorphize → lower → optimize). The link phase then consumes it and
//! produces a [`crate::flat_package::FlatPackage`] for WIR building and codegen.

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::symbol::SymbolTable;
use crate::tir::{TirModule, TypeId};
use crate::world_registry::{self, WorldRegistry};

/// A Wado package in per-module form.
///
/// Contains TIR modules indexed by source, plus analysis metadata. Flows
/// through resolve → synthesis → monomorphize → lower → optimize, then
/// consumed by the link phase to produce a [`crate::flat_package::FlatPackage`].
#[derive(Debug)]
pub struct Package {
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules indexed by module source
    pub tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Symbol table from analysis phase
    pub symbols: SymbolTable,
    /// Implicitly imported modules (e.g., core:prelude)
    pub implicit_modules: IndexSet<ModuleSource>,
    /// Module name for the output (derived from filename)
    pub module_name: String,

    /// Registry of WASI imports from lib/wasi/*.wado
    pub wasi_registry: &'static WasiRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: &'static WorldRegistry,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    pub builtin_registry: BuiltinRegistry,

    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: IndexSet<String>,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// When true, skip Wasm validation after code generation.
    /// Returns raw bytes even if invalid — useful for debugging codegen.
    pub skip_validation: bool,
    /// Target world fully-qualified name (e.g., "wasi:cli/command", "wasi:http/service")
    pub target_world: String,

    /// Maps world export name → adapter function name.
    /// Populated by `synthesis::cm_binding` when export adapters are synthesized.
    /// For example: `"run"` → `"__cm_export__run"`.
    pub export_binding_names: IndexMap<String, String>,
    /// Flattened CM ABI parameter types for the `task-return` canonical intrinsic.
    /// Populated by `synthesis::cm_binding` when an export returns a Result type.
    /// Used by `optimize_dce` to override the builtin registry's single-`i32` signature.
    pub task_return_flat_params: Option<Vec<TypeId>>,

    /// Wasm asset bytes loaded by the loader from
    /// `use _ from "<path>" with { type: "wat"|"wasm" }` declarations.
    ///
    /// Keyed by canonical namespace string (matches `namespace` in
    /// `#[canonical("wasm:<path>", "<export>")]` attributes). Consumed by
    /// the codegen `embed_imported_wasm_modules` pass.
    pub wasm_assets: IndexMap<String, Vec<u8>>,
}

impl Package {
    /// Create a new Package from compilation artifacts (before optimization).
    pub fn new(
        entry_module_source: ModuleSource,
        tir_modules: IndexMap<ModuleSource, TirModule>,
        symbols: SymbolTable,
        implicit_modules: IndexSet<ModuleSource>,
        module_name: String,
        wasi_registry: &'static WasiRegistry,
        world_registry: &'static WorldRegistry,
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
            used_wasi_functions: IndexSet::default(),
            // Codegen options
            strip_names: false,
            skip_validation: false,
            target_world: "wasi:cli/command".to_string(),
            // CM export adapter mapping
            export_binding_names: IndexMap::default(),
            task_return_flat_params: None,
            // Wasm assets loaded from `use _ from "<path>" with { type: ... }`
            wasm_assets: IndexMap::default(),
        }
    }

    /// Get the entry module TIR.
    pub fn entry_module(&self) -> &TirModule {
        self.tir_modules
            .get(&self.entry_module_source)
            .expect("entry module should exist in TIR modules")
    }

    /// Check if the project targets the synthetic test world.
    ///
    /// When true, test functions are the component's exports and everything
    /// else (including world exports like `run`) is subject to DCE.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
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
