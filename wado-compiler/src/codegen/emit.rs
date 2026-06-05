//! WIR emission — converts a `WirPackage` into core Wasm module binary bytes.
//!
//! This module handles the mechanical translation from WIR's tree-structured
//! representation to flat Wasm instructions and binary encoding.

use crate::hashmap::{IndexMap, IndexSet};

use crate::wir::{
    WirAbstractHeapType, WirArrayType, WirExportDesc, WirFuncType, WirFunction, WirImportDesc,
    WirInstr, WirPackage, WirStructType, WirType, WirTypeDef, WirVariantType,
};

use wasm_encoder::{
    AbstractHeapType, BlockType, BranchHint, BranchHints, CodeSection, CompositeInnerType,
    CompositeType, ConstExpr, DataCountSection, DataSection, ElementSection, Elements, ExportKind,
    ExportSection, FieldType, Function, FunctionSection, GlobalSection, GlobalType, HeapType,
    ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module, NameMap, NameSection,
    RefType, StorageType, StructType, SubType, TypeSection, ValType,
};

/// Emit a core Wasm module from a `WirPackage`.
pub fn emit_core_module(
    wir: &WirPackage,
    strip_names: bool,
    codegen_flags: crate::codegen_flags::CodegenFlags,
) -> Vec<u8> {
    let mut emitter = WirEmitter::new(wir, strip_names, codegen_flags);
    emitter.emit()
}

/// Emitter state for converting `WirPackage` to Wasm binary.
struct WirEmitter<'a> {
    wir: &'a WirPackage,
    /// Map from `WirTypeDef` index → Wasm type section index.
    /// For structs: maps to the single struct type index.
    /// For variants: maps to the base type index (case subtypes follow immediately).
    /// For enums/flags: no entry (they don't produce Wasm types).
    /// For arrays: maps to the array type index.
    /// For funcs: maps to the func type index.
    type_index_map: IndexMap<u32, u32>,
    /// Variant case type indices: `wir_type_idx` → vec of (`case_index`, `wasm_type_idx`).
    variant_case_types: IndexMap<u32, Vec<(u32, u32)>>,
    /// Struct field indices: `wir_type_idx` → map of `field_name` → `field_index`.
    struct_field_map: IndexMap<u32, IndexMap<String, u32>>,
    /// Function index offset (import count).
    func_index_offset: u32,
    /// Map from `WirFuncId` index → Wasm function index.
    func_index_map: IndexMap<u32, u32>,
    /// Map from global fq name to Wasm global index.
    global_name_map: IndexMap<String, u32>,
    /// Local name map for current function.
    current_locals: IndexMap<String, u32>,
    /// Locals declared with ref types (need `ref.as_non_null` on `local.get`).
    ref_locals: IndexSet<String>,
    /// Names of scratch locals (used by `ArrayClone` / `ArrayCopy` emission)
    /// that have already been collected for the current function. Lets the
    /// pre-emission walker push each unique scratch name once even when the
    /// same (dst, src) type pair appears in multiple `ArrayCopy` sites or
    /// the same `type_id` appears in multiple `ArrayClone` sites — without
    /// the de-dup, every visit grew the Wasm locals table.
    scratch_local_names: IndexSet<String>,
    /// Next local index for current function.
    next_local: u32,
    /// Wasm type section counter.
    next_type_idx: u32,
    /// Pre-assigned variant type info: `wir_type_idx` → (`base_wasm_idx`, vec of (`case_idx`, `wasm_idx`)).
    variant_pre_assigned: IndexMap<u32, (u32, Vec<(u32, u32)>)>,
    /// Branch hints for the current function being emitted.
    current_branch_hints: Vec<(u32, bool)>,
    /// All collected branch hints: (`func_index`, vec of (`instr_offset`, likely)).
    all_branch_hints: Vec<(u32, Vec<(u32, bool)>)>,
    /// When true, strip debug name sections for smaller binary size (-Os).
    strip_names: bool,
    /// Fine-grained codegen feature flags (from the CLI's `-f <flag>` option).
    codegen_flags: crate::codegen_flags::CodegenFlags,
}

impl<'a> WirEmitter<'a> {
    fn new(
        wir: &'a WirPackage,
        strip_names: bool,
        codegen_flags: crate::codegen_flags::CodegenFlags,
    ) -> Self {
        Self {
            wir,
            type_index_map: IndexMap::default(),
            variant_case_types: IndexMap::default(),
            struct_field_map: IndexMap::default(),
            func_index_offset: 0,
            func_index_map: IndexMap::default(),
            global_name_map: IndexMap::default(),
            current_locals: IndexMap::default(),
            ref_locals: IndexSet::default(),
            scratch_local_names: IndexSet::default(),
            next_local: 0,
            next_type_idx: 0,
            variant_pre_assigned: IndexMap::default(),
            current_branch_hints: Vec::new(),
            all_branch_hints: Vec::new(),
            strip_names,
            codegen_flags,
        }
    }

    fn emit(&mut self) -> Vec<u8> {
        let mut module = Module::new();

        // 1. Type section
        let types = self.emit_type_section();
        module.section(&types);

        // 2. Import section
        let (imports, import_func_count) = self.emit_import_section();
        if import_func_count > 0 || self.has_memory_import() {
            module.section(&imports);
        }
        self.func_index_offset = import_func_count;

        // Build function index map
        self.build_func_index_map();

        // 3. Function section
        let funcs = self.emit_function_section();
        module.section(&funcs);

        // 4. Memory section (for standalone modules like the memory module)
        if !self.wir.memories.is_empty() {
            let memories = self.emit_memory_section();
            module.section(&memories);
        }

        // 5. Global section
        self.build_global_name_map();
        if !self.wir.globals.is_empty() {
            let globals = self.emit_global_section();
            module.section(&globals);
        }

        // 6. Export section
        let exports = self.emit_export_section();
        module.section(&exports);

        // 7. Element section (declarative for ref.func)
        let ref_func_indices = self.collect_ref_func_indices();
        if !ref_func_indices.is_empty() {
            let mut elements = ElementSection::new();
            elements.declared(Elements::Functions(std::borrow::Cow::Borrowed(
                &ref_func_indices,
            )));
            module.section(&elements);
        }

        // 8. Data count section (for passive data segments)
        if !self.wir.data.is_empty() {
            let data_count = DataCountSection {
                count: u32::try_from(self.wir.data.len()).unwrap(),
            };
            module.section(&data_count);
        }

        // 9. Code section
        let code = self.emit_code_section();

        // Branch hints section (must appear before code section)
        if !self.all_branch_hints.is_empty() {
            let mut hints = BranchHints::new();
            for (func_idx, func_hints) in &self.all_branch_hints {
                hints.function_hints(
                    *func_idx,
                    func_hints.iter().map(|(offset, likely)| BranchHint {
                        branch_func_offset: *offset,
                        branch_hint_value: u32::from(*likely),
                    }),
                );
            }
            module.section(&hints);
        }

        module.section(&code);

        // 10. Data section
        if !self.wir.data.is_empty() {
            let data = self.emit_data_section();
            module.section(&data);
        }

        // 11. Name section (optional)
        if !self.strip_names
            && (self.wir.names.module_name.is_some()
                || !self.wir.names.function_names.is_empty()
                || !self.wir.functions.is_empty())
        {
            let names = self.emit_name_section();
            module.section(&names);
        }

        module.finish()
    }

    // === Type Section ===

    fn emit_type_section(&mut self) -> TypeSection {
        let mut types = TypeSection::new();

        // Pre-assign Wasm type indices for ALL WIR types before emission.
        // This ensures `resolve_type_index` returns correct indices even for
        // forward references (e.g., array<Pair> referencing a Pair struct
        // that hasn't been emitted yet).
        self.pre_assign_type_indices();

        // Find func types that must go in the rec group (referenced from struct fields)
        let gc_func_types = self.find_gc_referenced_func_types();

        // Emit GC types (structs, arrays, variants) in a single `rec` group
        // so that forward references between them are allowed by the Wasm GC type system.
        // Function types are normally standalone (outside rec group) for component model
        // compatibility — types in a rec group are not structurally equivalent to
        // standalone types, which breaks import/export matching.
        // Exception: func types referenced from struct fields (e.g., canonical closure
        // func types) must be in the rec group to avoid forward reference errors.
        let mut gc_subtypes: Vec<SubType> = Vec::new();

        for (wir_idx, typedef) in self.wir.types.iter().enumerate() {
            let wir_idx = u32::try_from(wir_idx).unwrap();
            match typedef {
                WirTypeDef::Struct(_) if self.wir.variant_case_info.contains_key(&wir_idx) => {
                    // Variant case struct — emitted inline as part of its parent variant
                }
                WirTypeDef::Struct(s) => {
                    self.build_struct_subtype(s, wir_idx, &mut gc_subtypes);
                }
                WirTypeDef::Variant(v) => {
                    self.build_variant_subtypes(v, wir_idx, &mut gc_subtypes);
                }
                WirTypeDef::Array(a) => {
                    self.build_array_subtype(a, wir_idx, &mut gc_subtypes);
                }
                WirTypeDef::Func(f) if gc_func_types.contains(&wir_idx) => {
                    // Func type referenced from a GC struct — include in rec group
                    self.build_func_subtype(f, wir_idx, &mut gc_subtypes);
                }
                WirTypeDef::Enum(_) | WirTypeDef::Flags(_) | WirTypeDef::Func(_) => {}
            }
        }

        if !gc_subtypes.is_empty() {
            types.ty().rec(gc_subtypes);
        }

        // Emit function types standalone (outside rec group), excluding gc-referenced ones
        for (wir_idx, typedef) in self.wir.types.iter().enumerate() {
            let wir_idx = u32::try_from(wir_idx).unwrap();
            if let WirTypeDef::Func(f) = typedef
                && !gc_func_types.contains(&wir_idx)
            {
                self.emit_standalone_func_type(&mut types, f, wir_idx);
            }
        }

        types
    }

    /// Pre-assign Wasm type indices for all WIR types.
    ///
    /// GC types (structs, arrays, variants) are assigned first (they go in a rec group),
    /// then function types (standalone). This matches the emission order in `emit_type_section`.
    ///
    /// Exception: func types referenced by struct fields (e.g., canonical closure func types)
    /// must also go in the rec group to avoid invalid forward references.
    fn pre_assign_type_indices(&mut self) {
        let mut next_idx = 0u32;

        // Find func types referenced from struct fields — these must go in the rec group
        let gc_func_types = self.find_gc_referenced_func_types();

        // Pass 1: GC types (structs, arrays, variants) + func types referenced from GC types
        for (wir_idx, typedef) in self.wir.types.iter().enumerate() {
            let wir_idx = u32::try_from(wir_idx).unwrap();
            match typedef {
                WirTypeDef::Struct(_) if self.wir.variant_case_info.contains_key(&wir_idx) => {
                    // Variant case struct — index assigned by the parent variant below
                }
                WirTypeDef::Struct(_) | WirTypeDef::Array(_) => {
                    self.type_index_map.insert(wir_idx, next_idx);
                    next_idx += 1;
                }
                WirTypeDef::Variant(v) => {
                    {
                        // Base type
                        self.type_index_map.insert(wir_idx, next_idx);
                        let base_idx = next_idx;
                        next_idx += 1;

                        // Case subtypes
                        let mut case_types = Vec::new();
                        for case in &v.cases {
                            case_types.push((case.index, next_idx));
                            next_idx += 1;
                        }

                        // Map variant case WIR type indices to their Wasm type indices
                        for (&case_wir_idx, &(variant_wir_idx, case_idx)) in
                            &self.wir.variant_case_info
                        {
                            if variant_wir_idx == wir_idx
                                && let Some(&(_, wasm_idx)) =
                                    case_types.iter().find(|(idx, _)| *idx == case_idx)
                            {
                                self.type_index_map.insert(case_wir_idx, wasm_idx);
                            }
                        }

                        self.variant_pre_assigned
                            .insert(wir_idx, (base_idx, case_types));
                    }
                }
                WirTypeDef::Func(_) if gc_func_types.contains(&wir_idx) => {
                    // Func type referenced from a GC struct field — must be in rec group
                    self.type_index_map.insert(wir_idx, next_idx);
                    next_idx += 1;
                }
                WirTypeDef::Enum(_) | WirTypeDef::Flags(_) | WirTypeDef::Func(_) => {
                    // Enums/Flags: no type section entry
                    // Funcs: assigned in pass 2 (unless in gc_func_types)
                }
            }
        }

        // Pass 2: Function types — standalone, after the rec group
        for (wir_idx, typedef) in self.wir.types.iter().enumerate() {
            let wir_idx = u32::try_from(wir_idx).unwrap();
            if let WirTypeDef::Func(_) = typedef
                && !gc_func_types.contains(&wir_idx)
            {
                self.type_index_map.insert(wir_idx, next_idx);
                next_idx += 1;
            }
        }

        self.next_type_idx = next_idx;
    }

    /// Find func type indices that are referenced from struct fields.
    /// These must go in the rec group to avoid forward reference errors.
    fn find_gc_referenced_func_types(&self) -> IndexSet<u32> {
        let mut gc_func_types = IndexSet::default();
        for typedef in &self.wir.types {
            if let WirTypeDef::Struct(s) = typedef {
                for field in &s.fields {
                    if let WirType::Ref { type_id, .. } = &field.ty {
                        let ref_idx = type_id.index();
                        if (ref_idx as usize) < self.wir.types.len()
                            && matches!(self.wir.types[ref_idx as usize], WirTypeDef::Func(_))
                        {
                            gc_func_types.insert(ref_idx);
                        }
                    }
                }
            }
        }
        gc_func_types
    }

    fn emit_standalone_func_type(
        &mut self,
        types: &mut TypeSection,
        f: &WirFuncType,
        _wir_idx: u32,
    ) {
        let params: Vec<ValType> = f
            .params
            .iter()
            .map(|t| self.wir_type_to_val_type(t))
            .collect();
        let results: Vec<ValType> = f
            .results
            .iter()
            .map(|t| self.wir_type_to_val_type(t))
            .collect();
        types.ty().function(params, results);
    }

