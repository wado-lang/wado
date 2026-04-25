//! WIR builder context — accumulates types, functions, and other module-level
//! entries during the `tir_to_wir` translation, then produces a final `WirPackage`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::flat_package::FlatPackage;
use crate::name::{ModuleSource, StructName};
use crate::tir::{TirFunction, TypeId, TypeTable};
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

/// Builder context for the `tir_to_wir` translation.
///
/// Accumulates all WIR entities and provides lookup maps for resolving
/// type and function references during translation.
pub struct WirContext<'a> {
    /// Reference to the linked package data.
    pub package: &'a FlatPackage,

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
    pub func_map: IndexMap<String, WirFuncId>,
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
    /// String literal dedup: string → data segment index.
    pub string_literal_map: IndexMap<String, u32>,
    /// Bytes literal dedup: bytes → data segment index.
    pub bytes_literal_map: IndexMap<Vec<u8>, u32>,
    /// Name section entries.
    pub names: WirNames,

    /// Map from function signature string to canonical closure info.
    /// Key: stringified signature (e.g., "(i32, i32) -> i32")
    /// Value: (`canonical_fn_type_id`, `canonical_closure_struct_type_id`)
    pub canonical_closure_types: IndexMap<String, (WirTypeId, WirTypeId)>,
    /// Map from closure `(module_source, functor_id)` to canonical wrapper function `WirFuncId`.
    /// Keyed by module source + functor ID because functor IDs are per-module, not globally unique.
    pub closure_wrapper_funcs: IndexMap<(ModuleSource, u32), WirFuncId>,
    /// Counter for canonical closure type naming.
    pub canonical_closure_counter: u32,

    /// Collected string literals (from all TIR modules).
    pub string_literals: Vec<String>,
    /// Collected bytes literals (from all TIR modules).
    pub bytes_literals: Vec<Vec<u8>>,
    /// Available WASI function names (computed during component generation).
    pub available_wasi_funcs: IndexSet<String>,

    /// Map from `ModuleSource` to wasm module name (e.g., "mem").
    /// Functions/globals from these modules are extracted into separate wasm core modules.
    pub wasm_module_sources: IndexMap<ModuleSource, String>,

    /// Pending function bodies: (function index in self.functions, `TirFunction` ref, `TypeTable` ref)
    pub pending_bodies: Vec<PendingFunctionBody>,

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
    /// Create a new `WirContext` from a `FlatPackage`.
    pub fn new(package: &'a FlatPackage) -> Self {
        // Collect string literals (deduped)
        let mut seen: IndexSet<&str> = IndexSet::default();
        let mut string_literals = Vec::new();
        for s in &package.string_literals {
            if seen.insert(s.as_str()) {
                string_literals.push(s.clone());
            }
        }

        // Collect bytes literals (deduped)
        let mut seen_bytes: IndexSet<&[u8]> = IndexSet::default();
        let mut bytes_literals = Vec::new();
        for b in &package.bytes_literals {
            if seen_bytes.insert(b.as_slice()) {
                bytes_literals.push(b.clone());
            }
        }

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
            func_type_ids: Vec::new(),
            imports: Vec::new(),
            import_func_count: 0,
            import_func_map: IndexMap::default(),
            globals: Vec::new(),
            global_map: IndexMap::default(),
            exports: Vec::new(),
            data: Vec::new(),
            string_literal_map: IndexMap::default(),
            bytes_literal_map: IndexMap::default(),
            names: WirNames {
                module_name: Some(package.module_name.clone()),
                ..WirNames::default()
            },
            canonical_closure_types: IndexMap::default(),
            closure_wrapper_funcs: IndexMap::default(),
            canonical_closure_counter: 0,
            string_literals,
            bytes_literals,
            wasm_module_sources: IndexMap::<ModuleSource, String>::default(),
            available_wasi_funcs: IndexSet::default(),
            pending_bodies: Vec::new(),
            needed_canonicals: IndexMap::default(),
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
        let fn_type_id = self.register_func_type(fn_type_fq, fn_params, result_wirs);

        // Create canonical closure struct: { env: (ref null struct), func: (ref $fn_type) }
        let struct_fq = format!("canonical//CanonicalClosure_{id}");
        use crate::wir::{WirField, WirMeta, WirName, WirStructType};
        let struct_type_id = self.register_type(
            struct_fq.clone(),
            WirTypeDef::Struct(WirStructType {
                name: WirName { fq: struct_fq },
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
        let wir_name = WirName { fq: key };
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
                // String is always at ModuleSource::string().
                let lookup_module = if name == "String" {
                    ModuleSource::string()
                } else {
                    module_source.clone()
                };
                let lookup_name = StructName::new(lookup_module, name.clone());
                if let Some(type_id) = self.struct_type_map.get(&lookup_name) {
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
                type_args: elements,
                module_source,
            } if TypeTable::is_tuple_type(name, module_source) => {
                if let Some(type_id) = self.tuple_type_map.get(elements) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: false,
                    }
                } else {
                    // Cross-module TypeId mismatch: fall back to matching by element WIR types.
                    let elem_wir_types: Vec<WirType> = elements
                        .iter()
                        .map(|e| self.type_id_to_wir_type(type_table, *e))
                        .collect();
                    let found = self.tuple_type_map.iter().find(|(key_elems, _)| {
                        key_elems.len() == elem_wir_types.len()
                            && key_elems
                                .iter()
                                .zip(elem_wir_types.iter())
                                .all(|(k, w)| self.type_id_to_wir_type(type_table, *k) == *w)
                    });
                    if let Some((_, type_id)) = found {
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
                } else {
                    // Try as variant (in type_map)
                    let variant_fq = format!("{module_source}//{mangled}");
                    if let Some(tid) = self.type_map.get(&variant_fq) {
                        WirType::Ref {
                            type_id: tid.clone(),
                            nullable: false,
                        }
                    } else {
                        // Resolve newtypes in type_args and retry
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
            // Option<T> is handled as GenericInstance (variant).
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
                        let box_module = self
                            .package
                            .type_table
                            .borrow()
                            .box_module_source
                            .clone()
                            .unwrap_or_else(ModuleSource::prelude);
                        let box_sn = StructName::new(box_module, box_name);
                        if let Some(tid) = self.struct_type_map.get(&box_sn) {
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
            // Non-generic resources are opaque i32 handles
            ResolvedType::Resource { .. } => WirType::I32,
            // Flags are bitmasks stored as i32
            ResolvedType::Flags { .. } => WirType::I32,
            // These should never reach codegen — must be resolved by monomorphization
            ResolvedType::TypeParam { name, index } => {
                panic!("unsubstituted TypeParam `{name}` (index {index}) reached codegen")
            }
            ResolvedType::TypePack { name, index } => {
                panic!("unsubstituted TypePack `..{name}` (index {index}) reached codegen")
            }
            ResolvedType::AssocTypeProjection { assoc_name, .. } => {
                panic!("unsubstituted AssocTypeProjection `{assoc_name}` reached codegen")
            }
            ResolvedType::Error | ResolvedType::Unknown | ResolvedType::Reactive(_) => WirType::I32,
        }
    }

    /// Get the number of fields in a WIR struct type.
    #[allow(dead_code)]
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

    /// Find a tuple WIR type that matches the given TIR elements by WIR type compatibility.
    ///
    /// When CM binding synthesis creates tuple types, the `TypeIds` may not exactly match
    /// the ones in `tuple_type_map`. This fallback searches by matching WIR types of elements.
    pub fn find_tuple_type_for_elements(
        &self,
        type_table: &crate::tir::TypeTable,
        elements: &[crate::tir::TirExpr],
    ) -> Option<WirTypeId> {
        let elem_wir_types: Vec<WirType> = elements
            .iter()
            .map(|e| self.type_id_to_wir_type(type_table, e.type_id))
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
        elements: &[crate::tir::TirExpr],
    ) -> Option<WirTypeId> {
        let elem_wir_types: Vec<WirType> = elements
            .iter()
            .map(|e| self.type_id_to_wir_type(type_table, e.type_id))
            .filter(|t| !matches!(t, WirType::Unit))
            .collect();
        if elem_wir_types.is_empty() {
            return None;
        }
        let elem_names: Vec<String> = elements
            .iter()
            .filter(|e| {
                !matches!(
                    self.type_id_to_wir_type(type_table, e.type_id),
                    WirType::Unit
                )
            })
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
        });
        let type_id = self.register_type(display, struct_def);
        // Register in tuple_type_map using the TIR element TypeIds
        let elem_type_ids: Vec<crate::tir::TypeId> = elements
            .iter()
            .filter(|e| {
                !matches!(
                    self.type_id_to_wir_type(type_table, e.type_id),
                    WirType::Unit
                )
            })
            .map(|e| e.type_id)
            .collect();
        self.tuple_type_map.insert(elem_type_ids, type_id.clone());
        Some(type_id)
    }

    /// Consume this context and produce the final `WirPackage`.
    pub fn into_wir_package(self) -> WirPackage {
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
