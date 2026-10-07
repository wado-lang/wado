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
use crate::codegen_flags::CodegenFlags;
use crate::component_model::CmInterfaceRegistry;
use crate::elaborator::trait_env::TraitEnv;
use crate::hashmap;
use crate::hashmap::{IndexMap, IndexSet};
use crate::loader::{DEFAULT_PAGE_SIZE_LOG2, WasmAsset};
use crate::lower::plan::value_copy::ValueCopyHelpers;
use crate::module_source::ModuleSource;
use crate::name::{FunctionId, LocalMethodName, retired_name};
use crate::nir::{
    ClosureFunctor, FuncId, FunctionRef, NirEnum, NirFlags, NirFunction, NirGlobal, NirImport,
    NirStruct, NirTest, NirVariantDecl,
};
use crate::tir::{BuiltinDeclaration, BuiltinDeclarations, TypeId, TypeTable};
use crate::wir_build::component_plan::ComponentPlan;
use crate::world_registry::{self, GENERATOR_HOST_INTERFACE, WorldRegistry};

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
    pub func_index: IndexMap<FunctionId, FuncId>,
    /// Which `$value_copy$` helper copies each type — the one join, so a
    /// consumer holding a `TypeId` asks here rather than re-deriving the key.
    pub value_copy_helpers: ValueCopyHelpers<FuncId>,
    /// Functions `optimize/sroa_param` minted, each mapped to the original it
    /// derives from, through however many clones of clones. A clone may gain
    /// callers in a later round, so its current call sites are not its whole
    /// contract. Identity, not a name test — see the declaration-identity WEP.
    pub sroa_param_clones: IndexMap<FuncId, FuncId>,
    /// Each function `optimize/dae` has reshaped, as it stood before the first
    /// parameter was dropped, so its name lists every dropped parameter in
    /// declaration order however many rounds dropped them.
    pub dae_reshaped: IndexMap<FuncId, DaeBase>,
    /// Every function renamed in place, in order, so a cache keyed by store
    /// position can refresh what went stale. Append-only.
    pub renamed: Vec<FuncId>,
    /// For each of those clones, which of its locals holds a scalarized field,
    /// and the fields it was projected through, outermost first: the last names
    /// the field the local is now, and a clone of a clone carries its source's
    /// chains on. Durable because the fact is: a later run of the pass
    /// rewriting calls *inside* a clone must know its param already holds the
    /// field, or it projects the wrapper's field onto it a second time; and it
    /// must not unwrap a struct the chain already holds, which a struct
    /// reaching itself through its one field never ends.
    pub sroa_param_clone_fields: IndexMap<FuncId, IndexMap<u32, Vec<SroaParamProjection>>>,
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
    /// What each bodyless declaration stated about storage. A pass reasoning
    /// about a builtin call reads this rather than matching on its name.
    pub builtin_declarations: BuiltinDeclarations,

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
    /// The `org.wado-lang.coverage` section payload, under `wado test --coverage`.
    pub coverage_section: Option<Vec<u8>>,
    /// Fine-grained codegen feature flags from the CLI's `-f <flag>` option.
    /// Consulted by the WIR emitter to select alternative lowerings.
    pub codegen_flags: CodegenFlags,
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

    /// Wasm assets loaded by the loader. Keyed by canonical namespace
    /// string (matches `namespace` in `#[canonical("wasm:<path>",
    /// "<export>")]` attributes). Consumed by
    /// `codegen::component::embed_imported_wasm_modules`.
    pub wasm_assets: IndexMap<String, WasmAsset>,

    /// The pages [`Self::wasm_asset_reserved_pages`] answers, fixed by the
    /// first DCE: an optimizer pass folds `builtin::heap_base()` into code from
    /// it, and a later DCE that drops an asset must not move the heap under that
    /// code. `None` until then.
    pub reserved_memory_pages: Option<u32>,

    /// Project-wide trait knowledge inherited from `Package` and grown
    /// here by [`crate::monomorphize::monomorphize`], which adds the
    /// instantiation layer once it has materialised the concrete
    /// trait-method instances.
    pub trait_env: std::sync::Arc<TraitEnv>,
}

