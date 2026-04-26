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
use crate::name::{LocalMethodName, ModuleSource};
use crate::tir::{
    ClosureFunctor, TirEnum, TirFlags, TirFunction, TirGlobal, TirImport, TirStruct, TirTest,
    TirVariantDecl, TypeId, TypeTable,
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
    /// Index: `(module_source, name)` → index into `variants`.
    pub variant_index: IndexMap<(ModuleSource, String), usize>,
    /// All flags declarations (each carries its own `module_source`)
    pub flags: Vec<TirFlags>,
    /// All global variable declarations (each carries its own `module_source`)
    pub globals: Vec<TirGlobal>,
    /// Imports (from entry module only)
    pub imports: Vec<TirImport>,
    /// Test declarations (from entry module only)
    pub tests: Vec<TirTest>,
    /// All string literals (merged from all modules)
    pub string_literals: Vec<String>,
    /// All byte array literals (merged from all modules)
    pub bytes_literals: Vec<Vec<u8>>,
    /// Closure functor metadata (each carries its own `module_source`)
    pub closure_functors: Vec<ClosureFunctor>,
    /// Map of (`ModuleSource`, function name) to string literals it contains (for DCE)
    pub function_strings: IndexMap<(ModuleSource, String), Vec<String>>,
    /// Map of (`ModuleSource`, function name) to method info (for DCE)
    pub function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
    /// Map of module source to wasm module name (from `#![wasm_module("name")]`)
    pub wasm_module_sources: IndexMap<ModuleSource, String>,

    /// Module name for the output (derived from filename)
    pub module_name: String,
    /// Registry of WASI imports from lib/wasi/*.wado
    pub wasi_registry: &'static WasiRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: &'static WorldRegistry,

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

    /// Wasm assets loaded by the loader. Keyed by canonical namespace
    /// string (matches `namespace` in `#[canonical("wasm:<path>",
    /// "<export>")]` attributes). Consumed by
    /// `codegen::component::embed_imported_wasm_modules`.
    pub wasm_assets: IndexMap<String, crate::loader::WasmAsset>,
}

impl FlatPackage {
    /// Check if the project targets the synthetic test world.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
    }

    /// Check if the project targets the Kiln generator well-known world
    /// (`core:kiln/generator`). The codegen path uses this to emit the
    /// kiln-specific CM record/variant types and to point the
    /// `task-return` canon at the `result<response, error>` shape
    /// required by the generator's export signature.
    pub fn is_kiln_generator_world(&self) -> bool {
        self.target_world == "core:kiln/generator"
    }

    /// Look up a variant by `(module_source, name)`.
    pub fn find_variant(&self, ms: &ModuleSource, name: &str) -> Option<&TirVariantDecl> {
        self.variant_index
            .get(&(ms.clone(), name.to_string()))
            .and_then(|&idx| self.variants.get(idx))
    }

    /// Rebuild variant lookup indices after the variants list has been modified
    /// (e.g., after DCE removes unreachable variants).
    pub fn rebuild_variant_indices(&mut self) {
        self.variant_index.clear();
        for (i, v) in self.variants.iter().enumerate() {
            self.variant_index
                .entry((v.module_source.clone(), v.name.clone()))
                .or_insert(i);
        }
    }

    /// Check if any function from the given WASI effect is used.
    pub fn has_effect(&self, effect_name: &str) -> bool {
        let prefix = format!("{effect_name}::");
        self.used_wasi_functions
            .iter()
            .any(|f| f.starts_with(&prefix))
    }
}
