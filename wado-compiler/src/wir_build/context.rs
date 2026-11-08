//! WIR builder context — accumulates types, functions, and other module-level
//! entries during the `tir_to_wir` translation, then produces a final `WirPackage`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::MangledName;

use crate::canonical::CanonicalIntrinsic;
use crate::module_source::ModuleSource;
use crate::nir;
use crate::nir::{FuncId, FuncRef, StructDecls};
use crate::nir_package::NirPackage;
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TypeId};
use crate::type_table::{TypeMemo, TypeTable};
use crate::wir::{
    CmImportViolation, TraitBoundViolation, WasmModuleFunc, WasmModuleInfo, WirAbstractHeapType,
    WirComponent, WirData, WirExport, WirField, WirFuncId, WirFuncType, WirFunction, WirGlobal,
    WirImport, WirImportDesc, WirInstr, WirMemory, WirMeta, WirName, WirNames, WirPackage,
    WirStructType, WirType, WirTypeDef, WirTypeId,
};
use crate::world_registry::WorldSurface;

/// Base offset for defined function `WirFuncId` indices.
/// Import functions use indices 0..N, defined functions use `DEFINED_FUNC_BASE + 0..M`.
/// This prevents index collisions when `ensure_canonical` adds imports after
/// defined functions have already been registered with their `WirFuncId`.
pub const DEFINED_FUNC_BASE: u32 = 0x8000_0000;

/// The `$canonical_inspectable_base` vtable slot a `fn(..)^Inspect` stub reads.
pub const CANONICAL_INSPECT_SLOT: &str = "inspect";

/// A parameter a call site hands over field by field: `ParamAbi::MultiValue`.
#[derive(Clone, Debug)]
pub struct SplitParam {
    /// `(field_name, type_id)` of each Wasm slot, in order.
    pub fields: Vec<(String, TypeId)>,
    /// The fields are loaded off a reference at the call.
    pub through_ref: bool,
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
    /// The package's concrete struct declarations, by the type each reifies.
    pub struct_decls: StructDecls<'a>,
    /// The WIR type each concrete struct, tuple, variant, enum and array type
    /// lowers to, by its id.
    pub wado_types: IndexMap<TypeId, WirTypeId>,
    /// `$Resource`, the object an affine resource is: its handle, mutable in
    /// place, so a reference to the resource sees a store through another
    /// (WEP: Affine Types). Set while types register, before any is looked up.
    pub handle_box: Option<WirTypeId>,
    /// Each cell struct by [`TypeTable::cell_struct_name`].
    pub cell_type_map: IndexMap<String, WirTypeId>,
    /// What [`TypeTable::holds_affine_resource`] has answered, by type.
    affine: TypeMemo<bool>,
    /// Variant case type info: case WIR type index → (variant WIR type index, case index).
    pub variant_case_info: IndexMap<u32, (u32, u32)>,
    /// Each payload case's struct, by its variant's WIR type index and case
    /// index: the inverse of `variant_case_info`.
    pub variant_case_types: IndexMap<(u32, u32), WirTypeId>,

    /// All function definitions (with optional bodies).
    pub functions: Vec<WirFunction>,
    /// Map from fully-qualified function name to `WirFuncId`.
    pub func_map: IndexMap<MangledName, WirFuncId>,
    /// Map from a defined function's canonical [`crate::nir::FuncId`] to its
    /// `WirFuncId`. Lets a stamped call resolve its target by id, skipping the
    /// name reconstruction in `resolve_function_ref` (the name path stays for
    /// extern / unstamped callees).
    pub funcid_map: IndexMap<FuncId, WirFuncId>,
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
    /// the canonical struct carries the inspect vtable
    /// slots. See `inspectable_fn_dispatch`.
    pub canonical_closure_types: IndexMap<String, (WirTypeId, WirTypeId, bool)>,
    /// Map from closure `(module_source, functor_id)` to the per-functor
    /// canonical wrapper triple. Keyed by module source + functor ID
    /// because functor IDs are per-module, not globally unique.
    pub closure_wrapper_funcs: IndexMap<(ModuleSource, u32), ClosureWrapperFuncs>,
    /// Counter for canonical closure type naming.
    pub canonical_closure_counter: u32,
    /// The `(arity, return_type)` signatures whose `Fn^Inspect`
    /// dispatch stub survived DCE, scanned once at WIR build start. Gates whether
    /// a `CanonicalClosure_K` carries the `inspect` vtable slot:
    /// a signature absent here keeps the slim `{ env, func }` shape, so a build
    /// that never inspects closures pays nothing.
    pub inspectable_fn_dispatch: IndexSet<(usize, TypeId)>,
    /// Shared inspectable supertype `$canonical_inspectable_base`, created lazily
    /// at the first inspectable `CanonicalClosure_K` registration. The dispatch
    /// stub `ref.cast`s `self` to it, so every inspectable closure reaches the
    /// same slot positions whatever its parameter types — otherwise the stub
    /// would trap on any value from a different per-signature struct.
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

