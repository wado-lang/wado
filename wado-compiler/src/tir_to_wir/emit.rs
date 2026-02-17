//! WIR emission — converts a `WirModule` into core Wasm module binary bytes.
//!
//! This module handles the mechanical translation from WIR's tree-structured
//! representation to flat Wasm instructions and binary encoding.

use indexmap::IndexMap;

use crate::wir::{
    WirAbstractHeapType, WirArrayType, WirCopyType, WirExportDesc, WirFuncType, WirFunction,
    WirImportDesc, WirInstr, WirModule, WirStructType, WirType, WirTypeDef, WirTypeId,
    WirVariantType,
};

use wasm_encoder::{
    AbstractHeapType, BlockType, CodeSection, CompositeInnerType, CompositeType, ConstExpr,
    DataCountSection, DataSection, ExportKind, ExportSection, FieldType, Function, FunctionSection,
    GlobalSection, GlobalType, HeapType, ImportSection, Instruction, MemArg, Module, NameMap,
    NameSection, RefType, StorageType, StructType, SubType, TypeSection, ValType,
};

/// Emit a core Wasm module from a `WirModule`.
pub fn emit_core_module(wir: &WirModule) -> Vec<u8> {
    let mut emitter = WirEmitter::new(wir);
    emitter.emit()
}

/// Emitter state for converting `WirModule` to Wasm binary.
struct WirEmitter<'a> {
    wir: &'a WirModule,
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
    /// Next local index for current function.
    next_local: u32,
    /// Wasm type section counter.
    next_type_idx: u32,
}

