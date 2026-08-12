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
    ClosureFunctor, FuncId, FunctionRef, NirEnum, NirFlags, NirFunction, NirGlobal, NirImport,
    NirStruct, NirTest, NirVariantDecl,
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
    /// The function arena's reverse index: canonical [`crate::name::FunctionId`]
    /// → [`FuncId`] (the store position). Built once in `lower` (`translate`)
    /// and grown append-only by [`Self::intern_extern`] as the optimizer
    /// synthesizes calls to new builtins. Authoritative, never rebuilt or
    /// invalidated — the interner that keeps the "born resolved" invariant cheap
    /// (O(1) per synthesis site, no per-pass walk).
    pub func_index: IndexMap<crate::name::FunctionId, FuncId>,
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
    pub cm_interface_registry: std::sync::Arc<CmInterfaceRegistry>,
    /// Registry of world definitions from lib/wasi/*.wado
    pub world_registry: std::sync::Arc<WorldRegistry>,

    /// Set of used WASI functions (e.g., "`Stdout::write_via_stream`")
    pub used_wasi_functions: IndexSet<String>,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// Fine-grained codegen feature flags from the CLI's `-f <flag>` option.
    /// Consulted by the WIR emitter to select alternative lowerings.
    pub codegen_flags: crate::codegen_flags::CodegenFlags,
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

    /// The [`FuncId`] of a `builtin::<name>` callee, or `None` if no such call is
    /// interned in this package. Resolved once (e.g. at a pass's top) so an
    /// optimizer recognizer can identify a builtin call by integer id comparison
    /// against the call node's `func_id` — no per-call name materialization, and
    /// no `store[id]` deref (which a self-recursive callee would double-borrow).
    pub fn builtin_func_id(&self, name: &str) -> Option<FuncId> {
        self.func_id_of(&FunctionRef {
            module_source: crate::module_source::ModuleSource::builtin(),
            name: name.to_string(),
            monomorph_info: None,
            method_info: None,
        })
    }

    /// Resolve a callee `FunctionRef` to its [`FuncId`] via the reverse index.
    /// `Some` for every in-package function and every already-interned extern;
    /// `None` only for a builtin the optimizer has not interned yet (see
    /// [`Self::intern_extern`]). O(1), no `full_name` materialization.
    pub fn func_id_of(&self, func_ref: &FunctionRef) -> Option<FuncId> {
        self.func_index.get(&func_ref.function_id()).copied()
    }

    /// The [`FuncId`]s of pure builtin / monomorphized-builtin intrinsics
    /// (`array_get`, `array_len`, `select`, every `core:builtin` / wasm-asset
    /// function, …). The value-graph builder reads this to know a call writes no
    /// heap, so a field version forwards across a loop body that only calls such
    /// intrinsics. Resolved by `func_id` off the callee's arena record now that
    /// the call node carries no `FunctionRef`. O(functions); a pass computes it
    /// once before its per-function loop.
    pub fn pure_builtin_callee_ids(&self) -> IndexSet<FuncId> {
        self.functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                let descriptor = FunctionRef::from_resolved(&f, f.module_source.clone());
                let is_pure_builtin = descriptor.builtin_name().is_some()
                    || descriptor.monomorphized_builtin_name().is_some();
                is_pure_builtin.then(|| f.id.expect("func_id assigned at lower"))
            })
            .collect()
    }

    /// Intern an extern / builtin callee into the [`FuncId`] space, returning its
    /// id. Idempotent: a callee already present (in-package or previously
    /// interned) returns its existing id; otherwise an `extern_stub` record is
    /// appended at `FuncId == position` and indexed. Lets an optimizer pass that
    /// synthesizes a builtin call stamp the call "born resolved" at the synthesis
    /// site, keeping `func_id` total across the loop without a re-scan.
    pub fn intern_extern(&mut self, func_ref: &FunctionRef) -> FuncId {
        use cranelift_entity::EntityRef;
        let key = func_ref.function_id();
        if let Some(&id) = self.func_index.get(&key) {
            return id;
        }
        let id = FuncId::new(self.functions.len());
        let mut stub = NirFunction::extern_stub(func_ref);
        stub.id = Some(id);
        self.functions.push(Rc::new(RefCell::new(stub)));
        self.func_index.insert(key, id);
        id
    }

    /// The next free [`FuncId`] (one past the current maximum). Optimizer passes
    /// that synthesize functions (`value_copy_demote`'s shallow-copy twins,
    /// container SROA's per-field accessors) mint fresh ids from here so a new
    /// function never collides with an existing id. `FuncId` stays monotonic and
    /// intrinsic — independent of `dce` compaction.
    pub fn next_func_id(&self) -> FuncId {
        use cranelift_entity::EntityRef;
        let next = self
            .functions
            .iter()
            .filter_map(|f| f.borrow().id)
            .map(|id| id.index() + 1)
            .max()
            .unwrap_or(0);
        FuncId::new(next)
    }

    /// Check if the project targets the synthetic test world.
    pub fn is_test_world(&self) -> bool {
        self.target_world == world_registry::TEST_WORLD
    }

    /// Whether the target world provides an ambient sink for the given stdio
    /// interface (`Stdout` / `Stderr`) used by the `log_*` (panic /
    /// assert-diagnostic) path. True for the test world and any world importing
    /// the interface; false for `--lib` and kiln, where the ambient path traps
    /// silently instead of forcing the import. Each stream is gated on its own
    /// interface so a world importing only one is not mis-gated by the other.
    pub fn provides_ambient_stdio_sink(&self, interface_name: &str) -> bool {
        self.is_test_world() || self.world_imports_interface(interface_name)
    }

    /// Build the lookup of synthesized value-copy helpers, keyed by
    /// `(module_source, name)` → the type each helper deep-copies. Used by the
    /// `remarks` collector, which reports the copied type.
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

    /// The [`FuncId`]s of the synthesized `$value_copy$T` helpers, so a pass can
    /// identify a wrapper call by id membership (e.g. `value_copy_demote`).
    pub fn value_copy_func_ids(&self) -> crate::hashmap::IndexSet<FuncId> {
        self.functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                f.value_copy_type().and(f.id)
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

    /// Whether the target world is a kiln generator world (imports
    /// [`crate::world_registry::GENERATOR_HOST_INTERFACE`]).
    pub fn is_generator_world(&self) -> bool {
        self.world_imports_interface(crate::world_registry::GENERATOR_HOST_INTERFACE)
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