    /// Functions whose `return_abi` is `MultiValue`, and the per-result
    /// `(field_name, type_id)` each returns in declaration order.
    pub multi_value_return_funcs: IndexMap<FuncId, Vec<(String, TypeId)>>,
    /// Which parameter positions of a callee arrive as one Wasm slot per field.
    pub multi_value_param_funcs: IndexMap<FuncId, IndexMap<usize, SplitParam>>,
    /// The NIR field names, `()` ones included, of each struct or tuple type
    /// (by `WirTypeId::index`) whose Wasm fields leave out its `()` ones. A
    /// NIR field index counts these; any other struct's Wasm fields match its
    /// NIR fields one to one.
    pub nir_field_names: IndexMap<u32, Vec<String>>,
    /// Unresolved `Type^Trait::method` calls (unsatisfied trait bounds),
    /// collected rather than trapping; the driver reports them and bails.
    pub trait_bound_violations: Vec<TraitBoundViolation>,
    /// Calls to a `#[cm(...)]` member with no backing import, collected alongside
    /// [`Self::trait_bound_violations`] and reported the same way.
    pub cm_import_violations: Vec<CmImportViolation>,
}

/// A function body that needs to be translated from TIR to WIR.
pub struct PendingFunctionBody {
    /// Index into WirContext.functions
    pub wir_func_index: usize,
    /// The TIR function to translate
    pub tir_func: FuncRef,
    /// The type table for this function's module
    pub type_table: Rc<RefCell<TypeTable>>,
}

/// The `(arity, ret)` call shapes whose `^Inspect` impl is
/// still reachable: `synthesize_traits` emits a dispatch stub per module tagged
/// `FunctionKind::FnCanonicalDispatch`, and the ones DCE leaves behind drive the
/// per-signature schema gate that keeps an uninspected canonical closure slim.
fn compute_inspectable_fn_dispatch(package: &NirPackage) -> IndexSet<(usize, TypeId)> {
    let mut set: IndexSet<(usize, TypeId)> = IndexSet::default();
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        // `dce` marks unreachable functions dead in place (Phase 4) rather than
        // removing them, so a dead `FnCanonicalDispatch` stub lingers; gate on
        // liveness, not mere presence.
        if func.is_dead {
            continue;
        }
        if let Some((arity, return_type)) = func.fn_canonical_dispatch() {
            set.insert((arity, return_type));
        }
    }
    set
}

/// Per-functor wrapper functions for the `CanonicalClosure_K` vtable slots,
/// populated by `register_closure_wrappers` and read by
/// `translate_closure_to_canonical`. `inspect` is `None` where
/// [`WirContext::inspectable_fn_dispatch`] calls the signature unreachable, and
/// the struct then takes the slim `{ env, func }` schema.
#[derive(Clone, Debug)]
pub struct ClosureWrapperFuncs {
    /// `$closure_wrapper_N(env, args...) -> ret` — refcasts env to
    /// `&$Closure_N` and forwards to `$call`.
    pub call: WirFuncId,
    /// `$closure_inspect_wrapper_N(env, formatter)` — refcasts both
    /// args and forwards to `$Closure_N^Inspect::inspect`. `None`
    /// when the functor's `(N, Ret)` is not inspectable.
    pub inspect: Option<WirFuncId>,
}

/// The `(field_name, type_id)` pairs a multi-value ABI records, in the
/// declaration order both halves of it index by.
fn abi_fields(names: &[String], types: &[TypeId]) -> Vec<(String, TypeId)> {
    assert_eq!(
        names.len(),
        types.len(),
        "[WIR] a multi-value ABI paired {} names with {} types; a short zip \
         would pass fewer arguments than the signature declares",
        names.len(),
        types.len()
    );
    names.iter().cloned().zip(types.iter().copied()).collect()
}

