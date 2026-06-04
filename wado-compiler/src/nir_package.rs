//! `NirPackage` — post-lower compilation context for `optimize` and `wir_build`.
//!
//! `NirPackage` is the post-lower counterpart of [`crate::flat_package::FlatPackage`].
//! It mirrors `FlatPackage`'s field shapes, but its body-shape fields
//! ([`NirFunction`], [`NirGlobal`], …) live in [`crate::nir`] so the type
//! system enforces the "lower has run" precondition for downstream phases.
//!
//! See `docs/wep-2026-05-11-nir.md` for the rationale.

use std::cell::RefCell;
use std::rc::Rc;

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::nir::{
    ClosureFunctor, NirEnum, NirFlags, NirFunction, NirGlobal, NirImport, NirStruct, NirTest,
    NirVariantDecl,
};
use crate::tir::{TypeId, TypeTable};
use crate::wir_build::component_plan::ComponentPlan;
use crate::world_registry::{self, WorldRegistry};

/// A linked Wado package ready for WIR building and code generation.
///
/// Produced by [`crate::link::link`] from a [`crate::package::Package`].
/// Contains flattened NIR data (merged from all modules) plus metadata needed
/// by downstream phases (optimizer, WIR build, codegen).
#[derive(Debug)]
pub struct NirPackage {
    /// The entry module source
    pub entry_module_source: ModuleSource,

    /// Shared type table
    pub type_table: Rc<RefCell<TypeTable>>,

    /// All functions from all modules. Each `NirFunction` carries its own `module_source`.
    pub functions: Vec<Rc<RefCell<NirFunction>>>,
    /// All struct declarations (each carries its own `module_source`)
    pub structs: Vec<NirStruct>,
    /// All enum declarations (each carries its own `module_source`)
    pub enums: Vec<NirEnum>,
    /// All variant declarations (each carries its own `module_source`)
    pub variants: Vec<NirVariantDecl>,
    /// Index: `(module_source, name)` → index into `variants`.
    pub variant_index: IndexMap<(ModuleSource, String), usize>,
    /// All flags declarations (each carries its own `module_source`)
    pub flags: Vec<NirFlags>,
    /// All global variable declarations (each carries its own `module_source`)
    pub globals: Vec<NirGlobal>,
    /// Imports (from entry module only)
    pub imports: Vec<NirImport>,
    /// Test declarations (from entry module only)
    pub tests: Vec<NirTest>,
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
    pub cm_interface_registry: &'static CmInterfaceRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: &'static WorldRegistry,

    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: IndexSet<String>,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// Maximum UTF-8 byte length for a string literal to get a constant
    /// `array.new_fixed<u8>` repr (which lets a constant string global promote
    /// to an eager Wasm constant). Longer strings keep the compact
    /// `array.new_data` data-segment repr and stay lazy. Set by `optimize` from
    /// the opt level (raised at `-O3`); see `optimize::string_inline_max_bytes`.
    pub string_inline_max_bytes: usize,
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

    /// Project-wide trait knowledge inherited from `Package` and grown
    /// here by [`crate::monomorphize::monomorphize`], which adds the
    /// instantiation layer once it has materialised the concrete
    /// trait-method instances.
    pub trait_env: std::sync::Arc<crate::elaborator::trait_env::TraitEnv>,
}

impl NirPackage {
    /// Default short-string inline threshold (UTF-8 bytes), used for build
    /// paths that skip `optimize` (e.g. `wado dump --nir-lowered`). `optimize`
    /// overrides it per opt level.
    pub const DEFAULT_STRING_INLINE_MAX_BYTES: usize = 4;

    /// Check if the project targets the synthetic test world.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
    }

    /// Build the lookup of synthesized value-copy helpers, keyed by
    /// `(module_source, name)` → the type each helper deep-copies. Shared by the
    /// `value_copy_elide` optimizer pass and the `remarks` collector so the two
    /// stay in lock-step with the helper-identification convention.
    pub fn value_copy_helper_types(&self) -> IndexMap<(ModuleSource, String), TypeId> {
        self.functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                f.value_copy_type()
                    .map(|t| ((f.module_source.clone(), f.name.clone()), t))
            })
            .collect()
    }

    /// Check whether the active world declares an
    /// `import {interface_name} { ... }` block.
    ///
    /// Drives world-shape decisions in codegen / lowering / DCE that used to
    /// hinge on `target_world == "core:kiln/generator"` string matches —
    /// `imports_interface("KilnHost")` is true for the kiln generator world and
    /// any future world that imports the same interface, so adding a new
    /// generator-shaped world no longer needs new branches.
    ///
    /// Returns `false` for the synthetic test world and for unknown worlds
    /// (both have no entry in the registry).
    pub fn world_imports_interface(&self, interface_name: &str) -> bool {
        self.world_registry
            .get(&self.target_world)
            .is_some_and(|w| w.imports_interface(interface_name))
    }

    /// Look up a variant by `(module_source, name)`.
    pub fn find_variant(&self, ms: &ModuleSource, name: &str) -> Option<&NirVariantDecl> {
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
    pub fn has_interface(&self, interface_name: &str) -> bool {
        let prefix = format!("{interface_name}::");
        self.used_wasi_functions
            .iter()
            .any(|f| f.starts_with(&prefix))
    }
}
