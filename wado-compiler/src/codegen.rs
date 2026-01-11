// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use crate::ast::{
    Block, CallExpr, Expr, Function as AstFunction, Item, Literal, Module as AstModule, Stmt,
    TemplatePart,
};
use crate::bundled::wado_bundled_wasm;
use crate::symbol::SymbolTable;
use crate::wasm_postprocess;
use std::collections::HashMap;
use wasm_encoder::{
    Alias, ArrayType, CanonicalOption, CodeSection, ComponentBuilder, ComponentExportKind,
    ComponentOuterAliasKind, ComponentValType, CompositeInnerType, CompositeType, ConstExpr,
    DataCountSection, DataSection, DataSegment, DataSegmentMode, EntityType, ExportKind,
    ExportSection, FieldType, Function, FunctionSection, HeapType, ImportSection, InstanceType,
    Instruction, MemArg, MemorySection, MemoryType, Module, ModuleArg, NameMap, NameSection,
    PrimitiveValType, RefType, StorageType, SubType, TypeBounds, TypeSection, ValType,
};

/// Code generator that produces Component Model components
/// Targets WASI P3 (0.3.0-rc-2025-09-16)
pub struct Codegen {
    string_literals: Vec<String>,
}

/// Context for tracking local variables during function code generation
/// Local indices in Wasm: parameters come first, then declared locals
struct FunctionContext {
    /// Map from variable name to local index
    locals: HashMap<String, u32>,
    /// Map from variable name to type (for type inference)
    local_type_map: HashMap<String, ValType>,
    /// Map from variable name to semantic type (for bool/char detection in templates)
    semantic_type_map: HashMap<String, SemanticType>,
    /// Number of parameters (locals 0..param_count are parameters)
    #[allow(dead_code)]
    param_count: u32,
    /// Next available local index for new variables
    next_local: u32,
    /// Local types for non-parameter locals (for function declaration)
    local_types: Vec<ValType>,
    /// Return type of the function (for ref.as_non_null handling)
    return_type: Option<ValType>,
}

impl FunctionContext {
    fn new(param_count: u32) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            semantic_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
            return_type: None,
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

    /// Get semantic type for a variable
    fn get_semantic_type(&self, name: &str) -> Option<SemanticType> {
        self.semantic_type_map.get(name).copied()
    }

    /// Set semantic type for a variable
    fn set_semantic_type(&mut self, name: &str, semantic: SemanticType) {
        self.semantic_type_map.insert(name.to_string(), semantic);
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

    /// Get local type by name
    fn get_local_type(&self, name: &str) -> Option<ValType> {
        self.local_type_map.get(name).copied()
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

/// Semantic type for distinguishing bool/char from i32 in template interpolation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticType {
    Bool,
    Char,
    I32,
    Other,
}

// ============================================================================
// CoreModuleBuilder - Builder for Wasm core modules with dynamic index allocation
// ============================================================================

/// Builder for Wasm core modules with dynamic index allocation.
/// Eliminates hardcoded type/function indices by tracking them by name.
struct CoreModuleBuilder {
    // Wasm sections
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    #[allow(dead_code)] // Will be used when we fully migrate from manual name section building
    names: NameSection,

    // Type tracking
    type_names: HashMap<String, u32>,
    next_type_idx: u32,

    // Function tracking
    func_names: HashMap<String, u32>,
    func_type_names: HashMap<String, String>, // func_name -> type_name
    type_has_return: HashMap<String, bool>,   // type_name -> has_return
    next_func_idx: u32,

    // Memory tracking
    has_memory: bool,
}

impl CoreModuleBuilder {
    /// Create a new builder with all indices starting at 0
    fn new() -> Self {
        Self {
            types: TypeSection::new(),
            imports: ImportSection::new(),
            functions: FunctionSection::new(),
            exports: ExportSection::new(),
            names: NameSection::new(),
            type_names: HashMap::new(),
            next_type_idx: 0,
            func_names: HashMap::new(),
            func_type_names: HashMap::new(),
            type_has_return: HashMap::new(),
            next_func_idx: 0,
            has_memory: false,
        }
    }

    /// Define a function type and return its index
    fn define_func_type(&mut self, name: &str, params: &[ValType], results: &[ValType]) -> u32 {
        let idx = self.next_type_idx;
        self.types
            .ty()
            .function(params.iter().copied(), results.iter().copied());
        self.type_names.insert(name.to_string(), idx);
        self.type_has_return
            .insert(name.to_string(), !results.is_empty());
        self.next_type_idx += 1;
        idx
    }

    /// Define a GC array type and return its index
    fn define_gc_array_type(&mut self, name: &str, element: StorageType, mutable: bool) -> u32 {
        let idx = self.next_type_idx;
        self.types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Array(ArrayType(FieldType {
                    element_type: element,
                    mutable,
                })),
                shared: false,
                descriptor: None,
                describes: None,
            },
        });
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Import a function and return its function index
    fn import_func(&mut self, module: &str, name: &str, type_name: &str) -> u32 {
        let type_idx = self.type_idx(type_name);
        self.imports
            .import(module, name, EntityType::Function(type_idx));
        let func_idx = self.next_func_idx;
        self.func_names.insert(name.to_string(), func_idx);
        self.func_type_names
            .insert(name.to_string(), type_name.to_string());
        self.next_func_idx += 1;
        func_idx
    }