impl<'a> WirContext<'a> {
    /// Create a new `WirContext` from a `NirPackage`.
    pub fn new(package: &'a NirPackage) -> Self {
        // String and bytes literals live on `package`; `register_literal_data`
        // reads them directly and dedups into `data` via `packed_data_map`, so
        // the context keeps no separate copy.

        // The per-`(arity, ret)` inspectable gate. DCE has already run, so the
        // surviving `fn(..)^Inspect` stubs name exactly the shapes needing
        // vtable slots. A program that inspects no closure leaves this empty —
        // slim `{ env, func }`.
        let inspectable_fn_dispatch = compute_inspectable_fn_dispatch(package);

        // Both maps are keyed by `FuncId`, which is the index a call site
        // already resolves its callee by. A name is not an identity.
        let mut multi_value_return_funcs: IndexMap<FuncId, Vec<(String, TypeId)>> =
            IndexMap::default();
        let mut multi_value_param_funcs: IndexMap<FuncId, IndexMap<usize, SplitParam>> =
            IndexMap::default();
        for f in &package.functions {
            let f = f.borrow();
            // `f.id`, not the storage position: a `FuncId` is intrinsic to the
            // entity, and it is what a call site resolves its callee by.
            let id = || {
                f.id.unwrap_or_else(|| {
                    panic!(
                        "[WIR] `{}` takes a multi-value ABI with no `FuncId`, so no \
                         call site could lower against it",
                        f.name
                    )
                })
            };
            if let nir::ReturnAbi::MultiValue {
                result_types,
                field_names,
            } = &f.return_abi
            {
                multi_value_return_funcs.insert(id(), abi_fields(field_names, result_types));
            }
            for (param_idx, param) in f.params.iter().enumerate() {
                let nir::ParamAbi::MultiValue {
                    field_types,
                    field_names,
                    through_ref,
                } = &param.param_abi
                else {
                    continue;
                };
                multi_value_param_funcs.entry(id()).or_default().insert(
                    param_idx,
                    SplitParam {
                        fields: abi_fields(field_names, field_types),
                        through_ref: *through_ref,
                    },
                );
            }
        }

        Self {
            package,
            types: Vec::new(),
            type_map: IndexMap::default(),
            struct_decls: StructDecls::new(&package.structs),
            wado_types: IndexMap::default(),
            handle_box: None,
            cell_type_map: IndexMap::default(),
            affine: TypeMemo::default(),
            variant_case_info: IndexMap::default(),
            variant_case_types: IndexMap::default(),
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
            multi_value_param_funcs,
            nir_field_names: IndexMap::default(),
            trait_bound_violations: Vec::new(),
            cm_import_violations: Vec::new(),
        }
    }

