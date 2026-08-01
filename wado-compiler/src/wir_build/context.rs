//! WIR builder context — accumulates types, functions, and other module-level
//! entries during the `tir_to_wir` translation, then produces a final `WirPackage`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::MangledName;

use crate::module_source::ModuleSource;
use crate::name::StructName;
use crate::nir::NirFunction;
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};
use crate::wir::{
    CanonicalIntrinsic, WirComponent, WirData, WirExport, WirFuncId, WirFuncType, WirFunction,
    WirGlobal, WirImport, WirImportDesc, WirName, WirNames, WirPackage, WirType, WirTypeDef,
    WirTypeId,
};

/// Base offset for defined function `WirFuncId` indices.
/// Import functions use indices 0..N, defined functions use `DEFINED_FUNC_BASE + 0..M`.
/// This prevents index collisions when `ensure_canonical` adds imports after
/// defined functions have already been registered with their `WirFuncId`.
pub const DEFINED_FUNC_BASE: u32 = 0x8000_0000;

/// A declared type that has no WIR registration yet.
///
/// Expected during `register_types`, where the caller takes `placeholder` and
/// [`super::types::fixup_abstract_struct_fields`] repairs it later. A bug
/// anywhere after that.
pub struct UnregisteredType {
    /// What was looked for, for the panic message.
    description: String,
    /// The abstract ref the fixup pass recognises as unresolved.
    placeholder: WirType,
}

impl UnregisteredType {
    fn struct_ref(description: String) -> Self {
        Self::new(description, crate::wir::WirAbstractHeapType::Struct)
    }

    fn array_ref(description: String) -> Self {
        Self::new(description, crate::wir::WirAbstractHeapType::Array)
    }

    /// An enum is an i32 discriminant, so the placeholder is already the right
    /// representation and only loses the enum's WIR identity — nothing for the
    /// fixup pass to repair, hence no abstract ref.
    fn enum_i32(description: String) -> Self {
        Self {
            description,
            placeholder: WirType::I32,
        }
    }

    fn new(description: String, heap_type: crate::wir::WirAbstractHeapType) -> Self {
        Self {
            description,
            placeholder: WirType::AbstractRef {
                heap_type,
                nullable: false,
            },
        }
    }
}

