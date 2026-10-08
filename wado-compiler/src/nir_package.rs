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
use crate::name::{
    FunctionId, LocalMethodName, NarrowedParam, param_label, reshaped_name, retired_name,
};
use crate::nir::{
    ClosureFunctor, FuncCell, FuncId, FuncRef, FunctionRef, NirEnum, NirFlags, NirFunction,
    NirGlobal, NirImport, NirStruct, NirTest, NirVariantDecl,
};
use crate::nir_arena::{Body, ExprKind, NodeRef};
use crate::tir::{BuiltinDeclaration, BuiltinDeclarations, TypeId, TypeTable};
use crate::wir_build::component_plan::ComponentPlan;
use crate::world_registry::{self, GENERATOR_HOST_INTERFACE, WorldRegistry};

// The optimizer visits functions on several threads, each holding its own
// function mutably and reading the rest (WEP: Parallel Optimizer).
const _: () = {
    const fn shared_across_threads<T: Send + Sync>() {}
    shared_across_threads::<NirFunction>();
    shared_across_threads::<TypeTable>();
};

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
    pub functions: Vec<FuncRef>,
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
    /// Functions `optimize/sroa_param` minted. A clone may gain callers in a
    /// later round, so its current call sites are not its whole contract.
    /// Identity, not a name test — see the declaration-identity WEP.
    pub sroa_param_clones: IndexSet<FuncId>,
    /// How each function `sroa_param` minted, `dae` or `drve` reshaped, or
    /// `param_spec` cloned from one of those stands against the root it
    /// derives from. Its name is rendered from this alone, so two routes to one
    /// shape arrive at one name. A function absent here is its own root, its
    /// signature as declared.
    pub reshapes: IndexMap<FuncId, Reshape>,
    /// Every function renamed in place, in order, so a cache keyed by store
    /// position can refresh what went stale. Append-only.
    pub renamed: Vec<FuncId>,
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

/// How a function's signature stands against that of its root: the function
/// it derives from that no optimizer pass minted or reshaped.
#[derive(Debug, Clone)]
pub struct Reshape {
    /// The name the shape is spelled against, and what of it that name spells.
    spelled: Spelled,
    /// Every root parameter by name, in declaration order.
    params: Vec<(String, ParamShape)>,
    /// For each parameter the function takes now, its position among the
    /// root's. A name is no key: two parameters may share one (`_`).
    current: Vec<usize>,
    ret: ReturnShape,
}

/// A name a [`Reshape`] is spelled against: the root's own, which spells
/// nothing of it, or a `param_spec` clone's, which spells its source's shape.
#[derive(Debug, Clone)]
struct Spelled {
    name: String,
    method_name: Option<String>,
    params: Vec<ParamShape>,
    ret: ReturnShape,
}

/// What became of one root parameter.
#[derive(Debug, Clone)]
pub enum ParamShape {
    Kept,
    /// Taken as a field instead, projected through each step, outermost first.
    Narrowed(Vec<SroaParamProjection>),
    Dropped,
}

/// What became of the root's return, each later than the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReturnShape {
    Kept,
    /// `sroa_variant_return` returns the variant's cases as one tuple.
    Scalarized,
    /// `drve` returns `()`.
    Dropped,
}

impl Reshape {
    /// `func` as its own root: every parameter kept.
    pub fn root(func: &NirFunction) -> Self {
        let params: Vec<(String, ParamShape)> = func
            .params
            .iter()
            .map(|p| (p.name.clone(), ParamShape::Kept))
            .collect();
        Self {
            spelled: Spelled {
                name: func.name.clone(),
                method_name: func.method_info.as_ref().map(|m| m.method_name.clone()),
                params: vec![ParamShape::Kept; params.len()],
                ret: ReturnShape::Kept,
            },
            current: (0..params.len()).collect(),
            params,
            ret: ReturnShape::Kept,
        }
    }

