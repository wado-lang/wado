// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use crate::ast::Type;
use crate::builtin_registry::{BuiltinFunctionInfo, BuiltinRegistry};
use crate::bundled::wado_bundled_wasm;
use crate::name::{FreeFunctionName, FunctionId, MethodName, StructName, build_core_internal_name};
use crate::optimize::OptimizationHints;
use crate::symbol::SymbolTable;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirModule, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::wasi_registry::{
    WasiFunctionInfo, WasiRegistry, build_local_alias_name, is_wasi_function_supported,
    wasi_type_to_valtype,
};
use crate::wasm_builder::{ComponentModelContext, CoreModuleBuilder};
use crate::wasm_postprocess;
use crate::world_registry::{WorldExportInfo, WorldRegistry};
use heck::ToKebabCase;
use std::collections::HashMap;
use wasm_encoder::{
    Alias, BranchHint, BranchHints, CanonicalOption, CodeSection, ComponentBuilder,
    ComponentExportKind, ComponentOuterAliasKind, ComponentValType, ConstExpr, DataCountSection,
    DataSection, DataSegment, DataSegmentMode, ExportKind, ExportSection, FieldType, Function,
    FunctionSection, HeapType, InstanceType, Instruction, MemArg, MemorySection, MemoryType,
    Module, ModuleArg, NameMap, NameSection, PrimitiveValType, RefType, StorageType, TypeBounds,
    TypeSection, ValType,
};
use wasmparser::{Validator, WasmFeatures};

/// Information about a user-defined struct type
#[derive(Debug, Clone)]
struct StructTypeInfo {
    type_idx: u32,
}

/// Code generator that produces Component Model components
/// Targets WASI P3
pub struct Codegen {
    string_literals: Vec<String>,
    /// Registry of WASI imports from lib/wasi/*.wado
    wasi_registry: WasiRegistry,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    builtin_registry: BuiltinRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    world_registry: WorldRegistry,
    /// Type index for string-array (GC array<u8>), set when types are defined
    string_array_type_idx: u32,
    /// Registry of user-defined struct types (keyed by StructName for type safety)
    struct_types: HashMap<StructName, StructTypeInfo>,
}

/// Context for tracking local variables during function code generation
/// Local indices in Wasm: parameters come first, then declared locals
struct FunctionContext {
    /// Map from variable name to local index
    locals: HashMap<String, u32>,
    /// Map from variable name to type (for type inference)
    local_type_map: HashMap<String, ValType>,
    /// Number of parameters (locals 0..param_count are parameters)
    #[allow(dead_code)]
    param_count: u32,
    /// Next available local index for new variables
    next_local: u32,
    /// Local types for non-parameter locals (for function declaration)
    local_types: Vec<ValType>,
    /// Return type of the function (for ref.as_non_null handling)
    return_type: Option<ValType>,
    /// Pending branch hint from builtin::likely() or builtin::unlikely()
    /// None = no hint, Some(true) = likely taken, Some(false) = unlikely taken
    pending_branch_hint: Option<bool>,
    /// Collected branch hints for this function (offset, taken)
    branch_hints: Vec<(u32, bool)>,
    /// Module path of the current function (for access control checks)
    current_module_path: Vec<String>,
}

impl FunctionContext {
    fn new(param_count: u32) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
            return_type: None,
            pending_branch_hint: None,
            branch_hints: Vec::new(),
            current_module_path: Vec::new(),
        }
    }

    fn with_module_path(param_count: u32, module_path: Vec<String>) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
            return_type: None,
            pending_branch_hint: None,
            branch_hints: Vec::new(),
            current_module_path: module_path,
        }
    }

    /// Set a pending branch hint (from builtin::likely/unlikely)
    fn set_branch_hint(&mut self, taken: bool) {
        self.pending_branch_hint = Some(taken);
    }

    /// Consume pending branch hint and record it at the given offset
    fn consume_branch_hint(&mut self, offset: u32) {
        if let Some(taken) = self.pending_branch_hint.take() {
            self.branch_hints.push((offset, taken));
        }
    }

    fn set_return_type(&mut self, ty: ValType) {
        self.return_type = Some(ty);
    }

    /// Add a parameter (must be called before any locals)
    fn add_param(&mut self, name: &str, ty: ValType) {
        let index = self.locals.len() as u32;
        self.locals.insert(name.to_string(), index);
        self.local_type_map.insert(name.to_string(), ty);
    }

    /// Allocate a new local variable, or return existing if already allocated
    fn alloc_local(&mut self, name: &str, ty: ValType) -> u32 {
        // Return existing local if already allocated (for pre-allocated scratch locals)
        if let Some(&existing) = self.locals.get(name) {
            return existing;
        }
        // Make reference types nullable so they don't require initialization at function entry.
        // Wasm GC validation requires non-nullable ref locals to be definitely assigned before use,
        // but variables declared in control flow branches can't satisfy this requirement.
        let ty = match ty {
            ValType::Ref(ref_type) if !ref_type.nullable => ValType::Ref(RefType {
                nullable: true,
                heap_type: ref_type.heap_type,
            }),
            _ => ty,
        };
        let index = self.next_local;
        self.locals.insert(name.to_string(), index);
        self.local_type_map.insert(name.to_string(), ty);
        self.local_types.push(ty);
        self.next_local += 1;
        index
    }

    /// Get local index by name
    fn get_local(&self, name: &str) -> Option<u32> {
        self.locals.get(name).copied()
    }

    /// Get local types for function declaration (after params)
    fn get_local_decls(&self) -> Vec<(u32, ValType)> {
        // Group consecutive locals of the same type
        let mut decls: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.local_types {
            if let Some((count, last_ty)) = decls.last_mut()
                && last_ty == ty
            {
                *count += 1;
                continue;
            }
            decls.push((1, *ty));
        }
        decls
    }
}

// CoreModuleBuilder and ComponentModelContext are in wasm_builder.rs

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a snake_case identifier to kebab-case for Component Model
fn to_kebab_case(name: &str) -> String {
    name.to_kebab_case()
}

impl Codegen {
    /// Create a new code generator with registries built from stdlib
    pub fn new() -> Self {
        let (wasi_registry, world_registry) = WasiRegistry::build_from_stdlib();
        let builtin_registry = BuiltinRegistry::build_from_stdlib();

        Self {
            string_literals: Vec::new(),
            wasi_registry,
            builtin_registry,
            world_registry,
            string_array_type_idx: 0, // Set when types are defined
            struct_types: HashMap::new(),
        }
    }

    /// Create a new code generator (backwards compatible)
    pub fn new_with_source(_source_code: String) -> Self {
        Self::new()
    }

    /// Validate generated Wasm binary using wasmparser
    ///
    /// This catches codegen bugs early by ensuring the output is valid Wasm.
    /// Panics if validation fails, as this indicates a compiler bug.
    fn validate_wasm(wasm: &[u8]) {
        let mut validator = Validator::new_with_features(WasmFeatures::all());
        if let Err(e) = validator.validate_all(wasm) {
            panic!(
                "Internal compiler error: generated invalid Wasm\n\
                 This is a bug in the Wado compiler. Please report it.\n\
                 Validation error: {e}"
            );
        }
    }

    /// Look up a struct type by name and module path.
    /// Tries qualified StructName first, falls back to simple name (empty module_path).
    fn lookup_struct_type(&self, name: &str, module_path: &[String]) -> Option<&StructTypeInfo> {
        if !module_path.is_empty() {
            // Try qualified name first
            let qualified = StructName::from_path_and_name(module_path, name);
            if let Some(info) = self.struct_types.get(&qualified) {
                return Some(info);
            }
        }
        // Fall back to simple name (empty module_path)
        let simple = StructName {
            module_path: Vec::new(),
            name: name.to_string(),
        };
        self.struct_types.get(&simple)
    }

    /// Generate Component Model binary Wasm
    pub fn generate_wasm(
        &mut self,
        entry_tir: &TirModule,
        all_tir_modules: &HashMap<Vec<String>, TirModule>,
        symbols: &SymbolTable,
        implicit_modules: &std::collections::HashSet<Vec<String>>,
        hints: &OptimizationHints,
        module_name: &str,
    ) -> Vec<u8> {
        // Collect pre-computed string literals from all TIR modules
        for tir_module in all_tir_modules.values() {
            for s in &tir_module.string_literals {
                if !self.string_literals.contains(s) {
                    self.string_literals.push(s.clone());
                }
            }
        }

        // Generate binary Wasm from TIR
        let wasm = self.generate_component(
            entry_tir,
            all_tir_modules,
            symbols,
            implicit_modules,
            hints,
            module_name,
        );

        // Validate the generated Wasm
        Self::validate_wasm(&wasm);

        wasm
    }

    /// Build main core module from TIR
    /// Build the main core Wasm module containing user-defined functions.
    fn build_main_module(
        &mut self,
        entry_tir: &TirModule,
        all_tir_modules: &HashMap<Vec<String>, TirModule>,
        symbols: &SymbolTable,
        _implicit_modules: &std::collections::HashSet<Vec<String>>,
        string_data: &[u8],
        hints: &OptimizationHints,
        module_name: &str,
    ) -> Vec<u8> {
        let mut module = Module::new();
        let mut builder = CoreModuleBuilder::new();
        let type_table = &entry_tir.type_table;

        // Collect ALL functions from loaded TIR modules (core:*, etc.)
        // We need to include all functions because they may have transitive dependencies
        // Format: (module_path, tir_func, type_table, qualified_name)
        let mut loaded_funcs: Vec<(Vec<String>, &TirFunction, &TypeTable, String)> = Vec::new();
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if path == &entry_tir.path {
                continue;
            }
            // Skip wasi:* modules (they only contain effect declarations)
            if path.first().map(|s| s == "wasi").unwrap_or(false) {
                continue;
            }
            for tir_func in &tir_mod.functions {
                // Skip run function
                if tir_func.name == "run" {
                    continue;
                }
                // Skip non-pub functions from other modules
                if !tir_func.is_pub {
                    continue;
                }
                // Skip bodyless functions
                if tir_func.body.is_none() {
                    continue;
                }
                // Skip methods (names containing "::") - they're handled in loaded_methods
                if tir_func.name.contains("::") {
                    continue;
                }
                // Skip functions with unsupported effects
                // Currently only Stdout, Stderr, and MonotonicClock effects are supported
                if !tir_func.effects.is_empty() {
                    let has_unsupported_effects = tir_func
                        .effects
                        .iter()
                        .any(|e| e != "Stdout" && e != "Stderr" && e != "MonotonicClock");
                    if has_unsupported_effects {
                        continue;
                    }
                }
                let func_id =
                    FunctionId::Free(FreeFunctionName::from_path_and_name(path, &tir_func.name));
                // Skip functions not reachable from entry point (DCE)
                if !hints.is_reachable(&func_id) {
                    continue;
                }
                let mangled_name = func_id.to_string();
                loaded_funcs.push((path.clone(), tir_func, &tir_mod.type_table, mangled_name));
            }
        }

        // Collect main module struct names first (for collision detection)
        let main_module_struct_names: std::collections::HashSet<String> =
            entry_tir.structs.iter().map(|s| s.name.clone()).collect();