/// Builder context for the `tir_to_wir` translation.
///
/// Accumulates all WIR entities and provides lookup maps for resolving
/// type and function references during translation.
pub struct WirContext<'a> {
    /// Reference to the linked package data.
    pub package: &'a NirPackage,

    /// All type definitions in registration order.
    pub types: Vec<WirTypeDef>,
    /// Map from fully-qualified type name to `WirTypeId`.
    pub type_map: IndexMap<String, WirTypeId>,
    /// Map from `StructName` to `WirTypeId` (for struct lookup by qualified name).
    pub struct_type_map: IndexMap<StructName, WirTypeId>,
    /// Map from element `TypeId` to `WirTypeId` for raw GC array types.
    pub array_type_map: IndexMap<TypeId, WirTypeId>,
    /// Map from element type name to `WirTypeId` (dedup for arrays).
    pub array_type_by_name: IndexMap<String, WirTypeId>,
    /// Map from tuple element `TypeIds` to `WirTypeId`.
    pub tuple_type_map: IndexMap<Vec<TypeId>, WirTypeId>,
    /// Map from variant qualified name to `WirTypeId`.
    pub variant_type_map: IndexMap<String, WirTypeId>,
    /// Variant case type info: case WIR type index → (variant WIR type index, case index).
    pub variant_case_info: IndexMap<u32, (u32, u32)>,

    /// All function definitions (with optional bodies).
    pub functions: Vec<WirFunction>,
    /// Map from fully-qualified function name to `WirFuncId`.
    pub func_map: IndexMap<crate::name::MangledName, WirFuncId>,
    /// Map from a defined function's canonical [`crate::nir::FuncId`] to its
    /// `WirFuncId`. Lets a stamped call resolve its target by id, skipping the
    /// name reconstruction in `resolve_function_ref` (the name path stays for
    /// extern / unstamped callees).
    pub funcid_map: IndexMap<crate::nir::FuncId, WirFuncId>,
    /// Function type index for each function (into types vec).
    pub func_type_ids: Vec<WirTypeId>,

    /// Core module imports.
    pub imports: Vec<WirImport>,
    /// Number of imported functions (these come before defined functions in Wasm).
    pub import_func_count: u32,
    /// Map from import name to function index (for resolving call targets).
    pub import_func_map: IndexMap<String, WirFuncId>,

    /// Global variables.
    pub globals: Vec<WirGlobal>,
    /// Map from qualified global name to index in `globals`.
    pub global_map: IndexMap<String, u32>,
    /// Exports.
    pub exports: Vec<WirExport>,
    /// Data segments (string and bytes literals).
    pub data: Vec<WirData>,
    /// Packed-array data dedup: byte payload → passive data segment index.
    /// Shared by `String` and `List<u8>` literal `repr`s, which converge on the
    /// same `array.new_data` segment when their bytes match.
    pub packed_data_map: IndexMap<Vec<u8>, u32>,
    /// Fully-unqualified names of globals with
    /// [`crate::nir::NirGlobal::prefer_fixed_string_repr`] set, precomputed
    /// once so [`FunctionTranslator`](super::translate::FunctionTranslator)
    /// doesn't rescan `package.globals` per `GlobalVarSet`.
    pub eager_repr_globals: IndexSet<String>,
    /// Name section entries.
    pub names: WirNames,

    /// Map from function signature string to canonical closure info.
    /// Key: stringified signature (e.g., "(i32, i32) -> i32")
    /// Value: (`canonical_fn_type_id`, `canonical_closure_struct_type_id`,
    ///         `is_inspectable`)
    /// `is_inspectable` is the per-`(N, Ret)` gate that decides whether
    /// the canonical struct carries the inspect / `inspect_alt` vtable
    /// slots. See `inspectable_fn_dispatch`.
    pub canonical_closure_types: IndexMap<String, (WirTypeId, WirTypeId, bool)>,
    /// Map from closure `(module_source, functor_id)` to the per-functor
    /// canonical wrapper triple. Keyed by module source + functor ID
    /// because functor IDs are per-module, not globally unique.
    pub closure_wrapper_funcs: IndexMap<(ModuleSource, u32), ClosureWrapperFuncs>,
    /// Counter for canonical closure type naming.
    pub canonical_closure_counter: u32,
    /// Set of `(arity, return_type)` signatures whose
    /// `Fn^Inspect` / `Fn^InspectAlt` dispatch stub survived DCE.
    /// Computed once at WIR build start by scanning
    /// `package.functions` for surviving
    /// `FunctionKind::FnCanonicalDispatch` entries. Drives the per-
    /// `(N, Ret)` gate that decides whether `CanonicalClosure_K`
    /// carries `inspect` / `inspect_alt` vtable slots — closures
    /// whose `(N, Ret)` is unreachable here keep the slim
    /// `{ env, func }` shape so production builds that never inspect
    /// closures pay nothing.
    pub inspectable_fn_dispatch: IndexSet<(usize, TypeId)>,
    /// Shared inspectable supertype `$canonical_inspectable_base`
    /// (`{ env, inspect, inspect_alt }`). Lazily created on the first
    /// inspectable `CanonicalClosure_K` registration; the
    /// `Fn<N, Ret>^Inspect` dispatch stub `ref.cast`s `self` to this
    /// type so any inspectable closure value reaches the same `inspect`
    /// / `inspect_alt` slot positions regardless of its parameter
    /// types. Without this shared supertype the dispatch stub would
    /// have to pick one specific `CanonicalClosure_K` per `(arity,
    /// return_type)` pair and trap whenever a runtime value belongs to
    /// a different per-signature canonical struct sharing the same
    /// `(arity, return_type)`.
    pub canonical_inspectable_base_type_id: Option<WirTypeId>,

    /// Available WASI function names (computed during component generation).
    pub available_wasi_funcs: IndexSet<String>,

    /// Map from `ModuleSource` to wasm module name (e.g., "mem").
    /// Functions/globals from these modules are extracted into separate wasm core modules.
    pub wasm_module_sources: IndexMap<ModuleSource, String>,

    /// Pending function bodies: (function index in self.functions, `NirFunction` ref, `TypeTable` ref)
    pub pending_bodies: Vec<PendingFunctionBody>,

    /// CM canonical imports registered lazily by WIR synthesis functions via `ensure_canonical`.
    /// Key: structured canonical intrinsic (e.g., `FutureNew(Some(S32))`).
    /// Value: the `WirFuncId` for the registered import.
    pub needed_canonicals: IndexMap<CanonicalIntrinsic, WirFuncId>,

    /// Map of `(function_name, module_source)` for functions whose TIR
    /// `return_abi` is `MultiValue`. Names alone are not unique across
    /// modules (e.g. two `make_pair` in different `.wado` files), so the
    /// pair is what the call-site translator queries.
    /// Value is the callee's per-result `(field_name, type_id)` in
    /// declaration order — `("0", i32)` / `("1", i32)` for tuple returns,
    /// `("x", i32)` / `("y", i32)` for user-struct returns. The
    /// call-site translator uses this to build named split locals
    /// without re-deriving the aggregate shape.
    /// Computed from `package.functions` at WIR-build start.
    pub multi_value_return_funcs: IndexMap<(String, ModuleSource), Vec<(String, TypeId)>>,
    /// Unresolved `Type^Trait::method` calls (unsatisfied trait bounds),
    /// collected rather than trapping; the driver reports them and bails.
    pub trait_bound_violations: Vec<crate::wir::TraitBoundViolation>,
}

/// A function body that needs to be translated from TIR to WIR.
pub struct PendingFunctionBody {
    /// Index into WirContext.functions
    pub wir_func_index: usize,
    /// The TIR function to translate
    pub tir_func: Rc<RefCell<NirFunction>>,
    /// The type table for this function's module
    pub type_table: Rc<RefCell<TypeTable>>,
}

/// Scan `package.functions` and return the set of `Fn<arity, ret>`
/// signatures whose `Fn^Inspect::inspect` or `Fn^InspectAlt::inspect_alt`
/// impl is reachable.
///
/// Auto-derived dispatch stubs for `Fn<arity, return_type>^Inspect`
/// / `^InspectAlt` are produced per-module by `synthesize_traits`
/// and tagged `FunctionKind::FnCanonicalDispatch`. After DCE prunes
/// unreachable functions, the survivors here drive the per-
/// `(N, Ret)` schema gate so canonical closures whose signature is
/// never inspected stay slim.
fn compute_inspectable_fn_dispatch(package: &NirPackage) -> IndexSet<(usize, TypeId)> {
    let mut set: IndexSet<(usize, TypeId)> = IndexSet::default();
    for func_rc in &package.functions {
        let Ok(func) = func_rc.try_borrow() else {
            continue;
        };
        // `dce` marks unreachable functions dead in place (Phase 4) rather than
        // removing them, so a dead `FnCanonicalDispatch` stub lingers; gate on
        // liveness, not mere presence.
        if func.is_dead {
            continue;
        }
        if let Some((_, arity, return_type)) = func.fn_canonical_dispatch() {
            set.insert((arity, return_type));
        }
    }
    set
}