/// A function `optimize/dae` reshaped, as it was declared to it.
#[derive(Debug, Clone)]
pub struct DaeBase {
    pub name: String,
    pub method_name: Option<String>,
    /// Every parameter name, in declaration order.
    pub params: Vec<String>,
    /// The ones dropped so far.
    pub dropped: IndexSet<String>,
}

/// One field `optimize/sroa_param` projected a parameter through.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SroaParamProjection {
    /// The struct, as `(name, module_source)`.
    pub struct_key: (String, ModuleSource),
    /// The field's declaration index in it.
    pub field_index: u32,
    /// Whether the parameter holds the field as `&mut` rather than by value.
    pub mutable: bool,
}

impl NirPackage {
    /// Default short-string inline threshold (UTF-8 bytes), used for build
    /// paths that skip `optimize` (e.g. `wado dump --nir-lowered`). `optimize`
    /// overrides it per opt level.
    pub const DEFAULT_STRING_INLINE_MAX_BYTES: usize = 4;

    /// The pages, from address 0, of the linear memory the embedded wasm assets
    /// reserve. 0 when no asset is referenced.
    ///
    /// The component has one linear memory, and codegen rewrites each embedded
    /// wasm asset to import it rather than define its own. An asset's own
    /// minimum covers its data segments and, for one built by a toolchain like
    /// Rust's, the stack below them: libm wants 17 pages. The memory must be at
    /// least that large, and the allocator must hand out nothing inside it.
    pub fn wasm_asset_reserved_pages(&self) -> u32 {
        self.reserved_memory_pages
            .expect("DCE resolves the imports before anything reads the reservation")
    }

    /// Fix [`Self::reserved_memory_pages`] from the assets `imports` now
    /// references. `resolve_imports` calls it each time it rebuilds them, and
    /// only the first call sets it: a later one sees a subset of the imports.
    pub fn reserve_asset_memory(&mut self) {
        let referenced: IndexSet<&str> = self
            .imports
            .iter()
            .map(|import| import.namespace.as_str())
            .filter(|namespace| self.wasm_assets.contains_key(*namespace))
            .collect();
        let pages = referenced
            .iter()
            .map(|namespace| self.wasm_assets[*namespace].min_memory_pages())
            .max()
            .unwrap_or(0);
        let pages = u32::try_from(pages).expect("the loader bounds an asset's memory");
        match self.reserved_memory_pages {
            None => self.reserved_memory_pages = Some(pages),
            Some(fixed) => assert!(
                pages <= fixed,
                "[DCE] the imports grew to reserve {pages} pages past the {fixed} already fixed"
            ),
        }
    }

    /// The address the allocator's heap starts at, past the pages the embedded
    /// wasm assets reserve. `builtin::heap_base()` returns it.
    pub fn heap_base(&self) -> i32 {
        i32::try_from(u64::from(self.wasm_asset_reserved_pages()) << DEFAULT_PAGE_SIZE_LOG2)
            .expect("the loader bounds an asset's memory below 2 GiB")
    }