    fn build_struct_subtype(
        &mut self,
        s: &WirStructType,
        wir_idx: u32,
        subtypes: &mut Vec<SubType>,
    ) {
        debug_assert!(self.type_index_map.contains_key(&wir_idx));

        let mut field_map = IndexMap::default();
        let fields: Vec<FieldType> = s
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                field_map.insert(f.name.clone(), u32::try_from(i).unwrap());
                FieldType {
                    element_type: self.wir_type_to_storage_type(&f.ty),
                    mutable: f.mutable,
                }
            })
            .collect();
        self.struct_field_map.insert(wir_idx, field_map);

        // Resolve the optional supertype to its already-emitted Wasm type
        // index. Pre-pass `pre_assign_type_indices` ensures every WirTypeId
        // is mapped before any struct uses it as a supertype.
        let supertype_idx = s.supertype.as_ref().map(|st_id| {
            *self.type_index_map.get(&st_id.index()).unwrap_or_else(|| {
                panic!(
                    "supertype WIR type id {} ({}) not pre-assigned in type_index_map",
                    st_id.index(),
                    st_id.fq()
                )
            })
        });

        // Use is_final: false so (ref (exact $T)) from struct.new is a subtype of (ref null $T)
        subtypes.push(SubType {
            is_final: false,
            supertype_idx,
            composite_type: CompositeType {
                inner: CompositeInnerType::Struct(StructType {
                    fields: fields.into_boxed_slice(),
                }),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
    }

    fn build_variant_subtypes(
        &mut self,
        v: &WirVariantType,
        wir_idx: u32,
        subtypes: &mut Vec<SubType>,
    ) {
        // Type indices already pre-assigned by pre_assign_type_indices
        let base_type_idx = self.type_index_map[&wir_idx];
        let case_types = self
            .variant_pre_assigned
            .get(&wir_idx)
            .map(|(_, ct)| ct.clone())
            .unwrap_or_default();

        // Build field map for the base type
        let mut field_map = IndexMap::default();
        field_map.insert("discriminant".to_string(), 0);
        self.struct_field_map.insert(wir_idx, field_map);

        // Base struct type (non-final, no supertype)
        subtypes.push(SubType {
            is_final: false,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Struct(StructType {
                    fields: Box::new([FieldType {
                        element_type: StorageType::Val(ValType::I32),
                        mutable: false,
                    }]),
                }),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });

        // Case subtypes
        for case in &v.cases {
            let mut fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];
            for payload_ty in &case.payload {
                fields.push(FieldType {
                    element_type: self.wir_type_to_storage_type(payload_ty),
                    mutable: false,
                });
            }

            subtypes.push(SubType {
                is_final: true,
                supertype_idx: Some(base_type_idx),
                composite_type: CompositeType {
                    inner: CompositeInnerType::Struct(StructType {
                        fields: fields.into_boxed_slice(),
                    }),
                    shared: false,
                    descriptor: None,
                    describes: None,
                },
            });
        }
        self.variant_case_types.insert(wir_idx, case_types);

        // Register field maps for variant case structs
        for (&case_wir_idx, &(variant_wir_idx, case_idx)) in &self.wir.variant_case_info {
            if variant_wir_idx == wir_idx {
                let mut case_field_map = IndexMap::default();
                case_field_map.insert("discriminant".to_string(), 0);
                if let Some(case_def) = v.cases.get(case_idx as usize) {
                    for (j, _) in case_def.payload.iter().enumerate() {
                        case_field_map
                            .insert(format!("payload_{j}"), u32::try_from(j + 1).unwrap());
                    }
                }
                self.struct_field_map.insert(case_wir_idx, case_field_map);
            }
        }
    }

    fn build_array_subtype(&mut self, a: &WirArrayType, wir_idx: u32, subtypes: &mut Vec<SubType>) {
        debug_assert!(self.type_index_map.contains_key(&wir_idx));

        // List element types must be nullable for `array.new_default` (defaultability).
        // We add `ref.as_non_null` after `array.get` for ref-type elements.
        let storage_type = self.wir_type_to_storage_type(&a.element_type.clone().as_nullable());
        subtypes.push(SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Array(wasm_encoder::ArrayType(FieldType {
                    element_type: storage_type,
                    mutable: a.mutable,
                })),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
    }

    fn build_func_subtype(&mut self, f: &WirFuncType, wir_idx: u32, subtypes: &mut Vec<SubType>) {
        debug_assert!(self.type_index_map.contains_key(&wir_idx));

        let params: Vec<ValType> = f
            .params
            .iter()
            .map(|t| self.wir_type_to_val_type(t))
            .collect();
        let results: Vec<ValType> = f
            .results
            .iter()
            .map(|t| self.wir_type_to_val_type(t))
            .collect();
        subtypes.push(SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Func(wasm_encoder::FuncType::new(params, results)),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
    }

    // === Import Section ===

    fn emit_import_section(&self) -> (ImportSection, u32) {
        let mut imports = ImportSection::new();
        let mut func_count = 0u32;

        for import in &self.wir.imports {
            match &import.desc {
                WirImportDesc::Func { type_id, .. } => {
                    let wasm_type_idx = self
                        .type_index_map
                        .get(&type_id.index())
                        .copied()
                        .unwrap_or(0);
                    imports.import(
                        &import.module,
                        &import.field,
                        wasm_encoder::EntityType::Function(wasm_type_idx),
                    );
                    func_count += 1;
                }
                WirImportDesc::Memory { min, max } => {
                    imports.import(
                        &import.module,
                        &import.field,
                        wasm_encoder::MemoryType {
                            minimum: u64::from(*min),
                            maximum: max.map(u64::from),
                            memory64: false,
                            shared: false,
                            page_size_log2: None,
                        },
                    );
                }
                _ => {}
            }
        }

        (imports, func_count)
    }

    fn has_memory_import(&self) -> bool {
        self.wir
            .imports
            .iter()
            .any(|i| matches!(&i.desc, WirImportDesc::Memory { .. }))
    }

    // === Function Section ===

    fn emit_function_section(&self) -> FunctionSection {
        let mut funcs = FunctionSection::new();

        for func in &self.wir.functions {
            let wasm_type_idx = self
                .type_index_map
                .get(&func.type_id.index())
                .copied()
                .unwrap_or(0);
            funcs.function(wasm_type_idx);
        }

        funcs
    }

    fn emit_memory_section(&self) -> MemorySection {
        let mut memories = MemorySection::new();
        for mem in &self.wir.memories {
            memories.memory(MemoryType {
                minimum: u64::from(mem.min),
                maximum: mem.max.map(u64::from),
                memory64: false,
                shared: false,
                page_size_log2: None,
            });
        }
        memories
    }

    fn build_func_index_map(&mut self) {
        use crate::wir_build::DEFINED_FUNC_BASE;
        // Import functions already have indices 0..import_func_count-1
        for i in 0..self.func_index_offset {
            self.func_index_map.insert(i, i);
        }
        // Defined functions use DEFINED_FUNC_BASE + list_position as WirFuncId,
        // mapped to actual Wasm indices starting after imports.
        for (wasm_idx, (i, _func)) in
            (self.func_index_offset..).zip(self.wir.functions.iter().enumerate())
        {
            let wir_func_idx = DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
            self.func_index_map.insert(wir_func_idx, wasm_idx);
        }
    }

    fn build_global_name_map(&mut self) {
        for (wasm_idx, global) in self.wir.globals.iter().enumerate() {
            self.global_name_map
                .insert(global.name.fq.clone(), u32::try_from(wasm_idx).unwrap());
        }
    }

    fn resolve_global(&self, name: &str) -> u32 {
        self.global_name_map.get(name).copied().unwrap_or_else(|| {
            eprintln!("[WIR emit] Warning: global '{name}' not found, using index 0");
            0
        })
    }

    /// True for nullable globals whose values are guaranteed non-null
    /// at every legitimate read (lazy-init globals — see `WirGlobal::lazy_init`).
    /// Codegen wraps `global.get` in `ref.as_non_null` for these so the
    /// downstream non-null `result_ty` is satisfied.
    ///
    /// Genuinely-nullable globals (e.g. `Option<&T> = null`) return false
    /// here: their `null` value is a legitimate runtime value and
    /// narrowing it would trap.
    fn is_lazy_init_global(&self, name: &str) -> bool {
        self.wir
            .globals
            .iter()
            .find(|g| g.name.fq == name)
            .is_some_and(|g| g.lazy_init)
    }

    // === Global Section ===

    fn emit_global_section(&self) -> GlobalSection {
        let mut globals = GlobalSection::new();
        for g in &self.wir.globals {
            let val_type = self.wir_type_to_val_type(&g.ty);
            let init = self.emit_const_expr(&g.init);
            globals.global(
                GlobalType {
                    val_type,
                    mutable: g.mutable,
                    shared: false,
                },
                &init,
            );
        }
        globals
    }

    // === Export Section ===

    fn emit_export_section(&self) -> ExportSection {
        let mut exports = ExportSection::new();

        for export in &self.wir.exports {
            match &export.desc {
                WirExportDesc::Func { func_id } => {
                    let wasm_idx = self
                        .func_index_map
                        .get(&func_id.index())
                        .copied()
                        .unwrap_or(func_id.index());
                    exports.export(&export.name, ExportKind::Func, wasm_idx);
                }
                WirExportDesc::Global { .. } => {
                    // TODO: global exports
                }
                WirExportDesc::Memory => {
                    exports.export(&export.name, ExportKind::Memory, 0);
                }
                WirExportDesc::Table { index } => {
                    exports.export(&export.name, ExportKind::Table, *index);
                }
            }
        }

        exports
    }

    // === Code Section ===

    fn emit_code_section(&mut self) -> CodeSection {
        let mut code = CodeSection::new();

        for (i, func) in self.wir.functions.iter().enumerate() {
            self.current_branch_hints.clear();
            let wasm_func = self.emit_function(func);
            code.function(&wasm_func);
            if !self.current_branch_hints.is_empty() {
                let wir_func_idx = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
                let func_idx = self.func_index_map[&wir_func_idx];
                self.all_branch_hints
                    .push((func_idx, std::mem::take(&mut self.current_branch_hints)));
            }
        }

        code
    }

    fn emit_function(&mut self, func: &WirFunction) -> Function {
        // Reset local tracking
        self.current_locals.clear();
        self.ref_locals.clear();
        self.scratch_local_names.clear();
        self.next_local = 0;

        // Get function type info — check if it has a non-void return type
        let has_results = self
            .get_func_type(func.type_id.index())
            .is_some_and(|ft| !ft.results.is_empty());

        // Pre-allocate parameter locals
        for (i, name) in func.param_names.iter().enumerate() {
            let idx: u32 = u32::try_from(i).unwrap();
            self.current_locals.insert(name.clone(), idx);
            // Also register __local_N alias for params, since TIR uses unified local indices
            self.current_locals.insert(format!("__local_{i}"), idx);
            self.next_local = idx + 1;
        }

        // Scan body for DeclareLocal instructions to pre-allocate
        let mut local_types: Vec<(String, ValType)> = Vec::new();
        if let Some(ref body) = func.body {
            self.collect_declared_locals(body, &mut local_types);
        }

        // Build locals array
        let mut locals: Vec<(u32, ValType)> = Vec::new();
        for (name, val_type) in &local_types {
            let idx = self.next_local;
            self.next_local += 1;
            self.current_locals.insert(name.clone(), idx);
            locals.push((1, *val_type));
        }

        let mut f = Function::new(locals);

        // Emit body
        if let Some(ref body) = func.body {
            for instr in body {
                self.emit_instr(&mut f, instr);
            }
        }

        // For functions with return values, add `unreachable` as a safety net.
        // When all paths use explicit `return`, control never reaches here, but
        // the Wasm validator still requires the stack to match the return type.
        if has_results {
            f.instruction(&Instruction::Unreachable);
        }

        f.instruction(&Instruction::End);
        f
    }

    /// Collect `DeclareLocal` instructions from a body to pre-allocate.
    /// Recursively walks the entire instruction tree to find all `DeclareLocal` nodes.
    fn collect_declared_locals(&mut self, body: &[WirInstr], locals: &mut Vec<(String, ValType)>) {
        for instr in body {
            self.collect_declared_locals_instr(instr, locals);
        }
    }

    /// Recursively collect `DeclareLocal` from a single instruction and all its children.
    /// Ref-type locals are made nullable (Wasm requires defaultable locals for patterns
    /// that initialize on branches). Tracked in `ref_locals` for `ref.as_non_null` on get.
    fn collect_declared_locals_instr(
        &mut self,
        instr: &WirInstr,
        locals: &mut Vec<(String, ValType)>,
    ) {
        match instr {
            WirInstr::DeclareLocal { name, ty } => {
                let mut val_type = self.wir_type_to_val_type(ty);
                // Non-null ref locals must be made nullable for Wasm defaultability.
                // We track them and add ref.as_non_null on local.get.
                if let ValType::Ref(rt) = &mut val_type
                    && !rt.nullable
                {
                    rt.nullable = true;
                    self.ref_locals.insert(name.clone());
                }
                locals.push((name.clone(), val_type));
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                self.collect_declared_locals(body, locals);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.collect_declared_locals_instr(condition, locals);
                self.collect_declared_locals(then_body, locals);
                if let Some(else_body) = else_body {
                    self.collect_declared_locals(else_body, locals);
                }
            }
            WirInstr::Seq(body) => {
                self.collect_declared_locals(body, locals);
            }
            // Recursively walk all other instruction types that contain children
            WirInstr::LocalSet { value, .. } | WirInstr::LocalTee { value, .. } => {
                self.collect_declared_locals_instr(value, locals);
            }
            WirInstr::Call { args, .. } => {
                for arg in args {
                    self.collect_declared_locals_instr(arg, locals);
                }
            }
            WirInstr::CallIndirect { args, index, .. } => {
                for arg in args {
                    self.collect_declared_locals_instr(arg, locals);
                }
                self.collect_declared_locals_instr(index, locals);
            }
            WirInstr::CallRef { args, func_ref, .. } => {
                for arg in args {
                    self.collect_declared_locals_instr(arg, locals);
                }
                self.collect_declared_locals_instr(func_ref, locals);
            }
            WirInstr::StructNew { fields, .. }
            | WirInstr::ArrayNewFixed {
                elements: fields, ..
            } => {
                for f in fields {
                    self.collect_declared_locals_instr(f, locals);
                }
            }
            WirInstr::StructGet { expr, .. }
            | WirInstr::RefCast { expr, .. }
            | WirInstr::RefTest { expr, .. } => {
                self.collect_declared_locals_instr(expr, locals);
            }
            WirInstr::StructSet { expr, value, .. } => {
                self.collect_declared_locals_instr(expr, locals);
                self.collect_declared_locals_instr(value, locals);
            }
            WirInstr::ArrayNew { init, len, .. }
            | WirInstr::ArrayNewData {
                offset: init, len, ..
            } => {
                self.collect_declared_locals_instr(init, locals);
                self.collect_declared_locals_instr(len, locals);
            }
            WirInstr::ArrayNewDefault { len, .. } => {
                self.collect_declared_locals_instr(len, locals);
            }
            WirInstr::ArrayGet { array, index, .. }
            | WirInstr::ArrayGetS { array, index, .. }
            | WirInstr::ArrayGetU { array, index, .. } => {
                self.collect_declared_locals_instr(array, locals);
                self.collect_declared_locals_instr(index, locals);
            }
            WirInstr::ArraySet {
                array,
                index,
                value,
                ..
            } => {
                self.collect_declared_locals_instr(array, locals);
                self.collect_declared_locals_instr(index, locals);
                self.collect_declared_locals_instr(value, locals);
            }
            WirInstr::ArrayFill {
                array,
                offset,
                value,
                len,
                ..
            } => {
                self.collect_declared_locals_instr(array, locals);
                self.collect_declared_locals_instr(offset, locals);
                self.collect_declared_locals_instr(value, locals);
                self.collect_declared_locals_instr(len, locals);
            }
            WirInstr::ArrayCopy {
                dest_type_id,
                src_type_id,
                dest,
                dest_offset,
                src,
                src_offset,
                len,
            } => {
                self.collect_declared_locals_instr(dest, locals);
                self.collect_declared_locals_instr(dest_offset, locals);
                self.collect_declared_locals_instr(src, locals);
                self.collect_declared_locals_instr(src_offset, locals);
                self.collect_declared_locals_instr(len, locals);
                // The native `array.copy` lowering (the default) needs no
                // scratch locals, so skip the loop's slot declarations. They
                // are only emitted for the `-f no-array-copy` loop path.
                if self.codegen_flags.array_copy {
                    return;
                }
                // Pre-declare scratch locals for the inlined copy loop.
                // We lower `array.copy` to a Wasm loop because wasmtime's
                // `array.copy` implementation has a known performance bug
                // for short copies. Naming is keyed on the (dst, src)
                // type-id pair so multiple ArrayCopy sites within the same
                // function that share both types reuse the same slots.
                let dst_wasm_idx = self.resolve_type_index(dest_type_id.index());
                let src_wasm_idx = self.resolve_type_index(src_type_id.index());
                // Match `ArrayClone`'s convention: declare scratch ref locals
                // as non-null. Init-tracking validates them because we always
                // `local.set` before `local.get` within the emitted loop.
                let dst_ref = RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(dst_wasm_idx),
                };
                let src_ref = RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(src_wasm_idx),
                };
                let dst_name = format!(
                    "__array_copy_dst_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let src_name = format!(
                    "__array_copy_src_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let dst_off_name = format!(
                    "__array_copy_dst_off_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let src_off_name = format!(
                    "__array_copy_src_off_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let len_name = format!(
                    "__array_copy_len_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let i_name = format!(
                    "__array_copy_i_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                // Dedup: multiple ArrayCopy sites with the same type pair
                // share these slot names. Without the guard, every site
                // would grow the locals table by 6 unused entries.
                if self.scratch_local_names.insert(dst_name.clone()) {
                    locals.push((dst_name, ValType::Ref(dst_ref)));
                    locals.push((src_name, ValType::Ref(src_ref)));
                    locals.push((dst_off_name, ValType::I32));
                    locals.push((src_off_name, ValType::I32));
                    locals.push((len_name, ValType::I32));
                    locals.push((i_name, ValType::I32));
                }
            }
            WirInstr::ArrayClone {
                type_id,
                src,
                element_copy_func,
            } => {
                self.collect_declared_locals_instr(src, locals);
                // Pre-declare the four temps the JIT-compiled clone loop uses.
                let arr_wasm_idx = self.resolve_type_index(type_id.index());
                let arr_ref = RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(arr_wasm_idx),
                };
                let src_name = format!("__copy_arr_src_{}", type_id.index());
                let dst_name = format!("__copy_arr_dst_{}", type_id.index());
                let len_name = format!("__copy_arr_len_{}", type_id.index());
                let loop_idx_name = format!("__copy_arr_i_{}", type_id.index());
                // Dedup: multiple ArrayClone sites with the same type_id
                // share these slot names. See `ArrayCopy` above for rationale.
                let new_clone_site = self.scratch_local_names.insert(src_name.clone());
                if new_clone_site {
                    locals.push((src_name, ValType::Ref(arr_ref)));
                    locals.push((dst_name, ValType::Ref(arr_ref)));
                    locals.push((len_name, ValType::I32));
                    locals.push((loop_idx_name, ValType::I32));
                }
                // When `element_copy_func` is set, the loop also
                // branches on per-element nullability (slots beyond
                // `List<T>::used` are default-null) so it needs an
                // extra temp of element-nullable-ref type.
                //
                // Dedup the element temp independently of `new_clone_site`:
                // a *shallow* `ArrayClone` of the same `type_id` (emitted by
                // `value_copy_demote`) claims `src_name` but carries no
                // element temp, so gating on `new_clone_site` would let it
                // suppress a sibling deep clone's declaration of `elem_name`.
                if element_copy_func.is_some()
                    && let Some(elem_val) = self.array_element_val_type(type_id.index())
                {
                    let elem_name = format!("__copy_arr_elem_{}", type_id.index());
                    if self.scratch_local_names.insert(elem_name.clone()) {
                        locals.push((elem_name, elem_val));
                    }
                }
            }
            WirInstr::GlobalSet { value, .. } => {
                self.collect_declared_locals_instr(value, locals);
            }
            WirInstr::Return { value: Some(v) } => {
                self.collect_declared_locals_instr(v, locals);
            }
            WirInstr::Drop(v) => {
                self.collect_declared_locals_instr(v, locals);
            }
            WirInstr::BrTable { index, .. } => {
                self.collect_declared_locals_instr(index, locals);
            }
            WirInstr::BrIf { condition, .. }
            | WirInstr::BranchHint {
                expr: condition, ..
            } => {
                self.collect_declared_locals_instr(condition, locals);
            }
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ..
            } => {
                self.collect_declared_locals_instr(condition, locals);
                self.collect_declared_locals_instr(if_true, locals);
                self.collect_declared_locals_instr(if_false, locals);
            }
            // For all other instructions, walk children generically
            other => {
                other.for_each_child(&mut |child| {
                    self.collect_declared_locals_instr(child, locals);
                });
            }
        }
    }

    /// Emit a single WIR instruction to the Wasm function.
    fn emit_instr(&mut self, f: &mut Function, instr: &WirInstr) {
        match instr {
            WirInstr::DeclareLocal { .. } => {
                // Already handled in pre-allocation
            }
            WirInstr::LocalGet { name, result_ty } => {
                let idx = self.resolve_local(name);
                f.instruction(&Instruction::LocalGet(idx));
                // Ref-type locals are nullable in Wasm (for defaultability).
                // Narrow to non-null when the WIR result type says non-null.
                // The WIR optimizer relaxes result_ty to nullable for GC access
                // operands (array.get, struct.get, etc.) where nullable is accepted.
                if self.ref_locals.contains(name.as_str()) && result_ty.is_nonnull_ref() {
                    f.instruction(&Instruction::RefAsNonNull);
                }
            }
            WirInstr::LocalSet { name, value } => {
                self.emit_instr(f, value);
                let idx = self.resolve_local(name);
                f.instruction(&Instruction::LocalSet(idx));
            }
            WirInstr::LocalTee { name, value } => {
                self.emit_instr(f, value);
                let idx = self.resolve_local(name);
                f.instruction(&Instruction::LocalTee(idx));
                if self.ref_locals.contains(name.as_str()) {
                    f.instruction(&Instruction::RefAsNonNull);
                }
            }
            WirInstr::GlobalGet { name, .. } => {
                let idx = self.resolve_global(&name.fq);
                f.instruction(&Instruction::GlobalGet(idx));
                // Lazy-init globals produce `(ref null $T)` from `global.get`
                // but are guaranteed non-null after `__initialize_module`
                // runs. Narrow to `(ref $T)` so the downstream non-null
                // `result_ty` is satisfied.
                //
                // Genuinely-nullable globals (e.g. `Option<&T> = null`)
                // skip this narrowing — `null` is a legitimate runtime
                // value for them and `ref.as_non_null` would trap on
                // every `None` read.
                if self.is_lazy_init_global(&name.fq) {
                    f.instruction(&Instruction::RefAsNonNull);
                }
            }
            WirInstr::GlobalSet { name, value } => {
                self.emit_instr(f, value);
                let idx = self.resolve_global(&name.fq);
                f.instruction(&Instruction::GlobalSet(idx));
            }

            // Constants
            WirInstr::I32Const(v) => {
                f.instruction(&Instruction::I32Const(*v));
            }
            WirInstr::I64Const(v) => {
                f.instruction(&Instruction::I64Const(*v));
            }
            WirInstr::F32Const(v) => {
                f.instruction(&Instruction::F32Const((*v).into()));
            }
            WirInstr::F64Const(v) => {
                f.instruction(&Instruction::F64Const((*v).into()));
            }

            // i32 arithmetic
            WirInstr::I32Add(l, r) => self.emit_binary(f, l, r, Instruction::I32Add),
            WirInstr::I32Sub(l, r) => self.emit_binary(f, l, r, Instruction::I32Sub),
            WirInstr::I32Mul(l, r) => self.emit_binary(f, l, r, Instruction::I32Mul),
            WirInstr::I32DivS(l, r) => self.emit_binary(f, l, r, Instruction::I32DivS),
            WirInstr::I32DivU(l, r) => self.emit_binary(f, l, r, Instruction::I32DivU),
            WirInstr::I32RemS(l, r) => self.emit_binary(f, l, r, Instruction::I32RemS),
            WirInstr::I32RemU(l, r) => self.emit_binary(f, l, r, Instruction::I32RemU),
            WirInstr::I32And(l, r) => self.emit_binary(f, l, r, Instruction::I32And),
            WirInstr::I32Or(l, r) => self.emit_binary(f, l, r, Instruction::I32Or),
            WirInstr::I32Xor(l, r) => self.emit_binary(f, l, r, Instruction::I32Xor),
            WirInstr::I32Shl(l, r) => self.emit_binary(f, l, r, Instruction::I32Shl),
            WirInstr::I32ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I32ShrS),
            WirInstr::I32ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I32ShrU),
            WirInstr::I32Eq(l, r) => self.emit_binary(f, l, r, Instruction::I32Eq),
            WirInstr::I32Ne(l, r) => self.emit_binary(f, l, r, Instruction::I32Ne),
            WirInstr::I32LtS(l, r) => self.emit_binary(f, l, r, Instruction::I32LtS),
            WirInstr::I32LtU(l, r) => self.emit_binary(f, l, r, Instruction::I32LtU),
            WirInstr::I32GtS(l, r) => self.emit_binary(f, l, r, Instruction::I32GtS),
            WirInstr::I32GtU(l, r) => self.emit_binary(f, l, r, Instruction::I32GtU),
            WirInstr::I32LeS(l, r) => self.emit_binary(f, l, r, Instruction::I32LeS),
            WirInstr::I32LeU(l, r) => self.emit_binary(f, l, r, Instruction::I32LeU),
            WirInstr::I32GeS(l, r) => self.emit_binary(f, l, r, Instruction::I32GeS),
            WirInstr::I32GeU(l, r) => self.emit_binary(f, l, r, Instruction::I32GeU),
            WirInstr::I32Eqz(o) => self.emit_unary(f, o, Instruction::I32Eqz),
            WirInstr::I32WrapI64(o) => self.emit_unary(f, o, Instruction::I32WrapI64),
            WirInstr::I32Extend8S(o) => self.emit_unary(f, o, Instruction::I32Extend8S),
            WirInstr::I32Extend16S(o) => self.emit_unary(f, o, Instruction::I32Extend16S),

            // i64 arithmetic
            WirInstr::I64Add(l, r) => self.emit_binary(f, l, r, Instruction::I64Add),
            WirInstr::I64Sub(l, r) => self.emit_binary(f, l, r, Instruction::I64Sub),
            WirInstr::I64Mul(l, r) => self.emit_binary(f, l, r, Instruction::I64Mul),
            WirInstr::I64DivS(l, r) => self.emit_binary(f, l, r, Instruction::I64DivS),
            WirInstr::I64DivU(l, r) => self.emit_binary(f, l, r, Instruction::I64DivU),
            WirInstr::I64RemS(l, r) => self.emit_binary(f, l, r, Instruction::I64RemS),
            WirInstr::I64RemU(l, r) => self.emit_binary(f, l, r, Instruction::I64RemU),
            WirInstr::I64And(l, r) => self.emit_binary(f, l, r, Instruction::I64And),
            WirInstr::I64Or(l, r) => self.emit_binary(f, l, r, Instruction::I64Or),
            WirInstr::I64Xor(l, r) => self.emit_binary(f, l, r, Instruction::I64Xor),
            WirInstr::I64Shl(l, r) => self.emit_binary(f, l, r, Instruction::I64Shl),
            WirInstr::I64ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I64ShrS),
            WirInstr::I64ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I64ShrU),
            WirInstr::I64Eq(l, r) => self.emit_binary(f, l, r, Instruction::I64Eq),
            WirInstr::I64Ne(l, r) => self.emit_binary(f, l, r, Instruction::I64Ne),
            WirInstr::I64LtS(l, r) => self.emit_binary(f, l, r, Instruction::I64LtS),
            WirInstr::I64LtU(l, r) => self.emit_binary(f, l, r, Instruction::I64LtU),
            WirInstr::I64GtS(l, r) => self.emit_binary(f, l, r, Instruction::I64GtS),
            WirInstr::I64GtU(l, r) => self.emit_binary(f, l, r, Instruction::I64GtU),
            WirInstr::I64LeS(l, r) => self.emit_binary(f, l, r, Instruction::I64LeS),
            WirInstr::I64LeU(l, r) => self.emit_binary(f, l, r, Instruction::I64LeU),
            WirInstr::I64GeS(l, r) => self.emit_binary(f, l, r, Instruction::I64GeS),
            WirInstr::I64GeU(l, r) => self.emit_binary(f, l, r, Instruction::I64GeU),
            WirInstr::I64Eqz(o) => self.emit_unary(f, o, Instruction::I64Eqz),
            WirInstr::I64Clz(o) => self.emit_unary(f, o, Instruction::I64Clz),
            WirInstr::I64Ctz(o) => self.emit_unary(f, o, Instruction::I64Ctz),
            WirInstr::I64Popcnt(o) => self.emit_unary(f, o, Instruction::I64Popcnt),
            WirInstr::I64ExtendI32S(o) => self.emit_unary(f, o, Instruction::I64ExtendI32S),
            WirInstr::I64ExtendI32U(o) => self.emit_unary(f, o, Instruction::I64ExtendI32U),
            WirInstr::I64TruncF64S(o) => self.emit_unary(f, o, Instruction::I64TruncF64S),
            WirInstr::I64TruncF64U(o) => self.emit_unary(f, o, Instruction::I64TruncF64U),
            WirInstr::I64TruncF32S(o) => self.emit_unary(f, o, Instruction::I64TruncF32S),
            WirInstr::I64TruncF32U(o) => self.emit_unary(f, o, Instruction::I64TruncF32U),

            // i32 extra
            WirInstr::I32Clz(o) => self.emit_unary(f, o, Instruction::I32Clz),
            WirInstr::I32Ctz(o) => self.emit_unary(f, o, Instruction::I32Ctz),
            WirInstr::I32Popcnt(o) => self.emit_unary(f, o, Instruction::I32Popcnt),
            WirInstr::I32TruncF32S(o) => self.emit_unary(f, o, Instruction::I32TruncF32S),
            WirInstr::I32TruncF32U(o) => self.emit_unary(f, o, Instruction::I32TruncF32U),
            WirInstr::I32TruncF64U(o) => self.emit_unary(f, o, Instruction::I32TruncF64U),

            // f32 arithmetic
            WirInstr::F32Add(l, r) => self.emit_binary(f, l, r, Instruction::F32Add),
            WirInstr::F32Sub(l, r) => self.emit_binary(f, l, r, Instruction::F32Sub),
            WirInstr::F32Mul(l, r) => self.emit_binary(f, l, r, Instruction::F32Mul),
            WirInstr::F32Div(l, r) => self.emit_binary(f, l, r, Instruction::F32Div),
            WirInstr::F32Neg(o) => self.emit_unary(f, o, Instruction::F32Neg),
            WirInstr::F32Abs(o) => self.emit_unary(f, o, Instruction::F32Abs),
            WirInstr::F32Ceil(o) => self.emit_unary(f, o, Instruction::F32Ceil),
            WirInstr::F32Floor(o) => self.emit_unary(f, o, Instruction::F32Floor),
            WirInstr::F32Trunc(o) => self.emit_unary(f, o, Instruction::F32Trunc),
            WirInstr::F32Nearest(o) => self.emit_unary(f, o, Instruction::F32Nearest),
            WirInstr::F32Sqrt(o) => self.emit_unary(f, o, Instruction::F32Sqrt),
            WirInstr::F32Min(l, r) => self.emit_binary(f, l, r, Instruction::F32Min),
            WirInstr::F32Max(l, r) => self.emit_binary(f, l, r, Instruction::F32Max),
            WirInstr::F32Copysign(l, r) => self.emit_binary(f, l, r, Instruction::F32Copysign),
            WirInstr::F32Eq(l, r) => self.emit_binary(f, l, r, Instruction::F32Eq),
            WirInstr::F32Ne(l, r) => self.emit_binary(f, l, r, Instruction::F32Ne),
            WirInstr::F32Lt(l, r) => self.emit_binary(f, l, r, Instruction::F32Lt),
            WirInstr::F32Gt(l, r) => self.emit_binary(f, l, r, Instruction::F32Gt),
            WirInstr::F32Le(l, r) => self.emit_binary(f, l, r, Instruction::F32Le),
            WirInstr::F32Ge(l, r) => self.emit_binary(f, l, r, Instruction::F32Ge),
            WirInstr::F32DemoteF64(o) => self.emit_unary(f, o, Instruction::F32DemoteF64),
            WirInstr::F32ConvertI32S(o) => self.emit_unary(f, o, Instruction::F32ConvertI32S),
            WirInstr::F32ConvertI32U(o) => self.emit_unary(f, o, Instruction::F32ConvertI32U),
            WirInstr::F32ConvertI64S(o) => self.emit_unary(f, o, Instruction::F32ConvertI64S),
            WirInstr::F32ConvertI64U(o) => self.emit_unary(f, o, Instruction::F32ConvertI64U),
            WirInstr::F32ReinterpretI32(o) => self.emit_unary(f, o, Instruction::F32ReinterpretI32),
            WirInstr::I32ReinterpretF32(o) => self.emit_unary(f, o, Instruction::I32ReinterpretF32),

            // f64 arithmetic
            WirInstr::F64Add(l, r) => self.emit_binary(f, l, r, Instruction::F64Add),
            WirInstr::F64Sub(l, r) => self.emit_binary(f, l, r, Instruction::F64Sub),
            WirInstr::F64Mul(l, r) => self.emit_binary(f, l, r, Instruction::F64Mul),
            WirInstr::F64Div(l, r) => self.emit_binary(f, l, r, Instruction::F64Div),
            WirInstr::F64Neg(o) => self.emit_unary(f, o, Instruction::F64Neg),
            WirInstr::F64Abs(o) => self.emit_unary(f, o, Instruction::F64Abs),
            WirInstr::F64Ceil(o) => self.emit_unary(f, o, Instruction::F64Ceil),
            WirInstr::F64Floor(o) => self.emit_unary(f, o, Instruction::F64Floor),
            WirInstr::F64Trunc(o) => self.emit_unary(f, o, Instruction::F64Trunc),
            WirInstr::F64Nearest(o) => self.emit_unary(f, o, Instruction::F64Nearest),
            WirInstr::F64Sqrt(o) => self.emit_unary(f, o, Instruction::F64Sqrt),
            WirInstr::F64Min(l, r) => self.emit_binary(f, l, r, Instruction::F64Min),
            WirInstr::F64Max(l, r) => self.emit_binary(f, l, r, Instruction::F64Max),
            WirInstr::F64Copysign(l, r) => self.emit_binary(f, l, r, Instruction::F64Copysign),
            WirInstr::F64Eq(l, r) => self.emit_binary(f, l, r, Instruction::F64Eq),
            WirInstr::F64Ne(l, r) => self.emit_binary(f, l, r, Instruction::F64Ne),
            WirInstr::F64Lt(l, r) => self.emit_binary(f, l, r, Instruction::F64Lt),
            WirInstr::F64Gt(l, r) => self.emit_binary(f, l, r, Instruction::F64Gt),
            WirInstr::F64Le(l, r) => self.emit_binary(f, l, r, Instruction::F64Le),
            WirInstr::F64Ge(l, r) => self.emit_binary(f, l, r, Instruction::F64Ge),
            WirInstr::F64ConvertI32S(o) => self.emit_unary(f, o, Instruction::F64ConvertI32S),
            WirInstr::F64ConvertI32U(o) => self.emit_unary(f, o, Instruction::F64ConvertI32U),
            WirInstr::F64ConvertI64S(o) => self.emit_unary(f, o, Instruction::F64ConvertI64S),
            WirInstr::F64ConvertI64U(o) => self.emit_unary(f, o, Instruction::F64ConvertI64U),
            WirInstr::F64PromoteF32(o) => self.emit_unary(f, o, Instruction::F64PromoteF32),
            WirInstr::I32TruncF64S(o) => self.emit_unary(f, o, Instruction::I32TruncF64S),
            WirInstr::F64ReinterpretI64(o) => self.emit_unary(f, o, Instruction::F64ReinterpretI64),
            WirInstr::I64ReinterpretF64(o) => self.emit_unary(f, o, Instruction::I64ReinterpretF64),

            // SIMD v128
            WirInstr::V128Const(v) => {
                f.instruction(&Instruction::V128Const(*v));
            }
            WirInstr::V128Not(a) => self.emit_unary(f, a, Instruction::V128Not),
            WirInstr::V128And(l, r) => self.emit_binary(f, l, r, Instruction::V128And),
            WirInstr::V128Or(l, r) => self.emit_binary(f, l, r, Instruction::V128Or),
            WirInstr::V128Xor(l, r) => self.emit_binary(f, l, r, Instruction::V128Xor),
            WirInstr::V128Bitselect(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::V128Bitselect);
            }
            WirInstr::I8x16Splat(a) => self.emit_unary(f, a, Instruction::I8x16Splat),
            WirInstr::I8x16ExtractLaneS(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I8x16ExtractLaneS(*lane));
            }
            WirInstr::I8x16ExtractLaneU(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I8x16ExtractLaneU(*lane));
            }
            WirInstr::I8x16ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::I8x16ReplaceLane(*lane));
            }
            WirInstr::I8x16Add(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Add),
            WirInstr::I8x16Sub(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Sub),
            WirInstr::I8x16Neg(a) => self.emit_unary(f, a, Instruction::I8x16Neg),
            WirInstr::I8x16Eq(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Eq),
            WirInstr::I8x16Ne(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Ne),
            WirInstr::I8x16LtS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16LtS),
            WirInstr::I8x16GtS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16GtS),
            WirInstr::I8x16LeS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16LeS),
            WirInstr::I8x16GeS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16GeS),
            WirInstr::I8x16LtU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16LtU),
            WirInstr::I8x16GtU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16GtU),
            WirInstr::I8x16LeU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16LeU),
            WirInstr::I8x16GeU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16GeU),
            WirInstr::I8x16Shl(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Shl),
            WirInstr::I8x16ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16ShrS),
            WirInstr::I8x16ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16ShrU),
            WirInstr::I8x16Swizzle(l, r) => self.emit_binary(f, l, r, Instruction::I8x16Swizzle),
            WirInstr::I8x16Shuffle(lanes, a, b) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                f.instruction(&Instruction::I8x16Shuffle(*lanes));
            }
            WirInstr::I16x8Splat(a) => self.emit_unary(f, a, Instruction::I16x8Splat),
            WirInstr::I16x8ExtractLaneS(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I16x8ExtractLaneS(*lane));
            }
            WirInstr::I16x8ExtractLaneU(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I16x8ExtractLaneU(*lane));
            }
            WirInstr::I16x8ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::I16x8ReplaceLane(*lane));
            }
            WirInstr::I16x8Add(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Add),
            WirInstr::I16x8Sub(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Sub),
            WirInstr::I16x8Mul(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Mul),
            WirInstr::I16x8Neg(a) => self.emit_unary(f, a, Instruction::I16x8Neg),
            WirInstr::I16x8Eq(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Eq),
            WirInstr::I16x8Ne(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Ne),
            WirInstr::I16x8LtS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8LtS),
            WirInstr::I16x8GtS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8GtS),
            WirInstr::I16x8LeS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8LeS),
            WirInstr::I16x8GeS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8GeS),
            WirInstr::I16x8LtU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8LtU),
            WirInstr::I16x8GtU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8GtU),
            WirInstr::I16x8LeU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8LeU),
            WirInstr::I16x8GeU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8GeU),
            WirInstr::I16x8Shl(l, r) => self.emit_binary(f, l, r, Instruction::I16x8Shl),
            WirInstr::I16x8ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8ShrS),
            WirInstr::I16x8ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8ShrU),
            WirInstr::I32x4Splat(a) => self.emit_unary(f, a, Instruction::I32x4Splat),
            WirInstr::I32x4ExtractLane(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I32x4ExtractLane(*lane));
            }
            WirInstr::I32x4ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::I32x4ReplaceLane(*lane));
            }
            WirInstr::I32x4Add(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Add),
            WirInstr::I32x4Sub(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Sub),
            WirInstr::I32x4Mul(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Mul),
            WirInstr::I32x4Neg(a) => self.emit_unary(f, a, Instruction::I32x4Neg),
            WirInstr::I32x4Eq(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Eq),
            WirInstr::I32x4Ne(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Ne),
            WirInstr::I32x4LtS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4LtS),
            WirInstr::I32x4GtS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4GtS),
            WirInstr::I32x4LeS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4LeS),
            WirInstr::I32x4GeS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4GeS),
            WirInstr::I32x4LtU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4LtU),
            WirInstr::I32x4GtU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4GtU),
            WirInstr::I32x4LeU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4LeU),
            WirInstr::I32x4GeU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4GeU),
            WirInstr::I32x4Shl(l, r) => self.emit_binary(f, l, r, Instruction::I32x4Shl),
            WirInstr::I32x4ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4ShrS),
            WirInstr::I32x4ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4ShrU),
            WirInstr::I64x2Splat(a) => self.emit_unary(f, a, Instruction::I64x2Splat),
            WirInstr::I64x2ExtractLane(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::I64x2ExtractLane(*lane));
            }
            WirInstr::I64x2ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::I64x2ReplaceLane(*lane));
            }
            WirInstr::I64x2Add(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Add),
            WirInstr::I64x2Sub(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Sub),
            WirInstr::I64x2Mul(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Mul),
            WirInstr::I64x2Neg(a) => self.emit_unary(f, a, Instruction::I64x2Neg),
            WirInstr::I64x2Eq(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Eq),
            WirInstr::I64x2Ne(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Ne),
            WirInstr::I64x2LtS(l, r) => self.emit_binary(f, l, r, Instruction::I64x2LtS),
            WirInstr::I64x2GtS(l, r) => self.emit_binary(f, l, r, Instruction::I64x2GtS),
            WirInstr::I64x2LeS(l, r) => self.emit_binary(f, l, r, Instruction::I64x2LeS),
            WirInstr::I64x2GeS(l, r) => self.emit_binary(f, l, r, Instruction::I64x2GeS),
            WirInstr::I64x2Shl(l, r) => self.emit_binary(f, l, r, Instruction::I64x2Shl),
            WirInstr::I64x2ShrS(l, r) => self.emit_binary(f, l, r, Instruction::I64x2ShrS),
            WirInstr::I64x2ShrU(l, r) => self.emit_binary(f, l, r, Instruction::I64x2ShrU),
            WirInstr::F32x4Splat(a) => self.emit_unary(f, a, Instruction::F32x4Splat),
            WirInstr::F32x4ExtractLane(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::F32x4ExtractLane(*lane));
            }
            WirInstr::F32x4ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::F32x4ReplaceLane(*lane));
            }
            WirInstr::F32x4Add(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Add),
            WirInstr::F32x4Sub(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Sub),
            WirInstr::F32x4Mul(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Mul),
            WirInstr::F32x4Div(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Div),
            WirInstr::F32x4Neg(a) => self.emit_unary(f, a, Instruction::F32x4Neg),
            WirInstr::F32x4Sqrt(a) => self.emit_unary(f, a, Instruction::F32x4Sqrt),
            WirInstr::F32x4Abs(a) => self.emit_unary(f, a, Instruction::F32x4Abs),
            WirInstr::F32x4Eq(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Eq),
            WirInstr::F32x4Ne(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Ne),
            WirInstr::F32x4Lt(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Lt),
            WirInstr::F32x4Gt(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Gt),
            WirInstr::F32x4Le(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Le),
            WirInstr::F32x4Ge(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Ge),
            WirInstr::F32x4Min(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Min),
            WirInstr::F32x4Max(l, r) => self.emit_binary(f, l, r, Instruction::F32x4Max),
            WirInstr::F64x2Splat(a) => self.emit_unary(f, a, Instruction::F64x2Splat),
            WirInstr::F64x2ExtractLane(lane, a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::F64x2ExtractLane(*lane));
            }
            WirInstr::F64x2ReplaceLane(lane, v, val) => {
                self.emit_instr(f, v);
                self.emit_instr(f, val);
                f.instruction(&Instruction::F64x2ReplaceLane(*lane));
            }
            WirInstr::F64x2Add(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Add),
            WirInstr::F64x2Sub(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Sub),
            WirInstr::F64x2Mul(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Mul),
            WirInstr::F64x2Div(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Div),
            WirInstr::F64x2Neg(a) => self.emit_unary(f, a, Instruction::F64x2Neg),
            WirInstr::F64x2Sqrt(a) => self.emit_unary(f, a, Instruction::F64x2Sqrt),
            WirInstr::F64x2Abs(a) => self.emit_unary(f, a, Instruction::F64x2Abs),
            WirInstr::F64x2Eq(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Eq),
            WirInstr::F64x2Ne(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Ne),
            WirInstr::F64x2Lt(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Lt),
            WirInstr::F64x2Gt(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Gt),
            WirInstr::F64x2Le(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Le),
            WirInstr::F64x2Ge(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Ge),
            WirInstr::F64x2Min(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Min),
            WirInstr::F64x2Max(l, r) => self.emit_binary(f, l, r, Instruction::F64x2Max),
            WirInstr::I8x16Abs(a) => self.emit_unary(f, a, Instruction::I8x16Abs),
            WirInstr::I8x16AddSatS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16AddSatS),
            WirInstr::I8x16AddSatU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16AddSatU),
            WirInstr::I8x16SubSatS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16SubSatS),
            WirInstr::I8x16SubSatU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16SubSatU),
            WirInstr::I8x16MinS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16MinS),
            WirInstr::I8x16MinU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16MinU),
            WirInstr::I8x16MaxS(l, r) => self.emit_binary(f, l, r, Instruction::I8x16MaxS),
            WirInstr::I8x16MaxU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16MaxU),
            WirInstr::I8x16AvgrU(l, r) => self.emit_binary(f, l, r, Instruction::I8x16AvgrU),
            WirInstr::I8x16AllTrue(a) => self.emit_unary(f, a, Instruction::I8x16AllTrue),
            WirInstr::I8x16Bitmask(a) => self.emit_unary(f, a, Instruction::I8x16Bitmask),
            WirInstr::I8x16NarrowI16x8S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I8x16NarrowI16x8S);
            }
            WirInstr::I8x16NarrowI16x8U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I8x16NarrowI16x8U);
            }
            WirInstr::I8x16Popcnt(a) => self.emit_unary(f, a, Instruction::I8x16Popcnt),
            WirInstr::I16x8Abs(a) => self.emit_unary(f, a, Instruction::I16x8Abs),
            WirInstr::I16x8AddSatS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8AddSatS),
            WirInstr::I16x8AddSatU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8AddSatU),
            WirInstr::I16x8SubSatS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8SubSatS),
            WirInstr::I16x8SubSatU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8SubSatU),
            WirInstr::I16x8MinS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8MinS),
            WirInstr::I16x8MinU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8MinU),
            WirInstr::I16x8MaxS(l, r) => self.emit_binary(f, l, r, Instruction::I16x8MaxS),
            WirInstr::I16x8MaxU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8MaxU),
            WirInstr::I16x8AvgrU(l, r) => self.emit_binary(f, l, r, Instruction::I16x8AvgrU),
            WirInstr::I16x8AllTrue(a) => self.emit_unary(f, a, Instruction::I16x8AllTrue),
            WirInstr::I16x8Bitmask(a) => self.emit_unary(f, a, Instruction::I16x8Bitmask),
            WirInstr::I16x8NarrowI32x4S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8NarrowI32x4S);
            }
            WirInstr::I16x8NarrowI32x4U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8NarrowI32x4U);
            }
            WirInstr::I16x8ExtendLowI8x16S(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtendLowI8x16S);
            }
            WirInstr::I16x8ExtendHighI8x16S(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtendHighI8x16S);
            }
            WirInstr::I16x8ExtendLowI8x16U(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtendLowI8x16U);
            }
            WirInstr::I16x8ExtendHighI8x16U(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtendHighI8x16U);
            }
            WirInstr::I16x8ExtMulLowI8x16S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8ExtMulLowI8x16S);
            }
            WirInstr::I16x8ExtMulHighI8x16S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8ExtMulHighI8x16S);
            }
            WirInstr::I16x8ExtMulLowI8x16U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8ExtMulLowI8x16U);
            }
            WirInstr::I16x8ExtMulHighI8x16U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8ExtMulHighI8x16U);
            }
            WirInstr::I16x8ExtAddPairwiseI8x16S(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtAddPairwiseI8x16S);
            }
            WirInstr::I16x8ExtAddPairwiseI8x16U(a) => {
                self.emit_unary(f, a, Instruction::I16x8ExtAddPairwiseI8x16U);
            }
            WirInstr::I16x8Q15MulrSatS(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8Q15MulrSatS);
            }
            WirInstr::I32x4Abs(a) => self.emit_unary(f, a, Instruction::I32x4Abs),
            WirInstr::I32x4AllTrue(a) => self.emit_unary(f, a, Instruction::I32x4AllTrue),
            WirInstr::I32x4Bitmask(a) => self.emit_unary(f, a, Instruction::I32x4Bitmask),
            WirInstr::I32x4MinS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4MinS),
            WirInstr::I32x4MinU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4MinU),
            WirInstr::I32x4MaxS(l, r) => self.emit_binary(f, l, r, Instruction::I32x4MaxS),
            WirInstr::I32x4MaxU(l, r) => self.emit_binary(f, l, r, Instruction::I32x4MaxU),
            WirInstr::I32x4DotI16x8S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I32x4DotI16x8S);
            }
            WirInstr::I32x4ExtendLowI16x8S(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtendLowI16x8S);
            }
            WirInstr::I32x4ExtendHighI16x8S(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtendHighI16x8S);
            }
            WirInstr::I32x4ExtendLowI16x8U(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtendLowI16x8U);
            }
            WirInstr::I32x4ExtendHighI16x8U(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtendHighI16x8U);
            }
            WirInstr::I32x4ExtMulLowI16x8S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I32x4ExtMulLowI16x8S);
            }
            WirInstr::I32x4ExtMulHighI16x8S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I32x4ExtMulHighI16x8S);
            }
            WirInstr::I32x4ExtMulLowI16x8U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I32x4ExtMulLowI16x8U);
            }
            WirInstr::I32x4ExtMulHighI16x8U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I32x4ExtMulHighI16x8U);
            }
            WirInstr::I32x4ExtAddPairwiseI16x8S(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtAddPairwiseI16x8S);
            }
            WirInstr::I32x4ExtAddPairwiseI16x8U(a) => {
                self.emit_unary(f, a, Instruction::I32x4ExtAddPairwiseI16x8U);
            }
            WirInstr::I32x4TruncSatF32x4S(a) => {
                self.emit_unary(f, a, Instruction::I32x4TruncSatF32x4S);
            }
            WirInstr::I32x4TruncSatF32x4U(a) => {
                self.emit_unary(f, a, Instruction::I32x4TruncSatF32x4U);
            }
            WirInstr::I32x4TruncSatF64x2SZero(a) => {
                self.emit_unary(f, a, Instruction::I32x4TruncSatF64x2SZero);
            }
            WirInstr::I32x4TruncSatF64x2UZero(a) => {
                self.emit_unary(f, a, Instruction::I32x4TruncSatF64x2UZero);
            }
            WirInstr::I64x2Abs(a) => self.emit_unary(f, a, Instruction::I64x2Abs),
            WirInstr::I64x2AllTrue(a) => self.emit_unary(f, a, Instruction::I64x2AllTrue),
            WirInstr::I64x2Bitmask(a) => self.emit_unary(f, a, Instruction::I64x2Bitmask),
            WirInstr::I64x2ExtendLowI32x4S(a) => {
                self.emit_unary(f, a, Instruction::I64x2ExtendLowI32x4S);
            }
            WirInstr::I64x2ExtendHighI32x4S(a) => {
                self.emit_unary(f, a, Instruction::I64x2ExtendHighI32x4S);
            }
            WirInstr::I64x2ExtendLowI32x4U(a) => {
                self.emit_unary(f, a, Instruction::I64x2ExtendLowI32x4U);
            }
            WirInstr::I64x2ExtendHighI32x4U(a) => {
                self.emit_unary(f, a, Instruction::I64x2ExtendHighI32x4U);
            }
            WirInstr::I64x2ExtMulLowI32x4S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I64x2ExtMulLowI32x4S);
            }
            WirInstr::I64x2ExtMulHighI32x4S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I64x2ExtMulHighI32x4S);
            }
            WirInstr::I64x2ExtMulLowI32x4U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I64x2ExtMulLowI32x4U);
            }
            WirInstr::I64x2ExtMulHighI32x4U(l, r) => {
                self.emit_binary(f, l, r, Instruction::I64x2ExtMulHighI32x4U);
            }
            WirInstr::F32x4Ceil(a) => self.emit_unary(f, a, Instruction::F32x4Ceil),
            WirInstr::F32x4Floor(a) => self.emit_unary(f, a, Instruction::F32x4Floor),
            WirInstr::F32x4Trunc(a) => self.emit_unary(f, a, Instruction::F32x4Trunc),
            WirInstr::F32x4Nearest(a) => self.emit_unary(f, a, Instruction::F32x4Nearest),
            WirInstr::F32x4PMin(l, r) => self.emit_binary(f, l, r, Instruction::F32x4PMin),
            WirInstr::F32x4PMax(l, r) => self.emit_binary(f, l, r, Instruction::F32x4PMax),
            WirInstr::F32x4ConvertI32x4S(a) => {
                self.emit_unary(f, a, Instruction::F32x4ConvertI32x4S);
            }
            WirInstr::F32x4ConvertI32x4U(a) => {
                self.emit_unary(f, a, Instruction::F32x4ConvertI32x4U);
            }
            WirInstr::F32x4DemoteF64x2Zero(a) => {
                self.emit_unary(f, a, Instruction::F32x4DemoteF64x2Zero);
            }
            WirInstr::F64x2Ceil(a) => self.emit_unary(f, a, Instruction::F64x2Ceil),
            WirInstr::F64x2Floor(a) => self.emit_unary(f, a, Instruction::F64x2Floor),
            WirInstr::F64x2Trunc(a) => self.emit_unary(f, a, Instruction::F64x2Trunc),
            WirInstr::F64x2Nearest(a) => self.emit_unary(f, a, Instruction::F64x2Nearest),
            WirInstr::F64x2PMin(l, r) => self.emit_binary(f, l, r, Instruction::F64x2PMin),
            WirInstr::F64x2PMax(l, r) => self.emit_binary(f, l, r, Instruction::F64x2PMax),
            WirInstr::F64x2ConvertLowI32x4S(a) => {
                self.emit_unary(f, a, Instruction::F64x2ConvertLowI32x4S);
            }
            WirInstr::F64x2ConvertLowI32x4U(a) => {
                self.emit_unary(f, a, Instruction::F64x2ConvertLowI32x4U);
            }
            WirInstr::F64x2PromoteLowF32x4(a) => {
                self.emit_unary(f, a, Instruction::F64x2PromoteLowF32x4);
            }
            WirInstr::V128AndNot(l, r) => self.emit_binary(f, l, r, Instruction::V128AndNot),
            WirInstr::V128AnyTrue(a) => self.emit_unary(f, a, Instruction::V128AnyTrue),
            WirInstr::I8x16RelaxedSwizzle(l, r) => {
                self.emit_binary(f, l, r, Instruction::I8x16RelaxedSwizzle);
            }
            WirInstr::I8x16RelaxedLaneselect(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::I8x16RelaxedLaneselect);
            }
            WirInstr::I16x8RelaxedLaneselect(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::I16x8RelaxedLaneselect);
            }
            WirInstr::I32x4RelaxedLaneselect(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::I32x4RelaxedLaneselect);
            }
            WirInstr::I64x2RelaxedLaneselect(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::I64x2RelaxedLaneselect);
            }
            WirInstr::F32x4RelaxedMadd(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::F32x4RelaxedMadd);
            }
            WirInstr::F32x4RelaxedNmadd(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::F32x4RelaxedNmadd);
            }
            WirInstr::F64x2RelaxedMadd(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::F64x2RelaxedMadd);
            }
            WirInstr::F64x2RelaxedNmadd(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::F64x2RelaxedNmadd);
            }
            WirInstr::F32x4RelaxedMin(l, r) => {
                self.emit_binary(f, l, r, Instruction::F32x4RelaxedMin);
            }
            WirInstr::F32x4RelaxedMax(l, r) => {
                self.emit_binary(f, l, r, Instruction::F32x4RelaxedMax);
            }
            WirInstr::F64x2RelaxedMin(l, r) => {
                self.emit_binary(f, l, r, Instruction::F64x2RelaxedMin);
            }
            WirInstr::F64x2RelaxedMax(l, r) => {
                self.emit_binary(f, l, r, Instruction::F64x2RelaxedMax);
            }
            WirInstr::I32x4RelaxedTruncF32x4S(a) => {
                self.emit_unary(f, a, Instruction::I32x4RelaxedTruncF32x4S);
            }
            WirInstr::I32x4RelaxedTruncF32x4U(a) => {
                self.emit_unary(f, a, Instruction::I32x4RelaxedTruncF32x4U);
            }
            WirInstr::I32x4RelaxedTruncF64x2SZero(a) => {
                self.emit_unary(f, a, Instruction::I32x4RelaxedTruncF64x2SZero);
            }
            WirInstr::I32x4RelaxedTruncF64x2UZero(a) => {
                self.emit_unary(f, a, Instruction::I32x4RelaxedTruncF64x2UZero);
            }
            WirInstr::I16x8RelaxedQ15mulrS(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8RelaxedQ15mulrS);
            }
            WirInstr::I16x8RelaxedDotI8x16I7x16S(l, r) => {
                self.emit_binary(f, l, r, Instruction::I16x8RelaxedDotI8x16I7x16S);
            }
            WirInstr::I32x4RelaxedDotI8x16I7x16AddS(a, b, c) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                self.emit_instr(f, c);
                f.instruction(&Instruction::I32x4RelaxedDotI8x16I7x16AddS);
            }

            // GC: Struct
            WirInstr::StructNew { type_id, fields } => {
                for field in fields {
                    self.emit_instr(f, field);
                }
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::StructNew(wasm_idx));
            }
            WirInstr::StructGet {
                type_id,
                field_name,
                expr,
                ..
            } => {
                self.emit_instr(f, expr);
                let wasm_idx = self.resolve_type_index(type_id.index());
                let field_idx = self.resolve_field_index(type_id.index(), field_name);
                match self.is_field_packed(type_id.index(), field_name) {
                    Some(true) => {
                        // Signed packed (I8/I16) → struct.get_s
                        f.instruction(&Instruction::StructGetS {
                            struct_type_index: wasm_idx,
                            field_index: field_idx,
                        });
                    }
                    Some(false) => {
                        // Unsigned packed (U8/U16/Bool) → struct.get_u
                        f.instruction(&Instruction::StructGetU {
                            struct_type_index: wasm_idx,
                            field_index: field_idx,
                        });
                    }
                    None => {
                        // Non-packed → regular struct.get
                        f.instruction(&Instruction::StructGet {
                            struct_type_index: wasm_idx,
                            field_index: field_idx,
                        });
                    }
                }
            }
            WirInstr::StructSet {
                type_id,
                field_name,
                expr,
                value,
            } => {
                self.emit_instr(f, expr);
                self.emit_instr(f, value);
                let wasm_idx = self.resolve_type_index(type_id.index());
                let field_idx = self.resolve_field_index(type_id.index(), field_name);
                f.instruction(&Instruction::StructSet {
                    struct_type_index: wasm_idx,
                    field_index: field_idx,
                });
            }

            // GC: Array
            WirInstr::ArrayNew { type_id, init, len } => {
                self.emit_instr(f, init);
                self.emit_instr(f, len);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayNew(wasm_idx));
            }
            WirInstr::ArrayNewData {
                type_id,
                data_index,
                offset,
                len,
            } => {
                self.emit_instr(f, offset);
                self.emit_instr(f, len);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayNewData {
                    array_type_index: wasm_idx,
                    array_data_index: *data_index,
                });
            }
            WirInstr::ArrayNewDefault { type_id, len } => {
                self.emit_instr(f, len);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayNewDefault(wasm_idx));
            }
            WirInstr::ArrayNewFixed { type_id, elements } => {
                for elem in elements {
                    self.emit_instr(f, elem);
                }
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayNewFixed {
                    array_type_index: wasm_idx,
                    array_size: u32::try_from(elements.len()).unwrap(),
                });
            }
            WirInstr::ArrayGet {
                type_id,
                array,
                index,
                ..
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, index);
                let wasm_idx = self.resolve_type_index(type_id.index());
                match self.is_array_packed(type_id.index()) {
                    Some(true) => f.instruction(&Instruction::ArrayGetS(wasm_idx)),
                    Some(false) => f.instruction(&Instruction::ArrayGetU(wasm_idx)),
                    None => f.instruction(&Instruction::ArrayGet(wasm_idx)),
                };
                // List elements are nullable in Wasm (for array.new_default).
                // Narrow to non-null since Wado values are always non-null.
                if self.is_array_element_ref(type_id.index()) {
                    f.instruction(&Instruction::RefAsNonNull);
                }
            }
            WirInstr::ArraySet {
                type_id,
                array,
                index,
                value,
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, index);
                self.emit_instr(f, value);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArraySet(wasm_idx));
            }
            WirInstr::ArrayLen(a) => {
                self.emit_instr(f, a);
                f.instruction(&Instruction::ArrayLen);
            }

            // GC: Reference
            WirInstr::RefNull { heap_type } => {
                let ht = self.wir_abstract_heap_to_wasm(heap_type);
                f.instruction(&Instruction::RefNull(ht));
            }
            WirInstr::RefIsNull(o) => self.emit_unary(f, o, Instruction::RefIsNull),
            WirInstr::RefAsNonNull(o) => {
                // If inner is a LocalGet for a ref_local, that handler already
                // emits ref.as_non_null — don't emit it again (would double-wrap).
                if let WirInstr::LocalGet { name, .. } = o.as_ref()
                    && self.ref_locals.contains(name.as_str())
                {
                    self.emit_instr(f, o);
                    return;
                }
                self.emit_unary(f, o, Instruction::RefAsNonNull);
            }
            WirInstr::RefEq(l, r) => self.emit_binary(f, l, r, Instruction::RefEq),

            // Control Flow
            WirInstr::Block {
                label: _,
                result,
                body,
            } => {
                let bt = self.wir_type_to_block_type(result);
                f.instruction(&Instruction::Block(bt));
                for instr in body {
                    self.emit_instr(f, instr);
                }
                f.instruction(&Instruction::End);
            }
            WirInstr::Loop { label: _, body } => {
                f.instruction(&Instruction::Loop(BlockType::Empty));
                for instr in body {
                    self.emit_instr(f, instr);
                }
                f.instruction(&Instruction::End);
            }
            WirInstr::If {
                condition,
                result,
                then_body,
                else_body,
            } => {
                // Check for branch hint wrapper on the condition
                let (actual_condition, hint) =
                    if let WirInstr::BranchHint { likely, expr } = condition.as_ref() {
                        (expr.as_ref(), Some(*likely))
                    } else {
                        (condition.as_ref(), None)
                    };
                self.emit_instr(f, actual_condition);
                if let Some(likely) = hint {
                    self.current_branch_hints
                        .push((f.byte_len() as u32, likely));
                }
                let bt = self.wir_type_to_block_type(result);
                f.instruction(&Instruction::If(bt));
                for instr in then_body {
                    self.emit_instr(f, instr);
                }
                if let Some(else_body) = else_body {
                    f.instruction(&Instruction::Else);
                    for instr in else_body {
                        self.emit_instr(f, instr);
                    }
                }
                f.instruction(&Instruction::End);
            }
            WirInstr::BranchHint { expr, .. } => {
                // Standalone branch hint (not consumed by If) — just emit the inner expr
                self.emit_instr(f, expr);
            }
            WirInstr::Br { depth } => {
                f.instruction(&Instruction::Br(*depth));
            }
            WirInstr::BrIf { depth, condition } => {
                self.emit_instr(f, condition);
                f.instruction(&Instruction::BrIf(*depth));
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.emit_instr(f, index);
                f.instruction(&Instruction::BrTable(targets.clone().into(), *default));
            }
            WirInstr::Return { value } => {
                if let Some(value) = value {
                    self.emit_instr(f, value);
                }
                f.instruction(&Instruction::Return);
            }
            WirInstr::Unreachable => {
                f.instruction(&Instruction::Unreachable);
            }
            WirInstr::Nop => {
                f.instruction(&Instruction::Nop);
            }
            // Pure marker consumed during WIR finalization; emits no code.
            WirInstr::ColdPath => {}
            WirInstr::Drop(o) => {
                self.emit_instr(f, o);
                if !o.always_diverges() {
                    f.instruction(&Instruction::Drop);
                }
            }

            // Calls
            WirInstr::Call { func_id, args } => {
                for arg in args {
                    self.emit_instr(f, arg);
                }
                let wasm_idx = self.resolve_func_index(func_id.index());
                f.instruction(&Instruction::Call(wasm_idx));
            }
            WirInstr::RefFunc { func_id } => {
                let wasm_idx = self.resolve_func_index(func_id.index());
                f.instruction(&Instruction::RefFunc(wasm_idx));
            }
            WirInstr::CallIndirect {
                type_id,
                table,
                index,
                args,
            } => {
                for arg in args {
                    self.emit_instr(f, arg);
                }
                self.emit_instr(f, index);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::CallIndirect {
                    type_index: wasm_idx,
                    table_index: *table,
                });
            }
            WirInstr::CallRef {
                type_id,
                func_ref,
                args,
            } => {
                for arg in args {
                    self.emit_instr(f, arg);
                }
                self.emit_instr(f, func_ref);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::CallRef(wasm_idx));
            }

            // Memory operations
            WirInstr::I32Load {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I32Load(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Load8U {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I32Load8U(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Load8S {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I32Load8S(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Load16U {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I32Load16U(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Load16S {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I32Load16S(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Store {
                offset,
                align,
                addr,
                value,
            } => {
                self.emit_instr(f, addr);
                self.emit_instr(f, value);
                f.instruction(&Instruction::I32Store(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Store8 {
                offset,
                align,
                addr,
                value,
            } => {
                self.emit_instr(f, addr);
                self.emit_instr(f, value);
                f.instruction(&Instruction::I32Store8(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I32Store16 {
                offset,
                align,
                addr,
                value,
            } => {
                self.emit_instr(f, addr);
                self.emit_instr(f, value);
                f.instruction(&Instruction::I32Store16(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I64Load {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::I64Load(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::I64Store {
                offset,
                align,
                addr,
                value,
            } => {
                self.emit_instr(f, addr);
                self.emit_instr(f, value);
                f.instruction(&Instruction::I64Store(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::V128Load {
                offset,
                align,
                addr,
            } => {
                self.emit_instr(f, addr);
                f.instruction(&Instruction::V128Load(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::V128Store {
                offset,
                align,
                addr,
                value,
            } => {
                self.emit_instr(f, addr);
                self.emit_instr(f, value);
                f.instruction(&Instruction::V128Store(MemArg {
                    offset: (*offset),
                    align: *align,
                    memory_index: 0,
                }));
            }
            WirInstr::MemorySize => {
                f.instruction(&Instruction::MemorySize(0));
            }
            WirInstr::MemoryGrow(o) => {
                self.emit_instr(f, o);
                f.instruction(&Instruction::MemoryGrow(0));
            }
            WirInstr::MemoryFill { dst, value, len } => {
                self.emit_instr(f, dst);
                self.emit_instr(f, value);
                self.emit_instr(f, len);
                f.instruction(&Instruction::MemoryFill(0));
            }

            // GC: Array (packed access)
            WirInstr::ArrayGetS {
                type_id,
                array,
                index,
                ..
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, index);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayGetS(wasm_idx));
            }
            WirInstr::ArrayGetU {
                type_id,
                array,
                index,
                ..
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, index);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayGetU(wasm_idx));
            }
            WirInstr::ArrayCopy {
                dest_type_id,
                src_type_id,
                dest,
                dest_offset,
                src,
                src_offset,
                len,
            } if self.codegen_flags.array_copy => {
                // Native lowering (the default): emit the Wasm `array.copy`
                // instruction directly. Benchmarking showed it wins big on
                // copy-heavy workloads (zlib decompress ~+41%, syntax-highlight
                // ~+10%) and is neutral elsewhere, so it is the default; the
                // arm below open-codes a loop instead and is selected with
                // `-f no-array-copy` (kept so we can keep re-measuring as the
                // wasmtime runtime evolves).
                //
                // Stack/immediate shape: `array.copy $dst $src` consumes
                // `dst_ref, dst_offset, src_ref, src_offset, len`. It accepts
                // nullable refs, so unlike the loop path no `ref.as_non_null`
                // narrowing is needed.
                let dst_wasm_idx = self.resolve_type_index(dest_type_id.index());
                let src_wasm_idx = self.resolve_type_index(src_type_id.index());
                self.emit_instr(f, dest);
                self.emit_instr(f, dest_offset);
                self.emit_instr(f, src);
                self.emit_instr(f, src_offset);
                self.emit_instr(f, len);
                f.instruction(&Instruction::ArrayCopy {
                    array_type_index_dst: dst_wasm_idx,
                    array_type_index_src: src_wasm_idx,
                });
            }
            WirInstr::ArrayCopy {
                dest_type_id,
                src_type_id,
                dest,
                dest_offset,
                src,
                src_offset,
                len,
            } => {
                // Lower `array.copy` to an inline Wasm loop. This is the
                // `-f no-array-copy` path, kept because wasmtime's `array.copy`
                // runtime path was historically much slower than an open-coded
                // loop for short copies (≲ a few hundred elements); open-coding
                // also lets Cranelift inline the bounds checks and the
                // per-element get/set. The native instruction (the arm above,
                // now the default) is selected when `array_copy` is set.
                let dst_wasm_idx = self.resolve_type_index(dest_type_id.index());
                let src_wasm_idx = self.resolve_type_index(src_type_id.index());
                let dst_name = format!(
                    "__array_copy_dst_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let src_name = format!(
                    "__array_copy_src_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let dst_off_name = format!(
                    "__array_copy_dst_off_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let src_off_name = format!(
                    "__array_copy_src_off_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let len_name = format!(
                    "__array_copy_len_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let i_name = format!(
                    "__array_copy_i_{}_{}",
                    dest_type_id.index(),
                    src_type_id.index()
                );
                let dst_local = self.resolve_local(&dst_name);
                let src_local = self.resolve_local(&src_name);
                let dst_off_local = self.resolve_local(&dst_off_name);
                let src_off_local = self.resolve_local(&src_off_name);
                let len_local = self.resolve_local(&len_name);
                let i_local = self.resolve_local(&i_name);

                // Stash the five argument expressions into locals. The dst/src
                // ref slots are declared non-null (init-tracking), so coerce
                // the producing expression to non-null before storing — the
                // upstream `array.copy` accepts nullable refs and the WIR
                // doesn't always narrow them at the call site.
                self.emit_instr(f, dest);
                f.instruction(&Instruction::RefAsNonNull);
                f.instruction(&Instruction::LocalSet(dst_local));
                self.emit_instr(f, dest_offset);
                f.instruction(&Instruction::LocalSet(dst_off_local));
                self.emit_instr(f, src);
                f.instruction(&Instruction::RefAsNonNull);
                f.instruction(&Instruction::LocalSet(src_local));
                self.emit_instr(f, src_offset);
                f.instruction(&Instruction::LocalSet(src_off_local));
                self.emit_instr(f, len);
                f.instruction(&Instruction::LocalSet(len_local));

                // Match `array.copy` semantics for overlapping copies: when the
                // destination offset is greater than the source offset, copy
                // from high index to low so we don't overwrite still-unread
                // source bytes. (Same-array shifts in fpfmt rely on this.)
                f.instruction(&Instruction::LocalGet(dst_off_local));
                f.instruction(&Instruction::LocalGet(src_off_local));
                f.instruction(&Instruction::I32GtS);
                f.instruction(&Instruction::If(BlockType::Empty));

                // Backward branch: i = len; while (i > 0) { i -= 1; dst[..+i] = src[..+i]; }
                f.instruction(&Instruction::LocalGet(len_local));
                f.instruction(&Instruction::LocalSet(i_local));
                f.instruction(&Instruction::Block(BlockType::Empty));
                f.instruction(&Instruction::Loop(BlockType::Empty));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::I32LeS);
                f.instruction(&Instruction::BrIf(1));
                // i -= 1
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::LocalSet(i_local));
                // dst, dst_off + i
                f.instruction(&Instruction::LocalGet(dst_local));
                f.instruction(&Instruction::LocalGet(dst_off_local));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Add);
                // src.get at src_off + i
                f.instruction(&Instruction::LocalGet(src_local));
                f.instruction(&Instruction::LocalGet(src_off_local));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Add);
                match self.is_array_packed(src_type_id.index()) {
                    Some(true) => {
                        f.instruction(&Instruction::ArrayGetS(src_wasm_idx));
                    }
                    Some(false) => {
                        f.instruction(&Instruction::ArrayGetU(src_wasm_idx));
                    }
                    None => {
                        f.instruction(&Instruction::ArrayGet(src_wasm_idx));
                    }
                }
                f.instruction(&Instruction::ArraySet(dst_wasm_idx));
                f.instruction(&Instruction::Br(0));
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::End);

                f.instruction(&Instruction::Else);

                // Forward branch: i = 0; while (i < len) { dst[..+i] = src[..+i]; i += 1; }
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::LocalSet(i_local));
                f.instruction(&Instruction::Block(BlockType::Empty));
                f.instruction(&Instruction::Loop(BlockType::Empty));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::LocalGet(len_local));
                f.instruction(&Instruction::I32GeS);
                f.instruction(&Instruction::BrIf(1));
                // dst, dst_off + i
                f.instruction(&Instruction::LocalGet(dst_local));
                f.instruction(&Instruction::LocalGet(dst_off_local));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Add);
                // src.get at src_off + i
                f.instruction(&Instruction::LocalGet(src_local));
                f.instruction(&Instruction::LocalGet(src_off_local));
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Add);
                match self.is_array_packed(src_type_id.index()) {
                    Some(true) => {
                        f.instruction(&Instruction::ArrayGetS(src_wasm_idx));
                    }
                    Some(false) => {
                        f.instruction(&Instruction::ArrayGetU(src_wasm_idx));
                    }
                    None => {
                        f.instruction(&Instruction::ArrayGet(src_wasm_idx));
                    }
                }
                f.instruction(&Instruction::ArraySet(dst_wasm_idx));
                // i += 1
                f.instruction(&Instruction::LocalGet(i_local));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(i_local));
                f.instruction(&Instruction::Br(0));
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::End);

                f.instruction(&Instruction::End);
            }
            WirInstr::ArrayFill {
                type_id,
                array,
                offset,
                value,
                len,
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, offset);
                self.emit_instr(f, value);
                self.emit_instr(f, len);
                let wasm_idx = self.resolve_type_index(type_id.index());
                f.instruction(&Instruction::ArrayFill(wasm_idx));
            }
            WirInstr::ArrayClone {
                type_id,
                src,
                element_copy_func,
            } => {
                // Allocate a fresh array the same length as `src` and copy
                // every element with a JIT-compiled loop. This was the
                // per-array-field code path inside the former
                // `emit_value_copy` for raw `builtin::array<T>` fields and is
                // now reachable only via the synthesized `$value_copy$`
                // helpers' explicit `builtin::array_clone::<T>(...)` calls.
                //
                // When `element_copy_func` is set the loop additionally
                // calls the named `$value_copy$T<id>` helper between
                // `array.get` and `array.set` so each destination element
                // is a fresh struct, not an aliased ref into the source.
                let arr_wasm_idx = self.resolve_type_index(type_id.index());
                let src_name = format!("__copy_arr_src_{}", type_id.index());
                let dst_name = format!("__copy_arr_dst_{}", type_id.index());
                let len_name = format!("__copy_arr_len_{}", type_id.index());
                let loop_idx_name = format!("__copy_arr_i_{}", type_id.index());
                let src_local = self.resolve_local(&src_name);
                let dst_local = self.resolve_local(&dst_name);
                let len_local = self.resolve_local(&len_name);
                let loop_idx_local = self.resolve_local(&loop_idx_name);
                self.emit_instr(f, src);
                f.instruction(&Instruction::LocalSet(src_local));
                f.instruction(&Instruction::LocalGet(src_local));
                f.instruction(&Instruction::ArrayLen);
                f.instruction(&Instruction::LocalSet(len_local));
                f.instruction(&Instruction::LocalGet(len_local));
                f.instruction(&Instruction::ArrayNewDefault(arr_wasm_idx));
                f.instruction(&Instruction::LocalSet(dst_local));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::LocalSet(loop_idx_local));
                f.instruction(&Instruction::Block(BlockType::Empty));
                f.instruction(&Instruction::Loop(BlockType::Empty));
                f.instruction(&Instruction::LocalGet(loop_idx_local));
                f.instruction(&Instruction::LocalGet(len_local));
                f.instruction(&Instruction::I32GeS);
                f.instruction(&Instruction::BrIf(1));
                f.instruction(&Instruction::LocalGet(dst_local));
                f.instruction(&Instruction::LocalGet(loop_idx_local));
                f.instruction(&Instruction::LocalGet(src_local));
                f.instruction(&Instruction::LocalGet(loop_idx_local));
                match self.is_array_packed(type_id.index()) {
                    Some(true) => {
                        f.instruction(&Instruction::ArrayGetS(arr_wasm_idx));
                    }
                    Some(false) => {
                        f.instruction(&Instruction::ArrayGetU(arr_wasm_idx));
                    }
                    None => {
                        f.instruction(&Instruction::ArrayGet(arr_wasm_idx));
                    }
                }
                if let Some(func_name) = element_copy_func {
                    let func_idx = self
                        .resolve_function_index_by_suffix(func_name)
                        .unwrap_or_else(|| {
                            panic!("WirInstr::ArrayClone references unknown helper {func_name}")
                        });
                    // Wasm GC `array.get` produces `(ref null T)`; the
                    // synthesized `$value_copy$T<id>` expects a
                    // non-null `(ref T)`. `List<T>::repr` is sized to
                    // capacity (≥ `used`), so the slots beyond `used`
                    // hold `array.new_default`'s null. Branch on
                    // null and short-circuit those slots — preserving
                    // the null in the destination — instead of
                    // unconditionally `ref.as_non_null` which would
                    // trap on every empty trailing slot.
                    let elem_name = format!("__copy_arr_elem_{}", type_id.index());
                    let elem_local = self.resolve_local(&elem_name);
                    let elem_val = self
                        .array_element_val_type(type_id.index())
                        .expect("array element val type known when element_copy_func is set");
                    f.instruction(&Instruction::LocalSet(elem_local));
                    f.instruction(&Instruction::LocalGet(elem_local));
                    f.instruction(&Instruction::RefIsNull);
                    f.instruction(&Instruction::If(BlockType::Result(elem_val)));
                    f.instruction(&Instruction::LocalGet(elem_local));
                    f.instruction(&Instruction::Else);
                    f.instruction(&Instruction::LocalGet(elem_local));
                    f.instruction(&Instruction::RefAsNonNull);
                    f.instruction(&Instruction::Call(func_idx));
                    f.instruction(&Instruction::End);
                }
                f.instruction(&Instruction::ArraySet(arr_wasm_idx));
                f.instruction(&Instruction::LocalGet(loop_idx_local));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(loop_idx_local));
                f.instruction(&Instruction::Br(0));
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::LocalGet(dst_local));
            }

            // GC: Reference (casts, i31, extern)
            WirInstr::RefCast {
                type_id,
                nullable,
                expr,
            } => {
                self.emit_instr(f, expr);
                let wasm_idx = self.resolve_type_index(type_id.index());
                if *nullable {
                    f.instruction(&Instruction::RefCastNullable(HeapType::Concrete(wasm_idx)));
                } else {
                    f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(wasm_idx)));
                }
            }
            WirInstr::RefTest {
                type_id,
                nullable,
                expr,
            } => {
                self.emit_instr(f, expr);
                let wasm_idx = self.resolve_type_index(type_id.index());
                if *nullable {
                    f.instruction(&Instruction::RefTestNullable(HeapType::Concrete(wasm_idx)));
                } else {
                    f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(wasm_idx)));
                }
            }
            WirInstr::RefI31(o) => self.emit_unary(f, o, Instruction::RefI31),
            WirInstr::I31GetS(o) => self.emit_unary(f, o, Instruction::I31GetS),
            WirInstr::I31GetU(o) => self.emit_unary(f, o, Instruction::I31GetU),
            WirInstr::ExternInternalize(o) => self.emit_unary(f, o, Instruction::AnyConvertExtern),
            WirInstr::ExternExternalize(o) => self.emit_unary(f, o, Instruction::ExternConvertAny),

            // Select
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ty,
            } => {
                self.emit_instr(f, if_true);
                self.emit_instr(f, if_false);
                self.emit_instr(f, condition);
                if let Some(wir_ty) = ty {
                    let vt = self.wir_type_to_val_type(wir_ty);
                    f.instruction(&Instruction::TypedSelect(vt));
                } else {
                    f.instruction(&Instruction::Select);
                }
            }

            // 128-bit integer multi-value operations
            WirInstr::I64Add128(a_lo, a_hi, b_lo, b_hi) => {
                self.emit_instr(f, a_lo);
                self.emit_instr(f, a_hi);
                self.emit_instr(f, b_lo);
                self.emit_instr(f, b_hi);
                f.instruction(&Instruction::I64Add128);
            }
            WirInstr::I64Sub128(a_lo, a_hi, b_lo, b_hi) => {
                self.emit_instr(f, a_lo);
                self.emit_instr(f, a_hi);
                self.emit_instr(f, b_lo);
                self.emit_instr(f, b_hi);
                f.instruction(&Instruction::I64Sub128);
            }
            WirInstr::I64MulWideU(a, b) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                f.instruction(&Instruction::I64MulWideU);
            }
            WirInstr::I64MulWideS(a, b) => {
                self.emit_instr(f, a);
                self.emit_instr(f, b);
                f.instruction(&Instruction::I64MulWideS);
            }
            // Multi-value local bind (tuple elision): emit the multi-value instr,
            // then local.set for each target in reverse order (top of stack first).
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.emit_instr(f, instr);
                for local_opt in locals.iter().rev() {
                    if let Some(name) = local_opt {
                        let idx = self.resolve_local(name);
                        f.instruction(&Instruction::LocalSet(idx));
                    } else {
                        f.instruction(&Instruction::Drop);
                    }
                }
            }

            // Sequence
            WirInstr::Seq(body) => {
                for instr in body {
                    self.emit_instr(f, instr);
                }
            }

            // Everything else - emit unreachable for unimplemented instructions
            other => {
                eprintln!("[WIR-EMIT] unhandled instruction: {other:?}");
                f.instruction(&Instruction::Unreachable);
            }
        }
    }

    // === Data Section ===

    fn emit_data_section(&self) -> DataSection {
        let mut data = DataSection::new();
        for d in &self.wir.data {
            // Passive data segment (no memory offset)
            data.passive(d.bytes.iter().copied());
        }
        data
    }

    // === Name Section ===

    fn emit_name_section(&self) -> NameSection {
        let mut names = NameSection::new();

        if let Some(ref module_name) = self.wir.names.module_name {
            names.module(module_name);
        }

        if !self.wir.names.function_names.is_empty() {
            // Pre-populated names (e.g., memory module)
            let mut name_map = NameMap::new();
            for &(idx, ref name) in &self.wir.names.function_names {
                name_map.append(idx, name);
            }
            names.functions(&name_map);
        } else if !self.wir.functions.is_empty() {
            // Generate names from WIR functions using remapped Wasm indices
            let mut name_map = NameMap::new();
            for (i, func) in self.wir.functions.iter().enumerate() {
                let wir_func_idx = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
                if let Some(&wasm_idx) = self.func_index_map.get(&wir_func_idx) {
                    name_map.append(wasm_idx, &func.name.fq);
                }
            }
            names.functions(&name_map);
        }

        names
    }

    // === Helpers ===

    fn emit_binary(
        &mut self,
        f: &mut Function,
        left: &WirInstr,
        right: &WirInstr,
        op: Instruction<'_>,
    ) {
        self.emit_instr(f, left);
        self.emit_instr(f, right);
        f.instruction(&op);
    }

    fn emit_unary(&mut self, f: &mut Function, operand: &WirInstr, op: Instruction<'_>) {
        self.emit_instr(f, operand);
        f.instruction(&op);
    }

    fn emit_const_expr(&self, instr: &WirInstr) -> ConstExpr {
        let mut instrs = Vec::new();
        self.push_const_instrs(instr, &mut instrs);
        ConstExpr::extended(instrs)
    }

    /// Recursively lower a constant-expressible `WirInstr` tree into the
    /// instruction sequence of a Wasm constant init expression. Mirrors the
    /// aggregate-construction arms of [`Self::emit_instr`]; only instructions
    /// valid in a Wasm 3.0 GC constant expression are accepted (scalar consts,
    /// `ref.null` / `ref.i31` / `ref.func`, and `struct.new` / `array.new_fixed`
    /// / `array.new_default`). This mirrors [`WirInstr::is_const_expressible`],
    /// the predicate `wir_optimize::const_global` gates promotion on; anything
    /// else reaching an init slot is an optimizer bug — a non-const initializer
    /// should never be placed here — so it ICEs rather than silently
    /// miscompiling to `i32.const 0`.
    fn push_const_instrs<'i>(&'i self, instr: &'i WirInstr, out: &mut Vec<Instruction<'i>>) {
        match instr {
            WirInstr::I32Const(v) => out.push(Instruction::I32Const(*v)),
            WirInstr::I64Const(v) => out.push(Instruction::I64Const(*v)),
            WirInstr::F32Const(v) => out.push(Instruction::F32Const((*v).into())),
            WirInstr::F64Const(v) => out.push(Instruction::F64Const((*v).into())),
            WirInstr::RefNull { heap_type } => {
                out.push(Instruction::RefNull(
                    self.wir_abstract_heap_to_wasm(heap_type),
                ));
            }
            WirInstr::RefI31(inner) => {
                self.push_const_instrs(inner, out);
                out.push(Instruction::RefI31);
            }
            // `struct.new` / `array.new_*` aggregate fields wrap non-null
            // ref values in `RefAsNonNull`; the constructor already yields a
            // non-null ref, and `ref.as_non_null` is not a valid constant
            // instruction, so drop the wrapper and emit the inner value.
            WirInstr::RefAsNonNull(inner) => {
                self.push_const_instrs(inner, out);
            }
            WirInstr::RefFunc { func_id } => {
                out.push(Instruction::RefFunc(
                    self.resolve_func_index(func_id.index()),
                ));
            }
            WirInstr::StructNew { type_id, fields } => {
                for field in fields {
                    self.push_const_instrs(field, out);
                }
                out.push(Instruction::StructNew(
                    self.resolve_type_index(type_id.index()),
                ));
            }
            WirInstr::ArrayNewFixed { type_id, elements } => {
                for elem in elements {
                    self.push_const_instrs(elem, out);
                }
                out.push(Instruction::ArrayNewFixed {
                    array_type_index: self.resolve_type_index(type_id.index()),
                    array_size: u32::try_from(elements.len()).unwrap(),
                });
            }
            WirInstr::ArrayNewDefault { type_id, len } => {
                self.push_const_instrs(len, out);
                out.push(Instruction::ArrayNewDefault(
                    self.resolve_type_index(type_id.index()),
                ));
            }
            other => panic!(
                "non-const instruction in global initializer: {other:?}; \
                 a non-const init is an optimizer bug"
            ),
        }
    }

    fn resolve_local(&self, name: &str) -> u32 {
        self.current_locals
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("unresolved local: {name}"))
    }

    fn resolve_type_index(&self, wir_idx: u32) -> u32 {
        self.type_index_map.get(&wir_idx).copied().unwrap_or(0)
    }

    fn resolve_field_index(&self, wir_type_idx: u32, field_name: &str) -> u32 {
        self.struct_field_map
            .get(&wir_type_idx)
            .and_then(|m| m.get(field_name))
            .copied()
            .unwrap_or(0)
    }

    /// Check if a struct field is a packed type (i8/i16).
    /// Returns `Some(true)` for signed packed (I8/I16), `Some(false)` for unsigned packed (U8/U16),
    /// or `None` if the field is not packed.
    /// Check if a struct field is a packed type (i8/i16 storage).
    /// Returns `Some(true)` for signed packed (I8/I16), `Some(false)` for unsigned packed (U8/U16/Bool),
    /// or `None` if the field is not packed.
    /// Check if an array type has packed elements (i8/i16 storage).
    /// Returns `Some(true)` for signed packed (I8/I16), `Some(false)` for unsigned packed (U8/U16/Bool),
    /// or `None` if the array element is not packed.
    fn is_array_element_ref(&self, wir_type_idx: u32) -> bool {
        let idx = wir_type_idx as usize;
        idx < self.wir.types.len()
            && matches!(
                &self.wir.types[idx],
                WirTypeDef::Array(arr)
                    if matches!(arr.element_type, WirType::Ref { .. } | WirType::AbstractRef { .. })
            )
    }

    fn is_array_packed(&self, wir_type_idx: u32) -> Option<bool> {
        let idx = wir_type_idx as usize;
        if idx < self.wir.types.len()
            && let WirTypeDef::Array(ref arr) = self.wir.types[idx]
        {
            return match &arr.element_type {
                WirType::I8 | WirType::I16 => Some(true),
                WirType::U8 | WirType::U16 | WirType::Bool => Some(false),
                _ => None,
            };
        }
        None
    }

    fn is_field_packed(&self, wir_type_idx: u32, field_name: &str) -> Option<bool> {
        let idx = wir_type_idx as usize;
        if idx < self.wir.types.len()
            && let WirTypeDef::Struct(ref st) = self.wir.types[idx]
        {
            for field in &st.fields {
                if field.name == field_name {
                    return match &field.ty {
                        WirType::I8 | WirType::I16 => Some(true),
                        WirType::U8 | WirType::U16 | WirType::Bool => Some(false),
                        _ => None,
                    };
                }
            }
        }
        None
    }

    fn resolve_func_index(&self, wir_func_idx: u32) -> u32 {
        self.func_index_map
            .get(&wir_func_idx)
            .copied()
            .unwrap_or(wir_func_idx) // fallback: use as-is (for imports)
    }

    /// Look up the (nullable) element `ValType` of a WIR array type by
    /// its WIR index. Returns `None` for non-array types or unsupported
    /// element shapes. Used by `WirInstr::ArrayClone`'s
    /// `element_copy_func` plumbing to size the per-element scratch
    /// local that holds the `(ref null T)` mid-loop value.
    fn array_element_val_type(&self, wir_type_idx: u32) -> Option<ValType> {
        let idx = wir_type_idx as usize;
        if idx >= self.wir.types.len() {
            return None;
        }
        if let WirTypeDef::Array(a) = &self.wir.types[idx] {
            let nullable = a.element_type.clone().as_nullable();
            return Some(self.wir_type_to_val_type(&nullable));
        }
        None
    }

    /// Look up the Wasm function index of a defined function whose
    /// fully-qualified WIR name ends with `name_suffix`. Used by
    /// `WirInstr::ArrayClone`'s `element_copy_func` plumbing to call
    /// the synthesized `$value_copy$T<id>` helper between `array.get`
    /// and `array.set` — synthesize.rs emits the bare helper name and
    /// the FQ form depends on which entry module mints it, which the
    /// codegen prefers not to thread through.
    fn resolve_function_index_by_suffix(&self, name_suffix: &str) -> Option<u32> {
        for (i, func) in self.wir.functions.iter().enumerate() {
            if func.name.fq.ends_with(name_suffix) {
                let wir_func_idx = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).ok()?;
                return self.func_index_map.get(&wir_func_idx).copied();
            }
        }
        None
    }

    fn get_func_type(&self, wir_type_idx: u32) -> Option<&WirFuncType> {
        let idx = wir_type_idx as usize;
        if idx < self.wir.types.len()
            && let WirTypeDef::Func(ref ft) = self.wir.types[idx]
        {
            return Some(ft);
        }
        None
    }

    /// Convert `WirType` to Wasm `ValType` (for locals and function signatures).
    fn wir_type_to_val_type(&self, ty: &WirType) -> ValType {
        match ty {
            WirType::I8
            | WirType::I16
            | WirType::I32
            | WirType::U8
            | WirType::U16
            | WirType::U32
            | WirType::Bool
            | WirType::Char
            | WirType::Unit => ValType::I32,
            WirType::I64 | WirType::U64 => ValType::I64,
            WirType::F32 => ValType::F32,
            WirType::F64 => ValType::F64,
            WirType::V128 => ValType::V128,
            WirType::Enum { .. } | WirType::Flags { .. } => ValType::I32,
            WirType::Ref { type_id, nullable } => {
                let wasm_idx = self.resolve_type_index(type_id.index());
                ValType::Ref(RefType {
                    nullable: *nullable,
                    heap_type: HeapType::Concrete(wasm_idx),
                })
            }
            WirType::AbstractRef {
                heap_type,
                nullable,
            } => {
                let ht = self.wir_abstract_heap_to_wasm_heap(heap_type);
                ValType::Ref(RefType {
                    nullable: *nullable,
                    heap_type: ht,
                })
            }
        }
    }

    /// Convert `WirType` to Wasm `StorageType` (for struct fields).
    fn wir_type_to_storage_type(&self, ty: &WirType) -> StorageType {
        match ty {
            WirType::I8 | WirType::U8 | WirType::Bool => StorageType::I8,
            WirType::I16 | WirType::U16 => StorageType::I16,
            _ => StorageType::Val(self.wir_type_to_val_type(ty)),
        }
    }

    fn wir_type_to_block_type(&self, result: &Option<WirType>) -> BlockType {
        match result {
            None => BlockType::Empty,
            Some(ty) => BlockType::Result(self.wir_type_to_val_type(ty)),
        }
    }

    fn wir_abstract_heap_to_wasm(&self, ht: &WirAbstractHeapType) -> HeapType {
        HeapType::Abstract {
            shared: false,
            ty: match ht {
                WirAbstractHeapType::Any => AbstractHeapType::Any,
                WirAbstractHeapType::Eq => AbstractHeapType::Eq,
                WirAbstractHeapType::Struct => AbstractHeapType::Struct,
                WirAbstractHeapType::Array => AbstractHeapType::Array,
                WirAbstractHeapType::Func => AbstractHeapType::Func,
                WirAbstractHeapType::None => AbstractHeapType::None,
                WirAbstractHeapType::NoFunc => AbstractHeapType::NoFunc,
                WirAbstractHeapType::Extern => AbstractHeapType::Extern,
            },
        }
    }

    fn wir_abstract_heap_to_wasm_heap(&self, ht: &WirAbstractHeapType) -> HeapType {
        self.wir_abstract_heap_to_wasm(ht)
    }

    /// Collect all function indices referenced by `RefFunc` instructions.
    /// These need to be declared in a declarative element segment.
    fn collect_ref_func_indices(&self) -> Vec<u32> {
        let mut indices = IndexSet::default();
        for func in &self.wir.functions {
            if let Some(ref body) = func.body {
                self.scan_ref_func_instrs(body, &mut indices);
            }
        }
        indices.into_iter().collect()
    }

    fn scan_ref_func_instrs(&self, body: &[WirInstr], indices: &mut IndexSet<u32>) {
        for instr in body {
            self.scan_ref_func_instr(instr, indices);
        }
    }

    fn scan_ref_func_instr(&self, instr: &WirInstr, indices: &mut IndexSet<u32>) {
        if let WirInstr::RefFunc { func_id } = instr {
            let wasm_idx = self.resolve_func_index(func_id.index());
            indices.insert(wasm_idx);
        }
        // Recursively scan children
        instr.for_each_child(&mut |child| {
            self.scan_ref_func_instr(child, indices);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::emit_core_module;
    use crate::hashmap::{IndexMap, IndexSet};
    use crate::wir::{
        WirComponent, WirField, WirGlobal, WirInstr, WirMeta, WirName, WirNames, WirPackage,
        WirStructType, WirType, WirTypeDef, WirTypeId,
    };
    use std::rc::Rc;

    fn empty_package() -> WirPackage {
        WirPackage {
            types: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            elements: Vec::new(),
            memories: Vec::new(),
            data: Vec::new(),
            names: WirNames::default(),
            component: WirComponent::default(),
            variant_case_info: IndexMap::default(),
            wasm_modules: IndexMap::default(),
            dead_type_indices: IndexSet::default(),
            dead_func_indices: IndexSet::default(),
            dead_global_indices: IndexSet::default(),
            needed_canonicals: IndexSet::default(),
            defined_func_base: 0,
        }
    }

    fn validate(bytes: &[u8]) -> Result<(), String> {
        let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        v.validate_all(bytes).map(|_| ()).map_err(|e| e.to_string())
    }

    /// A global whose initializer is a constant `struct.new` must be emitted as
    /// a Wasm GC constant init expression — not the scalar `i32.const 0`
    /// fallback, which leaves the `(ref $P)` global's init ill-typed.
    #[test]
    fn const_struct_global_init_validates() {
        let tid = WirTypeId::new(0, Rc::from("test//P"));
        let struct_ty = WirStructType {
            name: WirName {
                fq: "test//P".to_string(),
            },
            fields: vec![
                WirField {
                    name: "x".to_string(),
                    ty: WirType::I32,
                    mutable: false,
                },
                WirField {
                    name: "y".to_string(),
                    ty: WirType::I32,
                    mutable: false,
                },
            ],
            meta: WirMeta::default(),
            generic_origin: None,
            newtype_origin: None,
            supertype: None,
        };
        let global = WirGlobal {
            name: WirName {
                fq: "global:ORIGIN".to_string(),
            },
            ty: WirType::Ref {
                type_id: tid.clone(),
                nullable: false,
            },
            mutable: false,
            wado_mutable: false,
            init: WirInstr::StructNew {
                type_id: tid,
                fields: vec![WirInstr::I32Const(3), WirInstr::I32Const(4)],
            },
            lazy_init: false,
            meta: WirMeta::default(),
        };
        let mut pkg = empty_package();
        pkg.types.push(WirTypeDef::Struct(struct_ty));
        pkg.globals.push(global);

        let bytes = emit_core_module(&pkg, false, crate::codegen_flags::CodegenFlags::default());
        validate(&bytes).expect("const struct global init should validate as Wasm");
    }
}