        // Collect impl methods from loaded TIR modules
        // Note: With the current TIR design, methods are added as regular functions
        // (with mangled names like "Point::sum") in tir_mod.functions, not in impls.
        // This loop is kept for future when impls may be populated.
        // Format: (module_path, struct_lookup_name, tir_func, type_table, mangled_name)
        let mut loaded_methods: Vec<(Vec<String>, StructName, &TirFunction, &TypeTable, String)> =
            Vec::new();
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if path == &entry_tir.path {
                continue;
            }
            // Skip wasi:* modules
            if path.first().map(|s| s == "wasi").unwrap_or(false) {
                continue;
            }
            // Methods are stored as functions with mangled names like "Point::sum"
            // (resolver adds them to functions, not impls)
            for func in &tir_mod.functions {
                // Check if this is a method (name contains ::)
                if let Some(sep_pos) = func.name.find("::") {
                    let struct_name = &func.name[..sep_pos];
                    let method_name = &func.name[sep_pos + 2..];

                    // Skip non-pub methods
                    if !func.is_pub {
                        continue;
                    }
                    // Skip bodyless methods
                    if func.body.is_none() {
                        continue;
                    }
                    // Build function ID for DCE check: path/Struct::method
                    let method_id = FunctionId::Method(MethodName::new(
                        path.join("/"),
                        struct_name.to_string(),
                        None,
                        method_name.to_string(),
                    ));
                    // Skip methods not reachable from entry point (DCE)
                    if !hints.is_reachable(&method_id) {
                        continue;
                    }
                    let method_mangled = method_id.to_string();
                    // Determine struct lookup name - use qualified name if there's a collision
                    let struct_lookup_name = if main_module_struct_names.contains(struct_name) {
                        // Collision - use qualified StructName
                        StructName::from_path_and_name(path, struct_name)
                    } else {
                        // No collision - use simple StructName (empty module path)
                        StructName::new(vec![], struct_name.to_string())
                    };
                    // Use the same fully mangled name for registration
                    // This ensures consistency between DCE tracking and codegen
                    loaded_methods.push((
                        path.clone(),
                        struct_lookup_name,
                        func,
                        &tir_mod.type_table,
                        method_mangled,
                    ));
                }
            }
        }

        // Build import name → qualified name lookup table for call resolution
        let mut _import_lookup: HashMap<String, String> = HashMap::new();
        for (module_path, tir_func, _, qualified_name) in &loaded_funcs {
            if !module_path.is_empty() {
                _import_lookup.insert(tir_func.name.clone(), qualified_name.clone());
            }
        }

        // ========================================
        // Define types using the builder
        // ========================================

        // Builtin function types - derived from core/builtin.wado
        for func in self.builtin_registry.imported_builtins() {
            let canonical_name = func.canonical_name.as_ref().unwrap();
            let params = self.builtin_func_to_core_params(func);
            let results = self.builtin_func_to_core_results(func);
            builder.define_func_type(canonical_name, &params, &results);
        }

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        self.string_array_type_idx =
            builder.define_gc_array_type("string-array", StorageType::I8, true);

        let _string_array_idx_for_structs = builder.type_idx("string-array");

        // Register main module structs from TIR (with simple names - empty module path)
        // Note: main_module_struct_names was collected earlier for method collision detection
        for tir_struct in &entry_tir.structs {
            let struct_name = StructName::new(vec![], tir_struct.name.clone());
            self.register_struct_type(struct_name, tir_struct, type_table, &mut builder);
        }

        // Second pass: register loaded module structs with collision handling
        // - If no collision: register with simple name (empty module path)
        // - If collision with main module: register with qualified name (full module path)
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (already handled)
            if path == &entry_tir.path {
                continue;
            }
            for tir_struct in &tir_mod.structs {
                if !tir_struct.is_pub {
                    continue;
                }
                let struct_name = if main_module_struct_names.contains(&tir_struct.name) {
                    // Collision - use qualified name with full module path
                    StructName::from_path_and_name(path, &tir_struct.name)
                } else {
                    // No collision - use simple name (empty module path)
                    StructName::new(vec![], tir_struct.name.clone())
                };
                self.register_struct_type(
                    struct_name,
                    tir_struct,
                    &tir_mod.type_table,
                    &mut builder,
                );
            }
        }

        // Register struct type aliases (e.g., `Point as OtherPoint`)
        for (alias_name, alias_module_path, original_name) in symbols.get_struct_aliases() {
            let alias_struct_name = StructName::new(vec![], alias_name.clone());
            // Check if there's a collision (main module has same-named struct)
            if main_module_struct_names.contains(&original_name) && !alias_module_path.is_empty() {
                // Collision case - use qualified name from the alias's module
                let qualified_name =
                    StructName::from_path_and_name(&alias_module_path, &original_name);
                if let Some(info) = self.struct_types.get(&qualified_name).cloned() {
                    self.struct_types.insert(alias_struct_name, info);
                }
            } else {
                // No collision - use simple name (empty module path)
                let original_struct_name = StructName::new(vec![], original_name.clone());
                if let Some(info) = self.struct_types.get(&original_struct_name).cloned() {
                    self.struct_types.insert(alias_struct_name, info);
                }
            }
        }

        // WASI effect function types - derived from wasi/*.wado definitions
        for interface in self.wasi_registry.interfaces() {
            for func in &interface.functions {
                let local_name = func.local_alias_name();
                let params = self.wasi_func_to_core_params(func);
                let results = self.wasi_func_to_core_results(func);
                builder.define_func_type(&local_name, &params, &results);
            }
        }

        // Types for user-defined functions from entry TIR module
        for tir_func in &entry_tir.functions {
            let param_types: Vec<ValType> = tir_func
                .params
                .iter()
                .map(|p| self.type_id_to_valtype(type_table, p.type_id))
                .collect();

            // Never and Unit types have no Wasm return value
            let return_types: Vec<ValType> = if tir_func.return_type == TypeTable::NEVER
                || tir_func.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(type_table, tir_func.return_type)]
            };

            // Methods have names like "Point::sum" - use fully mangled name for type
            let type_name = if let Some(sep_pos) = tir_func.name.find("::") {
                let struct_name = &tir_func.name[..sep_pos];
                let method_name = &tir_func.name[sep_pos + 2..];
                MethodName::new(
                    entry_tir.path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
                )
                .to_string()
            } else {
                tir_func.name.clone()
            };
            builder.define_func_type(&type_name, &param_types, &return_types);
        }

        // Types for loaded module functions (TIR)
        for (_, tir_func, func_type_table, qualified_name) in &loaded_funcs {
            let param_types: Vec<ValType> = tir_func
                .params
                .iter()
                .map(|p| self.type_id_to_valtype(func_type_table, p.type_id))
                .collect();
            let return_types: Vec<ValType> = if tir_func.return_type == TypeTable::NEVER
                || tir_func.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(func_type_table, tir_func.return_type)]
            };
            builder.define_func_type(qualified_name, &param_types, &return_types);
        }

        // Types for impl methods from loaded modules (TIR)
        for (_module_path, struct_name, tir_method, method_type_table, mangled_name) in
            &loaded_methods
        {
            let mut param_types: Vec<ValType> = Vec::new();

            for param in &tir_method.params {
                if param.name == "self" {
                    // &self parameter: use reference to struct type
                    if let Some(struct_info) = self.struct_types.get(struct_name) {
                        let struct_ref_type = ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(struct_info.type_idx),
                        });
                        param_types.push(struct_ref_type);
                    } else {
                        panic!(
                            "struct type not found for &self parameter: {} (method: {}, available: {:?})",
                            struct_name,
                            mangled_name,
                            self.struct_types.keys().collect::<Vec<_>>()
                        );
                    }
                } else {
                    param_types.push(self.type_id_to_valtype(method_type_table, param.type_id));
                }
            }

            let return_types: Vec<ValType> = if tir_method.return_type == TypeTable::NEVER
                || tir_method.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(method_type_table, tir_method.return_type)]
            };
            builder.define_func_type(mangled_name, &param_types, &return_types);
        }

        // World export types - derived from Command world in wasi/cli.wado
        if let Some(run_export) = self.world_registry.get_export("Command", "run") {
            let params = self.world_export_to_core_params(run_export);
            let results = self.world_export_to_core_results(run_export);
            builder.define_func_type(&run_export.name, &params, &results);
        }

        // Add types section to module
        module.section(builder.types());

        // ========================================
        // Import section
        // ========================================
        // DCE: Only import stream intrinsics if Stdout/Stderr used or stream builtins called
        if hints.needs_stream_intrinsics {
            builder.import_func("wasi", "stream-new", "stream-new");
            builder.import_func("wasi", "stream-write", "stream-write");
            builder.import_func("wasi", "stream-drop-writable", "stream-drop-writable");
            builder.import_func("wasi", "stream-drop-readable", "stream-drop-readable");
        }

        // DCE: Only import Stdout/Stderr write functions if used
        if hints.is_effect_used("Stdout") {
            let stdout_import_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
            builder.import_func("wasi", &stdout_import_name, &stdout_import_name);
        }
        if hints.is_effect_used("Stderr") {
            let stderr_import_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
            builder.import_func("wasi", &stderr_import_name, &stderr_import_name);
        }

        // DCE: Only import async primitives if stream/effects are used
        if hints.needs_async_primitives {
            builder.import_func("wasi", "task-return", "task-return");
            builder.import_func("wasi", "waitable-set-new", "waitable-set-new");
            builder.import_func("wasi", "waitable-join", "waitable-join");
            builder.import_func("wasi", "waitable-set-wait", "waitable-set-wait");
            builder.import_func("wasi", "subtask-drop", "subtask-drop");
        }

        // DCE: Only import MonotonicClock if used
        if hints.is_effect_used("MonotonicClock")
            && self.wasi_registry.has_interface("monotonic-clock")
        {
            let monotonic_import_name = build_local_alias_name("clocks", "MonotonicClock", "now");
            builder.import_func("wasi", &monotonic_import_name, &monotonic_import_name);
        }
        builder.import_func("env", "realloc", "realloc");
        if hints.needs_f64_to_string {
            builder.import_func("env", "f64_to_buffer", "f64_to_buffer");
        }
        if hints.needs_f32_to_string {
            builder.import_func("env", "f32_to_buffer", "f32_to_buffer");
        }
        builder.import_memory("env", "memory", 1);
        module.section(builder.imports());

        // ========================================
        // Function section
        // ========================================
        // Declare all TIR functions except 'run' (which is handled as entry point)
        for tir_func in &entry_tir.functions {
            if tir_func.name == "run" {
                continue;
            }
            // Methods have names like "Point::sum" - use fully mangled name
            if let Some(sep_pos) = tir_func.name.find("::") {
                let struct_name = &tir_func.name[..sep_pos];
                let method_name = &tir_func.name[sep_pos + 2..];
                let mangled_name = MethodName::new(
                    entry_tir.path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
                )
                .to_string();
                builder.define_func(&mangled_name, &mangled_name);
            } else {
                builder.define_func(&tir_func.name, &tir_func.name);
            }
        }
        // Declare loaded module functions with simple name aliases
        // This matches the AST path behavior where functions can be called by simple name
        let internal_path = vec!["core".to_string(), "internal".to_string()];
        for (module_path, tir_func, _, qualified_name) in &loaded_funcs {
            let func_idx = builder.define_func(qualified_name, qualified_name);
            let is_from_internal = module_path == &internal_path;

            // Register simple name alias for all functions EXCEPT internal
            // Internal functions require explicit import to be accessible
            if qualified_name != &tir_func.name && !is_from_internal {
                builder.define_func_alias(&tir_func.name, func_idx);
            }

            // Track internal functions for access control
            if is_from_internal {
                builder.mark_as_internal(&tir_func.name);
            }
        }
        // Declare impl methods from loaded modules
        for (_module_path, _struct_name, _method, _, mangled_name) in &loaded_methods {
            builder.define_func(mangled_name, mangled_name);
        }
        // Declare 'run' as the entry point
        builder.define_func("run", "run");
        module.section(builder.functions());

        // ========================================
        // Export section
        // ========================================
        builder.export_func("run", "run");
        module.section(builder.exports());

        // Data count section (required for array.new_data with GC)
        let data_count = if string_data.is_empty() { 0 } else { 1 };
        module.section(&DataCountSection { count: data_count });

        // ========================================
        // Code section
        // ========================================
        let mut code = CodeSection::new();
        let mut all_branch_hints: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();
        let import_count = builder.import_func_count;
        let empty_path: &[String] = &[];

        // Generate user-defined functions from entry TIR (excluding 'run' which is handled specially)
        for (idx, tir_func) in entry_tir.functions.iter().enumerate() {
            if tir_func.name == "run" {
                continue; // Skip run - it's handled separately as entry point
            }
            let (wasm_func, hints) =
                self.generate_function(tir_func, type_table, &builder, empty_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((import_count + idx as u32, hints));
            }
        }

        // Generate loaded module functions (TIR path)
        for (module_path, tir_func, func_type_table, _qualified_name) in &loaded_funcs {
            let (wasm_func, _hints) =
                self.generate_function(tir_func, func_type_table, &builder, module_path);
            code.function(&wasm_func);
        }

        // Generate impl methods from loaded modules (TIR path)
        for (module_path, _struct_name, tir_method, method_type_table, _mangled_name) in
            &loaded_methods
        {
            let (wasm_func, _hints) =
                self.generate_function(tir_method, method_type_table, &builder, module_path);
            code.function(&wasm_func);
        }

        // Generate run function (entry point with task.return wrapper)
        let run_tir = entry_tir.functions.iter().find(|f| f.name == "run");

        let run_wasm_func = if let Some(run_tir) = run_tir {
            // Generate run body using the TIR function body generation
            self.generate_run_function(run_tir, type_table, &builder)
        } else {
            // No run function - create empty entry point
            let mut func = Function::new(vec![]);
            let task_return_idx = builder.func_idx("task-return");
            func.instruction(&Instruction::I32Const(0));
            func.instruction(&Instruction::Call(task_return_idx));
            func.instruction(&Instruction::End);
            func
        };

        code.function(&run_wasm_func);

        // Branch hints section
        if !all_branch_hints.is_empty() {
            let mut hints = BranchHints::new();
            for (func_idx, func_hints) in all_branch_hints {
                hints.function_hints(
                    func_idx,
                    func_hints.into_iter().map(|(offset, taken)| BranchHint {
                        branch_func_offset: offset,
                        branch_hint_value: if taken { 1 } else { 0 },
                    }),
                );
            }
            module.section(&hints);
        }

        module.section(&code);

        // Data section
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.passive(string_data.iter().copied());
            module.section(&data);
        }

        // Name section (skip in size-optimized builds)
        if !hints.strip_names {
            let names = builder.build_name_section(module_name);
            module.section(&names);
        }

        module.finish()
    }

    /// Generate component from TIR for WASI P3
    /// Uses native stream<T> types and imports wasi:cli/stdout
    fn generate_component(
        &mut self,
        entry_tir: &TirModule,
        all_tir_modules: &HashMap<Vec<String>, TirModule>,
        symbols: &SymbolTable,
        implicit_modules: &std::collections::HashSet<Vec<String>>,
        hints: &OptimizationHints,
        module_name: &str,
    ) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentModelContext::new();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // ========================================
        // Generate WASI imports dynamically from registry
        // (same as AST path - imports all supported interfaces)
        // ========================================
        self.generate_wasi_imports(&mut builder, &mut ctx, hints);

        // ========================================
        // Type: stream<u8> for stream intrinsics
        // ========================================
        let stream_u8_type = ctx.register_type("stream-u8");
        {
            let (_, enc) = builder.ty(Some("stream-u8"));
            enc.defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
        }

        // ========================================
        // Type: result unit for run function (needed for task.return)
        // ========================================
        let result_unit_type = ctx.register_type("result-unit");
        {
            let (_, enc) = builder.ty(Some("result-unit"));
            enc.defined_type().result(None, None);
        }

        // ========================================
        // Core memory module
        // ========================================
        let mem_module = self.build_memory_module(&string_data, hints);
        ctx.register_core_module("mem-mod");
        builder.core_module_raw(Some("mem-mod"), &mem_module);

        // Instantiate memory module
        ctx.register_core_instance("mem");
        builder.core_instantiate(
            Some("mem"),
            ctx.core_module_idx("mem-mod"),
            Vec::<(&str, ModuleArg)>::new(),
        );

        // Alias memory and realloc from mem instance
        ctx.set_memory(0); // memory is always index 0 at core level
        builder.core_alias_export(
            Some("memory"),
            ctx.core_instance_idx("mem"),
            "memory",
            ExportKind::Memory,
        );
        ctx.register_core_func("realloc");
        builder.core_alias_export(
            Some("realloc"),
            ctx.core_instance_idx("mem"),
            "realloc",
            ExportKind::Func,
        );

        // ========================================
        // Float-to-string conversion module (conditionally included)
        // ========================================
        if hints.needs_float_to_string() {
            let fts_module =
                wasm_postprocess::convert_memory_to_import(wado_bundled_wasm(), "env", "memory")
                    .expect("Failed to process float-to-string module");
            ctx.register_core_module("fts-mod");
            builder.core_module_raw(Some("fts-mod"), &fts_module);

            // Create env instance for float-to-string (just memory)
            ctx.register_core_instance("fts-env");
            let fts_env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
            let fts_env_instance =
                builder.core_instantiate_exports(Some("fts-env-instance"), fts_env_exports);

            // Instantiate float-to-string module with memory
            ctx.register_core_instance("fts");
            builder.core_instantiate(
                Some("fts"),
                ctx.core_module_idx("fts-mod"),
                [("env", ModuleArg::Instance(fts_env_instance))],
            );

            // Alias float-to-string exports (only the ones needed)
            if hints.needs_f64_to_string {
                ctx.register_core_func("f64-to-buffer");
                builder.core_alias_export(
                    Some("f64-to-buffer"),
                    ctx.core_instance_idx("fts"),
                    "f64_to_buffer",
                    ExportKind::Func,
                );
            }

            if hints.needs_f32_to_string {
                ctx.register_core_func("f32-to-buffer");
                builder.core_alias_export(
                    Some("f32-to-buffer"),
                    ctx.core_instance_idx("fts"),
                    "f32_to_buffer",
                    ExportKind::Func,
                );
            }
        }

        // ========================================
        // Stream canonical intrinsics for stream<u8>
        // ========================================
        ctx.register_core_func("stream-new");
        builder.stream_new(stream_u8_type);

        ctx.register_core_func("stream-write");
        builder.stream_write(
            stream_u8_type,
            [
                CanonicalOption::Memory(ctx.memory_idx()),
                CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
            ],
        );

        ctx.register_core_func("stream-drop-writable");
        builder.stream_drop_writable(stream_u8_type);

        ctx.register_core_func("stream-drop-readable");
        builder.stream_drop_readable(stream_u8_type);

        // Lower write-via-stream (stdout) - only if stdout interface is available
        let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if ctx.has_comp_func(&stdout_func_name) {
            ctx.register_core_func(&stdout_func_name);
            builder.lower_func(
                Some(&stdout_func_name),
                ctx.comp_func_idx(&stdout_func_name),
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // Lower write-via-stream (stderr) - only if stderr interface is available
        let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if ctx.has_comp_func(&stderr_func_name) {
            ctx.register_core_func(&stderr_func_name);
            builder.lower_func(
                Some(&stderr_func_name),
                ctx.comp_func_idx(&stderr_func_name),
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // Lower monotonic-clock-now component func to core func (if available)
        // This is a sync function: func() -> u64, no memory/realloc needed
        let monotonic_clock_func_name = build_local_alias_name("clocks", "MonotonicClock", "now");
        if ctx.has_comp_func(&monotonic_clock_func_name) {
            ctx.register_core_func(&monotonic_clock_func_name);
            builder.lower_func(
                Some(&monotonic_clock_func_name),
                ctx.comp_func_idx(&monotonic_clock_func_name),
                [],
            );
        }

        // task.return for completing async tasks
        ctx.register_core_func("task-return");
        builder.task_return(Some(ComponentValType::Type(result_unit_type)), []);

        // Async intrinsics
        ctx.register_core_func("waitable-set-new");
        builder.waitable_set_new();

        ctx.register_core_func("waitable-join");
        builder.waitable_join();

        ctx.register_core_func("waitable-set-wait");
        builder.waitable_set_wait(false, ctx.memory_idx());

        ctx.register_core_func("subtask-drop");
        builder.subtask_drop();

        // ========================================
        // Main core module
        // ========================================
        let main_module = self.build_main_module(
            entry_tir,
            all_tir_modules,
            symbols,
            implicit_modules,
            &string_data,
            hints,
            module_name,
        );
        // Validate main module before embedding
        {
            let mut validator = Validator::new_with_features(WasmFeatures::all());
            if let Err(e) = validator.validate_all(&main_module) {
                panic!("Core module validation failed: {e}");
            }
        }
        ctx.register_core_module("main-mod");
        builder.core_module_raw(Some("main-mod"), &main_module);

        // Create wasi instance with stream intrinsics + lowered WASI functions + async intrinsics
        let mut wasi_exports: Vec<(&str, ExportKind, u32)> = vec![
            (
                "stream-new",
                ExportKind::Func,
                ctx.core_func_idx("stream-new"),
            ),
            (
                "stream-write",
                ExportKind::Func,
                ctx.core_func_idx("stream-write"),
            ),
            (
                "stream-drop-writable",
                ExportKind::Func,
                ctx.core_func_idx("stream-drop-writable"),
            ),
            (
                "stream-drop-readable",
                ExportKind::Func,
                ctx.core_func_idx("stream-drop-readable"),
            ),
            (
                "task-return",
                ExportKind::Func,
                ctx.core_func_idx("task-return"),
            ),
            (
                "waitable-set-new",
                ExportKind::Func,
                ctx.core_func_idx("waitable-set-new"),
            ),
            (
                "waitable-join",
                ExportKind::Func,
                ctx.core_func_idx("waitable-join"),
            ),
            (
                "waitable-set-wait",
                ExportKind::Func,
                ctx.core_func_idx("waitable-set-wait"),
            ),
            (
                "subtask-drop",
                ExportKind::Func,
                ctx.core_func_idx("subtask-drop"),
            ),
        ];
        // Conditionally add stdout/stderr write-via-stream if registered
        let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if ctx.has_comp_func(&stdout_func_name) {
            wasi_exports.push((
                &stdout_func_name,
                ExportKind::Func,
                ctx.core_func_idx(&stdout_func_name),
            ));
        }
        let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if ctx.has_comp_func(&stderr_func_name) {
            wasi_exports.push((
                &stderr_func_name,
                ExportKind::Func,
                ctx.core_func_idx(&stderr_func_name),
            ));
        }
        // Conditionally add monotonic-clock-now if registered
        let monotonic_clock_func_name = build_local_alias_name("clocks", "MonotonicClock", "now");
        if ctx.has_comp_func(&monotonic_clock_func_name) {
            wasi_exports.push((
                &monotonic_clock_func_name,
                ExportKind::Func,
                ctx.core_func_idx(&monotonic_clock_func_name),
            ));
        }
        let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports);
        ctx.register_core_instance("wasi");

        let mut env_exports: Vec<(&str, ExportKind, u32)> = vec![
            ("memory", ExportKind::Memory, ctx.memory_idx()),
            ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
        ];
        if hints.needs_f64_to_string {
            env_exports.push((
                "f64_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f64-to-buffer"),
            ));
        }
        if hints.needs_f32_to_string {
            env_exports.push((
                "f32_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f32-to-buffer"),
            ));
        }
        let env_instance = builder.core_instantiate_exports(Some("env-instance"), env_exports);
        ctx.register_core_instance("env");

        // Instantiate main module
        ctx.register_core_instance("main");
        builder.core_instantiate(
            Some("main"),
            ctx.core_module_idx("main-mod"),
            [
                ("wasi", ModuleArg::Instance(wasi_instance)),
                ("env", ModuleArg::Instance(env_instance)),
            ],
        );

        // Alias run function from main instance
        ctx.register_core_func("run-core");
        builder.core_alias_export(
            Some("run-core"),
            ctx.core_instance_idx("main"),
            "run",
            ExportKind::Func,
        );

        // Type: async run function type () -> result
        let run_func_type = ctx.register_type("run-func-type");
        {
            let (_, enc) = builder.ty(Some("run-func-type"));
            enc.function()
                .async_(true)
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Type(result_unit_type)));
        }

        // Lift run function with Async option
        ctx.register_comp_func("run");
        builder.lift_func(
            Some("run"),
            ctx.core_func_idx("run-core"),
            run_func_type,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
            ],
        );

        // Export run function
        builder.export(
            "run",
            ComponentExportKind::Func,
            ctx.comp_func_idx("run"),
            None,
        );

        // Add component-level debug names (skip in size-optimized builds)
        if !hints.strip_names {
            builder.append_names();
        }

        builder.finish()
    }

    /// Generate WASI imports dynamically from the registry
    ///
    /// This generates Component Model imports based on the WASI registry data
    /// populated from lib/wasi/*.wado files.
    fn generate_wasi_imports(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        hints: &OptimizationHints,
    ) {
        // Get the CLI version from the registry
        let cli_version = self
            .wasi_registry
            .get_cli_version()
            .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

        // First, import wasi:cli/types for shared types (error-code)
        // This must come first as other interfaces reference error-code
        let types_instance_type = ctx.register_type("types-instance-type");
        {
            let (_, enc) = builder.ty(Some("types-instance-type"));
            let mut instance_type = InstanceType::new();
            instance_type
                .ty()
                .defined_type()
                .enum_type(["io", "illegal-byte-sequence", "pipe"]);
            instance_type.export(
                "error-code",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(0)),
            );
            enc.instance(&instance_type);
        }

        ctx.register_instance("types");
        let types_import_path = format!("wasi:cli/types@{}", cli_version);
        builder.import(
            &types_import_path,
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        ctx.register_type("error-code");
        builder.alias_export(
            ctx.instance_idx("types"),
            "error-code",
            ComponentExportKind::Type,
        );

        // Now generate imports for each interface in the registry
        // Dynamically filter based on whether function types are supported
        for interface_info in self.wasi_registry.interfaces() {
            // Skip interfaces that define exports (not imports)
            // The "run" interface defines the component's entry point export.
            // Note: "run" is needed for the wasi:cli Command world, which Wado
            // doesn't fully implement yet. When Command world support is added,
            // this should be handled as an export, not an import.
            if interface_info.interface == "run" {
                continue;
            }

            // Only include interfaces where ALL functions have supported types
            // This ensures we're requesting exactly what we can generate,
            // avoiding mismatches with runtime-provided interfaces
            let all_functions_supported = interface_info
                .functions
                .iter()
                .all(is_wasi_function_supported);

            if !all_functions_supported {
                continue;
            }

            // DCE: Skip interfaces where the effect is not used
            // Get effect name from first function (all functions in an interface share the same effect)
            if let Some(first_func) = interface_info.functions.first()
                && !hints.is_effect_used(&first_func.effect_name)
            {
                continue;
            }

            // All functions are supported, so use them all
            let supported_functions: Vec<_> = interface_info.functions.iter().collect();

            // Build instance type for this interface
            let instance_type_name = format!("{}-instance-type", interface_info.interface);
            let instance_type_idx = ctx.register_type(&instance_type_name);
            {
                let (_, enc) = builder.ty(Some(&instance_type_name));
                let mut instance_type = InstanceType::new();
                let mut local_type_idx = 0u32;

                // Track which functions need which types
                // We'll build types first, then functions
                for func in &supported_functions {
                    // Determine what types this function needs
                    let needs_stream_u8 = func
                        .params
                        .iter()
                        .any(|(_, ty)| matches!(ty, Type::Generic(g) if g.name == "Stream"));
                    let needs_error_code = func
                        .return_type
                        .as_ref()
                        .is_some_and(|ty| matches!(ty, Type::Generic(g) if g.name == "Result"));

                    // Stream<u8> type
                    let stream_type_idx = if needs_stream_u8 {
                        instance_type
                            .ty()
                            .defined_type()
                            .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Error-code alias (if needed for result type)
                    let error_code_idx = if needs_error_code {
                        let outer_error_code = ctx.type_idx("error-code");
                        instance_type.alias(Alias::Outer {
                            kind: ComponentOuterAliasKind::Type,
                            count: 1,
                            index: outer_error_code,
                        });
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Result type (if needed)
                    let result_type_idx = if let Some(err_idx) = error_code_idx {
                        instance_type
                            .ty()
                            .defined_type()
                            .result(None, Some(ComponentValType::Type(err_idx)));
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Build function type
                    // Build params - convert names to kebab-case for CM
                    let kebab_params: Vec<(String, ComponentValType)> = func
                        .params
                        .iter()
                        .map(|(name, ty)| {
                            let val_type =
                                self.wado_type_to_cm_val_type(ty, stream_type_idx, error_code_idx);
                            (to_kebab_case(name), val_type)
                        })
                        .collect();
                    // Convert to references for the encoder
                    let params: Vec<(&str, ComponentValType)> = kebab_params
                        .iter()
                        .map(|(name, val_type)| (name.as_str(), *val_type))
                        .collect();

                    // Build result
                    let result_type = func
                        .return_type
                        .as_ref()
                        .map(|ty| self.wado_type_to_cm_result_type(ty, result_type_idx));

                    // Create function type with params, result, and async flag
                    let mut func_encoder = instance_type.ty().function();
                    if func.is_async {
                        func_encoder.async_(true).params(params).result(result_type);
                    } else {
                        func_encoder.params(params).result(result_type);
                    }

                    let func_type_idx = local_type_idx;
                    local_type_idx += 1;

                    // Export the function
                    instance_type.export(
                        &func.wasi_func_name,
                        wasm_encoder::ComponentTypeRef::Func(func_type_idx),
                    );
                }

                enc.instance(&instance_type);
            }

            // Import the interface instance
            ctx.register_instance(&interface_info.interface);
            builder.import(
                &interface_info.path,
                wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
            );

            // Alias each function from the instance
            for func in &supported_functions {
                let local_name = self
                    .wasi_registry
                    .get_local_name(&interface_info.path, &func.wasi_func_name)
                    .cloned()
                    .unwrap_or_else(|| {
                        format!("{}-{}", interface_info.interface, func.wasi_func_name)
                    });

                ctx.register_comp_func(&local_name);
                builder.alias_export(
                    ctx.instance_idx(&interface_info.interface),
                    &func.wasi_func_name,
                    ComponentExportKind::Func,
                );
            }
        }

        // Import stdout/stderr if needed but not already imported from registry
        // (previously always imported for panic support, now DCE-aware)
        self.ensure_stdout_stderr_imported(builder, ctx, cli_version, hints);
    }

    /// Ensure stdout and stderr are imported if they're used.
    ///
    /// This provides a fallback if the effect wasn't imported through the registry loop
    /// but is still needed (e.g., for panic/logging support).
    /// Uses the version from the registry.
    fn ensure_stdout_stderr_imported(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        cli_version: &str,
        hints: &OptimizationHints,
    ) {
        // Import stdout if used but not already imported
        let stdout_local_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if hints.is_effect_used("Stdout") && !ctx.has_comp_func(&stdout_local_name) {
            // Try to get function info from registry for dynamic signature
            let func_info = self.wasi_registry.get_stdout_write_via_stream();
            let is_async = func_info.map(|f| f.is_async).unwrap_or(true);

            let stdout_instance_type = ctx.register_type("stdout-instance-type");
            {
                let (_, enc) = builder.ty(Some("stdout-instance-type"));
                let mut instance_type = InstanceType::new();
                // Type 0: stream<u8>
                instance_type
                    .ty()
                    .defined_type()
                    .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                // Type 1: error-code alias
                let outer_error_code = ctx.type_idx("error-code");
                instance_type.alias(Alias::Outer {
                    kind: ComponentOuterAliasKind::Type,
                    count: 1,
                    index: outer_error_code,
                });
                // Type 2: result<_, error-code>
                instance_type
                    .ty()
                    .defined_type()
                    .result(None, Some(ComponentValType::Type(1)));
                // Type 3: func(stream<u8>) -> result<_, error-code>
                let mut func_encoder = instance_type.ty().function();
                if is_async {
                    func_encoder.async_(true);
                }
                func_encoder
                    .params([("data", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(2)));
                instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
                enc.instance(&instance_type);
            }

            ctx.register_instance("stdout");
            let stdout_import_path = format!("wasi:cli/stdout@{}", cli_version);
            builder.import(
                &stdout_import_path,
                wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
            );

            ctx.register_comp_func(&stdout_local_name);
            builder.alias_export(
                ctx.instance_idx("stdout"),
                "write-via-stream",
                ComponentExportKind::Func,
            );
        }

        // Import stderr if used but not already imported
        let stderr_local_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if hints.is_effect_used("Stderr") && !ctx.has_comp_func(&stderr_local_name) {
            // Try to get function info from registry for dynamic signature
            let func_info = self.wasi_registry.get_stderr_write_via_stream();
            let is_async = func_info.map(|f| f.is_async).unwrap_or(true);

            let stderr_instance_type = ctx.register_type("stderr-instance-type");
            {
                let (_, enc) = builder.ty(Some("stderr-instance-type"));
                let mut instance_type = InstanceType::new();
                // Type 0: stream<u8>
                instance_type
                    .ty()
                    .defined_type()
                    .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                // Type 1: error-code alias
                let outer_error_code = ctx.type_idx("error-code");
                instance_type.alias(Alias::Outer {
                    kind: ComponentOuterAliasKind::Type,
                    count: 1,
                    index: outer_error_code,
                });
                // Type 2: result<_, error-code>
                instance_type
                    .ty()
                    .defined_type()
                    .result(None, Some(ComponentValType::Type(1)));
                // Type 3: func(stream<u8>) -> result<_, error-code>
                let mut func_encoder = instance_type.ty().function();
                if is_async {
                    func_encoder.async_(true);
                }
                func_encoder
                    .params([("data", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(2)));
                instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
                enc.instance(&instance_type);
            }

            ctx.register_instance("stderr");
            let stderr_import_path = format!("wasi:cli/stderr@{}", cli_version);
            builder.import(
                &stderr_import_path,
                wasm_encoder::ComponentTypeRef::Instance(stderr_instance_type),
            );

            ctx.register_comp_func(&stderr_local_name);
            builder.alias_export(
                ctx.instance_idx("stderr"),
                "write-via-stream",
                ComponentExportKind::Func,
            );
        }
    }

    /// Convert a Wado type to a Component Model value type
    ///
    /// Panics if the type is not supported - callers must validate with
    /// `is_param_type_supported` first. Type aliases should already be resolved.
    fn wado_type_to_cm_val_type(
        &self,
        ty: &Type,
        stream_type_idx: Option<u32>,
        _error_code_idx: Option<u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                _ => panic!("unsupported Wado param type for CM: {}", named.name),
            },
            Type::Generic(generic) => match generic.name.as_str() {
                "Stream" => {
                    // Use the pre-defined stream type index
                    ComponentValType::Type(stream_type_idx.expect("stream type not defined"))
                }
                _ => panic!("unsupported generic param type for CM: {}", generic.name),
            },
            _ => panic!("unsupported Wado param type for CM: {:?}", ty),
        }
    }

    /// Convert a Wado return type to a Component Model result type
    ///
    /// Panics if the type is not supported - callers must validate with
    /// `is_return_type_supported` first. Type aliases should already be resolved.
    fn wado_type_to_cm_result_type(
        &self,
        ty: &Type,
        result_type_idx: Option<u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                _ => panic!("unsupported Wado return type for CM: {}", named.name),
            },
            Type::Generic(generic) if generic.name == "Result" => {
                // Use the pre-defined result type index
                ComponentValType::Type(result_type_idx.expect("result type not defined"))
            }
            _ => panic!("unsupported Wado return type for CM: {:?}", ty),
        }
    }

    /// Get the offset of a string in the string data section
    fn get_string_offset(&self, s: &str) -> u32 {
        let mut offset = 0u32;
        for lit in &self.string_literals {
            if lit == s {
                return offset;
            }
            offset += lit.len() as u32;
        }
        panic!("String not found in literals: {s}");
    }

    /// Register a struct type from TIR with a StructName key
    fn register_struct_type(
        &mut self,
        struct_name: StructName,
        tir_struct: &crate::tir::TirStruct,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        let mut fields = Vec::new();

        for field in &tir_struct.fields {
            let wasm_type = self.type_id_to_valtype(type_table, field.type_id);
            let storage_type = match wasm_type {
                ValType::I32 => StorageType::Val(ValType::I32),
                ValType::I64 => StorageType::Val(ValType::I64),
                ValType::F32 => StorageType::Val(ValType::F32),
                ValType::F64 => StorageType::Val(ValType::F64),
                ValType::Ref(rt) => StorageType::Val(ValType::Ref(rt)),
                _ => StorageType::Val(ValType::I32), // Default fallback
            };
            fields.push(FieldType {
                element_type: storage_type,
                mutable: true, // All fields are mutable by default
            });
        }

        // Use the struct name's string representation for Wasm type naming
        let type_idx = builder.define_gc_struct_type(&struct_name.name, &fields);

        self.struct_types
            .insert(struct_name, StructTypeInfo { type_idx });

        type_idx
    }

    /// Convert TIR TypeId to Wasm ValType
    fn type_id_to_valtype(&self, type_table: &TypeTable, type_id: TypeId) -> ValType {
        match type_table.get(type_id) {
            // Primitive types
            ResolvedType::Primitive(prim) => match prim {
                PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32 => ValType::I32,
                PrimitiveType::I64 | PrimitiveType::U64 => ValType::I64,
                PrimitiveType::I128 | PrimitiveType::U128 => {
                    // TODO: i128/u128 need special handling (tuple of i64s)
                    panic!("i128/u128 not yet supported in TIR codegen")
                }
                PrimitiveType::F32 => ValType::F32,
                PrimitiveType::F64 => ValType::F64,
                PrimitiveType::Bool | PrimitiveType::Char => ValType::I32,
            },

            // Unit type - no value on stack
            ResolvedType::Unit => ValType::I32, // Placeholder, unit is typically elided

            // Never type - functions that never return
            ResolvedType::Never => ValType::I32, // Placeholder, never actually used

            // String type - GC array<u8>
            ResolvedType::String => ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(self.string_array_type_idx),
            }),

            // Struct type
            ResolvedType::Struct { name, module_path } => {
                if let Some(struct_info) = self.lookup_struct_type(name, module_path) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else {
                    panic!("unknown struct type in type_id_to_valtype: {name}")
                }
            }

            // Array<T> - GC array
            ResolvedType::Array(_element_type) => {
                // For now, use the generic array type
                // TODO: Create specialized array types based on element type
                ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
                })
            }

            // Option<T> - nullable reference
            ResolvedType::Option(inner) => {
                let inner_valtype = self.type_id_to_valtype(type_table, *inner);
                match inner_valtype {
                    ValType::Ref(ref_type) => ValType::Ref(RefType {
                        nullable: true,
                        ..ref_type
                    }),
                    // Primitives use i32 with sentinel value for None
                    _ => ValType::I32,
                }
            }

            // Reference types - pass through to inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.type_id_to_valtype(type_table, *inner)
            }

            // Function type - use i32 as placeholder (function index)
            // TODO: Create proper function reference type
            ResolvedType::Function { .. } => ValType::I32,

            // Tuple type
            ResolvedType::Tuple(elements) => {
                if elements.is_empty() {
                    ValType::I32 // Unit-like
                } else {
                    // TODO: Create GC struct for tuples
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(self.string_array_type_idx),
                    })
                }
            }

            // Complex types that need special handling
            ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Result { .. }
            | ResolvedType::Stream(_)
            | ResolvedType::Future(_)
            | ResolvedType::Dict { .. }
            | ResolvedType::Reactive(_) => {
                // TODO: Implement proper handling for these types
                // Use i32 as placeholder for now
                ValType::I32
            }

            // Placeholder types (shouldn't appear in final TIR)
            ResolvedType::Unknown | ResolvedType::Error => {
                panic!("unexpected Unknown/Error type in codegen")
            }
        }
    }

    // ========================================================================
    // Code Generation
    // ========================================================================

    /// Generate code for a TIR expression with span-based cache lookup
    /// This prevents re-evaluation of expressions that have already been cached
    fn generate_expr_with_cache(
        &self,
        func: &mut Function,
        expr: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
        span_cache: &std::collections::HashMap<(usize, usize), u32>,
    ) {
        // Check if this expression was already cached
        let span_key = (expr.span.start, expr.span.end);
        if let Some(&local_idx) = span_cache.get(&span_key) {
            func.instruction(&Instruction::LocalGet(local_idx));
            return;
        }

        // Not cached - handle recursively for compound expressions
        match &expr.kind {
            TirExprKind::Binary { left, op, right } => {
                // Recursively handle with cache for sub-expressions
                self.generate_expr_with_cache(func, left, type_table, ctx, builder, span_cache);

                // Check if i32 to i64 promotion is needed
                let left_is_i64 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let right_is_i64 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let left_is_i32 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );
                let right_is_i32 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );

                // Promote left if needed
                if right_is_i64 && left_is_i32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

                self.generate_expr_with_cache(func, right, type_table, ctx, builder, span_cache);

                // Promote right if needed
                if left_is_i64 && right_is_i32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

                // Determine effective type and emit op
                let left_type = type_table.get(left.type_id);
                let use_i64 = left_is_i64 || right_is_i64;
                let use_f64 = matches!(left_type, ResolvedType::Primitive(PrimitiveType::F64));
                let use_f32 = matches!(left_type, ResolvedType::Primitive(PrimitiveType::F32));

                if use_f64 {
                    self.emit_f64_binary_op(func, op);
                } else if use_f32 {
                    self.emit_f32_binary_op(func, op);
                } else if use_i64 {
                    self.emit_i64_binary_op(func, op);
                } else {
                    self.emit_i32_binary_op(func, op);
                }
            }
            TirExprKind::Unary { op, expr: inner } => {
                self.generate_expr_with_cache(func, inner, type_table, ctx, builder, span_cache);
                self.generate_unary_op(func, *op, inner.type_id, type_table);
            }
            // For other expressions, delegate to the non-cached version
            _ => {
                self.generate_expr(func, expr, type_table, ctx, builder);
            }
        }
    }

    /// Emit i32 binary operation instruction
    fn emit_i32_binary_op(&self, func: &mut Function, op: &TirBinaryOp) {
        match op {
            TirBinaryOp::Add => func.instruction(&Instruction::I32Add),
            TirBinaryOp::Sub => func.instruction(&Instruction::I32Sub),
            TirBinaryOp::Mul => func.instruction(&Instruction::I32Mul),
            TirBinaryOp::Div => func.instruction(&Instruction::I32DivS),
            TirBinaryOp::Mod => func.instruction(&Instruction::I32RemS),
            TirBinaryOp::Eq => func.instruction(&Instruction::I32Eq),
            TirBinaryOp::NotEq => func.instruction(&Instruction::I32Ne),
            TirBinaryOp::Lt => func.instruction(&Instruction::I32LtS),
            TirBinaryOp::LtEq => func.instruction(&Instruction::I32LeS),
            TirBinaryOp::Gt => func.instruction(&Instruction::I32GtS),
            TirBinaryOp::GtEq => func.instruction(&Instruction::I32GeS),
            TirBinaryOp::And => func.instruction(&Instruction::I32And),
            TirBinaryOp::Or => func.instruction(&Instruction::I32Or),
            TirBinaryOp::BitAnd => func.instruction(&Instruction::I32And),
            TirBinaryOp::BitOr => func.instruction(&Instruction::I32Or),
            TirBinaryOp::BitXor => func.instruction(&Instruction::I32Xor),
            TirBinaryOp::Shl => func.instruction(&Instruction::I32Shl),
            TirBinaryOp::Shr => func.instruction(&Instruction::I32ShrS),
        };
    }

    /// Emit i64 binary operation instruction
    fn emit_i64_binary_op(&self, func: &mut Function, op: &TirBinaryOp) {
        match op {
            TirBinaryOp::Add => func.instruction(&Instruction::I64Add),
            TirBinaryOp::Sub => func.instruction(&Instruction::I64Sub),
            TirBinaryOp::Mul => func.instruction(&Instruction::I64Mul),
            TirBinaryOp::Div => func.instruction(&Instruction::I64DivS),
            TirBinaryOp::Mod => func.instruction(&Instruction::I64RemS),
            TirBinaryOp::Eq => func.instruction(&Instruction::I64Eq),
            TirBinaryOp::NotEq => func.instruction(&Instruction::I64Ne),
            TirBinaryOp::Lt => func.instruction(&Instruction::I64LtS),
            TirBinaryOp::LtEq => func.instruction(&Instruction::I64LeS),
            TirBinaryOp::Gt => func.instruction(&Instruction::I64GtS),
            TirBinaryOp::GtEq => func.instruction(&Instruction::I64GeS),
            TirBinaryOp::And => func.instruction(&Instruction::I32And), // Logical AND: result is i32
            TirBinaryOp::Or => func.instruction(&Instruction::I32Or),   // Logical OR: result is i32
            TirBinaryOp::BitAnd => func.instruction(&Instruction::I64And),
            TirBinaryOp::BitOr => func.instruction(&Instruction::I64Or),
            TirBinaryOp::BitXor => func.instruction(&Instruction::I64Xor),
            TirBinaryOp::Shl => func.instruction(&Instruction::I64Shl),
            TirBinaryOp::Shr => func.instruction(&Instruction::I64ShrS),
        };
    }

    /// Emit f32 binary operation instruction
    fn emit_f32_binary_op(&self, func: &mut Function, op: &TirBinaryOp) {
        match op {
            TirBinaryOp::Add => func.instruction(&Instruction::F32Add),
            TirBinaryOp::Sub => func.instruction(&Instruction::F32Sub),
            TirBinaryOp::Mul => func.instruction(&Instruction::F32Mul),
            TirBinaryOp::Div => func.instruction(&Instruction::F32Div),
            TirBinaryOp::Eq => func.instruction(&Instruction::F32Eq),
            TirBinaryOp::NotEq => func.instruction(&Instruction::F32Ne),
            TirBinaryOp::Lt => func.instruction(&Instruction::F32Lt),
            TirBinaryOp::LtEq => func.instruction(&Instruction::F32Le),
            TirBinaryOp::Gt => func.instruction(&Instruction::F32Gt),
            TirBinaryOp::GtEq => func.instruction(&Instruction::F32Ge),
            _ => func.instruction(&Instruction::I32Const(0)), // Unsupported ops
        };
    }

    /// Emit f64 binary operation instruction
    fn emit_f64_binary_op(&self, func: &mut Function, op: &TirBinaryOp) {
        match op {
            TirBinaryOp::Add => func.instruction(&Instruction::F64Add),
            TirBinaryOp::Sub => func.instruction(&Instruction::F64Sub),
            TirBinaryOp::Mul => func.instruction(&Instruction::F64Mul),
            TirBinaryOp::Div => func.instruction(&Instruction::F64Div),
            TirBinaryOp::Eq => func.instruction(&Instruction::F64Eq),
            TirBinaryOp::NotEq => func.instruction(&Instruction::F64Ne),
            TirBinaryOp::Lt => func.instruction(&Instruction::F64Lt),
            TirBinaryOp::LtEq => func.instruction(&Instruction::F64Le),
            TirBinaryOp::Gt => func.instruction(&Instruction::F64Gt),
            TirBinaryOp::GtEq => func.instruction(&Instruction::F64Ge),
            _ => func.instruction(&Instruction::I32Const(0)), // Unsupported ops
        };
    }

    /// Generate code for a TIR expression
    fn generate_expr(
        &self,
        func: &mut Function,
        expr: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match &expr.kind {
            // === Literals ===
            TirExprKind::IntLiteral { value, .. } => {
                // Check the target type to generate appropriate instruction
                match type_table.get(expr.type_id) {
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                        func.instruction(&Instruction::I64Const(*value));
                    }
                    _ => {
                        func.instruction(&Instruction::I32Const(*value as i32));
                    }
                }
            }

            TirExprKind::FloatLiteral { value, .. } => match type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::F32) => {
                    func.instruction(&Instruction::F32Const(((*value) as f32).into()));
                }
                _ => {
                    func.instruction(&Instruction::F64Const((*value).into()));
                }
            },

            TirExprKind::BoolLiteral(b) => {
                func.instruction(&Instruction::I32Const(if *b { 1 } else { 0 }));
            }

            TirExprKind::CharLiteral(c) => {
                func.instruction(&Instruction::I32Const(*c as i32));
            }

            TirExprKind::StringLiteral(s) => {
                let offset = self.get_string_offset(s);
                let len = s.len();
                func.instruction(&Instruction::I32Const(offset as i32));
                func.instruction(&Instruction::I32Const(len as i32));
                func.instruction(&Instruction::ArrayNewData {
                    array_type_index: self.string_array_type_idx,
                    array_data_index: 0,
                });
            }

            TirExprKind::Null => {
                func.instruction(&Instruction::I32Const(0));
            }

            TirExprKind::Unit => {
                func.instruction(&Instruction::I32Const(0));
            }

            // === Variables ===
            TirExprKind::Local { index, .. } => {
                func.instruction(&Instruction::LocalGet(*index));
                // For reference types, locals are nullable but we may need non-nullable
                // Check if this is a reference type and add RefAsNonNull
                let val_type = self.type_id_to_valtype(type_table, expr.type_id);
                if matches!(val_type, ValType::Ref(rt) if !rt.nullable) {
                    func.instruction(&Instruction::RefAsNonNull);
                }
            }

            TirExprKind::Global { module_path, name } => {
                // TODO: Handle global references properly
                let full_name = if module_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}::{}", module_path.join("::"), name)
                };
                if let Some(func_idx) = builder.try_func_idx(&full_name) {
                    // If it's a function, push a reference to it
                    // For now, just push the index as i32 (placeholder)
                    func.instruction(&Instruction::I32Const(func_idx as i32));
                } else {
                    panic!("unknown global: {full_name}");
                }
            }

            // === Binary Operations ===
            TirExprKind::Binary { left, op, right } => {
                // Determine if we need type promotion (e.g., i32 to i64)
                let left_is_i64 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let right_is_i64 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let left_is_i32 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );
                let right_is_i32 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );

                // Generate left operand
                self.generate_expr(func, left, type_table, ctx, builder);
                // Promote left if needed
                if right_is_i64 && left_is_i32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

                // Generate right operand
                self.generate_expr(func, right, type_table, ctx, builder);
                // Promote right if needed
                if left_is_i64 && right_is_i32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

                // Use i64 instructions if either operand is i64
                let effective_type = if left_is_i64 || right_is_i64 {
                    TypeTable::I64
                } else {
                    left.type_id
                };
                self.generate_binary_op(func, *op, effective_type, type_table);
            }

            // === Unary Operations ===
            TirExprKind::Unary { op, expr: inner } => {
                self.generate_expr(func, inner, type_table, ctx, builder);
                self.generate_unary_op(func, *op, inner.type_id, type_table);
            }

            // === Assignment ===
            TirExprKind::Assign { target, value } => {
                match &target.kind {
                    TirExprKind::Local { index, .. } => {
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Use local.tee to both store and keep value on stack
                        func.instruction(&Instruction::LocalTee(*index));
                    }
                    TirExprKind::FieldAccess {
                        expr, field_index, ..
                    } => {
                        // For struct.set, stack order is: struct_ref, value
                        // Get the struct type from the receiver expression
                        let struct_type_idx = match type_table.get(expr.type_id) {
                            ResolvedType::Struct { name, module_path } => {
                                if let Some(info) = self.lookup_struct_type(name, module_path) {
                                    info.type_idx
                                } else {
                                    panic!("unknown struct type in field assignment: {name}");
                                }
                            }
                            other => {
                                panic!("field assignment on non-struct type: {:?}", other);
                            }
                        };

                        // Generate struct reference first
                        self.generate_expr(func, expr, type_table, ctx, builder);
                        // Then generate value
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Emit struct.set (consumes both values, leaves nothing)
                        func.instruction(&Instruction::StructSet {
                            struct_type_index: struct_type_idx,
                            field_index: *field_index,
                        });
                        // Push the assigned value back for expression result
                        // (Regenerate the field access to get the value)
                        self.generate_expr(func, expr, type_table, ctx, builder);
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: struct_type_idx,
                            field_index: *field_index,
                        });
                    }
                    _ => panic!("invalid assignment target in TIR"),
                }
            }

            // === Type Cast ===
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Special case: integer literal cast to i64/u64 - generate I64Const directly
                // to avoid truncation through i32
                let is_i64_target = matches!(
                    type_table.get(*target_type),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                if let TirExprKind::IntLiteral { value, .. } = &inner.kind
                    && is_i64_target
                {
                    func.instruction(&Instruction::I64Const(*value));
                    return;
                }
                self.generate_expr(func, inner, type_table, ctx, builder);
                self.generate_cast(func, inner.type_id, *target_type, type_table);
            }

            // === Function Call ===
            TirExprKind::Call {
                module_path,
                func_name,
                args,
            } => {
                // Helper to check if this is a specific builtin
                let is_builtin = |name: &str| {
                    (module_path.len() == 1 && module_path[0] == "builtin" && func_name == name)
                        || (module_path.is_empty() && func_name == &format!("builtin::{}", name))
                };

                // Handle builtin functions that generate Wasm instructions directly
                if is_builtin("likely") || is_builtin("unlikely") {
                    // Generate argument (the condition)
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    // Set branch hint - the value passes through unchanged
                    ctx.set_branch_hint(is_builtin("likely"));
                } else if is_builtin("unreachable") {
                    // builtin::unreachable() traps immediately
                    func.instruction(&Instruction::Unreachable);
                } else if is_builtin("effect_wait") {
                    // builtin::effect_wait() waits for pending async effects
                    self.generate_effect_wait(func, ctx, builder);
                } else if is_builtin("array_len") {
                    // builtin::array_len(arr) -> array.len
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::ArrayLen);
                } else if is_builtin("array_get_u8") {
                    // builtin::array_get_u8(arr, idx) -> array.get_u
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::ArrayGetU(self.string_array_type_idx));
                } else if is_builtin("array_set_u8") {
                    // builtin::array_set_u8(arr, idx, value) -> array.set
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::ArraySet(self.string_array_type_idx));
                } else if is_builtin("string_new") {
                    // builtin::string_new(len) -> array.new_default
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::ArrayNewDefault(self.string_array_type_idx));
                } else if is_builtin("memory_store8") {
                    // builtin::memory_store8(addr, value) -> i32.store8
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::I32Store8(MemArg {
                        offset: 0,
                        align: 0,
                        memory_index: 0,
                    }));
                } else if is_builtin("memory_load8_u") {
                    // builtin::memory_load8_u(addr) -> i32.load8_u
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::I32Load8U(MemArg {
                        offset: 0,
                        align: 0,
                        memory_index: 0,
                    }));
                } else if is_builtin("i32_and") {
                    // builtin::i32_and(a, b) -> i32.and
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::I32And);
                } else if is_builtin("i32_eqz") {
                    // builtin::i32_eqz(a) -> i32.eqz
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::I32Eqz);
                } else if is_builtin("call_indirect_stdout_write_via_stream") {
                    // Generate argument first (rx stream handle)
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    // Call stdout write_via_stream(rx, outptr) - returns subtask handle
                    func.instruction(&Instruction::I32Const(2048)); // outptr for result
                    let stdout_func = build_local_alias_name("cli", "Stdout", "write_via_stream");
                    let func_idx = builder.func_idx(&stdout_func);
                    func.instruction(&Instruction::Call(func_idx));
                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect("__subtask should be pre-allocated for functions with Stdout/Stderr effects");
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else if is_builtin("call_indirect_stderr_write_via_stream") {
                    // Generate argument first (rx stream handle)
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    // Call stderr write_via_stream(rx, outptr) - returns subtask handle
                    func.instruction(&Instruction::I32Const(2048)); // outptr for result
                    let stderr_func = build_local_alias_name("cli", "Stderr", "write_via_stream");
                    let func_idx = builder.func_idx(&stderr_func);
                    func.instruction(&Instruction::Call(func_idx));
                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect("__subtask should be pre-allocated for functions with Stdout/Stderr effects");
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else if (module_path == &["Stdout"] || module_path == &["Stderr"])
                    && func_name == "write_via_stream"
                {
                    // Direct effect operation call: Stdout::write_via_stream(rx) or Stderr::write_via_stream(rx)
                    // These need special handling because WASI P3 async operations require:
                    // 1. An extra outptr argument
                    // 2. Storing the result (subtask handle) for later waiting

                    // Generate the rx argument
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

                    // Add the outptr argument (WASI P3 async operations need this)
                    func.instruction(&Instruction::I32Const(2048));

                    // Resolve the WASI function
                    let effect_name = &module_path[0];
                    let local_name = build_local_alias_name("cli", effect_name, func_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect(
                        "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                    );
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else {
                    // Generate arguments first
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

                    // Resolve function index using multiple strategies
                    let func_idx = self.resolve_call_target(
                        module_path,
                        func_name,
                        &ctx.current_module_path,
                        builder,
                    );
                    func.instruction(&Instruction::Call(func_idx));
                }
            }

            // === Effect Operation Call ===
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
            } => {
                // Special handling for async WASI operations
                if (effect_name == "Stdout" || effect_name == "Stderr")
                    && op_name == "write_via_stream"
                {
                    // Generate the rx argument
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

                    // Add the outptr argument (WASI P3 async operations need this)
                    func.instruction(&Instruction::I32Const(2048));

                    // Resolve the WASI function
                    let local_name = build_local_alias_name("cli", effect_name, op_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect(
                        "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                    );
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else {
                    // Regular effect call
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                    let full_name = format!("{}::{}", effect_name, op_name);
                    if let Some(func_idx) = builder.try_func_idx(&full_name) {
                        func.instruction(&Instruction::Call(func_idx));
                    } else {
                        panic!("unknown effect operation: {full_name}");
                    }
                }
            }

            // === Method Call ===
            TirExprKind::MethodCall {
                receiver,
                method_name,
                args,
            } => {
                match type_table.get(receiver.type_id) {
                    // Struct method call
                    ResolvedType::Struct { name, module_path } => {
                        // Build the fully mangled method name: path/Struct::method
                        let mangled_name = MethodName::new(
                            module_path.join("/"),
                            name.clone(),
                            None,
                            method_name.to_string(),
                        )
                        .to_string();

                        // Look up the method function index
                        if let Some(idx) = builder.try_func_idx(&mangled_name) {
                            // Generate code for the receiver (self parameter)
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Generate code for other arguments
                            for arg in args {
                                self.generate_expr(func, arg, type_table, ctx, builder);
                            }

                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            panic!("unknown method: {mangled_name}");
                        }
                    }

                    // Primitive method calls (e.g., i32.to_string())
                    ResolvedType::Primitive(prim) => {
                        if method_name == "to_string" {
                            // Generate the receiver value first
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Call the appropriate builtin to_string function
                            let func_name = match prim {
                                PrimitiveType::I32
                                | PrimitiveType::I8
                                | PrimitiveType::I16
                                | PrimitiveType::U8
                                | PrimitiveType::U16
                                | PrimitiveType::U32 => "core/internal/i32_to_string",
                                PrimitiveType::I64 | PrimitiveType::U64 => {
                                    "core/internal/i64_to_string"
                                }
                                PrimitiveType::F32 => "core/internal/f32_to_string",
                                PrimitiveType::F64 => "core/internal/f64_to_string",
                                PrimitiveType::Bool => "core/internal/bool_to_string",
                                PrimitiveType::Char => "core/internal/char_to_string",
                                _ => {
                                    panic!("to_string not supported for primitive type: {:?}", prim)
                                }
                            };
                            if let Some(func_idx) = builder.try_func_idx(func_name) {
                                func.instruction(&Instruction::Call(func_idx));
                            } else {
                                panic!("missing builtin function: {func_name}");
                            }
                        } else {
                            panic!(
                                "unknown method {} on primitive type {:?}",
                                method_name, prim
                            );
                        }
                    }

                    other => {
                        panic!(
                            "method call receiver is not a struct or primitive type: {:?}",
                            other
                        );
                    }
                }
            }

            // === Field Access ===
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                // Get the struct type from the inner expression
                let struct_type_idx = match type_table.get(inner.type_id) {
                    ResolvedType::Struct { name, module_path } => {
                        if let Some(info) = self.lookup_struct_type(name, module_path) {
                            info.type_idx
                        } else {
                            panic!("unknown struct type in field access: {name}");
                        }
                    }
                    other => {
                        panic!("field access on non-struct type: {:?}", other);
                    }
                };

                self.generate_expr(func, inner, type_table, ctx, builder);
                func.instruction(&Instruction::StructGet {
                    struct_type_index: struct_type_idx,
                    field_index: *field_index,
                });
            }

            // === Index Access ===
            TirExprKind::Index { expr: array, index } => {
                self.generate_expr(func, array, type_table, ctx, builder);
                self.generate_expr(func, index, type_table, ctx, builder);
                func.instruction(&Instruction::ArrayGet(self.string_array_type_idx));
            }

            // === Block Expression ===
            TirExprKind::Block(block) => {
                self.generate_block(func, block, type_table, ctx, builder);
            }

            // === If Expression ===
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.generate_expr(func, condition, type_table, ctx, builder);
                let result_type = self.type_id_to_valtype(type_table, expr.type_id);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    result_type,
                )));
                self.generate_block(func, then_branch, type_table, ctx, builder);
                if let Some(else_block) = else_branch {
                    func.instruction(&Instruction::Else);
                    self.generate_block(func, else_block, type_table, ctx, builder);
                }
                func.instruction(&Instruction::End);
            }

            // === Match Expression ===
            TirExprKind::Match {
                expr: scrutinee,
                arms: _,
            } => {
                // TODO: Implement proper pattern matching
                self.generate_expr(func, scrutinee, type_table, ctx, builder);
                panic!("match expressions not yet implemented in TIR codegen");
            }

            // === Struct Literal ===
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                // Generate field values in order
                for field in fields {
                    self.generate_expr(func, &field.value, type_table, ctx, builder);
                }
                // Create struct using struct.new
                // Use the struct_type to get the correct lookup name (handles name collisions)
                let struct_info = if let ResolvedType::Struct { name, module_path } =
                    type_table.get(*struct_type)
                {
                    self.lookup_struct_type(name, module_path)
                } else {
                    // Fall back to simple name lookup using struct_name
                    self.lookup_struct_type(struct_name, &[])
                };

                if let Some(struct_info) = struct_info {
                    func.instruction(&Instruction::StructNew(struct_info.type_idx));
                } else {
                    panic!("unknown struct type: {struct_name}");
                }
            }

            // === Array Literal ===
            TirExprKind::ArrayLiteral { elements } => {
                // TODO: Create proper GC array
                for elem in elements {
                    self.generate_expr(func, elem, type_table, ctx, builder);
                }
                panic!("array literals not yet implemented in TIR codegen");
            }

            // === Tuple Literal ===
            TirExprKind::TupleLiteral { elements } => {
                // TODO: Create proper tuple representation
                for elem in elements {
                    self.generate_expr(func, elem, type_table, ctx, builder);
                }
                if elements.is_empty() {
                    func.instruction(&Instruction::I32Const(0)); // Unit
                } else {
                    panic!("non-unit tuple literals not yet implemented in TIR codegen");
                }
            }

            // === Closure ===
            TirExprKind::Closure {
                params: _,
                body: _,
                captures: _,
            } => {
                // TODO: Implement closures
                panic!("closures not yet implemented in TIR codegen");
            }
        }
    }

    /// Generate code for a TIR binary operation
    fn generate_binary_op(
        &self,
        func: &mut Function,
        op: TirBinaryOp,
        operand_type: TypeId,
        type_table: &TypeTable,
    ) {
        let is_i64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f32 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F32)
        );
        let is_f64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F64)
        );

        let instr = match op {
            TirBinaryOp::Add => {
                if is_f64 {
                    Instruction::F64Add
                } else if is_f32 {
                    Instruction::F32Add
                } else if is_i64 {
                    Instruction::I64Add
                } else {
                    Instruction::I32Add
                }
            }
            TirBinaryOp::Sub => {
                if is_f64 {
                    Instruction::F64Sub
                } else if is_f32 {
                    Instruction::F32Sub
                } else if is_i64 {
                    Instruction::I64Sub
                } else {
                    Instruction::I32Sub
                }
            }
            TirBinaryOp::Mul => {
                if is_f64 {
                    Instruction::F64Mul
                } else if is_f32 {
                    Instruction::F32Mul
                } else if is_i64 {
                    Instruction::I64Mul
                } else {
                    Instruction::I32Mul
                }
            }
            TirBinaryOp::Div => {
                if is_f64 {
                    Instruction::F64Div
                } else if is_f32 {
                    Instruction::F32Div
                } else if is_i64 {
                    Instruction::I64DivS
                } else {
                    Instruction::I32DivS
                }
            }
            TirBinaryOp::Mod => {
                if is_i64 {
                    Instruction::I64RemS
                } else {
                    Instruction::I32RemS
                }
            }
            TirBinaryOp::Eq => {
                if is_f64 {
                    Instruction::F64Eq
                } else if is_f32 {
                    Instruction::F32Eq
                } else if is_i64 {
                    Instruction::I64Eq
                } else {
                    Instruction::I32Eq
                }
            }
            TirBinaryOp::NotEq => {
                if is_f64 {
                    Instruction::F64Ne
                } else if is_f32 {
                    Instruction::F32Ne
                } else if is_i64 {
                    Instruction::I64Ne
                } else {
                    Instruction::I32Ne
                }
            }
            TirBinaryOp::Lt => {
                if is_f64 {
                    Instruction::F64Lt
                } else if is_f32 {
                    Instruction::F32Lt
                } else if is_i64 {
                    Instruction::I64LtS
                } else {
                    Instruction::I32LtS
                }
            }
            TirBinaryOp::LtEq => {
                if is_f64 {
                    Instruction::F64Le
                } else if is_f32 {
                    Instruction::F32Le
                } else if is_i64 {
                    Instruction::I64LeS
                } else {
                    Instruction::I32LeS
                }
            }
            TirBinaryOp::Gt => {
                if is_f64 {
                    Instruction::F64Gt
                } else if is_f32 {
                    Instruction::F32Gt
                } else if is_i64 {
                    Instruction::I64GtS
                } else {
                    Instruction::I32GtS
                }
            }
            TirBinaryOp::GtEq => {
                if is_f64 {
                    Instruction::F64Ge
                } else if is_f32 {
                    Instruction::F32Ge
                } else if is_i64 {
                    Instruction::I64GeS
                } else {
                    Instruction::I32GeS
                }
            }
            TirBinaryOp::And => Instruction::I32And,
            TirBinaryOp::Or => Instruction::I32Or,
            TirBinaryOp::BitAnd => {
                if is_i64 {
                    Instruction::I64And
                } else {
                    Instruction::I32And
                }
            }
            TirBinaryOp::BitOr => {
                if is_i64 {
                    Instruction::I64Or
                } else {
                    Instruction::I32Or
                }
            }
            TirBinaryOp::BitXor => {
                if is_i64 {
                    Instruction::I64Xor
                } else {
                    Instruction::I32Xor
                }
            }
            TirBinaryOp::Shl => {
                if is_i64 {
                    Instruction::I64Shl
                } else {
                    Instruction::I32Shl
                }
            }
            TirBinaryOp::Shr => {
                if is_i64 {
                    Instruction::I64ShrS
                } else {
                    Instruction::I32ShrS
                }
            }
        };
        func.instruction(&instr);
    }

    /// Generate code for a TIR unary operation
    fn generate_unary_op(
        &self,
        func: &mut Function,
        op: TirUnaryOp,
        operand_type: TypeId,
        type_table: &TypeTable,
    ) {
        let is_i64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f32 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F32)
        );
        let is_f64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F64)
        );

        match op {
            TirUnaryOp::Neg => {
                if is_f64 {
                    func.instruction(&Instruction::F64Neg);
                } else if is_f32 {
                    func.instruction(&Instruction::F32Neg);
                } else if is_i64 {
                    // value is already on stack, negate by multiplying by -1
                    func.instruction(&Instruction::I64Const(-1));
                    func.instruction(&Instruction::I64Mul);
                } else {
                    // value is already on stack, negate by multiplying by -1
                    func.instruction(&Instruction::I32Const(-1));
                    func.instruction(&Instruction::I32Mul);
                }
            }
            TirUnaryOp::Not => {
                func.instruction(&Instruction::I32Eqz);
            }
            TirUnaryOp::BitNot => {
                if is_i64 {
                    func.instruction(&Instruction::I64Const(-1));
                    func.instruction(&Instruction::I64Xor);
                } else {
                    func.instruction(&Instruction::I32Const(-1));
                    func.instruction(&Instruction::I32Xor);
                }
            }
            TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => {
                // References are transparent in Wasm GC - no operation needed
            }
        }
    }

    /// Generate code for a TIR type cast
    fn generate_cast(
        &self,
        func: &mut Function,
        from_type: TypeId,
        to_type: TypeId,
        type_table: &TypeTable,
    ) {
        let from = type_table.get(from_type);
        let to = type_table.get(to_type);

        match (from, to) {
            // i32 -> i64
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32S);
            }
            // i64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32WrapI64);
            }
            // i32 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64ConvertI32S);
            }
            // i32 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32ConvertI32S);
            }
            // f64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32TruncF64S);
            }
            // f32 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32TruncF32S);
            }
            // f32 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64PromoteF32);
            }
            // f64 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32DemoteF64);
            }
            // Same type - no conversion needed
            _ if from_type == to_type => {}
            // Other conversions - placeholder
            _ => {
                // TODO: Handle more type conversions
            }
        }
    }

    /// Generate code for a TIR block
    fn generate_block(
        &self,
        func: &mut Function,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        for stmt in &block.stmts {
            self.generate_stmt(func, stmt, type_table, ctx, builder);
        }
    }

    /// Generate code for a TIR statement
    fn generate_stmt(
        &self,
        func: &mut Function,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match &stmt.kind {
            TirStmtKind::Let {
                name: _,
                local_index,
                type_id,
                value,
                ..
            } => {
                // Generate the initializer
                self.generate_expr(func, value, type_table, ctx, builder);

                // Add implicit type promotion if needed (e.g., i32 literal to i64 local)
                let value_type = value.type_id;
                if value_type != *type_id {
                    // Check for i32 -> i64 promotion (common with integer literals)
                    let is_i32_to_i64 = matches!(
                        (type_table.get(value_type), type_table.get(*type_id)),
                        (
                            ResolvedType::Primitive(PrimitiveType::I32),
                            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                        )
                    );
                    if is_i32_to_i64 {
                        func.instruction(&Instruction::I64ExtendI32S);
                    }
                }

                // Store to local
                func.instruction(&Instruction::LocalSet(*local_index));
            }

            TirStmtKind::Expr(expr) => {
                self.generate_expr(func, expr, type_table, ctx, builder);
                // Drop the result if the expression has a non-unit type
                if expr.type_id != TypeTable::UNIT && expr.type_id != TypeTable::NEVER {
                    func.instruction(&Instruction::Drop);
                }
            }

            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.generate_expr(func, expr, type_table, ctx, builder);
                }
                func.instruction(&Instruction::Return);
            }

            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.generate_expr(func, condition, type_table, ctx, builder);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                self.generate_block(func, then_block, type_table, ctx, builder);
                if let Some(else_blk) = else_block {
                    func.instruction(&Instruction::Else);
                    self.generate_block(func, else_blk, type_table, ctx, builder);
                }
                func.instruction(&Instruction::End);
            }

            TirStmtKind::While { condition, body } => {
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
                // Check condition, break if false
                self.generate_expr(func, condition, type_table, ctx, builder);
                func.instruction(&Instruction::I32Eqz);
                func.instruction(&Instruction::BrIf(1)); // Break out of block
                // Execute body
                self.generate_block(func, body, type_table, ctx, builder);
                // Continue loop
                func.instruction(&Instruction::Br(0));
                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End block
            }

            TirStmtKind::Loop { body } => {
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
                self.generate_block(func, body, type_table, ctx, builder);
                func.instruction(&Instruction::Br(0)); // Continue loop
                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End block
            }

            TirStmtKind::Break => {
                func.instruction(&Instruction::Br(1)); // Break to outer block
            }

            TirStmtKind::Continue => {
                func.instruction(&Instruction::Br(0)); // Continue to loop start
            }

            TirStmtKind::Assert {
                condition,
                condition_source,
                message,
                intermediates,
            } => {
                // Power-assert: Cache intermediate values, then check condition
                // If false, build detailed error message and panic

                let string_array_type = builder.type_idx("string-array");

                // 1. Allocate locals for intermediates (don't add to cache yet)
                // Store TypeId (not ValType) so we can call the correct to_string function
                let mut cached_locals: Vec<(String, u32, TypeId)> = Vec::new();
                for (name, _, type_id) in intermediates {
                    let val_type = self.type_id_to_valtype(type_table, *type_id);
                    let local_idx =
                        ctx.alloc_local(&format!("__assert_{}", name.replace(' ', "_")), val_type);
                    cached_locals.push((name.clone(), local_idx, *type_id));
                }

                // 2. Evaluate intermediates and build cache incrementally
                // Important: Add to cache AFTER evaluating, so sub-expressions can be cached
                let mut span_cache: std::collections::HashMap<(usize, usize), u32> =
                    std::collections::HashMap::new();
                for (i, (_, expr, _)) in intermediates.iter().enumerate() {
                    // Evaluate using current cache (may have earlier expressions cached)
                    self.generate_expr_with_cache(
                        func,
                        expr,
                        type_table,
                        ctx,
                        builder,
                        &span_cache,
                    );
                    let local_idx = cached_locals[i].1;
                    func.instruction(&Instruction::LocalSet(local_idx));
                    // Now add this expression to cache so later expressions can use it
                    span_cache.insert((expr.span.start, expr.span.end), local_idx);
                }

                // 3. Evaluate condition using cached values
                let cond_local = ctx.alloc_local("__assert_cond", ValType::I32);
                self.generate_expr_with_cache(
                    func,
                    condition,
                    type_table,
                    ctx,
                    builder,
                    &span_cache,
                );
                func.instruction(&Instruction::LocalSet(cond_local));

                // 3. Check condition: if (!condition) { ... }
                func.instruction(&Instruction::LocalGet(cond_local));
                func.instruction(&Instruction::I32Eqz);

                // Set branch hint: failure is unlikely
                ctx.set_branch_hint(false);
                let if_offset = func.byte_len() as u32;
                ctx.consume_branch_hint(if_offset);

                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

                // 4. Build power-assert message
                // Allocate result local for message accumulation
                let result_local = ctx.alloc_local(
                    "__assert_msg",
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(string_array_type),
                    }),
                );

                // Start with header
                if let Some(msg_expr) = message {
                    // "Assertion failed: "
                    self.generate_string_from_data(func, "Assertion failed: ", builder);
                    func.instruction(&Instruction::LocalSet(result_local));

                    // Append user's message
                    func.instruction(&Instruction::LocalGet(result_local));
                    func.instruction(&Instruction::RefAsNonNull);
                    self.generate_expr(func, msg_expr, type_table, ctx, builder);
                    func.instruction(&Instruction::Call(
                        builder.func_idx("core/internal/string_concat"),
                    ));
                    func.instruction(&Instruction::LocalSet(result_local));

                    // Append newline
                    func.instruction(&Instruction::LocalGet(result_local));
                    func.instruction(&Instruction::RefAsNonNull);
                    self.generate_string_from_data(func, "\n", builder);
                    func.instruction(&Instruction::Call(
                        builder.func_idx("core/internal/string_concat"),
                    ));
                    func.instruction(&Instruction::LocalSet(result_local));
                } else {
                    // "Assertion failed:\n"
                    self.generate_string_from_data(func, "Assertion failed:\n", builder);
                    func.instruction(&Instruction::LocalSet(result_local));
                }

                // Append condition source: "condition: <source>\n"
                func.instruction(&Instruction::LocalGet(result_local));
                func.instruction(&Instruction::RefAsNonNull);
                let condition_line = format!("condition: {}\n", condition_source);
                self.generate_string_from_data(func, &condition_line, builder);
                func.instruction(&Instruction::Call(
                    builder.func_idx("core/internal/string_concat"),
                ));
                func.instruction(&Instruction::LocalSet(result_local));

                // For each cached value, append "<name>: <value>\n"
                for (name, local_idx, type_id) in &cached_locals {
                    // Append "<name>: "
                    func.instruction(&Instruction::LocalGet(result_local));
                    func.instruction(&Instruction::RefAsNonNull);
                    let name_prefix = format!("{}: ", name);
                    self.generate_string_from_data(func, &name_prefix, builder);
                    func.instruction(&Instruction::Call(
                        builder.func_idx("core/internal/string_concat"),
                    ));
                    func.instruction(&Instruction::LocalSet(result_local));

                    // Append value (convert to string based on type)
                    func.instruction(&Instruction::LocalGet(result_local));
                    func.instruction(&Instruction::RefAsNonNull);
                    func.instruction(&Instruction::LocalGet(*local_idx));
                    self.generate_value_to_string_from_type_id(func, *type_id, type_table, builder);
                    func.instruction(&Instruction::Call(
                        builder.func_idx("core/internal/string_concat"),
                    ));
                    func.instruction(&Instruction::LocalSet(result_local));

                    // Append newline
                    func.instruction(&Instruction::LocalGet(result_local));
                    func.instruction(&Instruction::RefAsNonNull);
                    self.generate_string_from_data(func, "\n", builder);
                    func.instruction(&Instruction::Call(
                        builder.func_idx("core/internal/string_concat"),
                    ));
                    func.instruction(&Instruction::LocalSet(result_local));
                }

                // 5. Final result on stack
                func.instruction(&Instruction::LocalGet(result_local));
                func.instruction(&Instruction::RefAsNonNull);

                // 6. Call panic
                func.instruction(&Instruction::Call(builder.func_idx("panic")));

                // 7. Unreachable (panic never returns)
                func.instruction(&Instruction::Unreachable);

                func.instruction(&Instruction::End);
            }
        }
    }

    /// Generate a Wasm function from TIR function
    fn generate_function(
        &self,
        tir_func: &TirFunction,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
        module_path: &[String],
    ) -> (Function, Vec<(u32, bool)>) {
        // Create function context - TIR already has local count and types
        let mut func_ctx =
            FunctionContext::with_module_path(tir_func.params.len() as u32, module_path.to_vec());

        // Set return type
        if tir_func.return_type != TypeTable::UNIT {
            func_ctx.set_return_type(self.type_id_to_valtype(type_table, tir_func.return_type));
        }

        // Add parameters to context
        for param in &tir_func.params {
            let param_type = self.type_id_to_valtype(type_table, param.type_id);
            func_ctx.add_param(&param.name, param_type);
        }

        // Pre-allocate locals from TIR (skip params which are already added)
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            // Skip if it's a param (already added)
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }
            let local_type = self.type_id_to_valtype(type_table, local_type_id);
            let local_name = format!("_local_{}", local_idx);
            func_ctx.alloc_local(&local_name, local_type);
        }

        // Pre-allocate locals for assert statements
        if let Some(body) = &tir_func.body {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_assert_locals(body, type_table, &mut func_ctx, string_array_type);
        }

        // Pre-allocate scratch locals for stream handling
        // These are needed for builtin::call_indirect_stdout/stderr_write_via_stream
        // and builtin::effect_wait (used by ambient logging functions like log_stdout)
        // The overhead of unused locals is negligible
        if tir_func.body.is_some() {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
        }

        // Generate the function code
        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // Add implicit return if needed
        if tir_func.return_type == TypeTable::UNIT {
            // Unit return - no value needed
        }
        wasm_func.instruction(&Instruction::End);

        // Collect branch hints (empty for now)
        let branch_hints = Vec::new();

        (wasm_func, branch_hints)
    }

    /// Generate the 'run' function for TIR with task.return wrapper
    ///
    /// This is a special case of function generation for the WASI CLI entry point.
    /// It generates the function body and appends task.return before End.
    fn generate_run_function(
        &self,
        tir_func: &TirFunction,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
    ) -> Function {
        // Create function context
        let mut func_ctx = FunctionContext::new(tir_func.params.len() as u32);

        // Add parameters to context
        for param in &tir_func.params {
            let param_type = self.type_id_to_valtype(type_table, param.type_id);
            func_ctx.add_param(&param.name, param_type);
        }

        // Pre-allocate locals from TIR
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }
            let local_type = self.type_id_to_valtype(type_table, local_type_id);
            let local_name = format!("_local_{}", local_idx);
            func_ctx.alloc_local(&local_name, local_type);
        }

        // Pre-allocate locals for assert statements
        if let Some(body) = &tir_func.body {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_assert_locals(body, type_table, &mut func_ctx, string_array_type);
        }

        // Pre-allocate scratch locals for stream handling
        // These are needed for builtin::call_indirect_stdout/stderr_write_via_stream
        // and builtin::effect_wait (used by ambient logging functions like log_stdout)
        if tir_func.body.is_some() {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
        }

        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // Call task.return to complete the async task (0 = ok)
        let task_return_idx = builder.func_idx("task-return");
        wasm_func.instruction(&Instruction::I32Const(0));
        wasm_func.instruction(&Instruction::Call(task_return_idx));
        wasm_func.instruction(&Instruction::End);

        wasm_func
    }

    /// Resolve a TIR function call target to its function index
    ///
    /// This handles:
    /// 1. Local functions (simple name lookup)
    /// 2. Builtin functions (builtin:: namespace)
    /// 3. Core library functions (core:: namespace)
    /// 4. Qualified names from imported modules
    fn resolve_call_target(
        &self,
        module_path: &[String],
        func_name: &str,
        current_module_path: &[String],
        builder: &CoreModuleBuilder,
    ) -> u32 {
        // Strategy 1: Try simple name lookup first (for local functions)
        if module_path.is_empty()
            && let Some(idx) = builder.try_func_idx(func_name)
        {
            return idx;
        }

        // Strategy 2: Check if it's a builtin function
        // Can come as module_path=["builtin"] or module_path=["core", "builtin"] or func_name="builtin::..."
        if module_path == ["builtin"]
            || module_path == ["core", "builtin"]
            || func_name.starts_with("builtin::")
        {
            let builtin_name = func_name.strip_prefix("builtin::").unwrap_or(func_name);
            if let Some(builtin_info) = self.builtin_registry.get(builtin_name)
                && let Some(canonical_name) = &builtin_info.canonical_name
                && let Some(idx) = builder.try_func_idx(canonical_name)
            {
                return idx;
            }
        }

        // Invariant: TirExprKind::Call should never have method names (containing "::")
        // Methods use TirExprKind::MethodCall instead. The only exception is "builtin::*".
        debug_assert!(
            !func_name.contains("::") || func_name.starts_with("builtin::"),
            "TirExprKind::Call should not have method-style names: {}",
            func_name
        );

        // Strategy 3: Build mangled name and try lookup
        let mangled_name = if module_path.is_empty() {
            func_name.to_string()
        } else {
            FreeFunctionName::from_path_and_name(module_path, func_name).to_string()
        };

        if let Some(idx) = builder.try_func_idx(&mangled_name) {
            return idx;
        }

        // Strategy 4: Try current module path for local function calls
        // When module_path is empty, try the current function's module
        if module_path.is_empty() && !current_module_path.is_empty() {
            let current_mangled_name =
                FreeFunctionName::from_path_and_name(current_module_path, func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&current_mangled_name) {
                return idx;
            }
        }

        // Strategy 5: Try core internal name format
        if module_path == ["core", "internal"] {
            let internal_name = build_core_internal_name(func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&internal_name) {
                return idx;
            }
        }

        // Strategy 6: Try core/cli function format (for println, eprintln, etc.)
        if module_path == ["core", "cli"] {
            let cli_name = FreeFunctionName::from_strs(&["core", "cli"], func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&cli_name) {
                return idx;
            }
        }

        // Strategy 7: Try WASI effect operation resolution
        // When module_path has a single element like ["Stdout"], try resolving as an effect operation
        if module_path.len() == 1 {
            let effect_qualified_name = format!("{}::{}", module_path[0], func_name);
            if let Some(wasi_local_name) = self.wasi_registry.resolve(&effect_qualified_name)
                && let Some(idx) = builder.try_func_idx(&wasi_local_name)
            {
                return idx;
            }
        }

        // If we get here, the function wasn't found
        let full_name = if module_path.is_empty() {
            func_name.to_string()
        } else {
            format!("{}::{}", module_path.join("::"), func_name)
        };
        panic!("unknown function: {}", full_name);
    }

    /// Generate wait logic for pending effect subtasks
    ///
    /// This should be called at the end of functions that use Stdout/Stderr effects.
    /// It waits for the subtask started by write-via-stream to complete.
    fn generate_effect_wait(
        &self,
        func: &mut Function,
        ctx: &FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Check if we have a subtask to wait for
        let subtask_local = match ctx.get_local("__subtask") {
            Some(idx) => idx,
            None => return, // No pending subtask
        };

        let waitable_set_new_idx = builder.func_idx("waitable-set-new");
        let waitable_join_idx = builder.func_idx("waitable-join");
        let waitable_set_wait_idx = builder.func_idx("waitable-set-wait");
        let subtask_drop_idx = builder.func_idx("subtask-drop");
        let waitable_set_local = ctx.get_local("__waitable_set").unwrap_or(subtask_local + 1);

        // Check if subtask is pending
        // If (status & 1) == 0, the operation is still pending and we need to wait
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::I32And);
        func.instruction(&Instruction::I32Eqz);
        func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

        // Subtask is pending - need to wait for it
        // Create waitable-set
        func.instruction(&Instruction::Call(waitable_set_new_idx));
        func.instruction(&Instruction::LocalSet(waitable_set_local));

        // Join subtask to waitable-set
        func.instruction(&Instruction::LocalGet(waitable_set_local));
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::Call(waitable_join_idx));

        // Wait for completion
        func.instruction(&Instruction::LocalGet(waitable_set_local));
        func.instruction(&Instruction::I32Const(2048)); // outptr
        func.instruction(&Instruction::Call(waitable_set_wait_idx));
        func.instruction(&Instruction::Drop); // drop wait result

        // Drop subtask
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::Call(subtask_drop_idx));

        func.instruction(&Instruction::End); // end if
    }

    /// Pre-allocate scratch locals that builtins might need during code generation
    ///
    /// Some builtins allocate temporary locals at runtime.
    /// These need to be declared in the function's local declarations.
    fn preallocate_builtin_scratch_locals(
        &self,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        // Scratch locals for stream handling builtins
        // Use nullable refs so they default to ref.null and don't require initialization
        ctx.alloc_local(
            "__arr_ref",
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );
        ctx.alloc_local("__len", ValType::I32);
        ctx.alloc_local("__ptr", ValType::I32);
        ctx.alloc_local("__i", ValType::I32);
        ctx.alloc_local("__ret64", ValType::I64);
        ctx.alloc_local("__rx", ValType::I32);
        ctx.alloc_local("__tx", ValType::I32);
        ctx.alloc_local("__alloc_size", ValType::I32);
        // Scratch locals for write_via_stream async handling
        ctx.alloc_local("__subtask", ValType::I32);
        ctx.alloc_local("__waitable_set", ValType::I32);
        // Scratch locals for template string accumulation and concatenation
        // Use nullable refs so they default to ref.null and don't require initialization
        ctx.alloc_local(
            "__template_result",
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );
        ctx.alloc_local(
            "__concat_new",
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );
        ctx.alloc_local("__result_len", ValType::I32);
        ctx.alloc_local("__part_len", ValType::I32);
    }

    /// Pre-allocate locals for TIR assert statements in a block
    fn preallocate_assert_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        for stmt in &block.stmts {
            self.preallocate_assert_locals_from_stmt(stmt, type_table, ctx, string_array_type);
        }
    }

    /// Pre-allocate locals from a single TIR statement (recursively handles nested blocks)
    fn preallocate_assert_locals_from_stmt(
        &self,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        match &stmt.kind {
            TirStmtKind::Assert { intermediates, .. } => {
                // Pre-allocate locals for intermediate values
                for (name, _, type_id) in intermediates {
                    let val_type = self.type_id_to_valtype(type_table, *type_id);
                    ctx.alloc_local(&format!("__assert_{}", name.replace(' ', "_")), val_type);
                }

                // Pre-allocate condition local
                ctx.alloc_local("__assert_cond", ValType::I32);

                // Pre-allocate message accumulator local (nullable ref)
                ctx.alloc_local(
                    "__assert_msg",
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(string_array_type),
                    }),
                );
            }
            TirStmtKind::While { body, .. } => {
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::Loop { body } => {
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.preallocate_assert_locals(then_block, type_table, ctx, string_array_type);
                if let Some(else_blk) = else_block {
                    self.preallocate_assert_locals(else_blk, type_table, ctx, string_array_type);
                }
            }
            _ => {}
        }
    }

    /// Convert a WASI function type to Core Wasm params
    ///
    /// For async functions, an extra i32 param (outptr) is added per Component Model ABI.
    /// For sync functions, params are mapped directly.
    fn wasi_func_to_core_params(&self, func: &WasiFunctionInfo) -> Vec<ValType> {
        let mut params: Vec<ValType> = func
            .params
            .iter()
            .map(|(_, ty)| wasi_type_to_valtype(ty))
            .collect();

        // Async functions have an additional outptr parameter for the result
        if func.is_async {
            params.push(ValType::I32); // outptr
        }

        params
    }

    /// Convert a WASI function type to Core Wasm results
    ///
    /// For async functions, the result is always i32 (subtask handle).
    /// For sync functions, the return type is mapped directly.
    fn wasi_func_to_core_results(&self, func: &WasiFunctionInfo) -> Vec<ValType> {
        if func.is_async {
            // Async functions return a subtask handle (i32)
            vec![ValType::I32]
        } else if let Some(ret_ty) = &func.return_type {
            vec![wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Convert a builtin function type to Core Wasm params
    fn builtin_func_to_core_params(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        func.params
            .iter()
            .map(|(_, ty)| wasi_type_to_valtype(ty))
            .collect()
    }

    /// Convert a builtin function type to Core Wasm results
    fn builtin_func_to_core_results(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        if func.diverges {
            // Diverging functions have no return type
            vec![]
        } else if let Some(ret_ty) = &func.return_type {
            vec![wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Convert a world export function type to Core Wasm params
    ///
    /// For async exports, the core function has no params (async uses task_return).
    /// For sync exports, params are mapped directly.
    fn world_export_to_core_params(&self, export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no params in core (lifted signature differs)
            vec![]
        } else {
            export
                .params
                .iter()
                .map(|(_, ty)| wasi_type_to_valtype(ty))
                .collect()
        }
    }

    /// Convert a world export function type to Core Wasm results
    ///
    /// For async exports, there's no return (result passed via task_return).
    /// For sync exports, the return type is mapped directly.
    fn world_export_to_core_results(&self, export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no return in core (use task_return)
            vec![]
        } else if let Some(ret_ty) = &export.return_type {
            vec![wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Infer expression type with function context (for looking up variable types)
    /// If builder is provided, can look up user function return types
    fn generate_string_from_data(&self, func: &mut Function, s: &str, builder: &CoreModuleBuilder) {
        let string_array_type = builder.type_idx("string-array");
        let offset = self.get_string_offset(s);
        let len = s.len();
        func.instruction(&Instruction::I32Const(offset as i32));
        func.instruction(&Instruction::I32Const(len as i32));
        func.instruction(&Instruction::ArrayNewData {
            array_type_index: string_array_type,
            array_data_index: 0,
        });
    }

    /// Generate value to string conversion based on TypeId (semantic type)
    /// This preserves the distinction between bool and i32 (both are ValType::I32 in Wasm)
    fn generate_value_to_string_from_type_id(
        &self,
        func: &mut Function,
        type_id: TypeId,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
    ) {
        match type_table.get(type_id) {
            ResolvedType::Primitive(prim) => {
                let func_name = match prim {
                    PrimitiveType::I32
                    | PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32 => "core/internal/i32_to_string",
                    PrimitiveType::I64 | PrimitiveType::U64 => "core/internal/i64_to_string",
                    PrimitiveType::F32 => "core/internal/f32_to_string",
                    PrimitiveType::F64 => "core/internal/f64_to_string",
                    PrimitiveType::Bool => "core/internal/bool_to_string",
                    PrimitiveType::Char => "core/internal/char_to_string",
                    _ => return, // I128, U128 not yet supported
                };
                func.instruction(&Instruction::Call(builder.func_idx(func_name)));
            }
            ResolvedType::String => {
                // String is already a string - no conversion needed
            }
            _ => {
                // For other types (structs, etc.), treat as string (no conversion)
            }
        }
    }

    /// Build the memory module (provides shared memory and realloc for all core modules)
    fn build_memory_module(&self, string_data: &[u8], hints: &OptimizationHints) -> Vec<u8> {
        let mut module = Module::new();

        // Type section: realloc type
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
        module.section(&types);

        // Function section
        let mut functions = FunctionSection::new();
        functions.function(0); // realloc uses type 0
        module.section(&functions);

        // Memory section
        // Minimum 17 pages to satisfy the float-to-string module's memory requirements
        // (the bundled module needs ~1MB for its data segment)
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 17,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        // Export section
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("realloc", ExportKind::Func, 0);
        module.section(&exports);

        // Code section: realloc function
        let mut code = CodeSection::new();
        let mut realloc_func = Function::new([]);
        realloc_func.instruction(&Instruction::I32Const(1024));
        realloc_func.instruction(&Instruction::End);
        code.function(&realloc_func);
        module.section(&code);

        // Data section: string literals
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.segment(DataSegment {
                mode: DataSegmentMode::Active {
                    memory_index: 0,
                    offset: &ConstExpr::i32_const(0),
                },
                data: string_data.iter().copied(),
            });
            module.section(&data);
        }

        // Name section (skip in size-optimized builds)
        if !hints.strip_names {
            let mut names = NameSection::new();
            let mut func_names = NameMap::new();
            func_names.append(0, "realloc");
            names.functions(&func_names);
            let mut type_names = NameMap::new();
            type_names.append(0, "realloc");
            names.types(&type_names);
            module.section(&names);
        }

        module.finish()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_generate_binary() {
        let wasm = crate::compile(
            r#"
            fn add(a: i32, b: i32) -> i32 {
                return a + b;
            }

            fn run() {
                let result = add(1, 2);
            }
        "#,
        )
        .expect("compilation failed");

        // Verify it starts with Wasm magic number
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");
    }
}
