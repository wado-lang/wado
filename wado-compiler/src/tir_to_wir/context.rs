//! WIR builder context — accumulates types, functions, and other module-level
//! entries during the `tir_to_wir` translation, then produces a final `WirModule`.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::name::{ModuleSource, StructName};
use crate::project::Project;
use crate::tir::{TirFunction, TirModule, TypeId, TypeTable};
use crate::wir::{
    WirComponent, WirData, WirExport, WirExportDesc, WirFuncId, WirFuncType, WirFunction,
    WirGlobal, WirImport, WirImportDesc, WirModule, WirName, WirNames, WirRecGroup, WirType,
    WirTypeDef, WirTypeId,
};

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
    /// Exports.
    pub exports: Vec<WirExport>,
    /// Data segments (string literals).
    pub data: Vec<WirData>,
    /// String literal dedup: string → data segment index.
    pub string_literal_map: IndexMap<String, u32>,
    /// Name section entries.
    pub names: WirNames,

    // === Scratch state ===
    /// Collected string literals (from all TIR modules).
    pub string_literals: Vec<String>,
    /// Available WASI function names (computed during component generation).
    pub available_wasi_funcs: IndexSet<String>,

    // === Function body translation helpers ===
    /// Pending function bodies: (function index in self.functions, `TirFunction` ref, `TypeTable` ref)
    pub pending_bodies: Vec<PendingFunctionBody>,
}

/// A function body that needs to be translated from TIR to WIR.
pub struct PendingFunctionBody {
    /// Index into WirContext.functions
    pub wir_func_index: usize,
    /// The TIR function to translate
    pub tir_func: Rc<RefCell<TirFunction>>,
    /// The type table for this function's module
    pub type_table: Rc<RefCell<TypeTable>>,
    /// The module source for this function
    pub module_source: ModuleSource,
}

