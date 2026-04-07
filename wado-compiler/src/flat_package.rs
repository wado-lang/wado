//! FlatPackage — linked compilation context for WIR building and code generation
//!
//! `FlatPackage` is produced by the link phase from a `Package`. It carries
//! the TIR modules and metadata needed by `wir_build` and `codegen`.
//!
//! Conceptually, the link phase consumes per-module TIR and produces a linked
//! representation. In this initial version, `tir_modules` is kept as-is;
//! future work will flatten it into a single module-independent function/type
//! list so that `wir_build` no longer needs per-module iteration.

use std::cell::RefCell;
use std::rc::Rc;

use crate::component_model::WasiRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::{FunctionId, ModuleSource};
use crate::tir::{TirModule, TypeTable};
use crate::wir_build::component_plan::ComponentPlan;
use crate::world_registry::{self, WorldRegistry};

/// A linked Wado package ready for WIR building and code generation.
///
/// Produced by [`crate::link::link`] from a [`crate::package::Package`].
/// Contains the TIR modules plus all metadata needed by downstream phases
/// (WIR build, WIR optimize, codegen).
#[derive(Debug)]
pub struct FlatPackage {
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules indexed by module source.
    ///
    /// In a future refactoring step, this will be replaced by flat lists of
    /// functions, structs, variants, etc.
    pub tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Shared type table (same `Rc` as every `TirModule.type_table`)
    pub type_table: Rc<RefCell<TypeTable>>,
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
}

impl FlatPackage {
    /// Get the entry module TIR.
    pub fn entry_module(&self) -> &TirModule {
        self.tir_modules
            .get(&self.entry_module_source)
            .expect("entry module should exist in TIR modules")
    }

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
