//! WIR builder context — accumulates types, functions, and other module-level
//! entries during the `tir_to_wir` translation, then produces a final `WirModule`.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::name::{ModuleSource, StructName};
use crate::project::Project;
use crate::tir::{TirFunction, TypeId, TypeTable};
use crate::wir::{
    CanonicalIntrinsic, WirComponent, WirData, WirExport, WirFuncId, WirFuncType, WirFunction,
    WirGlobal, WirImport, WirImportDesc, WirModule, WirName, WirNames, WirRecGroup, WirType,
    WirTypeDef, WirTypeId,
};

/// Base offset for defined function `WirFuncId` indices.
/// Import functions use indices 0..N, defined functions use `DEFINED_FUNC_BASE + 0..M`.
/// This prevents index collisions when `ensure_canonical` adds imports after
/// defined functions have already been registered with their `WirFuncId`.
pub const DEFINED_FUNC_BASE: u32 = 0x8000_0000;

/// Builder context for the `tir_to_wir` translation.
///
/// Accumulates all WIR entities and provides lookup maps for resolving
/// type and function references during translation.
pub struct WirContext<'a> {
    /// Reference to the immutable project data.
    pub project: &'a Project,

    // === Type Registry ===
    /// All type definitions in registration order.
    pub types: Vec<WirTypeDef>,
    /// Map from fully-qualified type name to `WirTypeId`.
    pub type_map: IndexMap<String, WirTypeId>,
    /// Rec groups (mutually recursive types).
    pub rec_groups: Vec<WirRecGroup>,
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

    // === Function Registry ===
    /// All function definitions (with optional bodies).
    pub functions: Vec<WirFunction>,
    /// Map from fully-qualified function name to `WirFuncId`.
    pub func_map: IndexMap<String, WirFuncId>,
    /// Function type index for each function (into types vec).
    pub func_type_ids: Vec<WirTypeId>,

    // === Import Registry ===
    /// Core module imports.
    pub imports: Vec<WirImport>,
    /// Number of imported functions (these come before defined functions in Wasm).
    pub import_func_count: u32,
    /// Map from import name to function index (for resolving call targets).
    pub import_func_map: IndexMap<String, WirFuncId>,

    // === Other sections ===
    /// Global variables.
    pub globals: Vec<WirGlobal>,
    /// Map from qualified global name to index in `globals`.
    pub global_map: IndexMap<String, u32>,
    /// Exports.
    pub exports: Vec<WirExport>,
    /// Data segments (string and bytes literals).
    pub data: Vec<WirData>,
    /// String literal dedup: string → data segment index.
    pub string_literal_map: IndexMap<String, u32>,
    /// Bytes literal dedup: bytes → data segment index.
    pub bytes_literal_map: IndexMap<Vec<u8>, u32>,
    /// Name section entries.
    pub names: WirNames,

    // === Canonical Closure Types ===
    /// Map from function signature string to canonical closure info.
    /// Key: stringified signature (e.g., "(i32, i32) -> i32")
    /// Value: (`canonical_fn_type_id`, `canonical_closure_struct_type_id`)
    pub canonical_closure_types: IndexMap<String, (WirTypeId, WirTypeId)>,
    /// Map from closure `functor_id` to canonical wrapper function `WirFuncId`.
    pub closure_wrapper_funcs: IndexMap<u32, WirFuncId>,
    /// Counter for canonical closure type naming.
    pub canonical_closure_counter: u32,

    // === Scratch state ===
    /// Collected string literals (from all TIR modules).
    pub string_literals: Vec<String>,
    /// Collected bytes literals (from all TIR modules).
    pub bytes_literals: Vec<Vec<u8>>,
    /// Available WASI function names (computed during component generation).
    pub available_wasi_funcs: IndexSet<String>,

    // === Wasm module tracking ===
    /// Map from `ModuleSource` prefix (e.g., "core/allocator") to wasm module name (e.g., "mem").
    /// Functions/globals from these modules are extracted into separate wasm core modules.
    pub wasm_module_sources: IndexMap<String, String>,

    // === Function body translation helpers ===
    /// Pending function bodies: (function index in self.functions, `TirFunction` ref, `TypeTable` ref)
    pub pending_bodies: Vec<PendingFunctionBody>,

    // === Canonical intrinsic registry ===
    /// CM canonical imports registered lazily by WIR synthesis functions via `ensure_canonical`.
    /// Key: structured canonical intrinsic (e.g., `FutureNew(Some(S32))`).
    /// Value: the `WirFuncId` for the registered import.
    pub needed_canonicals: IndexMap<CanonicalIntrinsic, WirFuncId>,
}