impl<'a> WirContext<'a> {
    /// Create a new `WirContext` from a Project.
    pub fn new(project: &'a Project) -> Self {
        // Collect string literals from all TIR modules (deduped)
        let mut string_literals = Vec::new();
        for tir_module in project.tir_modules.values() {
            for s in &tir_module.string_literals {
                if !string_literals.contains(s) {
                    string_literals.push(s.clone());
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
            exports: Vec::new(),
            data: Vec::new(),
            string_literal_map: IndexMap::new(),
            names: WirNames::default(),
            string_literals,
            available_wasi_funcs: IndexSet::new(),
            pending_bodies: Vec::new(),
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

    /// Look up a type by fully-qualified name.
    pub fn lookup_type(&self, fq: &str) -> Option<&WirTypeId> {
        self.type_map.get(fq)
    }

    /// Look up a struct type by name only (ignoring module_source).
    /// Used as fallback when module_source doesn't match (e.g., monomorphized
    /// structs where the type's module_source is the use site, not the definition site).
    fn lookup_struct_by_name(&self, name: &str) -> Option<&WirTypeId> {
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
            self.import_func_count + u32::try_from(self.functions.len()).expect("too many funcs");
        let fq = func.name.fq.clone();
        let fq_rc: Rc<str> = Rc::from(fq.as_str());
        let func_id = WirFuncId::new(func_idx, fq_rc);
        self.func_map.insert(fq, func_id.clone());
        self.func_type_ids.push(func.type_id.clone());
        self.functions.push(func);
        func_id
    }

    /// Look up a function by fully-qualified name.
    pub fn lookup_func(&self, fq: &str) -> Option<&WirFuncId> {
        self.func_map.get(fq)
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

    // === Export Registration ===

    /// Register a function export.
    pub fn register_export(&mut self, name: String, func_id: WirFuncId) {
        self.exports.push(WirExport {
            name,
            desc: WirExportDesc::Func { func_id },
        });
    }

    // === Helpers ===

    /// Get the entry module TIR.
    pub fn entry_tir(&self) -> &TirModule {
        self.project.entry_module()
    }

    /// Get the entry module's type table (borrowed).
    pub fn entry_type_table(&self) -> std::cell::Ref<'_, TypeTable> {
        self.entry_tir().type_table.borrow()
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
                    StructName::new(
                        ModuleSource::core("prelude/string.wado"),
                        "String".to_string(),
                    )
                } else {
                    struct_name
                };
                if let Some(type_id) = self.struct_type_map.get(&lookup_name) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: true,
                    }
                } else if let Some(type_id) = self.lookup_struct_by_name(name) {
                    // Fallback: monomorphized structs may have a different module_source
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: true,
                    }
                } else {
                    // Fallback: use abstract struct ref
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: true,
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
                        nullable: true,
                    }
                } else {
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: true,
                    }
                }
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args: _,
            } => {
                // GenericInstance.name is the base name (e.g., "Box"), but after
                // monomorphization the struct is registered with the mangled name
                // (e.g., "Box<i32>"). Build the mangled name to look up.
                let mangled = type_table.mangle_type_name(type_id);
                let struct_name = StructName::new(module_source.clone(), mangled.clone());
                if let Some(tid) = self.struct_type_map.get(&struct_name) {
                    WirType::Ref {
                        type_id: tid.clone(),
                        nullable: true,
                    }
                } else if let Some(tid) = self.lookup_struct_by_name(&mangled) {
                    // Fallback: monomorphized structs may have a different module_source
                    WirType::Ref {
                        type_id: tid.clone(),
                        nullable: true,
                    }
                } else {
                    // Try as variant (in type_map)
                    let variant_fq = format!("{module_source}//{mangled}");
                    if let Some(tid) = self.type_map.get(&variant_fq) {
                        WirType::Ref {
                            type_id: tid.clone(),
                            nullable: true,
                        }
                    } else {
                        WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Struct,
                            nullable: true,
                        }
                    }
                }
            }
            ResolvedType::BuiltinArray(elem_type_id) => {
                if let Some(type_id) = self.array_type_map.get(elem_type_id) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: true,
                    }
                } else {
                    // Fallback: look up by element type name (handles cross-module TypeIds)
                    let elem_name = type_table.mangle_type_name(*elem_type_id);
                    if let Some(type_id) = self.array_type_by_name.get(&elem_name) {
                        WirType::Ref {
                            type_id: type_id.clone(),
                            nullable: true,
                        }
                    } else {
                        WirType::AbstractRef {
                            heap_type: crate::wir::WirAbstractHeapType::Array,
                            nullable: true,
                        }
                    }
                }
            }
            ResolvedType::Option(inner) => {
                // Option<T> is nullable ref at Wasm level
                let inner_wir = self.type_id_to_wir_type(type_table, *inner);
                match inner_wir {
                    WirType::Ref { type_id, .. } => WirType::Ref {
                        type_id,
                        nullable: true,
                    },
                    _ => WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: true,
                    },
                }
            }
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
                        nullable: true,
                    }
                } else {
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: true,
                    }
                }
            }
            ResolvedType::Tuple(elements) => {
                if let Some(type_id) = self.tuple_type_map.get(elements) {
                    WirType::Ref {
                        type_id: type_id.clone(),
                        nullable: true,
                    }
                } else {
                    WirType::AbstractRef {
                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                        nullable: true,
                    }
                }
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // References are the same as the inner type at Wasm level
                self.type_id_to_wir_type(type_table, *inner)
            }
            ResolvedType::Function { .. } => {
                // Function references use abstract funcref
                WirType::AbstractRef {
                    heap_type: crate::wir::WirAbstractHeapType::Func,
                    nullable: true,
                }
            }
            ResolvedType::Newtype { base_type, .. } => {
                // Newtypes resolve to their base type
                self.type_id_to_wir_type(type_table, *base_type)
            }
            _ => {
                // For any unhandled types, use i32 as placeholder
                WirType::I32
            }
        }
    }

    // === Build Final WirModule ===

    /// Consume this context and produce the final `WirModule`.
    pub fn into_wir_module(self) -> WirModule {
        WirModule {
            types: self.types,
            rec_groups: self.rec_groups,
            imports: self.imports,
            functions: self.functions,
            globals: self.globals,
            exports: self.exports,
            elements: Vec::new(), // TODO: element section
            data: self.data,
            branch_hints: Vec::new(),
            names: self.names,
            component: WirComponent::default(),
            variant_case_info: self.variant_case_info,
            entry_point_path: Some(self.project.entry_module_source.to_string()),
        }
    }
}