    /// The [`FuncId`] of a `builtin::<name>` callee, or `None` if no such call is
    /// interned in this package. Resolved once (e.g. at a pass's top) so an
    /// optimizer recognizer can identify a builtin call by integer id comparison
    /// against the call node's `func_id` — no per-call name materialization, and
    /// no `store[id]` deref (which a self-recursive callee would double-borrow).
    pub fn builtin_func_id(&self, name: &str) -> Option<FuncId> {
        self.func_id_of(&FunctionRef {
            module_source: ModuleSource::builtin(),
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

    /// The callee descriptor of every function from store position `start` on.
    /// A call site's identity is read by its stamped `func_id`, which indexes
    /// this table, rather than off the call node.
    pub fn callee_descriptors_from(&self, start: usize) -> impl Iterator<Item = FunctionRef> + '_ {
        self.functions[start..].iter().map(|func_rc| {
            let func = func_rc.borrow();
            FunctionRef::from_resolved(&func, func.module_source.clone())
        })
    }

    /// The [`FuncId`]s of the calls that write no struct field: a builtin whose
    /// declared storage reaches nothing it could write but array elements, a
    /// value-copy helper, or a function that never returns. The value-graph
    /// builder reads this so a field version forwards across a loop body that
    /// only calls these. Resolved by `func_id` off the callee's arena record now that
    /// the call node carries no `FunctionRef`. O(functions); a pass computes it
    /// once before its per-function loop.
    pub fn pure_builtin_callee_ids(&self) -> IndexSet<FuncId> {
        let type_table = self.type_table.borrow();
        self.functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                let writes_no_slot = self
                    .builtin_declarations
                    .get(&*f)
                    .is_some_and(BuiltinDeclaration::writes_no_field)
                    || f.is_value_copy()
                    // Bodied only: an extern stub's `return_type` is not an id
                    // this table resolves.
                    || (f.body.is_some() && type_table.is_never(f.return_type));
                writes_no_slot.then(|| f.id.expect("func_id assigned at lower"))
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

    /// Give function `id` a new name, and a method its new method name, moving
    /// the entries keyed by its name along with it. A call names its callee by
    /// `FuncId`, so calls need no rewrite. Where another function already holds
    /// the name, nothing changes and that function is returned: a name states
    /// what a function is, so the two are one.
    pub fn rename_function(
        &mut self,
        id: FuncId,
        name: String,
        method_name: Option<String>,
    ) -> Option<FuncId> {
        use cranelift_entity::EntityRef;
        let mut func = self.functions[id.index()].borrow_mut();
        let old = FunctionRef::from_resolved(&func, func.module_source.clone());
        let mut renamed = old.clone();
        renamed.name = name;
        match (&mut renamed.method_info, method_name) {
            (Some(info), Some(method)) => info.method_name = method,
            (None, None) => {}
            _ => unreachable!("a method is renamed with its method name, and only a method"),
        }
        let new_key = renamed.function_id();
        if let Some(&holder) = self.func_index.get(&new_key) {
            assert_ne!(holder, id, "a rename changes the name");
            return Some(holder);
        }
        let old_key = old.function_id();
        let old_strings_key = (func.module_source.clone(), func.name.clone());
        func.name = renamed.name;
        if let (Some(info), Some(method)) = (&mut func.method_info, renamed.method_info) {
            info.method_name = method.method_name;
        }
        let new_strings_key = (func.module_source.clone(), func.name.clone());
        drop(func);
        assert_eq!(
            self.func_index.shift_remove(&old_key),
            Some(id),
            "a function is indexed by its name"
        );
        self.func_index.insert(new_key, id);
        if let Some(strings) = self.function_strings.shift_remove(&old_strings_key) {
            self.function_strings.insert(new_strings_key, strings);
        }
        self.renamed.push(id);
        None
    }

    /// Retire function `id`, whose callers now call another: it is dead, and
    /// gives up its name, which a function minted later may take.
    pub fn retire_function(&mut self, id: FuncId) {
        use cranelift_entity::EntityRef;
        let (name, method_name) = {
            let mut func = self.functions[id.index()].borrow_mut();
            func.is_dead = true;
            (
                retired_name(&func.name, id.index()),
                func.method_info
                    .as_ref()
                    .map(|m| retired_name(&m.method_name, id.index())),
            )
        };
        assert_eq!(
            self.rename_function(id, name, method_name),
            None,
            "a retired name is unique by the id it carries"
        );
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
    pub fn value_copy_func_ids(&self) -> hashmap::IndexSet<FuncId> {
        self.functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                f.value_copy_type().and(f.id)
            })
            .collect()
    }

    /// Whether the active world declares an `import {interface_name} { … }`
    /// block. Drives the world-shape decisions in codegen, lowering and DCE that
    /// once matched on `target_world` strings, so a new generator-shaped world
    /// needs no new branches. `false` for the synthetic test world and any
    /// unknown one, neither having a registry entry.
    pub fn world_imports_interface(&self, interface_name: &str) -> bool {
        self.world_registry
            .get(&self.target_world)
            .is_some_and(|w| w.imports_interface(interface_name))
    }

    /// Whether the target world is a kiln generator world (imports
    /// [`crate::world_registry::GENERATOR_HOST_INTERFACE`]).
    pub fn is_generator_world(&self) -> bool {
        self.world_imports_interface(GENERATOR_HOST_INTERFACE)
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