/// Per-functor wrapper functions stored in `CanonicalClosure_K` vtable
/// slots. Populated by `register_closure_wrappers` and consumed by
/// `translate_closure_to_canonical` to initialise the canonical
/// closure struct.
///
/// `inspect` and `inspect_alt` are `None` when the per-`(N, Ret)`
/// gate (`WirContext::inspectable_fn_dispatch`) reports the
/// signature as unreachable — in that case `CanonicalClosure_K` uses
/// the slim `{ env, func }` schema and `translate_closure_to_canonical`
/// only emits two `struct.new` operands.
#[derive(Clone, Debug)]
pub struct ClosureWrapperFuncs {
    /// `__closure_wrapper_N(env, args...) -> ret` — refcasts env to
    /// `&__Closure_N` and forwards to `__call`.
    pub call: WirFuncId,
    /// `__closure_inspect_wrapper_N(env, formatter)` — refcasts both
    /// args and forwards to `__Closure_N^Inspect::inspect`. `None`
    /// when the functor's `(N, Ret)` is not inspectable.
    pub inspect: Option<WirFuncId>,
    /// `__closure_inspect_alt_wrapper_N(env, formatter)` — refcasts
    /// both args and forwards to `__Closure_N^InspectAlt::inspect_alt`.
    /// `None` when the functor's `(N, Ret)` is not inspectable.
    pub inspect_alt: Option<WirFuncId>,
}

impl<'a> WirContext<'a> {
    /// Create a new `WirContext` from a `NirPackage`.
    pub fn new(package: &'a NirPackage) -> Self {
        // String and bytes literals live on `package`; `register_literal_data`
        // reads them directly and dedups into `data` via `packed_data_map`, so
        // the context keeps no separate copy.

        // Compute the per-`(N, Ret)` inspectable gate. After DCE,
        // `package.functions` only contains reachable functions, so the
        // surviving auto-derived `Fn<arity, ret>^Inspect / InspectAlt`
        // impls tell us exactly which signatures need vtable slots in
        // `CanonicalClosure_K`. Programs that don't inspect closures
        // observe an empty set here — every canonical closure stays
        // slim `{ env, func }`.
        let inspectable_fn_dispatch = compute_inspectable_fn_dispatch(package);

        // Pre-compute the map of multi-value-return functions (set by the
        // TIR `optimize::multi_value_return` pass). The translator queries
        // this map at call sites to decide between `LocalSet` (single
        // result) and `MultiValueLocalBind` (split into N locals), and to
        // get the per-result `(field_name, type_id)` info for naming the
        // split locals. Keyed by `(name, module_source)` because plain
        // names are not unique across modules.
        let multi_value_return_funcs: IndexMap<(String, ModuleSource), Vec<(String, TypeId)>> =
            package
                .functions
                .iter()
                .filter_map(|f| {
                    let f = f.try_borrow().ok()?;
                    if let crate::nir::ReturnAbi::MultiValue {
                        result_types,
                        field_names,
                    } = &f.return_abi
                    {
                        let pairs: Vec<(String, TypeId)> = field_names
                            .iter()
                            .cloned()
                            .zip(result_types.iter().copied())
                            .collect();
                        Some(((f.name.clone(), f.module_source.clone()), pairs))
                    } else {
                        None
                    }
                })
                .collect();

        Self {
            package,
            types: Vec::new(),
            type_map: IndexMap::default(),
            struct_type_map: IndexMap::default(),
            array_type_map: IndexMap::default(),
            array_type_by_name: IndexMap::default(),
            tuple_type_map: IndexMap::default(),
            variant_type_map: IndexMap::default(),
            variant_case_info: IndexMap::default(),
            functions: Vec::new(),
            func_map: IndexMap::default(),
            funcid_map: IndexMap::default(),
            func_type_ids: Vec::new(),
            imports: Vec::new(),
            import_func_count: 0,
            import_func_map: IndexMap::default(),
            globals: Vec::new(),
            global_map: IndexMap::default(),
            exports: Vec::new(),
            data: Vec::new(),
            packed_data_map: IndexMap::default(),
            eager_repr_globals: package
                .globals
                .iter()
                .filter(|g| g.prefer_fixed_string_repr)
                .map(|g| g.name.clone())
                .collect(),
            names: WirNames {
                module_name: Some(package.module_name.clone()),
                ..WirNames::default()
            },
            canonical_closure_types: IndexMap::default(),
            closure_wrapper_funcs: IndexMap::default(),
            canonical_closure_counter: 0,
            inspectable_fn_dispatch,
            canonical_inspectable_base_type_id: None,
            wasm_module_sources: IndexMap::<ModuleSource, String>::default(),
            available_wasi_funcs: IndexSet::default(),
            pending_bodies: Vec::new(),
            needed_canonicals: IndexMap::default(),
            multi_value_return_funcs,
            trait_bound_violations: Vec::new(),
        }
    }

    /// Register a type definition and return its `WirTypeId`.
    pub fn register_type(&mut self, fq: String, typedef: WirTypeDef) -> WirTypeId {
        // Dedup: if the same fq name is already registered, return the existing type.
        // This prevents cm_binding synthesis and WIR build from creating duplicate
        // struct types for the same logical type (e.g., tuple types that appear in
        // both the entry module and binding functions).
        if let Some(existing) = self.type_map.get(&fq) {
            return existing.clone();
        }
        let index = u32::try_from(self.types.len()).expect("too many types");
        let fq_rc: Rc<str> = Rc::from(fq.as_str());
        let type_id = WirTypeId::new(index, fq_rc);
        self.type_map.insert(fq, type_id.clone());
        self.types.push(typedef);
        type_id
    }

    /// Register a function type definition and return its `WirTypeId`.
    pub fn register_func_type(
        &mut self,
        fq: String,
        params: Vec<WirType>,
        results: Vec<WirType>,
    ) -> WirTypeId {
        // Check if already registered
        if let Some(existing) = self.type_map.get(&fq) {
            return existing.clone();
        }
        self.register_type(
            fq.clone(),
            WirTypeDef::Func(WirFuncType {
                name: WirName { fq },
                params,
                results,
            }),
        )
    }