/// A function body that needs to be translated from TIR to WIR.
pub struct PendingFunctionBody {
    /// Index into WirContext.functions
    pub wir_func_index: usize,
    /// The TIR function to translate
    pub tir_func: Rc<RefCell<TirFunction>>,
    /// The type table for this function's module
    pub type_table: Rc<RefCell<TypeTable>>,
}

impl<'a> WirContext<'a> {
    /// Create a new `WirContext` from a Project.
    pub fn new(project: &'a Project) -> Self {
        // Collect string literals from all TIR modules (deduped)
        let mut seen: IndexSet<&str> = IndexSet::new();
        let mut string_literals = Vec::new();
        for tir_module in project.tir_modules.values() {
            for s in &tir_module.string_literals {
                if seen.insert(s.as_str()) {
                    string_literals.push(s.clone());
                }
            }
        }

        // Collect bytes literals from all TIR modules (deduped)
        let mut seen_bytes: IndexSet<&[u8]> = IndexSet::new();
        let mut bytes_literals = Vec::new();
        for tir_module in project.tir_modules.values() {
            for b in &tir_module.bytes_literals {
                if seen_bytes.insert(b.as_slice()) {
                    bytes_literals.push(b.clone());
                }
            }
        }

        Self {
            project,
            types: Vec::new(),
            type_map: IndexMap::new(),
            rec_groups: Vec::new(),
            struct_type_map: IndexMap::new(),
            array_type_map: IndexMap::new(),
            array_type_by_name: IndexMap::new(),
            tuple_type_map: IndexMap::new(),
            variant_type_map: IndexMap::new(),
            variant_case_info: IndexMap::new(),
            functions: Vec::new(),
            func_map: IndexMap::new(),
            func_type_ids: Vec::new(),
            imports: Vec::new(),
            import_func_count: 0,
            import_func_map: IndexMap::new(),
            globals: Vec::new(),
            global_map: IndexMap::new(),
            exports: Vec::new(),
            data: Vec::new(),
            string_literal_map: IndexMap::new(),
            bytes_literal_map: IndexMap::new(),
            names: WirNames::default(),
            canonical_closure_types: IndexMap::new(),
            closure_wrapper_funcs: IndexMap::new(),
            canonical_closure_counter: 0,
            string_literals,
            bytes_literals,
            wasm_module_sources: IndexMap::new(),
            available_wasi_funcs: IndexSet::new(),
            pending_bodies: Vec::new(),
            needed_canonicals: IndexMap::new(),
        }
    }

    // === Type Registration ===

    /// Register a type definition and return its `WirTypeId`.
    pub fn register_type(&mut self, fq: String, typedef: WirTypeDef) -> WirTypeId {
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
        let display = fq.clone();
        self.register_type(
            fq,
            WirTypeDef::Func(WirFuncType {
                name: WirName {
                    display,
                    fq: String::new(), // filled from register_type
                },
                params,
                results,
            }),
        )
    }

    /// Look up a struct type by name only (ignoring `module_source`).
    /// Used as fallback when `module_source` doesn't match (e.g., monomorphized
    /// structs where the type's `module_source` is the use site, not the definition site).
    pub fn lookup_struct_by_name(&self, name: &str) -> Option<&WirTypeId> {
        self.struct_type_map
            .iter()
            .find(|(k, _)| k.name == name)
            .map(|(_, v)| v)
    }

