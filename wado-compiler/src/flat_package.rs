//! `FlatPackage` — linked compilation context for WIR building and code generation
//!
//! `FlatPackage` is produced by the link phase from a `Package`. It flattens all
//! per-module TIR into flat lists of functions, types, etc. Downstream phases
//! (optimizer, `wir_build`, `codegen`) work directly on these flat lists without
//! needing per-module iteration.

use std::cell::RefCell;
use std::rc::Rc;

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::{FunctionId, LocalMethodName, ModuleSource};
use crate::tir::{
    ClosureFunctor, TirEnum, TirFlags, TirFunction, TirGlobal, TirImpl, TirImport, TirNewtype,
    TirStruct, TirTest, TirTrait, TirVariantDecl, TypeId, TypeTable,
};
use crate::wir_build::component_plan::ComponentPlan;
use crate::world_registry::{self, WorldRegistry};

/// A linked Wado package ready for WIR building and code generation.
///
/// Produced by [`crate::link::link`] from a [`crate::package::Package`].
/// Contains flattened TIR data (merged from all modules) plus metadata needed
/// by downstream phases (optimizer, WIR build, codegen).
#[derive(Debug)]
pub struct FlatPackage {
    /// The entry module source
    pub entry_module_source: ModuleSource,

    /// Shared type table
    pub type_table: Rc<RefCell<TypeTable>>,

    /// All functions from all modules. Each `TirFunction` carries its own `module_source`.
    pub functions: Vec<Rc<RefCell<TirFunction>>>,
    /// All struct declarations (each carries its own `module_source`)
    pub structs: Vec<TirStruct>,
    /// All enum declarations (each carries its own `module_source`)
    pub enums: Vec<TirEnum>,
    /// All variant declarations (each carries its own `module_source`)
    pub variants: Vec<TirVariantDecl>,
    /// All flags declarations (each carries its own `module_source`)
    pub flags: Vec<TirFlags>,
    /// All newtype declarations (each carries its own `module_source`)
    pub newtypes: Vec<TirNewtype>,
    /// All global variable declarations (each carries its own `module_source`)
    pub globals: Vec<TirGlobal>,
    /// Imports (from entry module only)
    pub imports: Vec<TirImport>,
    /// Test declarations (from entry module only)
    pub tests: Vec<TirTest>,
    /// Trait declarations
    pub traits: Vec<TirTrait>,
    /// Trait impl blocks
    pub impls: Vec<TirImpl>,
    /// All string literals (merged from all modules)
    pub string_literals: Vec<String>,
    /// All byte array literals (merged from all modules)
    pub bytes_literals: Vec<Vec<u8>>,
    /// Closure functor metadata (each carries its own `module_source`)
    pub closure_functors: Vec<ClosureFunctor>,
    /// Data section content (from entry module)
    pub data_section: Option<String>,
    /// Map of (`ModuleSource`, function name) to string literals it contains (for DCE)
    pub function_strings: IndexMap<(ModuleSource, String), Vec<String>>,
    /// Map of (`ModuleSource`, function name) to method info (for DCE)
    pub function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
    /// Map of module source prefix to wasm module name (from `#![wasm_module("name")]`)
    pub wasm_module_sources: IndexMap<String, String>,

    /// Module name for the output (derived from filename)
    pub module_name: String,
    /// Registry of WASI imports from lib/wasi/*.wado
    pub wasi_registry: &'static WasiRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: &'static WorldRegistry,

    /// Set of reachable functions (from DCE analysis)
    pub reachable_functions: IndexSet<FunctionId>,
    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: IndexSet<String>,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// When true, skip Wasm validation after code generation.
    pub skip_validation: bool,
    /// Target world fully-qualified name (e.g., "wasi:cli/command", "wasi:http/service")
    pub target_world: String,

    /// When true, the target world exports an HTTP handler.
    pub has_http_handler_export: bool,
    /// Maps world export name → adapter function name.
    pub export_binding_names: IndexMap<String, String>,

    /// Component Model structure plan.
    pub component_plan: ComponentPlan,

    /// Registry of builtin functions (used by optimizer DCE)
    pub builtin_registry: BuiltinRegistry,
    /// Flat params for task-return type (used by DCE for async exports)
    pub task_return_flat_params: Option<Vec<TypeId>>,
}

impl FlatPackage {
    /// Check if a function is reachable (should be included in the binary)
    pub fn is_reachable(&self, func_id: &FunctionId) -> bool {
        self.reachable_functions.contains(func_id)
    }

    /// Check if the project targets the synthetic test world.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
    }

    /// Check if any function from the given WASI effect is used.
    pub fn has_effect(&self, effect_name: &str) -> bool {
        let prefix = format!("{effect_name}::");
        self.used_wasi_functions
            .iter()
            .any(|f| f.starts_with(&prefix))
    }
}