    /// Register a type definition and return its `WirTypeId`. A WIR-only type
    /// (a function type, a closure struct) is named by its shape, so a name
    /// already registered answers the type registered under it.
    pub fn register_type(&mut self, fq: String, typedef: WirTypeDef) -> WirTypeId {
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

    /// A WIR type named `fq`, for [`Self::define_type`] to define once every
    /// type it may reference has an id.
    pub fn reserve_type(&mut self, fq: String) -> WirTypeId {
        assert!(
            !self.type_map.contains_key(&fq),
            "one WIR name names one type: `{fq}`"
        );
        let placeholder = WirTypeDef::Struct(WirStructType {
            name: WirName { fq: fq.clone() },
            fields: Vec::new(),
            meta: WirMeta::default(),
            generic_origin: None,
            newtype_origin: None,
            supertype: None,
        });
        self.register_type(fq, placeholder)
    }

    /// [`Self::reserve_type`] for the Wado type `type_id`.
    pub fn reserve_wado_type(&mut self, type_id: TypeId, fq: String) -> WirTypeId {
        let id = self.reserve_type(fq);
        let previous = self.wado_types.insert(type_id, id.clone());
        assert!(previous.is_none(), "a Wado type is reserved once");
        id
    }

    pub fn define_type(&mut self, id: &WirTypeId, def: WirTypeDef) {
        self.types[id.index() as usize] = def;
    }

    /// The WIR array type of `Array<element>`.
    #[track_caller]
    pub fn array_type(&self, type_table: &TypeTable, element: TypeId) -> WirTypeId {
        let array = type_table.find_builtin_array(element).unwrap_or_else(|| {
            panic!(
                "[WIR] `Array<{}>` is not interned",
                type_table.name(element)
            )
        });
        match self.type_id_to_wir_type(type_table, array) {
            WirType::Ref { type_id, .. } => type_id,
            other => unreachable!("an array lowers to a reference, not {other:?}"),
        }
    }

    /// Register a function type definition and return its `WirTypeId`.
    pub fn register_func_type(
        &mut self,
        fq: String,
        params: Vec<WirType>,
        results: Vec<WirType>,
    ) -> WirTypeId {
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
    pub fn register_function(&mut self, func: WirFunction, nir_id: Option<FuncId>) -> WirFuncId {
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

    /// Get or create the `(fn_type_id, closure_struct_type_id)` pair for a
    /// signature — the vtable shape trait dispatch needs on a `fn(..)` held
    /// behind a parameter, field or global. An inspectable struct is laid out
    /// `{ env, inspect, func }`, the first two being the GC subtype
    /// prefix shared with `$canonical_inspectable_base`; others keep `{ env, func }`.
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
            heap_type: WirAbstractHeapType::Struct,
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
        let mut fields = vec![WirField {
            name: "env".to_string(),
            ty: abstract_struct_nullable,
            mutable: false,
        }];
        let supertype = if is_inspectable {
            let callback_fn_type_id = self.get_or_create_canonical_callback_fn_type();
            fields.push(WirField {
                name: CANONICAL_INSPECT_SLOT.to_string(),
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
    /// `$canonical_inspectable_base = (struct env inspect)`.
    ///
    /// Every inspectable `CanonicalClosure_K` declares this as its supertype,
    /// whatever its signature, so a `fn(..)^Inspect` stub `ref.cast`s here and
    /// reads `inspect` off the base layout.
    pub fn get_or_create_canonical_inspectable_base(&mut self) -> WirTypeId {
        if let Some(id) = &self.canonical_inspectable_base_type_id {
            return id.clone();
        }
        let callback_fn_type_id = self.get_or_create_canonical_callback_fn_type();
        let abstract_struct_nullable = WirType::AbstractRef {
            heap_type: WirAbstractHeapType::Struct,
            nullable: true,
        };
        let fields = vec![
            WirField {
                name: "env".to_string(),
                ty: abstract_struct_nullable,
                mutable: false,
            },
            WirField {
                name: CANONICAL_INSPECT_SLOT.to_string(),
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

    /// Func type for the inspect vtable slot:
    /// `(env: ref null struct, formatter: ref null struct) -> ()`.
    ///
    /// Both args use abstract `(ref null struct)` so a single fn type
    /// is compatible with every per-functor wrapper. The wrappers
    /// refcast `env` to the concrete `&$Closure_N` and `formatter` to
    /// `&Formatter` internally before forwarding.
    pub fn get_or_create_canonical_callback_fn_type(&mut self) -> WirTypeId {
        let fq = "functype/$canonical_callback_fn".to_string();
        if let Some(id) = self.type_map.get(&fq) {
            return id.clone();
        }
        let abstract_struct_nullable = WirType::AbstractRef {
            heap_type: WirAbstractHeapType::Struct,
            nullable: true,
        };
        self.register_func_type(
            fq,
            vec![abstract_struct_nullable.clone(), abstract_struct_nullable],
            vec![],
        )
    }

    /// Register a CM canonical import lazily and return its `WirFuncId`. Its
    /// callers are WIR synthesis, so it goes through neither TIR imports nor DCE.
    pub fn ensure_canonical(
        &mut self,
        intrinsic: CanonicalIntrinsic,
        params: Vec<WirType>,
        results: Vec<WirType>,
    ) -> WirFuncId {
        // The intrinsic decides, not its rendering: two renderings of one
        // declaration would otherwise register two imports for one canon.
        if let Some(func_id) = self.needed_canonicals.get(&intrinsic) {
            return func_id.clone();
        }
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

    fn ref_to(type_id: &WirTypeId) -> WirType {
        WirType::non_null_ref(type_id.clone())
    }

    pub fn handle_box(&self) -> &WirTypeId {
        self.handle_box
            .as_ref()
            .expect("`$Resource` registers before any type is looked up")
    }

    /// Convert a TIR `TypeId` to a `WirType`. Registration reserves every
    /// concrete type before it defines any, so a miss is a type registration
    /// never saw.
    #[track_caller]
    pub fn type_id_to_wir_type(&self, type_table: &TypeTable, type_id: TypeId) -> WirType {
        let wir = self
            .lookup_wir_type(type_table, type_id)
            .unwrap_or_else(|missing| panic!("[WIR] {missing} is not registered"));
        debug_assert_eq!(
            matches!(wir, WirType::Unit),
            type_table.is_stackless(type_id),
            "[WIR] {} disagrees with `is_stackless`: a type the signature lists and the body \
             drops (or the reverse) desyncs the two",
            type_table.name(type_id),
        );
        wir
    }

    /// The results a function returning `type_id` declares: none for a
    /// stackless type.
    pub fn result_wir_types(&self, type_table: &TypeTable, type_id: TypeId) -> Vec<WirType> {
        match self.type_id_to_wir_type(type_table, type_id) {
            WirType::Unit => vec![],
            wir_type => vec![wir_type],
        }
    }

    /// Whether a place holding a `type_id` is nullable: it holds an affine
    /// value, which a move empties (WEP: Affine Types).
    pub fn is_affine(&self, type_table: &TypeTable, type_id: TypeId) -> bool {
        self.affine
            .get_or_compute(type_id, || type_table.holds_affine_resource(type_id))
    }

    /// `ty`, the WIR type of a field, element or payload of type `type_id`, as
    /// that slot holds it: non-nullable, but for an affine value, and in its
    /// box where [`TypeTable::slot_box`] names one.
    pub fn slot_type(&self, type_table: &TypeTable, type_id: TypeId, ty: WirType) -> WirType {
        if type_table.slot_box(type_id).is_some() {
            self.boxed_slot_type(type_table, type_id, ty)
        } else {
            self.unboxed_slot_type(type_table, type_id, ty)
        }
    }

    /// [`Self::slot_type`] for the slot a box holds its value in.
    pub fn unboxed_slot_type(
        &self,
        type_table: &TypeTable,
        type_id: TypeId,
        ty: WirType,
    ) -> WirType {
        if self.is_affine(type_table, type_id) {
            ty.as_nullable()
        } else {
            ty.as_nonnull()
        }
    }

    /// `ty`, the WIR type of a slot of type `type_id`, as its box where
    /// [`TypeTable::slot_box`] names one.
    pub fn boxed_slot_type(&self, type_table: &TypeTable, type_id: TypeId, ty: WirType) -> WirType {
        match type_table.slot_box(type_id) {
            Some(wrapper) => self.type_id_to_wir_type(type_table, wrapper).as_nullable(),
            None => ty,
        }
    }

    /// The cell a slot holds a `type_id` in, where [`TypeTable::slot_box`]
    /// names one.
    pub fn slot_box(&self, type_table: &TypeTable, type_id: TypeId) -> Option<WirTypeId> {
        let wrapper = type_table.slot_box(type_id)?;
        match self.type_id_to_wir_type(type_table, wrapper) {
            WirType::Ref { type_id, .. } => Some(type_id),
            other => unreachable!("a slot cell is a struct, not {other:?}"),
        }
    }

    /// The WIR type `type_id` lowers to, or what is missing from the registry.
    fn lookup_wir_type(&self, type_table: &TypeTable, type_id: TypeId) -> Result<WirType, String> {
        let wir = self.lookup_wir_shape(type_table, type_id)?;
        Ok(if self.is_affine(type_table, type_id) {
            wir.as_nullable()
        } else {
            wir
        })
    }

    fn wado_type(&self, type_table: &TypeTable, type_id: TypeId) -> Result<&WirTypeId, String> {
        self.wado_types
            .get(&type_id)
            .ok_or_else(|| format!("`{}`", type_table.name(type_id)))
    }

    fn lookup_wir_shape(&self, type_table: &TypeTable, type_id: TypeId) -> Result<WirType, String> {
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
                PrimitiveType::F32 => WirType::F32,
                PrimitiveType::F64 => WirType::F64,
                PrimitiveType::V128 => WirType::V128,
                // A half is a `u16` below here: Wasm has no half precision
                // value type, and its packed storage types are `i8` and `i16`.
                PrimitiveType::F16 | PrimitiveType::Bf16 => WirType::U16,
                PrimitiveType::Bool => WirType::Bool,
                PrimitiveType::Char => WirType::Char,
            },
            ResolvedType::Unit => WirType::Unit,
            // A `Never`-typed expression diverges, so it leaves no value behind —
            // the same absence `Unit` denotes.
            ResolvedType::Never => WirType::Unit,
            ResolvedType::Resource { .. } => {
                if type_table.is_unrestricted_handle(type_id) {
                    self.lookup_wir_type(type_table, TypeTable::U64)?
                } else {
                    Self::ref_to(self.handle_box())
                }
            }
            ResolvedType::Struct { .. }
            | ResolvedType::Tuple(_)
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Variant { .. } => Self::ref_to(self.wado_type(type_table, type_id)?),
            ResolvedType::Enum { .. } => WirType::Enum {
                type_id: self.wado_type(type_table, type_id)?.clone(),
            },
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                if type_table.cell_payload(type_id).is_none() {
                    // `&T` = `T` for an object, whose reference is its handle.
                    return self.lookup_wir_type(type_table, *inner);
                }
                let name = type_table.cell_struct_name(type_id);
                let Some(cell) = self.cell_type_map.get(&name) else {
                    return Err(format!("cell `{name}`"));
                };
                // A reference to an affine value is nullable, as the value is.
                if self.is_affine(type_table, *inner) {
                    Self::ref_to(cell).as_nullable()
                } else {
                    Self::ref_to(cell)
                }
            }
            ResolvedType::Function { .. } => {
                // Function-typed values are canonical closure structs at runtime.
                // Use abstract structref so any concrete closure struct is a valid subtype.
                // IndirectCall will RefCast to the specific canonical closure struct.
                WirType::AbstractRef {
                    heap_type: WirAbstractHeapType::Struct,
                    nullable: false,
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                // Newtypes resolve to their base type
                self.lookup_wir_type(type_table, *base_type)?
            }
            // Flags are bitmasks stored as i32
            ResolvedType::Flags { .. } => WirType::I32,
            // These should never reach codegen — must be resolved by monomorphization
            ResolvedType::TypeParam { name, id } => {
                panic!("unsubstituted TypeParam `{name}` ({id:?}) reached codegen")
            }
            ResolvedType::AssocParam { name, index, .. } => {
                panic!("uninstantiated AssocParam `{name}` (index {index}) reached codegen")
            }
            ResolvedType::TypePack { name, id, .. } => {
                panic!("unsubstituted TypePack `..{name}` ({id:?}) reached codegen")
            }
            ResolvedType::PackElement { pack, .. } => {
                panic!("an element of pack {pack:?} reached codegen, which unrolling substitutes")
            }
            ResolvedType::InferVar(var) => {
                panic!("unsolved inference variable `{var}` reached codegen")
            }
            ResolvedType::AssocTypeProjection { assoc_name, .. } => {
                panic!("unsubstituted AssocTypeProjection `{assoc_name}` reached codegen")
            }
            // Type checking rejects a program that still holds these.
            ResolvedType::Error => panic!("error type reached codegen"),
            ResolvedType::Unknown => panic!("unresolved type reached codegen"),
        })
    }

    /// Consume this context and produce the final `WirPackage`.
    pub fn into_wir_package(self) -> WirPackage {
        // One page past the reserved ones holds the start of the heap, where the
        // freelist allocator keeps its bin heads.
        let memory = WirMemory {
            min: self.package.wasm_asset_reserved_pages() + 1,
            max: None,
        };
        let trait_bound_violations = self.trait_bound_violations;
        let cm_import_violations = self.cm_import_violations;
        let functions = self.functions;
        let globals = self.globals;
        let global_map = &self.global_map;

        // Extract functions and globals from #![wasm_module("...")] sources
        // into separate standalone packages.
        let mut wasm_modules: IndexMap<String, WirPackage> = IndexMap::default();
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

                let signature = self.types[func.type_id.index() as usize].expect_func();

                mod_functions.push(WasmModuleFunc {
                    export_name,
                    param_names: func.param_names.clone(),
                    params: signature.params.clone(),
                    results: signature.results.clone(),
                    body,
                    original_func_index: DEFINED_FUNC_BASE + func_idx,
                    is_exported: func.export_name.is_some(),
                });
            }

            let info = WasmModuleInfo {
                functions: mod_functions,
                globals: mod_globals,
                global_name_to_index: mod_global_name_to_index,
            };
            wasm_modules.insert(wasm_mod_name.clone(), info.to_wir_package(memory.clone()));
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
            world_surface: WorldSurface::default(),
            import_plan: Vec::new(),
            defined_func_base: DEFINED_FUNC_BASE,
            trait_bound_violations,
            cm_import_violations,
        }
    }
}

/// Collect fully-qualified global names referenced by WIR instructions.
fn collect_referenced_globals(instrs: &[WirInstr], out: &mut IndexMap<String, ()>) {
    for instr in instrs {
        collect_referenced_globals_instr(instr, out);
    }
}

fn collect_referenced_globals_instr(instr: &WirInstr, out: &mut IndexMap<String, ()>) {
    match instr {
        WirInstr::GlobalGet { name, .. } | WirInstr::GlobalSet { name, .. } => {
            out.insert(name.fq.clone(), ());
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| collect_referenced_globals_instr(child, out));
}