    // === Function Registration ===

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
            desc: WirImportDesc::Func {
                type_id,
                name: name.clone(),
            },
        });
        self.import_func_map.insert(fq.clone(), func_id.clone());
        self.func_map.insert(fq, func_id.clone());
        func_id
    }

    /// Register a defined function (with body) and return its `WirFuncId`.
    pub fn register_function(&mut self, func: WirFunction) -> WirFuncId {
        let func_idx =
            DEFINED_FUNC_BASE + u32::try_from(self.functions.len()).expect("too many funcs");
        let fq = func.name.fq.clone();
        let fq_rc: Rc<str> = Rc::from(fq.as_str());
        let func_id = WirFuncId::new(func_idx, fq_rc);
        self.func_map.insert(fq, func_id.clone());
        self.func_type_ids.push(func.type_id.clone());
        self.functions.push(func);
        func_id
    }

    // === Data Section ===

    /// Register a string literal and return its data segment index.
    pub fn register_string_literal(&mut self, s: &str) -> u32 {
        if let Some(&idx) = self.string_literal_map.get(s) {
            return idx;
        }
        let idx = u32::try_from(self.data.len()).expect("too many data segments");
        self.data.push(WirData {
            bytes: s.as_bytes().to_vec(),
            offset: None, // passive segment
        });
        self.string_literal_map.insert(s.to_string(), idx);
        idx
    }

    /// Register a bytes literal and return its data segment index.
    pub fn register_bytes_literal(&mut self, b: &[u8]) -> u32 {
        if let Some(&idx) = self.bytes_literal_map.get(b) {
            return idx;
        }
        let idx = u32::try_from(self.data.len()).expect("too many data segments");
        self.data.push(WirData {
            bytes: b.to_vec(),
            offset: None, // passive segment
        });
        self.bytes_literal_map.insert(b.to_vec(), idx);
        idx
    }

    // === Helpers ===

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
    pub fn get_or_create_canonical_closure_type(
        &mut self,
        param_wirs: Vec<WirType>,
        result_wirs: Vec<WirType>,
    ) -> (WirTypeId, WirTypeId) {
        let key = Self::canonical_closure_key(&param_wirs, &result_wirs);
        if let Some(existing) = self.canonical_closure_types.get(&key) {
            return existing.clone();
        }

        let id = self.canonical_closure_counter;
        self.canonical_closure_counter += 1;

        // Create canonical function type: (ref null struct, params...) -> results
        // The env param must be nullable to accept any struct ref subtype.
        let mut fn_params = vec![WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Struct,
            nullable: true,
        }];
        fn_params.extend(param_wirs.iter().cloned());

        let fn_type_fq = format!("functype/$canonical_closure_fn_{id}");
        let fn_type_id = self.register_func_type(fn_type_fq, fn_params, result_wirs.clone());

        // Create canonical closure struct: { env: (ref null struct), func: (ref $fn_type) }
        let struct_fq = format!("canonical//CanonicalClosure_{id}");
        use crate::wir::{WirField, WirMeta, WirName, WirStructType};
        let struct_type_id = self.register_type(
            struct_fq.clone(),
            WirTypeDef::Struct(WirStructType {
                name: WirName {
                    display: format!("CanonicalClosure_{id}"),
                    fq: struct_fq,
                },
                fields: vec![
                    WirField {
                        name: "env".to_string(),
                        ty: WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Struct,
                            nullable: true,
                        },
                        mutable: false,
                    },
                    WirField {
                        name: "func".to_string(),
                        ty: WirType::Ref {
                            type_id: fn_type_id.clone(),
                            nullable: false,
                        },
                        mutable: false,
                    },
                ],
                meta: WirMeta::default(),
                generic_origin: None,
                newtype_origin: None,
            }),
        );

        self.canonical_closure_types
            .insert(key, (fn_type_id.clone(), struct_type_id.clone()));

        (fn_type_id, struct_type_id)
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
        let key = format!("wasi/{name}");
        if let Some(func_id) = self.func_map.get(&key) {
            return func_id.clone();
        }
        let type_fq = format!("functype//wasi/{name}");
        let type_id = self.register_func_type(type_fq, params, results);
        let wir_name = WirName {
            display: name.clone(),
            fq: key.clone(),
        };
        let func_id = self.register_import_func("wasi".to_string(), name, type_id, wir_name);
        self.needed_canonicals.insert(intrinsic, func_id.clone());
        func_id
    }

    /// Convert a TIR `TypeId` to a `WirType`.
    pub fn type_id_to_wir_type(&self, type_table: &TypeTable, type_id: TypeId) -> WirType {
        use crate::tir::{PrimitiveType, ResolvedType};
        match type_table.get(type_id) {
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
            ResolvedType::Never => WirType::Unit, // placeholder
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                let struct_name = StructName::new(module_source.clone(), name.clone());
                // Special case: String struct
                let lookup_name = if name == "String" {
                    StructName::new(ModuleSource::string(), "String".to_string())
                } else {
                    struct_name
                };
                if let Some(type_id) = self.struct_type_map.get(&lookup_name) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else if let Some(type_id) = self.lookup_struct_by_name(name) {
                    // Fallback: monomorphized structs may have a different module_source
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    // Fallback: use abstract struct ref
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: false,
                    }
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => {
                // Look up Array<T> struct type
                let elem_type_name = type_table.mangle_type_name(type_args[0]);
                let array_fq = format!("core:prelude//Array<{elem_type_name}>");
                if let Some(type_id) = self.type_map.get(&array_fq) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    // Fallback: resolve newtypes in element type (e.g., FieldName → String)
                    let resolved_name =
                        type_table.mangle_type_name_resolving_newtypes(type_args[0]);
                    if resolved_name != elem_type_name {
                        let resolved_fq = format!("core:prelude//Array<{resolved_name}>");
                        if let Some(type_id) = self.type_map.get(&resolved_fq) {
                            return WirType::Ref {
                                type_id: type_id.clone(),
                                nullable: false,
                            };
                        }
                    }
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: false,
                    }
                }
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // GenericInstance.name is the base name (e.g., "Box"), but after
                // monomorphization the struct is registered with the mangled name
                // (e.g., "Box<i32>"). Build the mangled name to look up.
                let mangled = type_table.mangle_type_name(type_id);
                let struct_name = StructName::new(module_source.clone(), mangled.clone());
                if let Some(tid) = self.struct_type_map.get(&struct_name) {
                    WirType::Ref {
                        type_id: tid.clone(),
                        nullable: false,
                    }
                } else if let Some(tid) = self.lookup_struct_by_name(&mangled) {
                    // Fallback: monomorphized structs may have a different module_source
                    WirType::Ref {
                        type_id: tid.clone(),
                        nullable: false,
                    }
                } else {
                    // Try as variant (in type_map)
                    let variant_fq = format!("{module_source}//{mangled}");
                    if let Some(tid) = self.type_map.get(&variant_fq) {
                        WirType::Ref {
                            type_id: tid.clone(),
                            nullable: false,
                        }
                    } else {
                        // Fallback: resolve newtypes in type_args and retry
                        let resolved_args: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name_resolving_newtypes(*t))
                            .collect();
                        let resolved_mangled =
                            crate::name::mangle_generic_name(name, &resolved_args);
                        if resolved_mangled != mangled {
                            let resolved_sn =
                                StructName::new(module_source.clone(), resolved_mangled.clone());
                            if let Some(tid) = self.struct_type_map.get(&resolved_sn) {
                                return WirType::Ref {
                                    type_id: tid.clone(),
                                    nullable: false,
                                };
                            }
                            if let Some(tid) = self.lookup_struct_by_name(&resolved_mangled) {
                                return WirType::Ref {
                                    type_id: tid.clone(),
                                    nullable: false,
                                };
                            }
                            let resolved_fq = format!("{module_source}//{resolved_mangled}");
                            if let Some(tid) = self.type_map.get(&resolved_fq) {
                                return WirType::Ref {
                                    type_id: tid.clone(),
                                    nullable: false,
                                };
                            }
                        }
                        WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Struct,
                            nullable: false,
                        }
                    }
                }
            }
            ResolvedType::BuiltinArray(elem_type_id) => {
                if let Some(type_id) = self.array_type_map.get(elem_type_id) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    // Fallback: look up by element type name (handles cross-module TypeIds)
                    let elem_name = type_table.mangle_type_name(*elem_type_id);
                    if let Some(type_id) = self.array_type_by_name.get(&elem_name) {
                        WirType::Ref {
                            type_id: type_id.clone(),
                            nullable: false,
                        }
                    } else {
                        // Fallback: resolve newtypes in element type
                        let resolved_name =
                            type_table.mangle_type_name_resolving_newtypes(*elem_type_id);
                        if resolved_name != elem_name
                            && let Some(type_id) = self.array_type_by_name.get(&resolved_name)
                        {
                            return WirType::Ref {
                                type_id: type_id.clone(),
                                nullable: false,
                            };
                        }
                        WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Array,
                            nullable: false,
                        }
                    }
                }
            }
            // Option<T> is now handled as GenericInstance (SubtypeHierarchy variant)
            // TODO: Future optimization: use NullableRef for Option<T> when T is non-nullable
            ResolvedType::Enum {
                name,
                module_source,
                ..
            } => {
                let fq = format!("{module_source}//enum:{name}");
                if let Some(type_id) = self.type_map.get(&fq) {
                    WirType::Enum {
                        type_id: type_id.clone(),
                    }
                } else {
                    WirType::I32 // enums are i32 at Wasm level
                }
            }
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => {
                let fq = format!("{module_source}//{name}");
                if let Some(type_id) = self.type_map.get(&fq) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: false,
                    }
                }
            }
            ResolvedType::Tuple(elements) => {
                if let Some(type_id) = self.tuple_type_map.get(elements) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: false,
                    }
                }
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For reference types (structs, variants, strings, arrays), the inner type
                // is already a ref type in Wasm GC, so &T = T at the Wasm level.
                // For value types (primitives, enums), we need Box<T> to provide
                // a mutable location for &mut T semantics.
                let inner_wir = self.type_id_to_wir_type(type_table, *inner);
                match &inner_wir {
                    WirType::Ref { .. } | WirType::AbstractRef { .. } => inner_wir,
                    _ => {
                        // Value type: use Box<T> for reference semantics.
                        // Resolve through newtypes to find the base type name.
                        let base_inner = type_table.resolve_newtype_base(*inner);
                        let inner_name = type_table.mangle_type_name(base_inner);
                        let box_name = crate::name::mangle_generic_name(
                            "Box",
                            std::slice::from_ref(&inner_name),
                        );
                        if let Some(tid) = self.lookup_struct_by_name(&box_name) {
                            WirType::Ref {
                                type_id: tid.clone(),
                                nullable: false,
                            }
                        } else {
                            // Fallback: treat as value type (no boxing available)
                            inner_wir
                        }
                    }
                }
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
                self.type_id_to_wir_type(type_table, *base_type)
            }
            // Generic resource types (Future<T>, Stream<T>, etc.) are opaque i32 handles
            ResolvedType::GenericResource { .. } => WirType::I32,
            _ => {
                // For any unhandled types, use i32 as placeholder
                WirType::I32
            }
        }
    }

    /// Check if a WIR type ID refers to a variant type.
    pub fn is_variant_type(&self, type_id: &WirTypeId) -> bool {
        let idx = type_id.index() as usize;
        idx < self.types.len() && matches!(&self.types[idx], WirTypeDef::Variant(_))
    }

    /// Get the number of fields in a WIR struct type.
    pub fn get_struct_field_count(&self, type_id: &WirTypeId) -> u32 {
        let idx = type_id.index() as usize;
        if idx < self.types.len() {
            match &self.types[idx] {
                WirTypeDef::Struct(s) => u32::try_from(s.fields.len()).unwrap(),
                _ => 0,
            }
        } else {
            0
        }
    }

    // === Build Final WirModule ===

    /// Consume this context and produce the final `WirModule`.
    pub fn into_wir_module(self) -> WirModule {
        let functions = self.functions;
        let globals = self.globals;
        let global_map = &self.global_map;

        // Extract functions and globals from #![wasm_module("...")] sources
        // into separate WasmModuleInfo structures.
        let mut wasm_modules: IndexMap<String, crate::wir::WasmModuleInfo> = IndexMap::new();
        let mut dead_type_indices: IndexSet<u32> = IndexSet::new();
        let mut dead_func_indices: IndexSet<u32> = IndexSet::new();
        let mut dead_global_indices: IndexSet<u32> = IndexSet::new();

        for (source_prefix, wasm_mod_name) in &self.wasm_module_sources {
            let mut mod_functions = Vec::new();
            let mut mod_globals = Vec::new();
            let mut mod_global_name_to_index = IndexMap::new();

            // Find functions belonging to this wasm module (keep in list, mark as dead)
            for (i, func) in functions.iter().enumerate() {
                if !func.name.fq.starts_with(source_prefix) {
                    continue;
                }
                let func_idx = u32::try_from(i).unwrap();
                dead_func_indices.insert(func_idx);
                dead_type_indices.insert(func.type_id.index());

                let export_name = func.export_name.clone().unwrap_or_else(|| {
                    func.name
                        .fq
                        .strip_prefix(source_prefix)
                        .and_then(|s| s.strip_prefix('/'))
                        .unwrap_or(&func.name.fq)
                        .to_string()
                });
                let body = func.body.clone().unwrap_or_default();

                // Collect referenced globals
                let mut referenced_globals = IndexMap::new();
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

        // Build parallel list of stored WirFuncId indices for defined functions.
        // Each entry is the WirFuncId.index() that Call instructions use to call that function.
        // This may differ from `final_import_count + list_pos` when `ensure_canonical` adds
        // imports after functions were registered (which shifts the import count).
        let func_wir_indices: Vec<u32> = functions
            .iter()
            .map(|f| {
                self.func_map
                    .get(&f.name.fq)
                    .map_or(0, super::super::wir::WirFuncId::index)
            })
            .collect();

        WirModule {
            types: self.types,
            rec_groups: self.rec_groups,
            imports: self.imports,
            functions,
            globals,
            exports: self.exports,
            elements: Vec::new(), // TODO: element section
            memories: Vec::new(),
            data: self.data,
            branch_hints: Vec::new(),
            names: self.names,
            component: WirComponent::default(),
            variant_case_info: self.variant_case_info,
            entry_point_path: Some(self.project.entry_module_source.to_string()),
            wasm_modules,
            dead_type_indices,
            dead_func_indices,
            dead_global_indices,
            needed_canonicals,
            func_wir_indices,
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
        WirInstr::GlobalGet { name } | WirInstr::GlobalSet { name, .. } => {
            out.insert(name.fq.clone(), ());
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| collect_referenced_globals_instr(child, out));
}