    /// Register a function import and return its `WirFuncId`.
    pub fn register_import_func(
        &mut self,
        module: String,
        field: String,
        type_id: WirTypeId,
        name: WirName,
    ) -> WirFuncId {
        let func_idx = self.import_func_count;
        self.import_func_count += 1;
        let fq = name.fq.clone();
        let fq_rc: Rc<str> = Rc::from(fq.as_str());
        let func_id = WirFuncId::new(func_idx, fq_rc);

        self.imports.push(WirImport {
            module,
            field,
            desc: WirImportDesc::Func { type_id, name },
        });
        self.import_func_map.insert(fq.clone(), func_id.clone());
        self.func_map.insert(MangledName::new(fq), func_id.clone());
        func_id
    }

    /// Register a defined function (with body) and return its `WirFuncId`.
    /// `nir_id` is the source function's canonical id (`None` for synthesized
    /// functions with no NIR origin); when present it indexes `funcid_map` so a
    /// stamped call resolves by id.
    pub fn register_function(
        &mut self,
        func: WirFunction,
        nir_id: Option<crate::nir::FuncId>,
    ) -> WirFuncId {
        let func_idx =
            DEFINED_FUNC_BASE + u32::try_from(self.functions.len()).expect("too many funcs");
        let fq = func.name.fq.clone();
        let fq_rc: Rc<str> = Rc::from(fq.as_str());
        let func_id = WirFuncId::new(func_idx, fq_rc);
        self.func_map.insert(MangledName::new(fq), func_id.clone());
        if let Some(nir_id) = nir_id {
            self.funcid_map.insert(nir_id, func_id.clone());
        }
        self.func_type_ids.push(func.type_id.clone());
        self.functions.push(func);
        func_id
    }

    /// Register a packed-array byte payload (a `String` / `List<u8>` literal's
    /// `repr`) and return its passive data segment index, deduped by content.
    pub fn register_packed_data(&mut self, b: &[u8]) -> u32 {
        if let Some(&idx) = self.packed_data_map.get(b) {
            return idx;
        }
        let idx = u32::try_from(self.data.len()).expect("too many data segments");
        self.data.push(WirData {
            bytes: b.to_vec(),
            offset: None, // passive segment
        });
        self.packed_data_map.insert(b.to_vec(), idx);
        idx
    }

    /// Build a string key for canonical closure type lookup.
    pub fn canonical_closure_key(params: &[WirType], results: &[WirType]) -> String {
        format!(
            "({}) -> ({})",
            params
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            results
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// Get or create a canonical closure type pair (func type + closure struct) for a
    /// function signature. Returns `(fn_type_id, closure_struct_type_id)`.
    ///
    /// The struct schema is the vtable shape required by trait dispatch on
    /// `Fn<N, Ret>` values held behind function parameters, struct fields,
    /// or globals (the indirect-call case). Per WEP: Inspect (Debug
    /// Output) > Closure Inspect via Runtime Dispatch, the inspectable
    /// layout puts `env` + the two callback slots first so they form the
    /// Wasm GC subtype prefix shared with `$canonical_inspectable_base`,
    /// and the per-signature `func` slot comes last:
    ///
    /// ```text
    /// CanonicalClosure_K {
    ///     env:         (ref null struct),
    ///     inspect:     (ref $canonical_callback_fn),
    ///     inspect_alt: (ref $canonical_callback_fn),
    ///     func:        (ref $canonical_fn_K),
    /// }
    /// ```
    ///
    /// `$canonical_callback_fn = (env, formatter) -> ()` is shared across
    /// all signatures: the `env`/`formatter` slots use abstract `(ref null
    /// struct)` so a single function type covers every closure literal.
    /// Each per-literal wrapper refcasts both args back to its concrete
    /// `&__Closure_N` and `&Formatter` before forwarding.
    ///
    /// Inspectable canonical structs share the supertype
    /// `$canonical_inspectable_base` (built lazily via
    /// [`Self::get_or_create_canonical_inspectable_base`]); per-signature
    /// canonical structs add a typed `func` field at the end. The
    /// `Fn<N, Ret>^Inspect` dispatch stub casts to that base, so any
    /// inspectable closure value reaches the shared `inspect` /
    /// `inspect_alt` slots regardless of its parameter types — the only
    /// requirement is that the field layout `{ env, inspect, inspect_alt,
    /// func }` is identical across subtypes (Wasm GC `sub` constraint).
    ///
    /// Non-inspectable signatures keep the slim `{ env, func }` layout
    /// without any supertype: the dispatch stub is never synthesised for
    /// them, so the base type is irrelevant.
    pub fn get_or_create_canonical_closure_type(
        &mut self,
        param_wirs: Vec<WirType>,
        result_wirs: Vec<WirType>,
        is_inspectable: bool,
    ) -> (WirTypeId, WirTypeId) {
        let key = Self::canonical_closure_key(&param_wirs, &result_wirs);
        if let Some((fn_id, struct_id, _)) = self.canonical_closure_types.get(&key) {
            return (fn_id.clone(), struct_id.clone());
        }

        let id = self.canonical_closure_counter;
        self.canonical_closure_counter += 1;

        // Create canonical function type: (ref null struct, params...) -> results
        // The env param must be nullable to accept any struct ref subtype.
        let abstract_struct_nullable = WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Struct,
            nullable: true,
        };
        let mut fn_params = vec![abstract_struct_nullable.clone()];
        fn_params.extend(param_wirs.iter().cloned());

        let fn_type_fq = format!("functype/$canonical_closure_fn_{id}");
        let fn_type_id = self.register_func_type(fn_type_fq, fn_params, result_wirs);

        // Build the closure struct fields. Inspectable layout puts the
        // env + vtable slots first so they form a Wasm GC subtype prefix
        // shared with `$canonical_inspectable_base`; the per-signature
        // `func` field comes last. Non-inspectable layout is the slim
        // `{ env, func }` (no shared supertype).
        use crate::wir::{WirField, WirMeta, WirName, WirStructType};
        let mut fields = vec![WirField {
            name: "env".to_string(),
            ty: abstract_struct_nullable,
            mutable: false,
        }];
        let supertype = if is_inspectable {
            let callback_fn_type_id = self.get_or_create_canonical_callback_fn_type();
            fields.push(WirField {
                name: "inspect".to_string(),
                ty: WirType::Ref {
                    type_id: callback_fn_type_id.clone(),
                    nullable: false,
                },
                mutable: false,
            });
            fields.push(WirField {
                name: "inspect_alt".to_string(),
                ty: WirType::Ref {
                    type_id: callback_fn_type_id,
                    nullable: false,
                },
                mutable: false,
            });
            Some(self.get_or_create_canonical_inspectable_base())
        } else {
            None
        };
        fields.push(WirField {
            name: "func".to_string(),
            ty: WirType::Ref {
                type_id: fn_type_id.clone(),
                nullable: false,
            },
            mutable: false,
        });

        let struct_fq = format!("canonical//CanonicalClosure_{id}");
        let struct_type_id = self.register_type(
            struct_fq.clone(),
            WirTypeDef::Struct(WirStructType {
                name: WirName { fq: struct_fq },
                fields,
                meta: WirMeta::default(),
                generic_origin: None,
                newtype_origin: None,
                supertype,
            }),
        );

        self.canonical_closure_types.insert(
            key,
            (fn_type_id.clone(), struct_type_id.clone(), is_inspectable),
        );

        (fn_type_id, struct_type_id)
    }

    /// Lazily create (or fetch) the shared inspectable closure supertype
    /// `$canonical_inspectable_base = (struct env inspect inspect_alt)`.
    ///
    /// All inspectable per-signature `CanonicalClosure_K` declare this
    /// type as their supertype, so any inspectable closure value can be
    /// `ref.cast` to it. The `Fn<N, Ret>^Inspect` dispatch stub uses
    /// exactly this cast — independent of the `(N, Ret)` pair — and
    /// reads `inspect` / `inspect_alt` from the base layout.
    pub fn get_or_create_canonical_inspectable_base(&mut self) -> WirTypeId {
        if let Some(id) = &self.canonical_inspectable_base_type_id {
            return id.clone();
        }
        let callback_fn_type_id = self.get_or_create_canonical_callback_fn_type();
        let abstract_struct_nullable = WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Struct,
            nullable: true,
        };
        use crate::wir::{WirField, WirMeta, WirName, WirStructType};
        let fields = vec![
            WirField {
                name: "env".to_string(),
                ty: abstract_struct_nullable,
                mutable: false,
            },
            WirField {
                name: "inspect".to_string(),
                ty: WirType::Ref {
                    type_id: callback_fn_type_id.clone(),
                    nullable: false,
                },
                mutable: false,
            },
            WirField {
                name: "inspect_alt".to_string(),
                ty: WirType::Ref {
                    type_id: callback_fn_type_id,
                    nullable: false,
                },
                mutable: false,
            },
        ];
        let fq = "canonical//CanonicalInspectableBase".to_string();
        let id = self.register_type(
            fq.clone(),
            WirTypeDef::Struct(WirStructType {
                name: WirName { fq },
                fields,
                meta: WirMeta::default(),
                generic_origin: None,
                newtype_origin: None,
                supertype: None,
            }),
        );
        self.canonical_inspectable_base_type_id = Some(id.clone());
        id
    }