    /// Import memory
    fn import_memory(&mut self, module: &str, name: &str, min: u64) {
        self.imports.import(
            module,
            name,
            EntityType::Memory(MemoryType {
                minimum: min,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );
        self.has_memory = true;
    }

    /// Define a function (adds to function section) and return its index
    fn define_func(&mut self, name: &str, type_name: &str) -> u32 {
        let type_idx = self.type_idx(type_name);
        self.functions.function(type_idx);
        let func_idx = self.next_func_idx;
        self.func_names.insert(name.to_string(), func_idx);
        self.func_type_names
            .insert(name.to_string(), type_name.to_string());
        self.next_func_idx += 1;
        func_idx
    }

    /// Define a function with an alias (same index, different name)
    #[allow(dead_code)]
    fn define_func_alias(&mut self, alias_name: &str, func_idx: u32) {
        self.func_names.insert(alias_name.to_string(), func_idx);
    }

    /// Export a function
    fn export_func(&mut self, export_name: &str, func_name: &str) {
        let func_idx = self.func_idx(func_name);
        self.exports.export(export_name, ExportKind::Func, func_idx);
    }

    /// Get type index by name
    fn type_idx(&self, name: &str) -> u32 {
        *self
            .type_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown type: {name}"))
    }

    /// Get function index by name
    fn func_idx(&self, name: &str) -> u32 {
        *self
            .func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown function: {name}"))
    }

    /// Try to get function index by name, returns None if not found
    fn try_func_idx(&self, name: &str) -> Option<u32> {
        self.func_names.get(name).copied()
    }

    /// Check if a function has a return type
    fn func_has_return(&self, name: &str) -> bool {
        self.func_type_names
            .get(name)
            .and_then(|type_name| self.type_has_return.get(type_name))
            .copied()
            .unwrap_or(false)
    }

    /// Add a function name to the name section (names are automatically tracked)
    #[allow(dead_code)]
    fn add_func_name(&mut self, _func_name: &str) {
        // Names are tracked in func_names during define_func/import_func
        // build_name_section() uses func_names to build the name section
    }

    /// Get access to the types section for complex type definitions
    #[allow(dead_code)]
    fn types_mut(&mut self) -> &mut TypeSection {
        &mut self.types
    }

    /// Get access to the imports section
    #[allow(dead_code)]
    fn imports_mut(&mut self) -> &mut ImportSection {
        &mut self.imports
    }

    /// Get access to the functions section
    #[allow(dead_code)]
    fn functions_mut(&mut self) -> &mut FunctionSection {
        &mut self.functions
    }

    /// Get access to the exports section
    #[allow(dead_code)]
    fn exports_mut(&mut self) -> &mut ExportSection {
        &mut self.exports
    }

    /// Build the name section from tracked function names
    fn build_name_section(&self) -> NameSection {
        let mut names = NameSection::new();
        let mut func_names = NameMap::new();
        for (name, &idx) in &self.func_names {
            func_names.append(idx, name);
        }
        names.functions(&func_names);
        names
    }

    /// Create a RefType for the string array (GC array<u8>)
    fn string_ref_type(&self) -> RefType {
        RefType {
            nullable: false,
            heap_type: HeapType::Concrete(self.type_idx("string-array")),
        }
    }

    /// Create a ValType for the string array (GC array<u8>)
    #[allow(dead_code)]
    fn string_val_type(&self) -> ValType {
        ValType::Ref(self.string_ref_type())
    }
}

// ============================================================================
// ComponentContext - Tracks component-level indices for types, instances, core functions
// ============================================================================

/// Tracks component-level indices for types, instances, and core functions.
/// Used alongside wasm-encoder's ComponentBuilder to eliminate magic numbers.
struct ComponentContext {
    // Component type indices
    type_names: HashMap<String, u32>,
    next_type_idx: u32,

    // Component instance indices
    instance_names: HashMap<String, u32>,
    next_instance_idx: u32,

    // Core function indices (at component level - aliased/lowered functions)
    core_func_names: HashMap<String, u32>,
    next_core_func_idx: u32,

    // Core memory index
    core_memory_idx: Option<u32>,

    // Component-level function indices (lifted functions)
    comp_func_names: HashMap<String, u32>,
    next_comp_func_idx: u32,

    // Core module indices
    core_module_names: HashMap<String, u32>,
    next_core_module_idx: u32,

    // Core instance indices
    core_instance_names: HashMap<String, u32>,
    next_core_instance_idx: u32,
}

impl ComponentContext {
    /// Create a new context with all indices starting at 0
    fn new() -> Self {
        Self {
            type_names: HashMap::new(),
            next_type_idx: 0,
            instance_names: HashMap::new(),
            next_instance_idx: 0,
            core_func_names: HashMap::new(),
            next_core_func_idx: 0,
            core_memory_idx: None,
            comp_func_names: HashMap::new(),
            next_comp_func_idx: 0,
            core_module_names: HashMap::new(),
            next_core_module_idx: 0,
            core_instance_names: HashMap::new(),
            next_core_instance_idx: 0,
        }
    }

    /// Register a component type and return its index
    fn register_type(&mut self, name: &str) -> u32 {
        let idx = self.next_type_idx;
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Get component type index by name
    fn type_idx(&self, name: &str) -> u32 {
        *self
            .type_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component type: {name}"))
    }

    /// Register a component instance and return its index
    fn register_instance(&mut self, name: &str) -> u32 {
        let idx = self.next_instance_idx;
        self.instance_names.insert(name.to_string(), idx);
        self.next_instance_idx += 1;
        idx
    }

    /// Get component instance index by name
    fn instance_idx(&self, name: &str) -> u32 {
        *self
            .instance_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component instance: {name}"))
    }

    /// Register a core function (at component level) and return its index
    fn register_core_func(&mut self, name: &str) -> u32 {
        let idx = self.next_core_func_idx;
        self.core_func_names.insert(name.to_string(), idx);
        self.next_core_func_idx += 1;
        idx
    }

    /// Get core function index by name
    fn core_func_idx(&self, name: &str) -> u32 {
        *self
            .core_func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core function: {name}"))
    }

    /// Set the core memory index
    fn set_memory(&mut self, idx: u32) {
        self.core_memory_idx = Some(idx);
    }

    /// Get the core memory index
    fn memory_idx(&self) -> u32 {
        self.core_memory_idx.expect("memory not set")
    }

    /// Register a component-level function (lifted) and return its index
    fn register_comp_func(&mut self, name: &str) -> u32 {
        let idx = self.next_comp_func_idx;
        self.comp_func_names.insert(name.to_string(), idx);
        self.next_comp_func_idx += 1;
        idx
    }

    /// Get component-level function index by name
    fn comp_func_idx(&self, name: &str) -> u32 {
        *self
            .comp_func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component function: {name}"))
    }

    /// Register a core module and return its index
    fn register_core_module(&mut self, name: &str) -> u32 {
        let idx = self.next_core_module_idx;
        self.core_module_names.insert(name.to_string(), idx);
        self.next_core_module_idx += 1;
        idx
    }

    /// Get core module index by name
    fn core_module_idx(&self, name: &str) -> u32 {
        *self
            .core_module_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core module: {name}"))
    }

    /// Register a core instance and return its index
    fn register_core_instance(&mut self, name: &str) -> u32 {
        let idx = self.next_core_instance_idx;
        self.core_instance_names.insert(name.to_string(), idx);
        self.next_core_instance_idx += 1;
        idx
    }

    /// Get core instance index by name
    fn core_instance_idx(&self, name: &str) -> u32 {
        *self
            .core_instance_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core instance: {name}"))
    }
}

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

impl Codegen {
    /// Create a new code generator
    pub fn new() -> Self {
        Self {
            string_literals: Vec::new(),
        }
    }

    /// Generate Component Model binary Wasm
    pub fn generate_wasm(&mut self, module: &AstModule) -> Vec<u8> {
        // First pass: collect string literals
        self.collect_strings(module);

        // Generate binary Wasm
        self.generate_component(module)
    }

    /// Generate Component Model binary Wasm with support for multiple modules
    ///
    /// This version supports compiling multiple Wado modules into a single Wasm component.
    /// Functions from imported local modules are included in the generated code.
    pub fn generate_wasm_with_modules(
        &mut self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        symbols: &SymbolTable,
    ) -> Vec<u8> {
        // First pass: collect string literals from all modules
        self.collect_strings(main_module);
        for (_, module) in loaded_modules {
            self.collect_strings(module);
        }

        // Generate binary Wasm with multi-module support
        self.generate_component_with_modules(main_module, loaded_modules, symbols)
    }

    /// Generate WAT text format (for debugging)
    pub fn generate_wat(&mut self, module: &AstModule) -> String {
        let wasm = self.generate_wasm(module);
        wasmprinter::print_bytes(&wasm).unwrap_or_else(|e| format!("Error: {e}"))
    }

    fn collect_strings(&mut self, module: &AstModule) {
        for item in &module.items {
            if let Item::Function(func) = item {
                // Skip bodyless functions (compiler built-ins)
                if let Some(body) = &func.body {
                    self.collect_strings_from_block(body);
                }
            }
        }
    }

    fn collect_strings_from_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.collect_strings_from_stmt(stmt);
        }
    }

    fn collect_strings_from_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Expr(expr_stmt) => self.collect_strings_from_expr(&expr_stmt.expr),
            Stmt::Let(let_stmt) => self.collect_strings_from_expr(&let_stmt.value),
            Stmt::Return(ret_stmt) => {
                if let Some(expr) = &ret_stmt.value {
                    self.collect_strings_from_expr(expr);
                }
            }
            Stmt::If(if_stmt) => {
                self.collect_strings_from_expr(&if_stmt.condition);
                self.collect_strings_from_block(&if_stmt.then_block);
                if let Some(else_block) = &if_stmt.else_block {
                    self.collect_strings_from_block(else_block);
                }
            }
            Stmt::While(while_stmt) => {
                self.collect_strings_from_expr(&while_stmt.condition);
                self.collect_strings_from_block(&while_stmt.body);
            }
            Stmt::For(for_stmt) => {
                if let Some(init) = &for_stmt.init {
                    self.collect_strings_from_stmt(init);
                }
                if let Some(cond) = &for_stmt.condition {
                    self.collect_strings_from_expr(cond);
                }
                if let Some(update) = &for_stmt.update {
                    self.collect_strings_from_expr(update);
                }
                self.collect_strings_from_block(&for_stmt.body);
            }
            Stmt::Assert(assert_stmt) => {
                self.collect_strings_from_expr(&assert_stmt.condition);
            }
        }
    }

    fn collect_strings_from_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Literal(lit) => {
                if let Literal::String(s) = &lit.value
                    && !self.string_literals.contains(s)
                {
                    self.string_literals.push(s.clone());
                }
            }
            Expr::Call(call) => {
                self.collect_strings_from_expr(&call.callee);
                for arg in &call.args {
                    self.collect_strings_from_expr(arg);
                }
            }
            Expr::Binary(bin) => {
                self.collect_strings_from_expr(&bin.left);
                self.collect_strings_from_expr(&bin.right);
            }
            Expr::Unary(un) => {
                self.collect_strings_from_expr(&un.expr);
            }
            Expr::MethodCall(mc) => {
                self.collect_strings_from_expr(&mc.receiver);
                for arg in &mc.args {
                    self.collect_strings_from_expr(arg);
                }
            }
            Expr::FieldAccess(fa) => {
                self.collect_strings_from_expr(&fa.expr);
            }
            Expr::Index(idx) => {
                self.collect_strings_from_expr(&idx.expr);
                self.collect_strings_from_expr(&idx.index);
            }
            Expr::Closure(closure) => {
                self.collect_strings_from_expr(&closure.body);
            }
            Expr::Assign(assign) => {
                self.collect_strings_from_expr(&assign.target);
                self.collect_strings_from_expr(&assign.value);
            }
            Expr::TemplateString(template) => {
                for part in &template.parts {
                    match part {
                        TemplatePart::String(s) => {
                            if !self.string_literals.contains(s) {
                                self.string_literals.push(s.clone());
                            }
                        }
                        TemplatePart::Interpolation { expr, .. } => {
                            self.collect_strings_from_expr(expr);
                        }
                    }
                }
            }
            _ => {}
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

    /// Generate component for WASI P3
    /// Uses native stream<T> types and imports wasi:cli/stdout
    fn generate_component(&self, ast_module: &AstModule) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentContext::new();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // ========================================
        // Type: types instance type (for wasi:cli/types)
        // Contains error-code enum definition
        // ========================================
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

        // Import types instance from WASI P3
        ctx.register_instance("types");
        builder.import(
            "wasi:cli/types@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        // Alias error-code from types instance
        ctx.register_type("error-code");
        builder.alias_export(
            ctx.instance_idx("types"),
            "error-code",
            ComponentExportKind::Type,
        );

        // ========================================
        // Type: stdout instance type
        // Uses the aliased error-code type for result via outer alias
        // ========================================
        let stdout_instance_type = ctx.register_type("stdout-instance-type");
        {
            let (_, enc) = builder.ty(Some("stdout-instance-type"));
            let mut instance_type = InstanceType::new();
            // Type 0 within instance: stream<u8>
            instance_type
                .ty()
                .defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
            // Type 1 within instance: outer alias to error-code type
            let error_code_type = ctx.type_idx("error-code");
            instance_type.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: error_code_type,
            });
            // Type 2 within instance: result<_, error-code>
            instance_type
                .ty()
                .defined_type()
                .result(None, Some(ComponentValType::Type(1)));
            // Type 3 within instance: async func(stream<u8>) -> result<_, error-code>
            instance_type
                .ty()
                .function()
                .async_(true)
                .params([("data", ComponentValType::Type(0))])
                .result(Some(ComponentValType::Type(2)));
            instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
            enc.instance(&instance_type);
        }

        // Import stdout instance from WASI P3
        ctx.register_instance("stdout");
        builder.import(
            "wasi:cli/stdout@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
        );

        // Alias write-via-stream from stdout instance (component func)
        ctx.register_comp_func("write-via-stream");
        builder.alias_export(
            ctx.instance_idx("stdout"),
            "write-via-stream",
            ComponentExportKind::Func,
        );

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
        let mem_module = self.build_memory_module(&string_data);
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
        // Float-to-string conversion module
        // ========================================
        let fts_module =
            wasm_postprocess::convert_memory_to_import(wado_bundled_wasm(), "env", "memory")
                .expect("Failed to process float-to-string module");
        ctx.register_core_module("fts-mod");
        builder.core_module_raw(Some("fts-mod"), &fts_module);

        // Create env instance for float-to-string (just memory)
        // Note: core_instantiate_exports creates an instance, so we must track it
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

        // Alias float-to-string exports
        ctx.register_core_func("f64-to-buffer");
        builder.core_alias_export(
            Some("f64-to-buffer"),
            ctx.core_instance_idx("fts"),
            "f64_to_buffer",
            ExportKind::Func,
        );

        ctx.register_core_func("f32-to-buffer");
        builder.core_alias_export(
            Some("f32-to-buffer"),
            ctx.core_instance_idx("fts"),
            "f32_to_buffer",
            ExportKind::Func,
        );

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

        // Lower write-via-stream component func to core func
        ctx.register_core_func("write-via-stream-core");
        builder.lower_func(
            Some("write-via-stream-core"),
            ctx.comp_func_idx("write-via-stream"),
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
                CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
            ],
        );

        // task.return for completing async tasks
        ctx.register_core_func("task-return");
        builder.task_return(Some(ComponentValType::Type(result_unit_type)), []);

        // waitable-set.new
        ctx.register_core_func("waitable-set-new");
        builder.waitable_set_new();

        // waitable.join
        ctx.register_core_func("waitable-join");
        builder.waitable_join();

        // waitable-set.wait
        ctx.register_core_func("waitable-set-wait");
        builder.waitable_set_wait(false, ctx.memory_idx());

        // subtask.drop
        ctx.register_core_func("subtask-drop");
        builder.subtask_drop();

        // ========================================
        // Main core module for P3
        // ========================================
        let main_module = self.build_main_module_p3(ast_module, &string_data);
        ctx.register_core_module("main-mod");
        builder.core_module_raw(Some("main-mod"), &main_module);

        // Create wasi instance with stream intrinsics + lowered WASI function + async intrinsics
        let wasi_exports = [
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
                "write-via-stream",
                ExportKind::Func,
                ctx.core_func_idx("write-via-stream-core"),
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
        let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports);
        ctx.register_core_instance("wasi");
        // Note: wasi_instance is the synthetic instance index returned by core_instantiate_exports

        let env_exports = [
            ("memory", ExportKind::Memory, ctx.memory_idx()),
            ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
            (
                "f64_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f64-to-buffer"),
            ),
            (
                "f32_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f32-to-buffer"),
            ),
        ];
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
        builder.finish()
    }

    /// Collect user-defined functions from the AST (excluding run and builtins)
    fn collect_user_functions<'a>(&self, ast_module: &'a AstModule) -> Vec<&'a AstFunction> {
        ast_module
            .items
            .iter()
            .filter_map(|item| {
                if let Item::Function(f) = item {
                    // Skip run (handled separately as the entry point)
                    if f.name == "run" {
                        return None;
                    }
                    // Skip bodyless functions (builtins)
                    f.body.as_ref()?;
                    return Some(f);
                }
                None
            })
            .collect()
    }

    /// Collect user-defined functions from all modules with their qualified names
    /// Returns (module_path, function, qualified_name)
    fn collect_all_user_functions<'a>(
        &self,
        main_module: &'a AstModule,
        loaded_modules: &'a [(&'a Vec<String>, &'a AstModule)],
    ) -> Vec<(Vec<String>, &'a AstFunction, String)> {
        let mut all_funcs = Vec::new();

        // Collect from main module (empty path)
        for item in &main_module.items {
            if let Item::Function(f) = item {
                // Skip run (handled separately)
                if f.name == "run" {
                    continue;
                }
                // Skip bodyless functions (builtins)
                if f.body.is_none() {
                    continue;
                }
                // Main module functions use unqualified names
                all_funcs.push((vec![], f, f.name.clone()));
            }
        }

        // Collect from imported modules (with qualified names)
        for (module_path, module) in loaded_modules {
            // Skip wasi:* modules (they only contain effect declarations, no function bodies)
            // Include core:* modules (they contain user-defined helper functions)
            if module_path.first().map(|s| s == "wasi").unwrap_or(false) {
                continue;
            }

            for item in &module.items {
                if let Item::Function(f) = item {
                    // Skip run
                    if f.name == "run" {
                        continue;
                    }
                    // Skip non-pub functions from other modules
                    if !f.is_pub {
                        continue;
                    }
                    // Skip bodyless functions
                    if f.body.is_none() {
                        continue;
                    }
                    // Skip functions that have unsupported effects
                    // Currently only Stdout and Stderr effects are supported (or no effects)
                    if !f.effects.is_empty() {
                        let has_unsupported_effects =
                            f.effects.iter().any(|e| e != "Stdout" && e != "Stderr");
                        if has_unsupported_effects {
                            continue;
                        }
                    }
                    // Create qualified name: "module_path::func_name"
                    let qualified_name = format!("{}::{}", module_path.join("::"), f.name);
                    all_funcs.push((module_path.to_vec(), f, qualified_name));
                }
            }
        }

        all_funcs
    }

    /// Generate component with multi-module support
    fn generate_component_with_modules(
        &self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        symbols: &SymbolTable,
    ) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentContext::new();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // ========================================
        // Type: types instance type (for wasi:cli/types)
        // Contains error-code enum definition
        // ========================================
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

        // Import types instance from WASI P3
        ctx.register_instance("types");
        builder.import(
            "wasi:cli/types@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        // Alias error-code from types instance
        ctx.register_type("error-code");
        builder.alias_export(
            ctx.instance_idx("types"),
            "error-code",
            ComponentExportKind::Type,
        );

        // ========================================
        // Type: stdout instance type
        // ========================================
        let stdout_instance_type = ctx.register_type("stdout-instance-type");
        {
            let (_, enc) = builder.ty(Some("stdout-instance-type"));
            let mut instance_type = InstanceType::new();
            instance_type
                .ty()
                .defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
            let error_code_type = ctx.type_idx("error-code");
            instance_type.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: error_code_type,
            });
            instance_type
                .ty()
                .defined_type()
                .result(None, Some(ComponentValType::Type(1)));
            instance_type
                .ty()
                .function()
                .async_(true)
                .params([("data", ComponentValType::Type(0))])
                .result(Some(ComponentValType::Type(2)));
            instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
            enc.instance(&instance_type);
        }

        // Import stdout instance from WASI P3
        ctx.register_instance("stdout");
        builder.import(
            "wasi:cli/stdout@0.3.0-rc-2025-09-16",
            wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
        );

        // Alias write-via-stream from stdout instance (component func)
        ctx.register_comp_func("write-via-stream");
        builder.alias_export(
            ctx.instance_idx("stdout"),
            "write-via-stream",
            ComponentExportKind::Func,
        );

        // Type: stream<u8>
        let stream_u8_type = ctx.register_type("stream-u8");
        {
            let (_, enc) = builder.ty(Some("stream-u8"));
            enc.defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
        }

        // Type: result (for run return type)
        let result_unit_type = ctx.register_type("result-unit");
        {
            let (_, enc) = builder.ty(Some("result-unit"));
            enc.defined_type().result(None, None);
        }

        // ========================================
        // Memory module
        // ========================================
        ctx.register_core_module("memory-mod");
        {
            let mut memory_module = Module::new();
            let mut types = TypeSection::new();
            types.ty().function(
                [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
                [ValType::I32],
            );
            memory_module.section(&types);

            let mut functions = FunctionSection::new();
            functions.function(0);
            memory_module.section(&functions);

            // Minimum 17 pages to satisfy the float-to-string module's memory requirements
            let mut memory = MemorySection::new();
            memory.memory(MemoryType {
                minimum: 17,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            });
            memory_module.section(&memory);

            let mut exports = ExportSection::new();
            exports.export("memory", ExportKind::Memory, 0);
            exports.export("realloc", ExportKind::Func, 0);
            memory_module.section(&exports);

            // Bump allocator: pointer is stored at 1060000 (after float-to-string data segment)
            // Initial allocation starts at 1060004 (after the pointer)
            const BUMP_PTR_ADDR: i32 = 1060000;
            const BUMP_INIT_VALUE: i32 = 1060004;

            let mut code = CodeSection::new();
            let mut realloc_func = Function::new([(1, ValType::I32)]);
            // Load current bump pointer
            realloc_func.instruction(&Instruction::I32Const(BUMP_PTR_ADDR));
            realloc_func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            realloc_func.instruction(&Instruction::LocalTee(4));
            // Add requested size
            realloc_func.instruction(&Instruction::LocalGet(3));
            realloc_func.instruction(&Instruction::I32Add);
            // Store updated bump pointer
            realloc_func.instruction(&Instruction::I32Const(BUMP_PTR_ADDR));
            realloc_func.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            // Return old pointer (allocation address)
            realloc_func.instruction(&Instruction::LocalGet(4));
            realloc_func.instruction(&Instruction::End);
            code.function(&realloc_func);
            memory_module.section(&code);

            // Initialize bump allocator pointer at BUMP_PTR_ADDR
            let mut data = DataSection::new();
            let init_ptr: [u8; 4] = BUMP_INIT_VALUE.to_le_bytes();
            data.segment(DataSegment {
                mode: DataSegmentMode::Active {
                    memory_index: 0,
                    offset: &ConstExpr::i32_const(BUMP_PTR_ADDR),
                },
                data: init_ptr.iter().copied(),
            });
            memory_module.section(&data);

            builder.core_module_raw(Some("memory-mod"), &memory_module.finish());
        }

        // Instantiate memory module
        ctx.register_core_instance("mem");
        builder.core_instantiate(
            Some("mem-instance"),
            ctx.core_module_idx("memory-mod"),
            Vec::<(&str, ModuleArg)>::new(),
        );

        // Alias memory and realloc from memory instance
        ctx.set_memory(0);
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
        // Float-to-string conversion module
        // ========================================
        let fts_module =
            wasm_postprocess::convert_memory_to_import(wado_bundled_wasm(), "env", "memory")
                .expect("Failed to process float-to-string module");
        ctx.register_core_module("fts-mod");
        builder.core_module_raw(Some("fts-mod"), &fts_module);

        // Create env instance for float-to-string (just memory)
        // Note: core_instantiate_exports creates an instance, so we must track it
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

        // Alias float-to-string exports
        ctx.register_core_func("f64-to-buffer");
        builder.core_alias_export(
            Some("f64-to-buffer"),
            ctx.core_instance_idx("fts"),
            "f64_to_buffer",
            ExportKind::Func,
        );

        ctx.register_core_func("f32-to-buffer");
        builder.core_alias_export(
            Some("f32-to-buffer"),
            ctx.core_instance_idx("fts"),
            "f32_to_buffer",
            ExportKind::Func,
        );

        // Stream intrinsics
        ctx.register_core_func("stream-new");
        builder.stream_new(stream_u8_type);

        ctx.register_core_func("stream-write");
        builder.stream_write(stream_u8_type, [CanonicalOption::Memory(ctx.memory_idx())]);

        ctx.register_core_func("stream-drop-writable");
        builder.stream_drop_writable(stream_u8_type);

        ctx.register_core_func("stream-drop-readable");
        builder.stream_drop_readable(stream_u8_type);

        // Lower write-via-stream
        ctx.register_core_func("write-via-stream-core");
        builder.lower_func(
            Some("write-via-stream-core"),
            ctx.comp_func_idx("write-via-stream"),
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
                CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
            ],
        );

        // task.return
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
        // Main core module for P3 with multi-module support
        // ========================================
        let main_core_module = self.build_main_module_p3_with_modules(
            main_module,
            loaded_modules,
            symbols,
            &string_data,
        );
        ctx.register_core_module("main-mod");
        builder.core_module_raw(Some("main-mod"), &main_core_module);

        // Create wasi instance with stream intrinsics + lowered WASI function + async intrinsics
        let wasi_exports = [
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
                "write-via-stream",
                ExportKind::Func,
                ctx.core_func_idx("write-via-stream-core"),
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
        let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports);
        ctx.register_core_instance("wasi");

        let env_exports = [
            ("memory", ExportKind::Memory, ctx.memory_idx()),
            ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
            (
                "f64_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f64-to-buffer"),
            ),
            (
                "f32_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f32-to-buffer"),
            ),
        ];
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

        // Type: async run function type
        let run_func_type = ctx.register_type("run-func-type");
        {
            let (_, enc) = builder.ty(Some("run-func-type"));
            enc.function()
                .async_(true)
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Type(result_unit_type)));
        }

        // Lift run function
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
        builder.finish()
    }

    /// Build main module for WASI P3 with multi-module support
    fn build_main_module_p3_with_modules(
        &self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        _symbols: &SymbolTable,
        string_data: &[u8],
    ) -> Vec<u8> {
        let mut module = Module::new();
        let mut builder = CoreModuleBuilder::new();

        // Collect all user-defined functions from all modules
        let all_funcs = self.collect_all_user_functions(main_module, loaded_modules);

        // Build import name → qualified name lookup table
        let mut import_lookup: HashMap<String, String> = HashMap::new();
        for (module_path, func, qualified_name) in &all_funcs {
            if !module_path.is_empty() {
                // This is from an imported module - register the function name → qualified name
                import_lookup.insert(func.name.clone(), qualified_name.clone());
            }
        }

        // ========================================
        // Define types using the builder
        // ========================================

        // WASI function types
        builder.define_func_type("stream-new", &[], &[ValType::I64]);
        builder.define_func_type(
            "stream-write",
            &[ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("stream-drop-writable", &[ValType::I32], &[]);
        builder.define_func_type("stream-drop-readable", &[ValType::I32], &[]);
        builder.define_func_type(
            "write-via-stream",
            &[ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("task-return", &[ValType::I32], &[]);
        builder.define_func_type("waitable-set-new", &[], &[ValType::I32]);
        builder.define_func_type("waitable-join", &[ValType::I32, ValType::I32], &[]);
        builder.define_func_type(
            "waitable-set-wait",
            &[ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("subtask-drop", &[ValType::I32], &[]);

        // realloc type
        builder.define_func_type(
            "realloc",
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        // IMPORTANT: This must be defined at this position to maintain type index 11
        // for string-array (hardcoded in several places for type inference)
        builder.define_gc_array_type("string-array", StorageType::I8, true);

        // Float-to-buffer types (after GC types to preserve type indices)
        builder.define_func_type(
            "f64_to_buffer",
            &[ValType::F64, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type(
            "f32_to_buffer",
            &[ValType::F32, ValType::I32],
            &[ValType::I32],
        );

        // Types for user-defined functions
        let string_array_idx = builder.type_idx("string-array");
        for (_, func, qualified_name) in &all_funcs {
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            let return_types: Vec<ValType> = if let Some(ret_ty) = &func.return_type {
                vec![self.wado_type_to_wasm_with_idx(ret_ty, string_array_idx)]
            } else {
                vec![]
            };
            builder.define_func_type(qualified_name, &param_types, &return_types);
        }

        // run type
        builder.define_func_type("run", &[], &[]);

        // Add types section to module
        module.section(&builder.types);

        // ========================================
        // Import section
        // ========================================
        builder.import_func("wasi", "stream-new", "stream-new");
        builder.import_func("wasi", "stream-write", "stream-write");
        builder.import_func("wasi", "stream-drop-writable", "stream-drop-writable");
        builder.import_func("wasi", "stream-drop-readable", "stream-drop-readable");
        builder.import_func("wasi", "write-via-stream", "write-via-stream");
        builder.import_func("wasi", "task-return", "task-return");
        builder.import_func("wasi", "waitable-set-new", "waitable-set-new");
        builder.import_func("wasi", "waitable-join", "waitable-join");
        builder.import_func("wasi", "waitable-set-wait", "waitable-set-wait");
        builder.import_func("wasi", "subtask-drop", "subtask-drop");
        builder.import_func("env", "realloc", "realloc");
        builder.import_func("env", "f64_to_buffer", "f64_to_buffer");
        builder.import_func("env", "f32_to_buffer", "f32_to_buffer");
        builder.import_memory("env", "memory", 1);
        module.section(&builder.imports);

        // ========================================
        // Function section
        // ========================================

        // Register all user-defined functions
        for (_, func, qualified_name) in &all_funcs {
            let func_idx = builder.define_func(qualified_name, qualified_name);
            // Also register with simple name as an alias for same-module lookup
            if qualified_name != &func.name {
                builder.define_func_alias(&func.name, func_idx);
            }
        }

        // run function
        builder.define_func("run", "run");
        module.section(&builder.functions);

        // ========================================
        // Export section
        // ========================================
        builder.export_func("run", "run");
        module.section(&builder.exports);

        // Data count section
        let data_count = if string_data.is_empty() { 0 } else { 1 };
        module.section(&DataCountSection { count: data_count });

        // Code section
        let mut code = CodeSection::new();

        // User-defined functions from all modules
        for (_, func, _) in &all_funcs {
            let wasm_func = self.generate_user_function(func, &builder);
            code.function(&wasm_func);
        }

        // run function
        let run_func_ast = main_module.items.iter().find_map(|item| {
            if let Item::Function(f) = item
                && f.name == "run"
            {
                return Some(f);
            }
            None
        });

        let local_decls = if let Some(run_ast) = run_func_ast {
            if let Some(body) = &run_ast.body {
                let mut func_ctx = FunctionContext::new(run_ast.params.len() as u32);
                for param in &run_ast.params {
                    let param_type = self.wado_type_to_wasm_primitive(&param.ty);
                    func_ctx.add_param(&param.name, param_type);
                }
                self.collect_locals_from_block(body, &mut func_ctx);
                // Pre-allocate scratch locals for builtins (including float conversion)
                let string_array_type = builder.type_idx("string-array");
                self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
                func_ctx.get_local_decls()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let mut run_func = Function::new(local_decls);
        self.generate_run_body_instructions_p3_with_builder(&mut run_func, main_module, &builder);

        // Call task.return to complete the async task
        let task_return_idx = builder.func_idx("task-return");
        run_func.instruction(&Instruction::I32Const(0));
        run_func.instruction(&Instruction::Call(task_return_idx));
        run_func.instruction(&Instruction::End);
        code.function(&run_func);
        module.section(&code);

        // Data section
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.passive(string_data.iter().copied());
            module.section(&data);
        }

        // Name section (uses builder's tracked names)
        let names = builder.build_name_section();
        module.section(&names);

        module.finish()
    }

    /// Build main module for WASI P3 with write-via-stream
    fn build_main_module_p3(&self, ast_module: &AstModule, string_data: &[u8]) -> Vec<u8> {
        let mut module = Module::new();
        let mut builder = CoreModuleBuilder::new();

        // Collect user-defined functions
        let user_funcs = self.collect_user_functions(ast_module);

        // ========================================
        // Define types using the builder
        // ========================================

        // WASI function types
        builder.define_func_type("stream-new", &[], &[ValType::I64]);
        builder.define_func_type(
            "stream-write",
            &[ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("stream-drop-writable", &[ValType::I32], &[]);
        builder.define_func_type("stream-drop-readable", &[ValType::I32], &[]);
        builder.define_func_type(
            "write-via-stream",
            &[ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("task-return", &[ValType::I32], &[]);
        builder.define_func_type("waitable-set-new", &[], &[ValType::I32]);
        builder.define_func_type("waitable-join", &[ValType::I32, ValType::I32], &[]);
        builder.define_func_type(
            "waitable-set-wait",
            &[ValType::I32, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type("subtask-drop", &[ValType::I32], &[]);

        // realloc type
        builder.define_func_type(
            "realloc",
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        );

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        // IMPORTANT: This must be defined at this position to maintain type index 11
        // for string-array (hardcoded in several places for type inference)
        builder.define_gc_array_type("string-array", StorageType::I8, true);

        // Float-to-buffer types (after GC types to preserve type indices)
        builder.define_func_type(
            "f64_to_buffer",
            &[ValType::F64, ValType::I32],
            &[ValType::I32],
        );
        builder.define_func_type(
            "f32_to_buffer",
            &[ValType::F32, ValType::I32],
            &[ValType::I32],
        );

        // Types for user-defined functions
        for func in &user_funcs {
            // Convert Wado params to Wasm types
            let string_array_idx = builder.type_idx("string-array");
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            let return_types: Vec<ValType> = if let Some(ret_ty) = &func.return_type {
                vec![self.wado_type_to_wasm_with_idx(ret_ty, string_array_idx)]
            } else {
                vec![]
            };
            builder.define_func_type(&func.name, &param_types, &return_types);
        }

        // run type
        builder.define_func_type("run", &[], &[]);

        // Add types section to module
        module.section(&builder.types);

        // ========================================
        // Import section
        // ========================================
        builder.import_func("wasi", "stream-new", "stream-new");
        builder.import_func("wasi", "stream-write", "stream-write");
        builder.import_func("wasi", "stream-drop-writable", "stream-drop-writable");
        builder.import_func("wasi", "stream-drop-readable", "stream-drop-readable");
        builder.import_func("wasi", "write-via-stream", "write-via-stream");
        builder.import_func("wasi", "task-return", "task-return");
        builder.import_func("wasi", "waitable-set-new", "waitable-set-new");
        builder.import_func("wasi", "waitable-join", "waitable-join");
        builder.import_func("wasi", "waitable-set-wait", "waitable-set-wait");
        builder.import_func("wasi", "subtask-drop", "subtask-drop");
        builder.import_func("env", "realloc", "realloc");
        builder.import_func("env", "f64_to_buffer", "f64_to_buffer");
        builder.import_func("env", "f32_to_buffer", "f32_to_buffer");
        builder.import_memory("env", "memory", 1);
        module.section(&builder.imports);

        // ========================================
        // Function section
        // ========================================

        // Register user-defined functions
        for func in &user_funcs {
            builder.define_func(&func.name, &func.name);
        }

        // run function
        builder.define_func("run", "run");
        module.section(&builder.functions);

        // ========================================
        // Export section
        // ========================================
        builder.export_func("run", "run");
        module.section(&builder.exports);

        // Data count section (required for array.new_data with GC)
        // Count is 1 if we have string data, 0 otherwise
        let data_count = if string_data.is_empty() { 0 } else { 1 };
        module.section(&DataCountSection { count: data_count });

        // Code section
        let mut code = CodeSection::new();

        // User-defined functions
        for func in &user_funcs {
            let wasm_func = self.generate_user_function(func, &builder);
            code.function(&wasm_func);
        }

        // ============================================
        // run function - user entry point (WASI CLI Command world)
        // ============================================
        // Find run function to collect locals
        let run_func_ast = ast_module.items.iter().find_map(|item| {
            if let Item::Function(f) = item
                && f.name == "run"
            {
                return Some(f);
            }
            None
        });

        // Collect locals from run function body
        let local_decls = if let Some(run_ast) = run_func_ast {
            if let Some(body) = &run_ast.body {
                let mut func_ctx = FunctionContext::new(run_ast.params.len() as u32);
                for param in &run_ast.params {
                    let param_type = self.wado_type_to_wasm_primitive(&param.ty);
                    func_ctx.add_param(&param.name, param_type);
                }
                self.collect_locals_from_block(body, &mut func_ctx);
                // Pre-allocate scratch locals for builtins (including float conversion)
                let string_array_type = builder.type_idx("string-array");
                self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
                func_ctx.get_local_decls()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let mut run_func = Function::new(local_decls);

        // Generate body from AST (calls to println, etc.)
        self.generate_run_body_instructions_p3_with_builder(&mut run_func, ast_module, &builder);

        // Call task.return to complete the async task
        // For result unit with no payload, pass discriminant directly (0 = ok)
        let task_return_idx = builder.func_idx("task-return");
        run_func.instruction(&Instruction::I32Const(0)); // 0 = ok discriminant
        run_func.instruction(&Instruction::Call(task_return_idx));

        // No return value - task.return already provided the result
        run_func.instruction(&Instruction::End);
        code.function(&run_func);
        module.section(&code);

        // Data section: string literals for array.new_data
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            // Passive data segment (no memory target) - used by array.new_data
            data.passive(string_data.iter().copied());
            module.section(&data);
        }

        // Name section (for debugging - uses builder's tracked names)
        let names = builder.build_name_section();
        module.section(&names);

        module.finish()
    }

    /// Convert Wado type to Wasm ValType (primitive types only, for local collection)
    fn wado_type_to_wasm_primitive(&self, ty: &crate::ast::Type) -> ValType {
        match ty {
            crate::ast::Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" | "char" => ValType::I32,
                "i64" | "u64" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                // For complex types, use a ref type with hardcoded index 11 (string-array)
                "String" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                }),
                _ => ValType::I32,
            },
            crate::ast::Type::Generic(generic) => match generic.name.as_str() {
                "Array" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                }),
                _ => ValType::I32,
            },
            _ => ValType::I32,
        }
    }

    /// Get semantic type from AST type (for bool/char detection in templates)
    fn wado_type_to_semantic(&self, ty: &crate::ast::Type) -> SemanticType {
        match ty {
            crate::ast::Type::Named(named) => match named.name.as_str() {
                "bool" => SemanticType::Bool,
                "char" => SemanticType::Char,
                "i32" | "u32" => SemanticType::I32,
                _ => SemanticType::Other,
            },
            _ => SemanticType::Other,
        }
    }

    /// Infer semantic type from expression
    fn infer_semantic_type_from_expr(&self, expr: &Expr) -> SemanticType {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Bool(_) => SemanticType::Bool,
                Literal::Char(_) => SemanticType::Char,
                Literal::Int(_) => SemanticType::I32,
                _ => SemanticType::Other,
            },
            Expr::Binary(bin) => {
                // Comparison and logical operations produce bool
                match bin.op {
                    crate::ast::BinaryOp::Eq
                    | crate::ast::BinaryOp::NotEq
                    | crate::ast::BinaryOp::Lt
                    | crate::ast::BinaryOp::LtEq
                    | crate::ast::BinaryOp::Gt
                    | crate::ast::BinaryOp::GtEq
                    | crate::ast::BinaryOp::And
                    | crate::ast::BinaryOp::Or => SemanticType::Bool,
                    _ => SemanticType::I32,
                }
            }
            Expr::Unary(unary) => match unary.op {
                crate::ast::UnaryOp::Not => SemanticType::Bool,
                _ => SemanticType::I32,
            },
            _ => SemanticType::Other,
        }
    }

    fn wado_type_to_wasm_with_idx(&self, ty: &crate::ast::Type, string_array_idx: u32) -> ValType {
        match ty {
            crate::ast::Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" => ValType::I32,
                "i64" | "u64" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                "String" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(string_array_idx),
                }),
                _ => ValType::I32,
            },
            crate::ast::Type::Generic(generic) => match generic.name.as_str() {
                "Stream" => ValType::I32,
                "Array" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(string_array_idx),
                }),
                _ => ValType::I32,
            },
            _ => ValType::I32,
        }
    }

    /// Generate a user-defined function
    fn generate_user_function(
        &self,
        ast_func: &crate::ast::Function,
        builder: &CoreModuleBuilder,
    ) -> Function {
        // First pass: analyze function body to collect locals
        let mut func_ctx = FunctionContext::new(ast_func.params.len() as u32);

        // Set return type for ref.as_non_null handling in return statements
        if let Some(ret_ty) = &ast_func.return_type {
            let string_array_type = 11_u32; // Pre-known type index for string-array
            func_ctx.set_return_type(self.wado_type_to_wasm_with_idx(ret_ty, string_array_type));
        }

        // Add parameters to context
        for param in &ast_func.params {
            let param_type = self.wado_type_to_wasm_primitive(&param.ty);
            func_ctx.add_param(&param.name, param_type);
        }

        // Collect local variables from body
        if let Some(body) = &ast_func.body {
            self.collect_locals_from_block(body, &mut func_ctx);
        }

        // Pre-allocate scratch locals that builtins might need
        // These are needed by builtins that allocate locals at runtime
        let string_array_type = builder.type_idx("string-array");
        self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);

        // Create Wasm function with collected locals (including pre-allocated scratch locals)
        let local_decls = func_ctx.get_local_decls();
        let mut wasm_func = Function::new(local_decls);

        // Use the same context for code generation (so local indices match)
        let mut gen_ctx = func_ctx;

        // Generate function body
        if let Some(body) = &ast_func.body {
            for stmt in &body.stmts {
                self.generate_stmt_with_builder(&mut wasm_func, stmt, &mut gen_ctx, builder);
            }
        }

        // For functions with Stdout/Stderr effects, wait for pending async operations
        // This ensures write-via-stream completes before the function returns
        if ast_func
            .effects
            .iter()
            .any(|e| e == "Stdout" || e == "Stderr")
        {
            self.generate_effect_wait(&mut wasm_func, &gen_ctx, builder);
        }

        // For functions with return types, add unreachable to indicate that
        // falling through to the end is not a valid path - all returns must be explicit.
        // This satisfies Wasm validation when all control flow paths use explicit return statements.
        if ast_func.return_type.is_some() {
            wasm_func.instruction(&Instruction::Unreachable);
        }

        wasm_func.instruction(&Instruction::End);
        wasm_func
    }

    /// Generate write-via-stream call (START only, no wait)
    ///
    /// write-via-stream is an async operation that returns a subtask handle.
    /// This function only starts the operation; wait is generated separately
    /// at the end of the function via generate_effect_wait.
    fn generate_write_via_stream_start(
        &self,
        func: &mut Function,
        ctx: &mut FunctionContext,
        _builder: &CoreModuleBuilder,
        write_via_stream_idx: u32,
    ) {
        // Allocate local for subtask handle (reused by generate_effect_wait)
        let subtask_local = ctx.alloc_local("__subtask", ValType::I32);

        // Stack has: stream handle (rx)
        // Call write-via-stream(rx, outptr) - returns subtask handle
        func.instruction(&Instruction::I32Const(2048)); // outptr for result
        func.instruction(&Instruction::Call(write_via_stream_idx));
        func.instruction(&Instruction::LocalSet(subtask_local));

        // Push dummy value so all effect function calls uniformly produce a value
        // This gets dropped by the expression statement handler
        func.instruction(&Instruction::I32Const(0));
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

    /// Collect local variables from a block
    fn collect_locals_from_block(&self, block: &Block, ctx: &mut FunctionContext) {
        for stmt in &block.stmts {
            self.collect_locals_from_stmt(stmt, ctx);
        }
    }

    /// Collect local variables from a statement
    fn collect_locals_from_stmt(&self, stmt: &Stmt, ctx: &mut FunctionContext) {
        match stmt {
            Stmt::Let(let_stmt) => {
                // Use explicit type annotation if present, otherwise infer from value
                let val_type = if let Some(ty) = &let_stmt.ty {
                    self.wado_type_to_wasm_primitive(ty)
                } else {
                    self.infer_expr_type_with_ctx(&let_stmt.value, ctx)
                };
                ctx.alloc_local(&let_stmt.name, val_type);
                // Track semantic type for bool/char detection in template interpolation
                let semantic_type = if let Some(ty) = &let_stmt.ty {
                    self.wado_type_to_semantic(ty)
                } else {
                    self.infer_semantic_type_from_expr(&let_stmt.value)
                };
                ctx.set_semantic_type(&let_stmt.name, semantic_type);
            }
            Stmt::For(for_stmt) => {
                if let Some(init) = &for_stmt.init {
                    self.collect_locals_from_stmt(init, ctx);
                }
                self.collect_locals_from_block(&for_stmt.body, ctx);
            }
            Stmt::While(while_stmt) => {
                self.collect_locals_from_block(&while_stmt.body, ctx);
            }
            Stmt::If(if_stmt) => {
                self.collect_locals_from_block(&if_stmt.then_block, ctx);
                if let Some(else_block) = &if_stmt.else_block {
                    self.collect_locals_from_block(else_block, ctx);
                }
            }
            _ => {}
        }
    }

    /// Generate statement with builder context
    fn generate_stmt_with_builder(
        &self,
        func: &mut Function,
        stmt: &Stmt,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match stmt {
            Stmt::Expr(expr_stmt) => {
                self.generate_expr_with_builder(func, &expr_stmt.expr, ctx, builder);
                // Drop any value left on stack by expression statements
                // (e.g., assignment expressions use LocalTee)
                if self.expr_produces_value(&expr_stmt.expr, builder) {
                    func.instruction(&Instruction::Drop);
                }
            }
            Stmt::Let(let_stmt) => {
                // Use explicit type annotation if present, otherwise infer from value
                let val_type = if let Some(ty) = &let_stmt.ty {
                    let string_array_type = builder.type_idx("string-array");
                    self.wado_type_to_wasm_with_idx(ty, string_array_type)
                } else {
                    self.infer_expr_type_with_ctx(&let_stmt.value, ctx)
                };
                let local_idx = ctx.alloc_local(&let_stmt.name, val_type);
                self.generate_expr_with_builder(func, &let_stmt.value, ctx, builder);
                func.instruction(&Instruction::LocalSet(local_idx));
            }
            Stmt::Return(ret_stmt) => {
                if let Some(value) = &ret_stmt.value {
                    self.generate_expr_with_builder(func, value, ctx, builder);
                    // If the expression type is a nullable ref but return type expects non-nullable,
                    // we need to add ref.as_non_null
                    let expr_type = self.infer_expr_type_with_ctx(value, ctx);
                    if let (
                        ValType::Ref(RefType {
                            nullable: true,
                            heap_type,
                        }),
                        Some(ValType::Ref(RefType {
                            nullable: false, ..
                        })),
                    ) = (expr_type, ctx.return_type)
                    {
                        func.instruction(&Instruction::RefAsNonNull);
                        // Silence warning about unused heap_type
                        let _ = heap_type;
                    }
                }
                func.instruction(&Instruction::Return);
            }
            Stmt::For(for_stmt) => {
                // Generate init
                if let Some(init) = &for_stmt.init {
                    self.generate_stmt_with_builder(func, init, ctx, builder);
                }
                // block $break
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                // loop $continue
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
                // Check condition
                if let Some(condition) = &for_stmt.condition {
                    self.generate_expr_with_builder(func, condition, ctx, builder);
                    func.instruction(&Instruction::I32Eqz);
                    func.instruction(&Instruction::BrIf(1));
                }
                // Body
                for s in &for_stmt.body.stmts {
                    self.generate_stmt_with_builder(func, s, ctx, builder);
                }
                // Update
                if let Some(update) = &for_stmt.update {
                    self.generate_expr_with_builder(func, update, ctx, builder);
                    func.instruction(&Instruction::Drop);
                }
                func.instruction(&Instruction::Br(0));
                func.instruction(&Instruction::End);
                func.instruction(&Instruction::End);
            }
            Stmt::While(while_stmt) => {
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
                self.generate_expr_with_builder(func, &while_stmt.condition, ctx, builder);
                func.instruction(&Instruction::I32Eqz);
                func.instruction(&Instruction::BrIf(1));
                for s in &while_stmt.body.stmts {
                    self.generate_stmt_with_builder(func, s, ctx, builder);
                }
                func.instruction(&Instruction::Br(0));
                func.instruction(&Instruction::End);
                func.instruction(&Instruction::End);
            }
            Stmt::If(if_stmt) => {
                self.generate_expr_with_builder(func, &if_stmt.condition, ctx, builder);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                for s in &if_stmt.then_block.stmts {
                    self.generate_stmt_with_builder(func, s, ctx, builder);
                }
                if let Some(else_block) = &if_stmt.else_block {
                    func.instruction(&Instruction::Else);
                    for s in &else_block.stmts {
                        self.generate_stmt_with_builder(func, s, ctx, builder);
                    }
                }
                func.instruction(&Instruction::End);
            }
            Stmt::Assert(assert_stmt) => {
                // Generate: if (!condition) { unreachable(); }
                self.generate_expr_with_builder(func, &assert_stmt.condition, ctx, builder);
                func.instruction(&Instruction::I32Eqz);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Unreachable);
                func.instruction(&Instruction::End);
            }
        }
    }

    /// Generate expression with builder context
    fn generate_expr_with_builder(
        &self,
        func: &mut Function,
        expr: &Expr,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match expr {
            Expr::Call(call) => {
                self.generate_call_with_builder(func, call, ctx, builder);
            }
            Expr::Ident(ident) => {
                if let Some(local_idx) = ctx.get_local(&ident.name) {
                    func.instruction(&Instruction::LocalGet(local_idx));
                    // If the local is a nullable ref, convert to non-nullable
                    // This is needed when passing to functions that expect non-nullable refs
                    if let Some(ValType::Ref(RefType { nullable: true, .. })) =
                        ctx.get_local_type(&ident.name)
                    {
                        func.instruction(&Instruction::RefAsNonNull);
                    }
                }
            }
            Expr::Literal(lit) => {
                self.generate_literal_p3(func, &lit.value);
            }
            Expr::Binary(bin) => {
                self.generate_expr_with_builder(func, &bin.left, ctx, builder);
                self.generate_expr_with_builder(func, &bin.right, ctx, builder);

                // Infer operand type to select correct instructions
                let operand_type = self.infer_expr_type_with_ctx(&bin.left, ctx);

                match operand_type {
                    ValType::F64 => {
                        // Float operations (f64)
                        match bin.op {
                            crate::ast::BinaryOp::Add => func.instruction(&Instruction::F64Add),
                            crate::ast::BinaryOp::Sub => func.instruction(&Instruction::F64Sub),
                            crate::ast::BinaryOp::Mul => func.instruction(&Instruction::F64Mul),
                            crate::ast::BinaryOp::Div => func.instruction(&Instruction::F64Div),
                            crate::ast::BinaryOp::Eq => func.instruction(&Instruction::F64Eq),
                            crate::ast::BinaryOp::NotEq => func.instruction(&Instruction::F64Ne),
                            crate::ast::BinaryOp::Lt => func.instruction(&Instruction::F64Lt),
                            crate::ast::BinaryOp::LtEq => func.instruction(&Instruction::F64Le),
                            crate::ast::BinaryOp::Gt => func.instruction(&Instruction::F64Gt),
                            crate::ast::BinaryOp::GtEq => func.instruction(&Instruction::F64Ge),
                            // Mod, And, Or not supported for floats - fall back to i32
                            _ => func.instruction(&Instruction::I32Const(0)),
                        };
                    }
                    ValType::F32 => {
                        // Float operations (f32)
                        match bin.op {
                            crate::ast::BinaryOp::Add => func.instruction(&Instruction::F32Add),
                            crate::ast::BinaryOp::Sub => func.instruction(&Instruction::F32Sub),
                            crate::ast::BinaryOp::Mul => func.instruction(&Instruction::F32Mul),
                            crate::ast::BinaryOp::Div => func.instruction(&Instruction::F32Div),
                            crate::ast::BinaryOp::Eq => func.instruction(&Instruction::F32Eq),
                            crate::ast::BinaryOp::NotEq => func.instruction(&Instruction::F32Ne),
                            crate::ast::BinaryOp::Lt => func.instruction(&Instruction::F32Lt),
                            crate::ast::BinaryOp::LtEq => func.instruction(&Instruction::F32Le),
                            crate::ast::BinaryOp::Gt => func.instruction(&Instruction::F32Gt),
                            crate::ast::BinaryOp::GtEq => func.instruction(&Instruction::F32Ge),
                            // Mod, And, Or not supported for floats
                            _ => func.instruction(&Instruction::I32Const(0)),
                        };
                    }
                    ValType::I64 => {
                        // i64 operations
                        match bin.op {
                            crate::ast::BinaryOp::Add => func.instruction(&Instruction::I64Add),
                            crate::ast::BinaryOp::Sub => func.instruction(&Instruction::I64Sub),
                            crate::ast::BinaryOp::Mul => func.instruction(&Instruction::I64Mul),
                            crate::ast::BinaryOp::Div => func.instruction(&Instruction::I64DivS),
                            crate::ast::BinaryOp::Mod => func.instruction(&Instruction::I64RemS),
                            crate::ast::BinaryOp::Eq => func.instruction(&Instruction::I64Eq),
                            crate::ast::BinaryOp::NotEq => func.instruction(&Instruction::I64Ne),
                            crate::ast::BinaryOp::Lt => func.instruction(&Instruction::I64LtS),
                            crate::ast::BinaryOp::LtEq => func.instruction(&Instruction::I64LeS),
                            crate::ast::BinaryOp::Gt => func.instruction(&Instruction::I64GtS),
                            crate::ast::BinaryOp::GtEq => func.instruction(&Instruction::I64GeS),
                            crate::ast::BinaryOp::And => func.instruction(&Instruction::I64And),
                            crate::ast::BinaryOp::Or => func.instruction(&Instruction::I64Or),
                            crate::ast::BinaryOp::BitAnd => func.instruction(&Instruction::I64And),
                            crate::ast::BinaryOp::BitOr => func.instruction(&Instruction::I64Or),
                            crate::ast::BinaryOp::BitXor => func.instruction(&Instruction::I64Xor),
                            crate::ast::BinaryOp::Shl => func.instruction(&Instruction::I64Shl),
                            crate::ast::BinaryOp::Shr => func.instruction(&Instruction::I64ShrS),
                        };
                    }
                    _ => {
                        // Integer operations (i32) - default for i32, bool, char, etc.
                        match bin.op {
                            crate::ast::BinaryOp::Add => func.instruction(&Instruction::I32Add),
                            crate::ast::BinaryOp::Sub => func.instruction(&Instruction::I32Sub),
                            crate::ast::BinaryOp::Mul => func.instruction(&Instruction::I32Mul),
                            crate::ast::BinaryOp::Div => func.instruction(&Instruction::I32DivS),
                            crate::ast::BinaryOp::Mod => func.instruction(&Instruction::I32RemS),
                            crate::ast::BinaryOp::Eq => func.instruction(&Instruction::I32Eq),
                            crate::ast::BinaryOp::NotEq => func.instruction(&Instruction::I32Ne),
                            crate::ast::BinaryOp::Lt => func.instruction(&Instruction::I32LtS),
                            crate::ast::BinaryOp::LtEq => func.instruction(&Instruction::I32LeS),
                            crate::ast::BinaryOp::Gt => func.instruction(&Instruction::I32GtS),
                            crate::ast::BinaryOp::GtEq => func.instruction(&Instruction::I32GeS),
                            crate::ast::BinaryOp::And => func.instruction(&Instruction::I32And),
                            crate::ast::BinaryOp::Or => func.instruction(&Instruction::I32Or),
                            crate::ast::BinaryOp::BitAnd => func.instruction(&Instruction::I32And),
                            crate::ast::BinaryOp::BitOr => func.instruction(&Instruction::I32Or),
                            crate::ast::BinaryOp::BitXor => func.instruction(&Instruction::I32Xor),
                            crate::ast::BinaryOp::Shl => func.instruction(&Instruction::I32Shl),
                            crate::ast::BinaryOp::Shr => func.instruction(&Instruction::I32ShrS),
                        };
                    }
                }
            }
            Expr::Assign(assign) => {
                self.generate_expr_with_builder(func, &assign.value, ctx, builder);
                if let Expr::Ident(ident) = &assign.target
                    && let Some(local_idx) = ctx.get_local(&ident.name)
                {
                    func.instruction(&Instruction::LocalTee(local_idx));
                }
            }
            Expr::TemplateString(template) => {
                self.generate_template_string(func, template, ctx, builder);
            }
            Expr::Unary(un) => {
                self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                match un.op {
                    crate::ast::UnaryOp::Neg => {
                        // Negate: 0 - value
                        // First push 0, swap, then subtract
                        // But simpler: i32.const 0, local.get, i32.sub
                        // Actually we already have the value on stack, so we can do:
                        // i32.const -1, i32.mul or push 0, swap, sub
                        // Easiest: i32.const 0 before expr, then i32.sub
                        // But expr is already generated, so we need to handle differently
                        // For i32: we can use (0 - value) but value is on stack
                        // Let's use: i32.const -1, i32.mul
                        func.instruction(&Instruction::I32Const(-1));
                        func.instruction(&Instruction::I32Mul);
                    }
                    crate::ast::UnaryOp::Not => {
                        // Logical not: value == 0
                        func.instruction(&Instruction::I32Eqz);
                    }
                    crate::ast::UnaryOp::BitNot => {
                        // Bitwise not: value xor -1 (all bits set)
                        func.instruction(&Instruction::I32Const(-1));
                        func.instruction(&Instruction::I32Xor);
                    }
                    crate::ast::UnaryOp::Ref | crate::ast::UnaryOp::Deref => {
                        // References not yet implemented
                    }
                }
            }
            Expr::Cast(cast) => {
                self.generate_expr_with_builder(func, &cast.expr, ctx, builder);
                self.generate_type_cast(func, &cast.expr, &cast.target_type, ctx);
            }
            _ => {}
        }
    }

    /// Generate type cast instruction based on source and target types
    fn generate_type_cast(
        &self,
        func: &mut Function,
        source_expr: &Expr,
        target_type: &crate::ast::Type,
        ctx: &FunctionContext,
    ) {
        let source_wasm_type = self.infer_expr_type_with_ctx(source_expr, ctx);
        let target_type_name = self.get_primitive_type_name(target_type);

        match (source_wasm_type, target_type_name.as_str()) {
            // i32 -> other types
            (ValType::I32, "i64" | "u64") => {
                func.instruction(&Instruction::I64ExtendI32S);
            }
            (ValType::I32, "f32") => {
                func.instruction(&Instruction::F32ConvertI32S);
            }
            (ValType::I32, "f64") => {
                func.instruction(&Instruction::F64ConvertI32S);
            }

            // i64 -> other types
            (ValType::I64, "i32" | "u32") => {
                func.instruction(&Instruction::I32WrapI64);
            }
            (ValType::I64, "f32") => {
                func.instruction(&Instruction::F32ConvertI64S);
            }
            (ValType::I64, "f64") => {
                func.instruction(&Instruction::F64ConvertI64S);
            }

            // f32 -> other types
            (ValType::F32, "i32" | "u32") => {
                func.instruction(&Instruction::I32TruncF32S);
            }
            (ValType::F32, "i64" | "u64") => {
                func.instruction(&Instruction::I64TruncF32S);
            }
            (ValType::F32, "f64") => {
                func.instruction(&Instruction::F64PromoteF32);
            }

            // f64 -> other types
            (ValType::F64, "i32" | "u32") => {
                func.instruction(&Instruction::I32TruncF64S);
            }
            (ValType::F64, "i64" | "u64") => {
                func.instruction(&Instruction::I64TruncF64S);
            }
            (ValType::F64, "f32") => {
                func.instruction(&Instruction::F32DemoteF64);
            }

            // Same type or unsupported - no-op
            _ => {}
        }
    }

    /// Get primitive type name from a Wado Type
    fn get_primitive_type_name(&self, ty: &crate::ast::Type) -> String {
        match ty {
            crate::ast::Type::Named(named) => named.name.clone(),
            _ => String::new(),
        }
    }

    /// Generate call with builder context (for user-defined functions)
    fn generate_call_with_builder(
        &self,
        func: &mut Function,
        call: &CallExpr,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        if let Expr::Ident(ident) = &call.callee {
            // First try builtins
            if self.is_builtin(&ident.name) {
                self.generate_builtin_call_with_builder(func, &ident.name, call, ctx, builder);
                return;
            }

            // Check for effect function calls (Effect::method syntax)
            if let Some(wasi_func_name) = self.resolve_effect_function(&ident.name)
                && let Some(func_idx) = builder.try_func_idx(&wasi_func_name)
            {
                // Generate arguments
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                // For write-via-stream, we start the operation but defer wait to function end
                if wasi_func_name == "write-via-stream" {
                    self.generate_write_via_stream_start(func, ctx, builder, func_idx);
                } else {
                    func.instruction(&Instruction::Call(func_idx));
                }
                return;
            }

            // Then try user-defined functions
            if let Some(func_idx) = builder.try_func_idx(&ident.name) {
                // Generate arguments
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(func_idx));
            }
        }
    }

    /// Resolve an effect function call to its WASI import name
    ///
    /// Maps Wado effect function syntax (Effect::method) to WASI function names.
    /// The mapping is based on the `#[wasi(...)]` attributes in wasi/*.wado modules.
    fn resolve_effect_function(&self, name: &str) -> Option<String> {
        // Check if this is an effect function call (contains ::)
        if !name.contains("::") {
            return None;
        }

        // Map known effect functions to their WASI import names
        // These correspond to the #[wasi(...)] attributes in wasi/cli.wado
        match name {
            // wasi:cli/stdout
            "Stdout::write_via_stream" => Some("write-via-stream".to_string()),
            // wasi:cli/stderr - TODO: use separate stderr write function
            // For now, use the same write-via-stream (stderr goes to stdout)
            "Stderr::write_via_stream" => Some("write-via-stream".to_string()),
            // wasi:cli/stdin
            "Stdin::read_via_stream" => Some("stdin-read-via-stream".to_string()),
            // wasi:cli/environment
            "Environment::get_arguments" => Some("get-arguments".to_string()),
            "Environment::get_environment" => Some("get-environment".to_string()),
            "Environment::get_initial_cwd" => Some("get-initial-cwd".to_string()),
            // wasi:cli/exit
            "Exit::exit" => Some("exit".to_string()),
            "Exit::exit_with_code" => Some("exit-with-code".to_string()),
            _ => None,
        }
    }

    /// Check if a function name is a builtin
    fn is_builtin(&self, name: &str) -> bool {
        // All builtin:: namespace functions are builtins
        name.starts_with("builtin::")
    }

    /// Generate builtin:: namespace function calls
    fn generate_builtin_call_with_builder(
        &self,
        func: &mut Function,
        name: &str,
        call: &CallExpr,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let builtin_name = name.strip_prefix("builtin::").unwrap_or(name);

        match builtin_name {
            // Stream intrinsics
            "stream_new" => {
                func.instruction(&Instruction::Call(builder.func_idx("stream-new")));
            }
            "stream_write" => {
                // stream_write(tx: i32, ptr: i32, len: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("stream-write")));
            }
            "stream_drop_writable" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("stream-drop-writable")));
            }
            "stream_drop_readable" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("stream-drop-readable")));
            }

            // Async task intrinsics
            "waitable_set_new" => {
                func.instruction(&Instruction::Call(builder.func_idx("waitable-set-new")));
            }
            "waitable_join" => {
                // waitable_join(set: i32, subtask: i32)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("waitable-join")));
            }
            "waitable_set_wait" => {
                // waitable_set_wait(set: i32, outptr: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("waitable-set-wait")));
            }
            "subtask_drop" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("subtask-drop")));
            }

            // i64 bit manipulation
            "i64_low32" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I32WrapI64);
            }
            "i64_high32" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I64Const(32));
                func.instruction(&Instruction::I64ShrU);
                func.instruction(&Instruction::I32WrapI64);
            }

            // i32 operations
            "i32_and" => {
                // i32_and(a: i32, b: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I32And);
            }
            "i32_eqz" => {
                // i32_eqz(a: i32) -> i32 (0 or 1)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I32Eqz);
            }

            // GC array operations
            "array_len" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::ArrayLen);
            }
            "array_get_u8" => {
                // array_get_u8(arr: Array<u8>, idx: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                let string_array_type = builder.type_idx("string-array");
                func.instruction(&Instruction::ArrayGetU(string_array_type));
            }
            "array_set_u8" => {
                // array_set_u8(arr: Array<u8>, idx: i32, value: i32)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                let string_array_type = builder.type_idx("string-array");
                func.instruction(&Instruction::ArraySet(string_array_type));
            }
            "string_new" => {
                // string_new(len: i32) -> String
                // Creates a new String (GC array<u8>) with given length, initialized to zeros
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                let string_array_type = builder.type_idx("string-array");
                func.instruction(&Instruction::ArrayNewDefault(string_array_type));
            }

            // Linear memory operations
            "memory_store8" => {
                // memory_store8(addr: i32, value: i32)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I32Store8(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "memory_load8_u" => {
                // memory_load8_u(addr: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::I32Load8U(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "realloc" => {
                // realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("realloc")));
            }

            // Float-to-string conversion
            "f64_to_buffer" => {
                // f64_to_buffer(value: f64, buffer_ptr: i32) -> i32 (length)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("f64_to_buffer")));
            }
            "f32_to_buffer" => {
                // f32_to_buffer(value: f32, buffer_ptr: i32) -> i32 (length)
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("f32_to_buffer")));
            }

            _ => {
                // Unknown builtin - generate error at runtime
                func.instruction(&Instruction::Unreachable);
            }
        }
    }

    /// Generate run body with builder context
    fn generate_run_body_instructions_p3_with_builder(
        &self,
        func: &mut Function,
        ast_module: &crate::ast::Module,
        builder: &CoreModuleBuilder,
    ) {
        // Find run function (WASI CLI Command world entry point)
        let run_func_ast = ast_module.items.iter().find_map(|item| {
            if let Item::Function(f) = item
                && f.name == "run"
            {
                return Some(f);
            }
            None
        });

        if let Some(run_ast) = run_func_ast
            && let Some(body) = &run_ast.body
        {
            // Create context and populate it the same way as local collection phase
            let mut ctx = FunctionContext::new(run_ast.params.len() as u32);
            for param in &run_ast.params {
                let param_type = self.wado_type_to_wasm_primitive(&param.ty);
                ctx.add_param(&param.name, param_type);
            }
            // Collect locals from body (must match the local collection phase)
            self.collect_locals_from_block(body, &mut ctx);
            // Pre-allocate scratch locals for builtins (must match the local collection phase)
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_builtin_scratch_locals(&mut ctx, string_array_type);

            for stmt in &body.stmts {
                self.generate_stmt_with_builder(func, stmt, &mut ctx, builder);
            }
        }
    }

    /// Infer expression type with function context (for looking up variable types)
    fn infer_expr_type_with_ctx(&self, expr: &Expr, ctx: &FunctionContext) -> ValType {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Int(_) => ValType::I32,
                Literal::Float(_) => ValType::F64,
                Literal::Bool(_) => ValType::I32,
                Literal::Char(_) => ValType::I32, // Unicode code point as i32
                Literal::String(_) => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                }),
                Literal::Null => ValType::I32, // TODO: proper Option type
                Literal::Unit => ValType::I32,
            },
            Expr::Ident(ident) => {
                // Look up variable type from context
                if let Some(ty) = ctx.get_local_type(&ident.name) {
                    ty
                } else {
                    ValType::I32 // Default
                }
            }
            Expr::Binary(bin) => {
                // For comparison ops, result is always i32
                match bin.op {
                    crate::ast::BinaryOp::Eq
                    | crate::ast::BinaryOp::NotEq
                    | crate::ast::BinaryOp::Lt
                    | crate::ast::BinaryOp::LtEq
                    | crate::ast::BinaryOp::Gt
                    | crate::ast::BinaryOp::GtEq
                    | crate::ast::BinaryOp::And
                    | crate::ast::BinaryOp::Or
                    | crate::ast::BinaryOp::BitAnd
                    | crate::ast::BinaryOp::BitOr
                    | crate::ast::BinaryOp::BitXor
                    | crate::ast::BinaryOp::Shl
                    | crate::ast::BinaryOp::Shr => ValType::I32,
                    // Arithmetic ops return the operand type
                    _ => self.infer_expr_type_with_ctx(&bin.left, ctx),
                }
            }
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    match ident.name.as_str() {
                        "builtin::stream_new" => ValType::I64,
                        "builtin::string_new" => {
                            // string_new returns String (ref array<u8>)
                            ValType::Ref(RefType {
                                nullable: false,
                                heap_type: HeapType::Concrete(11), // string-array type index
                            })
                        }
                        "builtin::i64_low32"
                        | "builtin::i64_high32"
                        | "builtin::array_len"
                        | "builtin::array_get_u8"
                        | "builtin::i32_and"
                        | "builtin::i32_eqz"
                        | "builtin::stream_write"
                        | "builtin::waitable_set_new"
                        | "builtin::waitable_set_wait"
                        | "builtin::memory_load8_u"
                        | "builtin::realloc"
                        | "builtin::f64_to_buffer"
                        | "builtin::f32_to_buffer" => ValType::I32,
                        _ => ValType::I32,
                    }
                } else {
                    ValType::I32
                }
            }
            Expr::TemplateString(_) => {
                // Template strings return a GC array<u8> (same as string literals)
                ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                })
            }
            Expr::Cast(cast) => {
                // Cast expressions return the target type
                let target_name = self.get_primitive_type_name(&cast.target_type);
                match target_name.as_str() {
                    "i32" | "u32" | "bool" => ValType::I32,
                    "i64" | "u64" => ValType::I64,
                    "f32" => ValType::F32,
                    "f64" => ValType::F64,
                    _ => ValType::I32,
                }
            }
            _ => ValType::I32,
        }
    }

    /// Infer the semantic type of an expression for template string interpolation
    /// This is used to distinguish bool/char from i32 at the AST level
    fn infer_semantic_type(&self, expr: &Expr, ctx: &FunctionContext) -> SemanticType {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Bool(_) => SemanticType::Bool,
                Literal::Char(_) => SemanticType::Char,
                Literal::Int(_) => SemanticType::I32,
                _ => SemanticType::Other,
            },
            Expr::Ident(ident) => {
                // Look up variable's semantic type if tracked
                if let Some(semantic) = ctx.get_semantic_type(&ident.name) {
                    return semantic;
                }
                // Fall back to checking Wasm type
                if let Some(ty) = ctx.get_local_type(&ident.name) {
                    // If it's not I32, it's definitely not a bool/char
                    if ty != ValType::I32 {
                        return SemanticType::Other;
                    }
                }
                // Default to I32 for identifiers (conservative choice)
                SemanticType::I32
            }
            Expr::Binary(bin) => {
                // Comparison and logical operations produce bool
                match bin.op {
                    crate::ast::BinaryOp::Eq
                    | crate::ast::BinaryOp::NotEq
                    | crate::ast::BinaryOp::Lt
                    | crate::ast::BinaryOp::LtEq
                    | crate::ast::BinaryOp::Gt
                    | crate::ast::BinaryOp::GtEq
                    | crate::ast::BinaryOp::And
                    | crate::ast::BinaryOp::Or => SemanticType::Bool,
                    // Arithmetic ops on i32 produce i32
                    _ => SemanticType::I32,
                }
            }
            Expr::Unary(unary) => {
                // Logical NOT produces bool
                match unary.op {
                    crate::ast::UnaryOp::Not => SemanticType::Bool,
                    _ => SemanticType::I32,
                }
            }
            _ => SemanticType::I32,
        }
    }

    /// Check if an expression produces a value on the stack
    /// Used to determine if we need to drop the result in expression statements
    fn expr_produces_value(&self, expr: &Expr, builder: &CoreModuleBuilder) -> bool {
        match expr {
            // Literals always produce values
            Expr::Literal(_) => true,
            // Identifiers produce values
            Expr::Ident(_) => true,
            // Binary ops produce values
            Expr::Binary(_) => true,
            // Unary ops produce values
            Expr::Unary(_) => true,
            // Assignments produce values (via LocalTee)
            Expr::Assign(_) => true,
            // Calls produce values based on their return type
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    // Handle builtin:: namespace - check if builtin returns a value
                    if ident.name.starts_with("builtin::") {
                        return self.builtin_produces_value(&ident.name);
                    }
                    // Effect function calls always produce values
                    // (either subtask handles or dummy values from special handling)
                    if ident.name.contains("::") {
                        return true;
                    }
                    // Look up function in builder to check return type
                    if builder.try_func_idx(&ident.name).is_some() {
                        return builder.func_has_return(&ident.name);
                    }
                }
                false
            }
            // Template strings always produce values (GC array<u8>)
            Expr::TemplateString(_) => true,
            // Cast expressions always produce values
            Expr::Cast(_) => true,
            _ => false,
        }
    }

    /// Check if a builtin:: function produces a value
    /// Void builtins return false, all others return true
    fn builtin_produces_value(&self, name: &str) -> bool {
        let builtin_name = name.strip_prefix("builtin::").unwrap_or(name);
        // These builtins are void (no return value)
        !matches!(
            builtin_name,
            "stream_drop_writable"
                | "stream_drop_readable"
                | "waitable_join"
                | "subtask_drop"
                | "memory_store8"
                | "array_set_u8"
        )
    }

    /// Generate literal value
    fn generate_literal_p3(&self, func: &mut Function, lit: &Literal) {
        match lit {
            Literal::Int(n) => {
                func.instruction(&Instruction::I32Const(*n as i32));
            }
            Literal::Float(f) => {
                func.instruction(&Instruction::F64Const((*f).into()));
            }
            Literal::Bool(b) => {
                func.instruction(&Instruction::I32Const(if *b { 1 } else { 0 }));
            }
            Literal::String(s) => {
                // Create GC array from data segment
                // ArrayNewData { type_index, data_index } pops (offset, length) from stack
                let offset = self.get_string_offset(s);
                let len = s.len();
                func.instruction(&Instruction::I32Const(offset as i32)); // offset in data segment
                func.instruction(&Instruction::I32Const(len as i32)); // length
                func.instruction(&Instruction::ArrayNewData {
                    array_type_index: 11, // GC array<u8> type
                    array_data_index: 0,  // Data segment 0
                });
            }
            Literal::Char(c) => {
                // Char is a Unicode code point, represented as i32
                func.instruction(&Instruction::I32Const(*c as i32));
            }
            Literal::Null => {
                // Null is equivalent to None, represented as i32 0 for now
                // TODO: proper Option type representation
                func.instruction(&Instruction::I32Const(0));
            }
            Literal::Unit => {
                // Unit type - represented as i32 0
                func.instruction(&Instruction::I32Const(0));
            }
        }
    }

    /// Generate code for template string expression
    /// Template strings are interpolated strings like `Hello, {name}!`
    fn generate_template_string(
        &self,
        func: &mut Function,
        template: &crate::ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // For now, implement a simple concatenation without format specifiers
        // TODO: Implement proper format specifiers (.2f, etc.)
        // TODO: Integrate tagged template string literals (docs/adr-2026-01-10-tagged-template-literals.md)

        let string_array_type = builder.type_idx("string-array");

        if template.parts.is_empty() {
            // Empty template string -> empty string
            // Use ArrayNewFixed with 0 elements to avoid requiring a data segment
            func.instruction(&Instruction::ArrayNewFixed {
                array_type_index: string_array_type,
                array_size: 0,
            });
            return;
        }

        // Strategy: Use string_concat from core:internals for concatenation
        // 1. Generate first part
        // 2. For each subsequent part, call string_concat(result, part)

        // Use pre-allocated scratch local for template string accumulation
        let result_local = ctx.alloc_local(
            "__template_result",
            ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );

        // Start with first part
        if let Some(first_part) = template.parts.first() {
            self.generate_template_part(func, first_part, ctx, builder);
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // Concatenate remaining parts using string_concat from core:internals
        for part in template.parts.iter().skip(1) {
            // Push result (first argument to string_concat)
            // Convert nullable ref to non-nullable since string_concat expects non-nullable
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);

            // Generate the next part (second argument to string_concat)
            self.generate_template_part(func, part, ctx, builder);

            // Call string_concat(result, part) -> new result
            // The function is registered as "string_concat" (alias) or "core::internals::string_concat" (qualified)
            func.instruction(&Instruction::Call(builder.func_idx("string_concat")));

            // Store the result for next iteration
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // Load final result, converting nullable ref to non-nullable
        func.instruction(&Instruction::LocalGet(result_local));
        func.instruction(&Instruction::RefAsNonNull);
    }

    /// Generate code for a single template part (string or interpolation)
    fn generate_template_part(
        &self,
        func: &mut Function,
        part: &TemplatePart,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let string_array_type = builder.type_idx("string-array");

        match part {
            TemplatePart::String(s) => {
                // Generate string literal
                let offset = self.get_string_offset(s);
                let len = s.len();
                func.instruction(&Instruction::I32Const(offset as i32));
                func.instruction(&Instruction::I32Const(len as i32));
                func.instruction(&Instruction::ArrayNewData {
                    array_type_index: string_array_type,
                    array_data_index: 0,
                });
            }
            TemplatePart::Interpolation { expr, format: _ } => {
                // TODO: Handle format specifiers
                // Determine the expression type to decide on conversion
                let expr_type = self.infer_expr_type_with_ctx(expr, ctx);

                match expr_type {
                    ValType::F64 => {
                        // Float-to-string conversion using core:internals::f64_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(builder.func_idx("f64_to_string")));
                    }
                    ValType::F32 => {
                        // Float-to-string conversion using core:internals::f32_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(builder.func_idx("f32_to_string")));
                    }
                    ValType::I64 => {
                        // i64-to-string conversion using core:internals::i64_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(builder.func_idx("i64_to_string")));
                    }
                    ValType::I32 => {
                        // For i32, we need to check if it's actually a bool or char literal
                        let semantic_type = self.infer_semantic_type(expr, ctx);
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        match semantic_type {
                            SemanticType::Bool => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("bool_to_string"),
                                ));
                            }
                            SemanticType::Char => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("char_to_string"),
                                ));
                            }
                            SemanticType::I32 | SemanticType::Other => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("i32_to_string"),
                                ));
                            }
                        }
                    }
                    ValType::Ref(_) => {
                        // Assume it's already a string (ref (array u8))
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                    }
                    ValType::V128 => {
                        // V128 is not supported in template strings - treat as i32
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(builder.func_idx("i32_to_string")));
                    }
                }
            }
        }
    }

    fn build_memory_module(&self, string_data: &[u8]) -> Vec<u8> {
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

        // Name section (for debugging)
        let mut names = NameSection::new();
        let mut func_names = NameMap::new();
        func_names.append(0, "realloc");
        names.functions(&func_names);
        module.section(&names);

        module.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Analyzer;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::symbol::SymbolTable;

    fn parse(source: &str) -> crate::ast::Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parser error")
    }

    fn analyze(module: &crate::ast::Module) -> SymbolTable {
        let mut analyzer = Analyzer::new();
        analyzer.analyze(module, &[]).expect("analysis error");
        analyzer.into_symbols()
    }

    #[test]
    fn test_collect_strings() {
        let ast = parse(
            r#"
            use {println, Stdout} from "core:cli";

            fn run() with Stdout {
                println("Hello, world!");
            }
        "#,
        );

        let _symbols = analyze(&ast);
        let mut codegen = Codegen::new();
        codegen.collect_strings(&ast);

        // String literals are collected as-is
        assert_eq!(codegen.string_literals.len(), 1);
        assert_eq!(codegen.string_literals[0], "Hello, world!");
    }

    #[test]
    fn test_generate_binary() {
        let ast = parse(
            r#"
            use {println, Stdout} from "core:cli";

            fn run() with Stdout {
                println("Hello!");
            }
        "#,
        );

        let _symbols = analyze(&ast);
        let mut codegen = Codegen::new();
        let wasm = codegen.generate_wasm(&ast);

        // Verify it starts with Wasm magic number
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");

        // Validate the generated Wasm using wasmparser
        let mut validator =
            wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
        validator
            .validate_all(&wasm)
            .expect("Wasm validation failed");
    }

    #[test]
    fn test_generate_wat() {
        let ast = parse(
            r#"
            use {println, Stdout} from "core:cli";

            fn run() with Stdout {
                println("Hello!");
            }
        "#,
        );

        let _symbols = analyze(&ast);
        let mut codegen = Codegen::new();
        let wat = codegen.generate_wat(&ast);

        // Verify it produces valid WAT
        assert!(wat.contains("(component"));
    }
}