impl<'a> WirEmitter<'a> {
    fn new(wir: &'a WirModule) -> Self {
        Self {
            wir,
            type_index_map: IndexMap::new(),
            variant_case_types: IndexMap::new(),
            struct_field_map: IndexMap::new(),
            func_index_offset: 0,
            func_index_map: IndexMap::new(),
            global_name_map: IndexMap::new(),
            current_locals: IndexMap::new(),
            next_local: 0,
            next_type_idx: 0,
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

        // 4. Memory section (if needed and not imported)
        // Memory is imported from component level, not defined here

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
        if !self.wir.functions.is_empty() {
            // Emit declarative element segment for any ref.func usage
            // This allows function references to be created
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
        module.section(&code);

        // 10. Data section
        if !self.wir.data.is_empty() {
            let data = self.emit_data_section();
            module.section(&data);
        }

        // 11. Name section (optional)
        if self.wir.names.module_name.is_some() || !self.wir.names.function_names.is_empty() {
            let names = self.emit_name_section();
            module.section(&names);
        }

        module.finish()
    }

    // === Type Section ===

    fn emit_type_section(&mut self) -> TypeSection {
        let mut types = TypeSection::new();

        for (wir_idx, typedef) in self.wir.types.iter().enumerate() {
            let wir_idx = u32::try_from(wir_idx).unwrap();
            match typedef {
                WirTypeDef::Struct(_) if self.wir.variant_case_info.contains_key(&wir_idx) => {
                    // Variant case struct — already emitted as part of the variant's rec group.
                    // Type index mapping is set up in emit_variant_type.
                }
                WirTypeDef::Struct(s) => {
                    self.emit_struct_type(&mut types, s, wir_idx);
                }
                WirTypeDef::Variant(v) => {
                    self.emit_variant_type(&mut types, v, wir_idx);
                }
                WirTypeDef::Enum(_) | WirTypeDef::Flags(_) => {
                    // Enums and flags don't produce Wasm type section entries
                }
                WirTypeDef::Array(a) => {
                    self.emit_array_type(&mut types, a, wir_idx);
                }
                WirTypeDef::Func(f) => {
                    self.emit_func_type(&mut types, f, wir_idx);
                }
            }
        }

        types
    }

    fn emit_struct_type(&mut self, types: &mut TypeSection, s: &WirStructType, wir_idx: u32) {
        let type_idx = self.next_type_idx;
        self.next_type_idx += 1;
        self.type_index_map.insert(wir_idx, type_idx);

        let mut field_map = IndexMap::new();
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

        // Use is_final: false so (ref (exact $T)) from struct.new is a subtype of (ref null $T)
        types.ty().subtype(&SubType {
            is_final: false,
            supertype_idx: None,
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

    fn emit_variant_type(&mut self, types: &mut TypeSection, v: &WirVariantType, wir_idx: u32) {
        // Base type: struct with just discriminant field
        let base_type_idx = self.next_type_idx;
        self.type_index_map.insert(wir_idx, base_type_idx);

        // Build field map for the base type
        let mut field_map = IndexMap::new();
        field_map.insert("discriminant".to_string(), 0);
        self.struct_field_map.insert(wir_idx, field_map);

        // Build all subtypes for the rec group
        let mut subtypes: Vec<SubType> = Vec::new();

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
        self.next_type_idx += 1;

        // Case subtypes
        let mut case_types = Vec::new();
        for case in &v.cases {
            let case_type_idx = self.next_type_idx;
            self.next_type_idx += 1;

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

            case_types.push((case.index, case_type_idx));
        }
        self.variant_case_types.insert(wir_idx, case_types.clone());

        // Map variant case WIR type indices to their Wasm type indices
        for (&case_wir_idx, &(variant_wir_idx, case_idx)) in &self.wir.variant_case_info {
            if variant_wir_idx == wir_idx
                && let Some(&(_, wasm_idx)) = case_types.iter().find(|(idx, _)| *idx == case_idx)
            {
                self.type_index_map.insert(case_wir_idx, wasm_idx);
                // Register field map for case struct
                let mut case_field_map = IndexMap::new();
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

        // Emit all types in a single rec group
        types.ty().rec(subtypes);
    }

    fn emit_array_type(&mut self, types: &mut TypeSection, a: &WirArrayType, wir_idx: u32) {
        let type_idx = self.next_type_idx;
        self.next_type_idx += 1;
        self.type_index_map.insert(wir_idx, type_idx);

        let storage_type = self.wir_type_to_storage_type(&a.element_type);
        types.ty().subtype(&SubType {
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

    fn emit_func_type(&mut self, types: &mut TypeSection, f: &WirFuncType, wir_idx: u32) {
        let type_idx = self.next_type_idx;
        self.next_type_idx += 1;
        self.type_index_map.insert(wir_idx, type_idx);

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

    fn build_func_index_map(&mut self) {
        // Import functions already have indices 0..import_func_count-1
        // Defined functions start at import_func_count
        for (i, _func) in self.wir.functions.iter().enumerate() {
            let wasm_idx = self.func_index_offset + u32::try_from(i).unwrap();
            // The WirFuncId index should be import_func_count + i
            let wir_func_idx = self.func_index_offset + u32::try_from(i).unwrap();
            self.func_index_map.insert(wir_func_idx, wasm_idx);
        }
        // Import functions map directly
        for i in 0..self.func_index_offset {
            self.func_index_map.insert(i, i);
        }
    }

    fn build_global_name_map(&mut self) {
        for (i, global) in self.wir.globals.iter().enumerate() {
            let idx = u32::try_from(i).unwrap();
            self.global_name_map.insert(global.name.fq.clone(), idx);
        }
    }

    fn resolve_global(&self, name: &str) -> u32 {
        self.global_name_map.get(name).copied().unwrap_or_else(|| {
            eprintln!("[WIR emit] Warning: global '{name}' not found, using index 0");
            0
        })
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
                    let wasm_idx = func_id.index();
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

        for func in &self.wir.functions {
            let wasm_func = self.emit_function(func);
            code.function(&wasm_func);
        }

        code
    }

    fn emit_function(&mut self, func: &WirFunction) -> Function {
        // Reset local tracking
        self.current_locals.clear();
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
    fn collect_declared_locals(&self, body: &[WirInstr], locals: &mut Vec<(String, ValType)>) {
        for instr in body {
            self.collect_declared_locals_instr(instr, locals);
        }
    }

    /// Recursively collect `DeclareLocal` from a single instruction and all its children.
    fn collect_declared_locals_instr(&self, instr: &WirInstr, locals: &mut Vec<(String, ValType)>) {
        match instr {
            WirInstr::DeclareLocal { name, ty } => {
                locals.push((name.clone(), self.wir_type_to_val_type(ty)));
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
                dest,
                dest_offset,
                src,
                src_offset,
                len,
                ..
            } => {
                self.collect_declared_locals_instr(dest, locals);
                self.collect_declared_locals_instr(dest_offset, locals);
                self.collect_declared_locals_instr(src, locals);
                self.collect_declared_locals_instr(src_offset, locals);
                self.collect_declared_locals_instr(len, locals);
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
            WirInstr::BrIf { condition, .. } => {
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
            // Value copy needs a temp local for the source struct ref
            WirInstr::ValueCopy {
                type_id, expr, ..
            } => {
                self.collect_declared_locals_instr(expr, locals);
                // Declare temp local for the struct copy source
                let wasm_type_idx = self.resolve_type_index(type_id.index());
                let ref_type = RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(wasm_type_idx),
                };
                let temp_name = format!("__copy_source_{}", type_id.index());
                locals.push((temp_name, ValType::Ref(ref_type)));
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
            WirInstr::LocalGet { name } => {
                let idx = self.resolve_local(name);
                f.instruction(&Instruction::LocalGet(idx));
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
            }
            WirInstr::GlobalGet { name } => {
                let idx = self.resolve_global(&name.fq);
                f.instruction(&Instruction::GlobalGet(idx));
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
            } => {
                self.emit_instr(f, array);
                self.emit_instr(f, index);
                let wasm_idx = self.resolve_type_index(type_id.index());
                match self.is_array_packed(type_id.index()) {
                    Some(true) => f.instruction(&Instruction::ArrayGetS(wasm_idx)),
                    Some(false) => f.instruction(&Instruction::ArrayGetU(wasm_idx)),
                    None => f.instruction(&Instruction::ArrayGet(wasm_idx)),
                };
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
            WirInstr::RefAsNonNull(o) => self.emit_unary(f, o, Instruction::RefAsNonNull),
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
                self.emit_instr(f, condition);
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
                // Don't emit nop
            }
            WirInstr::Drop(o) => {
                self.emit_instr(f, o);
                f.instruction(&Instruction::Drop);
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
            WirInstr::MemorySize => {
                f.instruction(&Instruction::MemorySize(0));
            }
            WirInstr::MemoryGrow(o) => {
                self.emit_instr(f, o);
                f.instruction(&Instruction::MemoryGrow(0));
            }

            // GC: Array (packed access)
            WirInstr::ArrayGetS {
                type_id,
                array,
                index,
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
            } => {
                self.emit_instr(f, dest);
                self.emit_instr(f, dest_offset);
                self.emit_instr(f, src);
                self.emit_instr(f, src_offset);
                self.emit_instr(f, len);
                let dst_idx = self.resolve_type_index(dest_type_id.index());
                let src_idx = self.resolve_type_index(src_type_id.index());
                f.instruction(&Instruction::ArrayCopy {
                    array_type_index_dst: dst_idx,
                    array_type_index_src: src_idx,
                });
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

            // 128-bit integer operations (not yet implemented at Wasm level)
            WirInstr::I64Add128(..)
            | WirInstr::I64Sub128(..)
            | WirInstr::I64MulWideU(..)
            | WirInstr::I64MulWideS(..) => {
                f.instruction(&Instruction::Unreachable);
            }

            // Sequence
            WirInstr::Seq(body) => {
                for instr in body {
                    self.emit_instr(f, instr);
                }
            }

            // Value copy — struct shallow copy (field-by-field)
            WirInstr::ValueCopy {
                type_id,
                source_type,
                expr,
            } => {
                self.emit_value_copy(f, type_id, source_type, expr);
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
            let mut name_map = NameMap::new();
            for &(idx, ref name) in &self.wir.names.function_names {
                name_map.append(idx, name);
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

    /// Emit a value copy instruction (struct shallow copy).
    fn emit_value_copy(
        &mut self,
        f: &mut Function,
        type_id: &WirTypeId,
        source_type: &WirCopyType,
        expr: &WirInstr,
    ) {
        match source_type {
            WirCopyType::Struct { fields } => {
                let wasm_type_idx = self.resolve_type_index(type_id.index());
                // Emit source expression
                self.emit_instr(f, expr);
                // Look up the pre-declared temp local
                let temp_name = format!("__copy_source_{}", type_id.index());
                let temp_idx = self.resolve_local(&temp_name);
                // Store source to temp
                f.instruction(&Instruction::LocalSet(temp_idx));
                // For each field: load from temp, get field (handle packed fields)
                for field in fields {
                    f.instruction(&Instruction::LocalGet(temp_idx));
                    match self.is_field_packed_by_index(type_id.index(), field.index) {
                        Some(true) => {
                            f.instruction(&Instruction::StructGetS {
                                struct_type_index: wasm_type_idx,
                                field_index: field.index,
                            });
                        }
                        Some(false) => {
                            f.instruction(&Instruction::StructGetU {
                                struct_type_index: wasm_type_idx,
                                field_index: field.index,
                            });
                        }
                        None => {
                            f.instruction(&Instruction::StructGet {
                                struct_type_index: wasm_type_idx,
                                field_index: field.index,
                            });
                        }
                    }
                }
                // Create new struct with all field values
                f.instruction(&Instruction::StructNew(wasm_type_idx));
            }
            WirCopyType::Array { element_copy: _ } => {
                // Array copy: pass through for now (shallow ref copy)
                self.emit_instr(f, expr);
            }
            WirCopyType::Tuple { field_copies: _ } => {
                // Tuple copy: same as struct copy (tuples are anonymous structs)
                let wasm_type_idx = self.resolve_type_index(type_id.index());
                self.emit_instr(f, expr);
                let temp_name = format!("__copy_source_{}", type_id.index());
                let temp_idx = self.resolve_local(&temp_name);
                f.instruction(&Instruction::LocalSet(temp_idx));
                let field_count = self.get_struct_field_count(type_id);
                for i in 0..field_count {
                    f.instruction(&Instruction::LocalGet(temp_idx));
                    f.instruction(&Instruction::StructGet {
                        struct_type_index: wasm_type_idx,
                        field_index: i,
                    });
                }
                f.instruction(&Instruction::StructNew(wasm_type_idx));
            }
            WirCopyType::Variant { .. } | WirCopyType::Option { .. } => {
                // Variant and option copies are complex; pass through for now
                self.emit_instr(f, expr);
            }
        }
    }

    /// Get the number of fields in a WIR struct type.
    fn get_struct_field_count(&self, type_id: &WirTypeId) -> u32 {
        let idx = type_id.index() as usize;
        if idx < self.wir.types.len() {
            match &self.wir.types[idx] {
                WirTypeDef::Struct(s) => u32::try_from(s.fields.len()).unwrap(),
                _ => 0,
            }
        } else {
            0
        }
    }

    fn emit_const_expr(&self, instr: &WirInstr) -> ConstExpr {
        match instr {
            WirInstr::I32Const(v) => ConstExpr::i32_const(*v),
            WirInstr::I64Const(v) => ConstExpr::i64_const(*v),
            WirInstr::F32Const(v) => ConstExpr::f32_const((*v).into()),
            WirInstr::F64Const(v) => ConstExpr::f64_const((*v).into()),
            WirInstr::RefNull { .. } => ConstExpr::ref_null(HeapType::Abstract {
                shared: false,
                ty: AbstractHeapType::None,
            }),
            _ => ConstExpr::i32_const(0), // fallback
        }
    }

    fn resolve_local(&self, name: &str) -> u32 {
        self.current_locals.get(name).copied().unwrap_or(0) // fallback
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

    /// Check if a struct field (by index) has packed storage.
    fn is_field_packed_by_index(&self, wir_type_idx: u32, field_index: u32) -> Option<bool> {
        let idx = wir_type_idx as usize;
        if idx < self.wir.types.len()
            && let WirTypeDef::Struct(ref st) = self.wir.types[idx]
        {
            if let Some(field) = st.fields.get(field_index as usize) {
                return match &field.ty {
                    WirType::I8 | WirType::I16 => Some(true),
                    WirType::U8 | WirType::U16 | WirType::Bool => Some(false),
                    _ => None,
                };
            }
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
}