    /// Func type for the inspect / `inspect_alt` vtable slots:
    /// `(env: ref null struct, formatter: ref null struct) -> ()`.
    ///
    /// Both args use abstract `(ref null struct)` so a single fn type
    /// is compatible with every per-functor wrapper. The wrappers
    /// refcast `env` to the concrete `&__Closure_N` and `formatter` to
    /// `&Formatter` internally before forwarding.
    pub fn get_or_create_canonical_callback_fn_type(&mut self) -> WirTypeId {
        let fq = "functype/$canonical_callback_fn".to_string();
        if let Some(id) = self.type_map.get(&fq) {
            return id.clone();
        }
        let abstract_struct_nullable = WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Struct,
            nullable: true,
        };
        self.register_func_type(
            fq,
            vec![abstract_struct_nullable.clone(), abstract_struct_nullable],
            vec![],
        )
    }

    /// Register a CM canonical import lazily and return its `WirFuncId`.
    ///
    /// If the canonical has already been registered, returns the existing `WirFuncId`.
    /// Called by WIR synthesis functions (`emit_stream_read`, `emit_waitable_set_new`, etc.)
    /// to declare the canonical imports they need without going through TIR imports or DCE.
    ///
    /// The import name is derived from `CanonicalIntrinsic::import_name()`.
    pub fn ensure_canonical(
        &mut self,
        intrinsic: CanonicalIntrinsic,
        params: Vec<WirType>,
        results: Vec<WirType>,
    ) -> WirFuncId {
        let name = intrinsic.import_name();
        let key = MangledName::wasi_import(&name);
        if let Some(func_id) = self.func_map.get(&key) {
            return func_id.clone();
        }
        let type_fq = format!("functype//wasi/{name}");
        let type_id = self.register_func_type(type_fq, params, results);
        let wir_name = WirName {
            fq: key.into_string(),
        };
        let func_id = self.register_import_func("wasi".to_string(), name, type_id, wir_name);
        self.needed_canonicals.insert(intrinsic, func_id.clone());
        func_id
    }

    /// A non-nullable reference to a registered WIR type.
    fn ref_to(type_id: &WirTypeId) -> WirType {
        WirType::Ref {
            type_id: type_id.clone(),
            nullable: false,
        }
    }

    /// Find a registered tuple whose elements have the same WIR types as
    /// `elements`, ignoring `TypeId` identity. CM binding synthesis interns its
    /// own `TypeId`s for element types that already have a registered tuple.
    fn find_tuple_type_by_element_wir_types(
        &self,
        type_table: &TypeTable,
        elements: &[TypeId],
    ) -> Option<WirTypeId> {
        // Every unresolved element compares as the same placeholder, so
        // admitting one would match any tuple unresolved in that position.
        let elem_wir_types: Vec<WirType> = elements
            .iter()
            .map(|e| self.lookup_wir_type(type_table, *e))
            .collect::<Result<_, _>>()
            .ok()?;
        self.tuple_type_map
            .iter()
            .find(|(key_elems, _)| {
                key_elems.len() == elem_wir_types.len()
                    && key_elems.iter().zip(elem_wir_types.iter()).all(|(k, w)| {
                        self.lookup_wir_type(type_table, *k).is_ok_and(|kw| kw == *w)
                    })
            })
            .map(|(_, type_id)| type_id.clone())
    }

    /// Convert a TIR `TypeId` to a `WirType`.
    ///
    /// A miss means the registrar's key and the key derived here disagree —
    /// both sides go through `name::wir_*_key` / [`StructName`] so they cannot.
    /// Degrading to `AbstractRef` instead would be indistinguishable from the
    /// deliberate one `ResolvedType::Function` produces.
    ///
    /// Use [`Self::type_id_to_wir_type_pending`] inside `register_types`.
    #[track_caller]
    pub fn type_id_to_wir_type(&self, type_table: &TypeTable, type_id: TypeId) -> WirType {
        self.lookup_wir_type(type_table, type_id)
            .unwrap_or_else(|pending| panic!("[WIR] {} is not registered", pending.description))
    }

    /// [`Self::type_id_to_wir_type`] for use during type registration, where a
    /// field can name a type a later phase defines — or the struct itself, since
    /// Wasm GC rec groups permit the cycle. It takes the placeholder, which
    /// [`super::types::fixup_abstract_struct_fields`] re-resolves.
    pub fn type_id_to_wir_type_pending(&self, type_table: &TypeTable, type_id: TypeId) -> WirType {
        self.lookup_wir_type(type_table, type_id)
            .unwrap_or_else(|pending| pending.placeholder)
    }

    /// The `WirType` of `type_id`, or `None` when it has no WIR registration.
    ///
    /// For the one caller with a real recovery: a tuple interned by CM binding
    /// synthesis can carry `TypeId`s the registrar never saw, and
    /// `tuple_constructor_args` then searches for or defines a matching struct.
    pub fn try_type_id_to_wir_type(
        &self,
        type_table: &TypeTable,
        type_id: TypeId,
    ) -> Option<WirType> {
        self.lookup_wir_type(type_table, type_id).ok()
    }

    fn lookup_wir_type(
        &self,
        type_table: &TypeTable,
        type_id: TypeId,
    ) -> Result<WirType, UnregisteredType> {
        use crate::tir::{PrimitiveType, ResolvedType};
        Ok(match type_table.get(type_id) {
            ResolvedType::Primitive(prim) => match prim {
                PrimitiveType::I8 => WirType::I8,
                PrimitiveType::I16 => WirType::I16,
                PrimitiveType::I32 => WirType::I32,
                PrimitiveType::I64 => WirType::I64,
                PrimitiveType::U8 => WirType::U8,
                PrimitiveType::U16 => WirType::U16,
                PrimitiveType::U32 => WirType::U32,
                PrimitiveType::U64 => WirType::U64,
                PrimitiveType::I128 | PrimitiveType::U128 => {
                    panic!("i128/u128 not yet supported in WIR")
                }
                PrimitiveType::F32 => WirType::F32,
                PrimitiveType::F64 => WirType::F64,
                PrimitiveType::V128 => WirType::V128,
                PrimitiveType::Bool => WirType::Bool,
                PrimitiveType::Char => WirType::Char,
            },
            ResolvedType::Unit => WirType::Unit,
            // A `Never`-typed expression diverges, so it leaves no value behind —
            // the same absence `Unit` denotes.
            ResolvedType::Never => WirType::Unit,
            ResolvedType::Struct {
                decl_name,
                module_source,
                type_args,
            } => {
                // The WIR struct map is keyed on the rendered spelling: each
                // instantiation is its own struct type.
                let name = &type_table.struct_rendered_name(decl_name, type_args);
                // String is always at ModuleSource::string().
                let lookup_module = if name == "String" {
                    ModuleSource::string()
                } else {
                    module_source.clone()
                };
                let lookup_name = StructName::new(lookup_module, name.clone());
                let Some(type_id) = self.struct_type_map.get(&lookup_name) else {
                    return Err(UnregisteredType::struct_ref(format!("struct `{lookup_name}`")));
                };
                Self::ref_to(type_id)
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "List" && type_args.len() == 1 => {
                let lookup_name = super::types::list_wrapper_struct_name(type_table, type_args[0]);
                let Some(type_id) = self.struct_type_map.get(&lookup_name) else {
                    return Err(UnregisteredType::struct_ref(format!(
                        "list wrapper struct `{lookup_name}`"
                    )));
                };
                Self::ref_to(type_id)
            }
            ResolvedType::GenericInstance {
                name,
                type_args: elements,
                module_source,
            } if TypeTable::is_tuple_type(name) => {
                // CM binding synthesis interns its own `TypeId`s for the same
                // elements, so a miss falls back to structural matching.
                let found = self
                    .tuple_type_map
                    .get(elements)
                    .cloned()
                    .or_else(|| self.find_tuple_type_by_element_wir_types(type_table, elements));
                let Some(type_id) = found else {
                    return Err(UnregisteredType::struct_ref(format!(
                        "tuple `{}` (no registered tuple matches its element types either)",
                        type_table.mangle_type_name(type_id)
                    )));
                };
                Self::ref_to(&type_id)
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // A generic instance is either a struct or a variant, and the
                // two live in different maps. Registration aliases the
                // newtype-resolved spelling onto the same type, so one key each.
                let mangled = super::types::generic_instance_name(type_table, name, type_args);
                let struct_name = StructName::new(module_source.clone(), mangled.clone());
                let type_id = self.struct_type_map.get(&struct_name).or_else(|| {
                    self.type_map
                        .get(&crate::name::wir_type_key(module_source, &mangled))
                });
                let Some(type_id) = type_id else {
                    return Err(UnregisteredType::struct_ref(format!(
                        "generic instance `{struct_name}` (as neither a struct nor a variant)"
                    )));
                };
                Self::ref_to(type_id)
            }
            ResolvedType::BuiltinArray(elem_type_id) => {
                // Cross-module `TypeId`s for one element resolve by the
                // element's name, which registration aliases both spellings of.
                let type_id = self.array_type_map.get(elem_type_id).or_else(|| {
                    let elem_name = type_table.mangle_type_arg_for_generic(*elem_type_id);
                    self.array_type_by_name.get(&elem_name)
                });
                let Some(type_id) = type_id else {
                    return Err(UnregisteredType::array_ref(format!(
                        "array of `{}`",
                        type_table.mangle_type_arg_for_generic(*elem_type_id)
                    )));
                };
                Self::ref_to(type_id)
            }
            // Option<T> is handled as GenericInstance (variant).
            ResolvedType::Enum {
                name,
                module_source,
                ..
            } => {
                let key = crate::name::wir_enum_type_key(module_source, name);
                let Some(type_id) = self.type_map.get(&key) else {
                    return Err(UnregisteredType::enum_i32(format!("enum `{key}`")));
                };
                WirType::Enum {
                    type_id: type_id.clone(),
                }
            }
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => {
                let key = crate::name::wir_type_key(module_source, name);
                let Some(type_id) = self.type_map.get(&key) else {
                    return Err(UnregisteredType::struct_ref(format!("variant `{key}`")));
                };
                Self::ref_to(type_id)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // `&T` = `T` for a GC type. A value type would need a `Box<T>`
                // cell, but `lower::plan::boxing` already rewrote those ids to
                // the struct — what reaches here is the kinds it leaves alone:
                // opaque handles, whose reference is the handle.
                let inner_wir = self.lookup_wir_type(type_table, *inner)?;
                if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
                    panic!(
                        "[WIR] `&{prim:?}` survived boxing unrewritten; \
                         `lower::plan::boxing` owns turning it into `Box<{prim:?}>`"
                    );
                }
                inner_wir
            }
            ResolvedType::Function { .. } => {
                // Function-typed values are canonical closure structs at runtime.
                // Use abstract structref so any concrete closure struct is a valid subtype.
                // IndirectCall will RefCast to the specific canonical closure struct.
                WirType::AbstractRef {
                    heap_type: crate::wir::WirAbstractHeapType::Struct,
                    nullable: false,
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                // Newtypes resolve to their base type
                self.lookup_wir_type(type_table, *base_type)?
            }
            // Generic resource types (Future<T>, Stream<T>, etc.) are opaque i32 handles
            ResolvedType::GenericResource { .. } => WirType::I32,
            // Non-generic resources are opaque i32 handles
            ResolvedType::Resource { .. } => WirType::I32,
            // Flags are bitmasks stored as i32
            ResolvedType::Flags { .. } => WirType::I32,
            // These should never reach codegen — must be resolved by monomorphization
            ResolvedType::TypeParam { name, index } => {
                panic!("unsubstituted TypeParam `{name}` (index {index}) reached codegen")
            }
            ResolvedType::TypePack { name, index, .. } => {
                panic!("unsubstituted TypePack `..{name}` (index {index}) reached codegen")
            }
            ResolvedType::AssocTypeProjection { assoc_name, .. } => {
                panic!("unsubstituted AssocTypeProjection `{assoc_name}` reached codegen")
            }
            // Type checking rejects a program that still holds these, and
            // `Reactive` is erased before lowering.
            ResolvedType::Error => panic!("error type reached codegen"),
            ResolvedType::Unknown => panic!("unresolved type reached codegen"),
            ResolvedType::Reactive(_) => panic!("unerased `Reactive` type reached codegen"),
        })
    }

    /// Find a tuple WIR type that matches the given TIR elements by WIR type compatibility.
    ///
    /// When CM binding synthesis creates tuple types, the `TypeIds` may not exactly match
    /// the ones in `tuple_type_map`. This fallback searches by matching WIR types of elements.
    pub fn find_tuple_type_for_elements(
        &self,
        type_table: &crate::tir::TypeTable,
        elem_type_ids: &[crate::tir::TypeId],
    ) -> Option<WirTypeId> {
        let elem_wir_types: Vec<WirType> = elem_type_ids
            .iter()
            .map(|tid| self.type_id_to_wir_type(type_table, *tid))
            .filter(|t| !matches!(t, WirType::Unit))
            .collect();
        // Search tuple_type_map for a matching tuple with same WIR field types
        for (elem_type_ids, wir_type_id) in &self.tuple_type_map {
            if elem_type_ids.len() == elem_wir_types.len() {
                let all_match = elem_type_ids
                    .iter()
                    .zip(elem_wir_types.iter())
                    .all(|(tid, wir)| self.type_id_to_wir_type(type_table, *tid) == *wir);
                if all_match {
                    return Some(wir_type_id.clone());
                }
            }
        }
        None
    }

    /// Define a new tuple struct for the given elements when no existing match is found.
    ///
    /// Creates a WIR struct with fields matching the WIR types of each element.
    /// Used for CM binding synthesis tuple returns that weren't pre-registered.
    pub fn define_tuple_struct_for_elements(
        &mut self,
        type_table: &crate::tir::TypeTable,
        elem_type_ids: &[crate::tir::TypeId],
    ) -> Option<WirTypeId> {
        let elem_wir_types: Vec<WirType> = elem_type_ids
            .iter()
            .map(|tid| self.type_id_to_wir_type(type_table, *tid))
            .filter(|t| !matches!(t, WirType::Unit))
            .collect();
        if elem_wir_types.is_empty() {
            return None;
        }
        let elem_names: Vec<String> = elem_type_ids
            .iter()
            .filter(|tid| !matches!(self.type_id_to_wir_type(type_table, **tid), WirType::Unit))
            .enumerate()
            .map(|(i, _)| i.to_string())
            .collect();
        let display = format!(
            "tuple/[{}]",
            elem_names
                .iter()
                .zip(elem_wir_types.iter())
                .map(|(_, t)| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let fields: Vec<crate::wir::WirField> = elem_names
            .iter()
            .zip(elem_wir_types.iter())
            .map(|(name, ty)| crate::wir::WirField {
                name: name.clone(),
                ty: ty.clone(),
                mutable: true,
            })
            .collect();
        let struct_def = crate::wir::WirTypeDef::Struct(crate::wir::WirStructType {
            name: crate::wir::WirName {
                fq: display.clone(),
            },
            fields,
            meta: crate::wir::WirMeta::default(),
            generic_origin: None,
            newtype_origin: None,
            supertype: None,
        });
        let type_id = self.register_type(display, struct_def);
        // Register in tuple_type_map using the TIR element TypeIds
        let filtered_type_ids: Vec<crate::tir::TypeId> = elem_type_ids
            .iter()
            .copied()
            .filter(|tid| !matches!(self.type_id_to_wir_type(type_table, *tid), WirType::Unit))
            .collect();
        self.tuple_type_map
            .insert(filtered_type_ids, type_id.clone());
        Some(type_id)
    }

    /// Consume this context and produce the final `WirPackage`.
    pub fn into_wir_package(self) -> WirPackage {
        let trait_bound_violations = self.trait_bound_violations;
        let functions = self.functions;
        let globals = self.globals;
        let global_map = &self.global_map;

        // Extract functions and globals from #![wasm_module("...")] sources
        // into separate WasmModuleInfo structures.
        let mut wasm_modules: IndexMap<String, crate::wir::WasmModuleInfo> = IndexMap::default();
        let mut dead_type_indices: IndexSet<u32> = IndexSet::default();
        let mut dead_func_indices: IndexSet<u32> = IndexSet::default();
        let mut dead_global_indices: IndexSet<u32> = IndexSet::default();

        for (source_ms, wasm_mod_name) in &self.wasm_module_sources {
            let source_prefix = source_ms.to_string();
            let mut mod_functions = Vec::new();
            let mut mod_globals = Vec::new();
            let mut mod_global_name_to_index = IndexMap::default();

            // Find functions belonging to this wasm module (keep in list, mark as dead)
            for (i, func) in functions.iter().enumerate() {
                if !func.name.fq.starts_with(&source_prefix) {
                    continue;
                }
                let func_idx = u32::try_from(i).unwrap();
                dead_func_indices.insert(func_idx);
                dead_type_indices.insert(func.type_id.index());

                let export_name = func.export_name.clone().unwrap_or_else(|| {
                    func.name
                        .fq
                        .strip_prefix(&source_prefix)
                        .and_then(|s| s.strip_prefix('/'))
                        .unwrap_or(&func.name.fq)
                        .to_string()
                });
                let body = func.body.clone().unwrap_or_default();

                // Collect referenced globals
                let mut referenced_globals = IndexMap::default();
                collect_referenced_globals(&body, &mut referenced_globals);

                for (global_fq, ()) in &referenced_globals {
                    if mod_global_name_to_index.contains_key(global_fq) {
                        continue;
                    }
                    if let Some(&global_idx) = global_map.get(global_fq.as_str()) {
                        let idx = global_idx as usize;
                        if idx < globals.len() {
                            dead_global_indices.insert(u32::try_from(idx).unwrap());
                            mod_global_name_to_index.insert(
                                global_fq.clone(),
                                u32::try_from(mod_globals.len()).unwrap(),
                            );
                            mod_globals.push(globals[idx].clone());
                        }
                    }
                }

                // Get result types from the function's type definition
                let results = if let Some(crate::wir::WirTypeDef::Func(ft)) =
                    self.types.get(func.type_id.index() as usize)
                {
                    ft.results.clone()
                } else {
                    vec![crate::wir::WirType::I32]
                };

                mod_functions.push(crate::wir::WasmModuleFunc {
                    export_name,
                    param_names: func.param_names.clone(),
                    results,
                    body,
                    original_func_index: DEFINED_FUNC_BASE + func_idx,
                    is_exported: func.export_name.is_some(),
                });
            }

            wasm_modules.insert(
                wasm_mod_name.clone(),
                crate::wir::WasmModuleInfo {
                    functions: mod_functions,
                    globals: mod_globals,
                    global_name_to_index: mod_global_name_to_index,
                },
            );
        }

        let needed_canonicals: IndexSet<CanonicalIntrinsic> =
            self.needed_canonicals.keys().cloned().collect();

        WirPackage {
            types: self.types,
            imports: self.imports,
            functions,
            globals,
            exports: self.exports,
            elements: Vec::new(), // TODO: element section
            memories: Vec::new(),
            data: self.data,
            names: self.names,
            component: WirComponent::default(),
            variant_case_info: self.variant_case_info,
            wasm_modules,
            dead_type_indices,
            dead_func_indices,
            dead_global_indices,
            needed_canonicals,
            // Resolved by `build_wir_package` once `needed_canonicals` is final.
            imported_cm_interfaces: Vec::new(),
            import_plan: Vec::new(),
            defined_func_base: DEFINED_FUNC_BASE,
            trait_bound_violations,
        }
    }
}

/// Collect fully-qualified global names referenced by WIR instructions.
fn collect_referenced_globals(instrs: &[crate::wir::WirInstr], out: &mut IndexMap<String, ()>) {
    for instr in instrs {
        collect_referenced_globals_instr(instr, out);
    }
}

fn collect_referenced_globals_instr(instr: &crate::wir::WirInstr, out: &mut IndexMap<String, ()>) {
    use crate::wir::WirInstr;
    match instr {
        WirInstr::GlobalGet { name, .. } | WirInstr::GlobalSet { name, .. } => {
            out.insert(name.fq.clone(), ());
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| collect_referenced_globals_instr(child, out));
}