    /// The record of `func` in `reshapes`, made for it as its own root where it
    /// has none yet.
    pub fn of<'r>(reshapes: &'r mut IndexMap<FuncId, Reshape>, func: &NirFunction) -> &'r mut Self {
        let id = func.id.expect("a function in the store has an id");
        reshapes.entry(id).or_insert_with(|| Self::root(func))
    }

    /// This shape, under a name that already spells all of it: a `param_spec`
    /// clone's.
    #[must_use]
    pub fn respelled(&self, name: String, method_name: Option<String>) -> Self {
        Self {
            spelled: Spelled {
                name,
                method_name,
                params: self.params.iter().map(|(_, s)| s.clone()).collect(),
                ret: self.ret,
            },
            ..self.clone()
        }
    }

    /// The shape of the parameter the function takes at `position` now.
    pub fn shape(&self, position: usize) -> &ParamShape {
        &self.params[self.current[position]].1
    }

    /// [`Self::shape`], to narrow it further.
    pub fn shape_mut(&mut self, position: usize) -> &mut ParamShape {
        &mut self.params[self.current[position]].1
    }

    /// Record what the function's return became.
    pub fn reshape_return(&mut self, ret: ReturnShape) {
        assert!(
            self.ret < ret,
            "a return only moves on: {:?} to {ret:?}",
            self.ret
        );
        self.ret = ret;
    }

    /// Drop the parameters the function takes now at the positions `dead`
    /// marks.
    pub fn drop_params(&mut self, dead: &[bool]) {
        assert_eq!(dead.len(), self.current.len(), "one verdict per parameter");
        for (&root, _) in self.current.iter().zip(dead).filter(|(_, d)| **d) {
            self.params[root].1 = ParamShape::Dropped;
        }
        let mut position = 0;
        self.current.retain(|_| {
            let kept = !dead[position];
            position += 1;
            kept
        });
    }

    /// The name and method name this shape gives a function: what it adds to
    /// the name it is spelled against.
    pub fn names(&self) -> (String, Option<String>) {
        let labels: Vec<String> = self
            .params
            .iter()
            .enumerate()
            .map(|(position, (name, _))| {
                let shared = self
                    .params
                    .iter()
                    .filter(|(other, _)| other == name)
                    .count()
                    > 1;
                param_label(name, shared.then_some(position))
            })
            .collect();
        let mut narrowed: Vec<NarrowedParam<'_>> = Vec::new();
        let mut dropped: Vec<&str> = Vec::new();
        for (((_, shape), spelled), label) in
            self.params.iter().zip(&self.spelled.params).zip(&labels)
        {
            match (spelled, shape) {
                (_, ParamShape::Kept) | (ParamShape::Dropped, ParamShape::Dropped) => {}
                (ParamShape::Kept | ParamShape::Narrowed(_), ParamShape::Dropped) => {
                    dropped.push(label);
                }
                (ParamShape::Kept, ParamShape::Narrowed(path)) => {
                    narrowed.push(NarrowedParam::new(label, path));
                }
                (ParamShape::Narrowed(before), ParamShape::Narrowed(path)) => {
                    assert!(path.starts_with(before), "a narrowing only goes deeper");
                    if path.len() > before.len() {
                        narrowed.push(NarrowedParam::new(label, &path[before.len()..]));
                    }
                }
                (ParamShape::Dropped, ParamShape::Narrowed(_)) => {
                    unreachable!("a dropped parameter stays dropped")
                }
            }
        }
        let ret = if self.ret > self.spelled.ret {
            self.ret
        } else {
            ReturnShape::Kept
        };
        (
            reshaped_name(&self.spelled.name, &narrowed, &dropped, ret),
            self.spelled
                .method_name
                .as_deref()
                .map(|m| reshaped_name(m, &narrowed, &dropped, ret)),
        )
    }
}

impl<'a> NarrowedParam<'a> {
    fn new(param: &'a str, path: &'a [SroaParamProjection]) -> Self {
        Self {
            param,
            fields: path.iter().map(|p| p.field_name.as_str()).collect(),
            form: path.last().expect("a narrowing projects a field").form,
        }
    }
}

/// One field `optimize/sroa_param` projected a parameter through.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SroaParamProjection {
    /// The struct, as `(name, module_source)`.
    pub struct_key: (String, ModuleSource),
    /// The field's declaration index and name in it.
    pub field_index: u32,
    pub field_name: String,
    /// How the parameter holds the field.
    pub form: FieldForm,
}

/// How a parameter `sroa_param` narrowed holds the field it was narrowed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldForm {
    /// By value: the canonical `Box<T>` case, where a reference would only
    /// re-box what the unwrap just removed.
    Value,
    /// As `&T`.
    Shared,
    /// As `&mut T`.
    Mutable,
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
        self.functions.push(FuncCell::new(stub));
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

    /// Give each function its new name, and a method its new method name,
    /// moving the entries keyed by its name along with it. A call names its
    /// callee by `FuncId`, so calls need no rewrite. Every old name is given up
    /// first, so one of these may take a name another gives up. A function
    /// whose new name another already holds is that function, reached by
    /// another route: it takes a retired name instead, and the map returned
    /// sends it to the holder, whose callers its own should become.
    pub fn rename_functions(
        &mut self,
        renames: Vec<(FuncId, String, Option<String>)>,
    ) -> IndexMap<FuncId, FuncId> {
        use cranelift_entity::EntityRef;
        let vacated: Vec<(ModuleSource, String)> = renames
            .iter()
            .map(|(id, _, _)| {
                let func = self.functions[id.index()].borrow();
                let key =
                    FunctionRef::from_resolved(&func, func.module_source.clone()).function_id();
                assert_eq!(
                    self.func_index.swap_remove(&key),
                    Some(*id),
                    "a function is indexed by its name"
                );
                (func.module_source.clone(), func.name.clone())
            })
            .collect();
        let mut merged = IndexMap::default();
        for ((id, name, method_name), old_strings) in renames.into_iter().zip(vacated) {
            let named = self.named(id, name, method_name);
            let named = match self.func_index.get(&named.function_id()) {
                Some(&holder) => {
                    merged.insert(id, holder);
                    let retired = retired_name(&named.name, id.index());
                    let retired_method = named
                        .method_info
                        .as_ref()
                        .map(|m| retired_name(&m.method_name, id.index()));
                    self.named(id, retired, retired_method)
                }
                None => named,
            };
            assert!(
                self.func_index.insert(named.function_id(), id).is_none(),
                "a retired name is unique by the position it carries"
            );
            let mut func = self.functions[id.index()].borrow_mut();
            func.name = named.name;
            if let (Some(info), Some(method)) = (&mut func.method_info, named.method_info) {
                info.method_name = method.method_name;
            }
            let new_strings = (func.module_source.clone(), func.name.clone());
            drop(func);
            if let Some(strings) = self.function_strings.swap_remove(&old_strings) {
                self.function_strings.insert(new_strings, strings);
            }
            self.renamed.push(id);
        }
        merged
    }

    /// Name each function in `ids` for its [`Reshape`]. One whose name another
    /// already holds is that function, reached by another route: its callers
    /// call the holder, and it is retired. Returns the store positions of the
    /// functions whose bodies changed.
    pub fn rename_reshaped(&mut self, ids: impl IntoIterator<Item = FuncId>) -> Vec<usize> {
        use cranelift_entity::EntityRef;
        let renames = ids
            .into_iter()
            .map(|id| {
                let (name, method_name) = self.reshapes[&id].names();
                (id, name, method_name)
            })
            .collect();
        let merged = self.rename_functions(renames);
        if merged.is_empty() {
            return Vec::new();
        }
        for (&id, &holder) in &merged {
            let func = self.functions[id.index()].borrow();
            let params: Vec<TypeId> = func.params.iter().map(|p| p.type_id).collect();
            assert!(
                self.answers_calls(holder, &params, func.return_type),
                "[NIR] {} was reshaped into a name that another signature holds",
                func.name
            );
        }
        let mut touched = Vec::new();
        for (i, func_rc) in self.functions.iter().enumerate() {
            if let Some(body) = func_rc.borrow_mut().body.as_mut()
                && retarget_calls(body, &merged)
            {
                touched.push(i);
            }
        }
        for global in &mut self.globals {
            retarget_calls(global.init.slot_expr_mut().body_mut(), &merged);
        }
        for &id in merged.keys() {
            self.retire_function(id);
        }
        touched
    }

    /// Function `id`'s descriptor under `name` and `method_name`.
    fn named(&self, id: FuncId, name: String, method_name: Option<String>) -> FunctionRef {
        use cranelift_entity::EntityRef;
        let func = self.functions[id.index()].borrow();
        let mut named = FunctionRef::from_resolved(&func, func.module_source.clone());
        named.name = name;
        match (&mut named.method_info, method_name) {
            (Some(info), Some(method)) => info.method_name = method,
            (None, None) => {}
            _ => unreachable!("a method is renamed with its method name, and only a method"),
        }
        named
    }

    /// Whether a call passing exactly the types `params` and reading `ret` can
    /// call function `id`, compared as types rather than as table slots. A
    /// return `sroa_variant_return` scalarized answers for the variant it came
    /// from: that pass reboxes a call reading the variant.
    pub fn answers_calls(&self, id: FuncId, params: &[TypeId], ret: TypeId) -> bool {
        use cranelift_entity::EntityRef;
        let func = self.functions[id.index()].borrow();
        let types = self.type_table.borrow();
        let same = |x: TypeId, y: TypeId| types.type_key(x) == types.type_key(y);
        (same(func.return_type, ret) || func.scalarized_from.is_some_and(|v| same(v, ret)))
            && func.params.len() == params.len()
            && func
                .params
                .iter()
                .zip(params)
                .all(|(p, &t)| same(p.type_id, t))
    }

    /// Retire function `id`, which `rename_functions` merged into another
    /// whose callers its own now are: it is dead, and nothing walks its body.
    pub fn retire_function(&mut self, id: FuncId) {
        use cranelift_entity::EntityRef;
        let mut func = self.functions[id.index()].borrow_mut();
        func.is_dead = true;
        func.body = None;
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

/// Point every call of a key of `merged` at its value.
fn retarget_calls(body: &mut Body, merged: &IndexMap<FuncId, FuncId>) -> bool {
    let mut calls = Vec::new();
    body.for_each_reachable_node(|node| {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Call { func_id, .. } = &body.exprs[id].kind
            && merged.contains_key(func_id)
        {
            calls.push(id);
        }
    });
    for &id in &calls {
        if let ExprKind::Call { func_id, .. } = &mut body.exprs[id].kind {
            *func_id = merged[&*func_id];
        }
    }
    !calls.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{FieldForm, ParamShape, Reshape, ReturnShape, Spelled, SroaParamProjection};
    use crate::module_source::ModuleSource;

    fn reshape(params: &[&str]) -> Reshape {
        Reshape {
            spelled: Spelled {
                name: "f".to_string(),
                method_name: None,
                params: vec![ParamShape::Kept; params.len()],
                ret: ReturnShape::Kept,
            },
            params: params
                .iter()
                .map(|p| (p.to_string(), ParamShape::Kept))
                .collect(),
            current: (0..params.len()).collect(),
            ret: ReturnShape::Kept,
        }
    }

    fn narrow_as(r: &mut Reshape, position: usize, field: &str, form: FieldForm) {
        let projection = SroaParamProjection {
            struct_key: ("S".to_string(), ModuleSource::builtin()),
            field_index: 0,
            field_name: field.to_string(),
            form,
        };
        let shape = r.shape_mut(position);
        match shape {
            ParamShape::Kept => *shape = ParamShape::Narrowed(vec![projection]),
            ParamShape::Narrowed(path) => path.push(projection),
            ParamShape::Dropped => unreachable!(),
        }
    }

    fn narrow(r: &mut Reshape, position: usize, field: &str) {
        narrow_as(r, position, field, FieldForm::Value);
    }

    #[test]
    fn each_way_of_holding_a_field_gives_its_own_name() {
        let names: Vec<String> = [FieldForm::Value, FieldForm::Shared, FieldForm::Mutable]
            .into_iter()
            .map(|form| {
                let mut r = reshape(&["a"]);
                narrow_as(&mut r, 0, "x", form);
                r.names().0
            })
            .collect();
        assert_eq!(names, ["f$sroa[a.x]", "f$sroa[&a.x]", "f$sroa[&mut(a.x)]"]);
    }

    #[test]
    fn a_reshaped_return_is_part_of_the_name() {
        let mut r = reshape(&["a", "b"]);
        narrow(&mut r, 0, "x");
        assert_eq!(r.names().0, "f$sroa[a.x]");
        r.reshape_return(ReturnShape::Scalarized);
        assert_eq!(r.names().0, "f$sroa[a.x,return]");
        r.drop_params(&[false, true]);
        r.reshape_return(ReturnShape::Dropped);
        assert_eq!(r.names().0, "f$sroa[a.x]$dae[b,return]");
    }

    /// A `param_spec` clone's name spells its source's shape already, so only
    /// what is reshaped after the clone is added to it.
    #[test]
    fn a_respelled_shape_names_only_what_follows() {
        let mut r = reshape(&["a", "b"]);
        narrow(&mut r, 0, "x");
        let mut spec = r.respelled("f$sroa[a.x]$spec0".to_string(), None);
        assert_eq!(spec.names().0, "f$sroa[a.x]$spec0");
        narrow(&mut spec, 0, "y");
        spec.drop_params(&[false, true]);
        assert_eq!(spec.names().0, "f$sroa[a.x]$spec0$sroa[a.y]$dae[b]");
        assert!(matches!(spec.shape(0), ParamShape::Narrowed(path) if path.len() == 2));
    }

    #[test]
    fn a_drop_finds_its_parameter_by_position_not_by_name() {
        let mut r = reshape(&["_", "a", "_"]);
        r.drop_params(&[false, false, true]);
        assert_eq!(r.names().0, "f$dae[_#2]");
        narrow(&mut r, 1, "x");
        assert_eq!(r.names().0, "f$sroa[a.x]$dae[_#2]");
    }

    #[test]
    fn two_routes_to_one_shape_give_one_name() {
        let mut narrowed_first = reshape(&["a", "b"]);
        narrow(&mut narrowed_first, 0, "x");
        narrowed_first.drop_params(&[false, true]);

        let mut dropped_first = reshape(&["a", "b"]);
        dropped_first.drop_params(&[false, true]);
        narrow(&mut dropped_first, 0, "x");

        assert_eq!(narrowed_first.names(), dropped_first.names());
        assert_eq!(narrowed_first.names().0, "f$sroa[a.x]$dae[b]");
    }
}
