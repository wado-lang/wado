// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use crate::ast::{Block, CallExpr, Expr, IdentExpr, Item, Literal, Module as AstModule, Stmt};
use crate::symbol::SymbolTable;
use std::collections::HashMap;
use wasm_encoder::{
    Alias, ArrayType, CanonicalOption, CodeSection, ComponentBuilder, ComponentExportKind,
    ComponentOuterAliasKind, ComponentValType, CompositeInnerType, CompositeType, ConstExpr,
    DataCountSection, DataSection, DataSegment, DataSegmentMode, EntityType, ExportKind,
    ExportSection, FieldType, Function, FunctionSection, HeapType, ImportSection, InstanceType,
    Instruction, MemorySection, MemoryType, Module, ModuleArg, NameMap, NameSection,
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
    /// Number of parameters (locals 0..param_count are parameters)
    #[allow(dead_code)]
    param_count: u32,
    /// Next available local index for new variables
    next_local: u32,
    /// Local types for non-parameter locals (for function declaration)
    local_types: Vec<ValType>,
}

impl FunctionContext {
    fn new(param_count: u32) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
        }
    }

    /// Add a parameter (must be called before any locals)
    fn add_param(&mut self, name: &str) {
        let index = self.locals.len() as u32;
        self.locals.insert(name.to_string(), index);
    }

    /// Allocate a new local variable
    fn alloc_local(&mut self, name: &str, ty: ValType) -> u32 {
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

    fn collect_strings(&mut self, module: &crate::ast::Module) {
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
                // Check if this is a println call - if so, append newline to string args
                let is_println = matches!(
                    &call.callee,
                    Expr::Ident(IdentExpr { name, .. }) if name == "println"
                );
                for arg in &call.args {
                    if is_println
                        && let Expr::Literal(lit) = arg
                        && let Literal::String(s) = &lit.value
                    {
                        // println appends newline - store the string with \n
                        let with_newline = format!("{s}\n");
                        if !self.string_literals.contains(&with_newline) {
                            self.string_literals.push(with_newline);
                        }
                        continue;
                    }
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
                use crate::ast::TemplatePart;
                for part in &template.parts {
                    match part {
                        TemplatePart::String(s) => {
                            if !self.string_literals.contains(s) {
                                self.string_literals.push(s.clone());
                            }
                        }
                        TemplatePart::Interpolation { expr, .. } => {
                            // Check if this is a float literal - format it with ryu and add to string literals
                            if let Expr::Literal(lit_expr) = &**expr
                                && let Literal::Float(f) = lit_expr.value
                            {
                                let mut buf = ryu::Buffer::new();
                                let formatted = buf.format(f).to_string();
                                if !self.string_literals.contains(&formatted) {
                                    self.string_literals.push(formatted);
                                }
                                continue;
                            }
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
    fn generate_component(&self, ast_module: &crate::ast::Module) -> Vec<u8> {
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
    fn collect_user_functions<'a>(
        &self,
        ast_module: &'a crate::ast::Module,
    ) -> Vec<&'a crate::ast::Function> {
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
    ) -> Vec<(Vec<String>, &'a crate::ast::Function, String)> {
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
            // Skip stdlib modules (they're handled specially)
            if module_path
                .first()
                .map(|s| s == "core" || s == "wasi")
                .unwrap_or(false)
            {
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

            let mut memory = MemorySection::new();
            memory.memory(MemoryType {
                minimum: 1,
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

            let mut code = CodeSection::new();
            let mut realloc_func = Function::new([(1, ValType::I32)]);
            realloc_func.instruction(&Instruction::I32Const(0));
            realloc_func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            realloc_func.instruction(&Instruction::LocalTee(4));
            realloc_func.instruction(&Instruction::LocalGet(3));
            realloc_func.instruction(&Instruction::I32Add);
            realloc_func.instruction(&Instruction::I32Const(0));
            realloc_func.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            realloc_func.instruction(&Instruction::LocalGet(4));
            realloc_func.instruction(&Instruction::End);
            code.function(&realloc_func);
            memory_module.section(&code);

            let mut data = DataSection::new();
            let init_ptr: [u8; 4] = 1024i32.to_le_bytes();
            data.segment(DataSegment {
                mode: DataSegmentMode::Active {
                    memory_index: 0,
                    offset: &ConstExpr::i32_const(0),
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

        // GC string array type (array<u8>)
        builder.define_gc_array_type("string-array", StorageType::I8, false);

        // println type - takes a ref to string array
        let string_ref = builder.string_ref_type();
        builder.define_func_type("println", &[ValType::Ref(string_ref)], &[]);

        // Types for user-defined functions
        let string_array_idx = builder.type_idx("string-array");
        for (_, func, qualified_name) in &all_funcs {
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            let return_types: Vec<ValType> = if func.return_type.is_some() {
                vec![ValType::I32]
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
        builder.import_memory("env", "memory", 1);
        module.section(&builder.imports);

        // ========================================
        // Function section
        // ========================================
        builder.define_func("println", "println");

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

        // println function
        let mut println_func =
            Function::new([(3, ValType::I32), (1, ValType::I64), (3, ValType::I32)]);
        self.generate_println_body(&mut println_func, &builder);
        println_func.instruction(&Instruction::End);
        code.function(&println_func);

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
                    func_ctx.add_param(&param.name);
                }
                self.collect_locals_from_block(body, &mut func_ctx);
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
    fn build_main_module_p3(&self, ast_module: &crate::ast::Module, string_data: &[u8]) -> Vec<u8> {
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

        // GC string array type (array<u8>)
        builder.define_gc_array_type("string-array", StorageType::I8, false);

        // println type - takes a ref to string array
        let string_ref = builder.string_ref_type();
        builder.define_func_type("println", &[ValType::Ref(string_ref)], &[]);

        // Types for user-defined functions
        for func in &user_funcs {
            // Convert Wado params to Wasm types
            let string_array_idx = builder.type_idx("string-array");
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            // Return type (default to i32 if specified, empty if none)
            let return_types: Vec<ValType> = if func.return_type.is_some() {
                vec![ValType::I32] // Simplified: all returns are i32 for now
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
        builder.import_memory("env", "memory", 1);
        module.section(&builder.imports);

        // ========================================
        // Function section
        // ========================================
        builder.define_func("println", "println");

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

        // ============================================
        // println function (index 11) - library function from core::cli
        // Parameters: str_arr (param 0: ref array<u8>)
        // Locals:
        //   local 1: len (i32) - string length
        //   local 2: ptr (i32) - allocated buffer pointer
        //   local 3: i (i32) - loop counter
        //   local 4: ret64 (i64) - stream.new result
        //   local 5: rx (i32) - readable stream handle
        //   local 6: tx (i32) - writable stream handle
        //   local 7: status (i32) - subtask status
        // ============================================
        let mut println_func = Function::new([
            (3, ValType::I32), // len, ptr, i
            (1, ValType::I64), // ret64
            (3, ValType::I32), // rx, tx, status
        ]);
        self.generate_println_body(&mut println_func, &builder);
        println_func.instruction(&Instruction::End);
        code.function(&println_func);

        // ============================================
        // User-defined functions
        // ============================================
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
                    func_ctx.add_param(&param.name);
                }
                self.collect_locals_from_block(body, &mut func_ctx);
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

    /// Convert Wado type to Wasm ValType
    #[allow(dead_code)] // Being replaced by wado_type_to_wasm_with_idx
    fn wado_type_to_wasm(&self, ty: &crate::ast::Type) -> ValType {
        match ty {
            crate::ast::Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" => ValType::I32,
                "i64" | "u64" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                // String is a struct wrapping builtin::array<u8> (type index 11)
                "String" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                }),
                _ => ValType::I32, // Default to i32
            },
            crate::ast::Type::Generic(generic) => match generic.name.as_str() {
                "Stream" => ValType::I32, // Stream handle
                // Array<T> is a GC array (type index 11 for Array<u8>)
                "Array" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(11),
                }),
                _ => ValType::I32,
            },
            _ => ValType::I32,
        }
    }

    /// Convert Wado type to Wasm ValType with explicit string array type index
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

        // Add parameters to context
        for param in &ast_func.params {
            func_ctx.add_param(&param.name);
        }

        // Collect local variables from body
        if let Some(body) = &ast_func.body {
            self.collect_locals_from_block(body, &mut func_ctx);
        }

        // Create Wasm function with collected locals
        let local_decls = func_ctx.get_local_decls();
        let mut wasm_func = Function::new(local_decls);

        // Reset context for code generation (keep param mappings, reset locals)
        let mut gen_ctx = FunctionContext::new(ast_func.params.len() as u32);
        for param in &ast_func.params {
            gen_ctx.add_param(&param.name);
        }

        // Generate function body
        if let Some(body) = &ast_func.body {
            for stmt in &body.stmts {
                self.generate_stmt_with_builder(&mut wasm_func, stmt, &mut gen_ctx, builder);
            }
        }

        wasm_func.instruction(&Instruction::End);
        wasm_func
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
                let val_type = self.infer_expr_type_with_ctx(&let_stmt.value, ctx);
                ctx.alloc_local(&let_stmt.name, val_type);
                // Also collect locals from the value expression (for template strings, etc.)
                self.collect_locals_from_expr(&let_stmt.value, ctx);
            }
            Stmt::Expr(expr_stmt) => {
                // Collect locals from expression statements (e.g., println with template strings)
                self.collect_locals_from_expr(&expr_stmt.expr, ctx);
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

    /// Collect local variables from an expression (for template strings, etc.)
    fn collect_locals_from_expr(&self, expr: &Expr, ctx: &mut FunctionContext) {
        use crate::ast::{Expr, TemplatePart};

        match expr {
            Expr::TemplateString(template) => {
                // Template strings with multiple parts need a local variable
                if template.parts.len() > 1 {
                    let string_array_heap_type = 11; // This is a placeholder - we don't have access to builder here
                    // For now, we'll use a simple heuristic: allocate a ref type local
                    let temp_name = format!("__template_result_{}", ctx.next_local);
                    ctx.alloc_local(
                        &temp_name,
                        ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(string_array_heap_type),
                        }),
                    );
                }
                // Recursively collect from interpolated expressions
                for part in &template.parts {
                    if let TemplatePart::Interpolation { expr, .. } = part {
                        self.collect_locals_from_expr(expr, ctx);
                    }
                }
            }
            Expr::Call(call) => {
                // Collect from callee and arguments
                self.collect_locals_from_expr(&call.callee, ctx);
                for arg in &call.args {
                    self.collect_locals_from_expr(arg, ctx);
                }
            }
            // Add other expression types as needed
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
                if self.expr_produces_value(&expr_stmt.expr) {
                    func.instruction(&Instruction::Drop);
                }
            }
            Stmt::Let(let_stmt) => {
                let val_type = self.infer_expr_type_with_ctx(&let_stmt.value, ctx);
                let local_idx = ctx.alloc_local(&let_stmt.name, val_type);
                self.generate_expr_with_builder(func, &let_stmt.value, ctx, builder);
                func.instruction(&Instruction::LocalSet(local_idx));
            }
            Stmt::Return(ret_stmt) => {
                if let Some(value) = &ret_stmt.value {
                    self.generate_expr_with_builder(func, value, ctx, builder);
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
                let is_float = matches!(operand_type, ValType::F32 | ValType::F64);

                if is_float {
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
                } else {
                    // Integer operations (i32)
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
                        crate::ast::BinaryOp::LShift => func.instruction(&Instruction::I32Shl),
                        crate::ast::BinaryOp::RShift => func.instruction(&Instruction::I32ShrS),
                    };
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
                // Infer operand type to select correct instructions
                let operand_type = self.infer_expr_type_with_ctx(&un.expr, ctx);
                let is_float = matches!(operand_type, ValType::F32 | ValType::F64);

                match un.op {
                    crate::ast::UnaryOp::Neg => {
                        if is_float {
                            self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                            func.instruction(&Instruction::F64Neg);
                        } else {
                            // For integers: -x = 0 - x
                            // Push 0 first, then x, then subtract
                            func.instruction(&Instruction::I32Const(0));
                            self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                            func.instruction(&Instruction::I32Sub);
                        }
                    }
                    crate::ast::UnaryOp::Not => {
                        self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                        // Logical NOT: convert to i32 (0 or 1), then XOR with 1
                        func.instruction(&Instruction::I32Eqz);
                    }
                    crate::ast::UnaryOp::BitNot => {
                        self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                        // Bitwise NOT: XOR with -1 (all bits set)
                        func.instruction(&Instruction::I32Const(-1));
                        func.instruction(&Instruction::I32Xor);
                    }
                    _ => {
                        self.generate_expr_with_builder(func, &un.expr, ctx, builder);
                        // Other unary operators (Ref, Deref) not yet implemented
                    }
                }
            }
            _ => {}
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

    /// Check if a function name is a builtin
    fn is_builtin(&self, name: &str) -> bool {
        matches!(
            name,
            "println"
                | "stream_new"
                | "stream_write"
                | "stream_write_string"
                | "stream_drop_writable"
                | "stream_drop_readable"
                | "i64_low32"
                | "i64_high32"
                | "array_len"
                | "string_ptr"
                | "string_len"
        )
    }

    /// Generate builtin call with builder context
    fn generate_builtin_call_with_builder(
        &self,
        func: &mut Function,
        name: &str,
        call: &CallExpr,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Get type/function indices from builder (no more hardcoded numbers)
        let string_array_type = builder.type_idx("string-array");

        match name {
            "println" => {
                // Takes a GC array<u8> reference
                if call.args.len() == 1 {
                    if let Expr::Literal(lit) = &call.args[0]
                        && let Literal::String(s) = &lit.value
                    {
                        // String literal with newline appended
                        let with_newline = format!("{s}\n");
                        let str_offset = self.get_string_offset(&with_newline);
                        let str_len = with_newline.len();
                        // Create GC array from data segment
                        func.instruction(&Instruction::I32Const(str_offset as i32));
                        func.instruction(&Instruction::I32Const(str_len as i32));
                        func.instruction(&Instruction::ArrayNewData {
                            array_type_index: string_array_type,
                            array_data_index: 0,
                        });
                    } else {
                        // Non-literal string - evaluate expression (produces GC array ref)
                        self.generate_expr_with_builder(func, &call.args[0], ctx, builder);
                    }
                    func.instruction(&Instruction::Call(builder.func_idx("println")));
                }
            }
            "stream_new" => {
                func.instruction(&Instruction::Call(builder.func_idx("stream-new")));
            }
            "stream_write" => {
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::Call(builder.func_idx("stream-write")));
            }
            "stream_write_string" => {
                // stream_write_string(tx: i32, data: String) -> i32
                //
                // GC string implementation:
                // 1. string is ref to GC array (string-array type)
                // 2. array.len to get string length
                // 3. Call realloc(0, 0, 1, len) to allocate buffer
                // 4. Loop: array.get_u + i32.store8 to copy bytes
                // 5. Call stream.write(tx, ptr, len)
                //
                // Uses 4 scratch locals: arr_ref (ref), len (i32), ptr (i32), i (i32)
                // These are allocated at fixed positions after function params/locals

                if call.args.len() == 2 {
                    // We need scratch locals for this operation
                    // Allocate them: arr_ref, len, ptr, i
                    let arr_ref_local = ctx.alloc_local(
                        "__arr_ref",
                        ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(string_array_type),
                        }),
                    );
                    let len_local = ctx.alloc_local("__len", ValType::I32);
                    let ptr_local = ctx.alloc_local("__ptr", ValType::I32);
                    let i_local = ctx.alloc_local("__i", ValType::I32);

                    // Evaluate tx and save for later
                    self.generate_expr_with_builder(func, &call.args[0], ctx, builder);
                    // tx is on stack

                    // Evaluate string argument - produces GC array ref
                    self.generate_expr_with_builder(func, &call.args[1], ctx, builder);
                    func.instruction(&Instruction::LocalSet(arr_ref_local));

                    // Get length: array.len
                    func.instruction(&Instruction::LocalGet(arr_ref_local));
                    func.instruction(&Instruction::ArrayLen);
                    func.instruction(&Instruction::LocalTee(len_local));

                    // Drop len from stack temporarily, save tx
                    func.instruction(&Instruction::Drop); // drop len (we have it in local)
                    // tx is on stack, save it
                    let tx_local = ctx.alloc_local("__tx", ValType::I32);
                    func.instruction(&Instruction::LocalSet(tx_local));

                    // Call realloc(0, 0, 1, len) to allocate buffer
                    func.instruction(&Instruction::I32Const(0)); // old_ptr
                    func.instruction(&Instruction::I32Const(0)); // old_size
                    func.instruction(&Instruction::I32Const(1)); // align
                    func.instruction(&Instruction::LocalGet(len_local)); // new_size
                    func.instruction(&Instruction::Call(builder.func_idx("realloc")));
                    func.instruction(&Instruction::LocalSet(ptr_local));

                    // Initialize loop counter: i = 0
                    func.instruction(&Instruction::I32Const(0));
                    func.instruction(&Instruction::LocalSet(i_local));

                    // Loop to copy bytes: while (i < len) { mem[ptr+i] = arr[i]; i++; }
                    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty)); // $break
                    func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty)); // $continue

                    // Check condition: i < len
                    func.instruction(&Instruction::LocalGet(i_local));
                    func.instruction(&Instruction::LocalGet(len_local));
                    func.instruction(&Instruction::I32GeU); // i >= len
                    func.instruction(&Instruction::BrIf(1)); // break if i >= len

                    // Store byte: mem[ptr + i] = arr[i]
                    func.instruction(&Instruction::LocalGet(ptr_local));
                    func.instruction(&Instruction::LocalGet(i_local));
                    func.instruction(&Instruction::I32Add); // ptr + i (address)
                    func.instruction(&Instruction::LocalGet(arr_ref_local));
                    func.instruction(&Instruction::LocalGet(i_local));
                    func.instruction(&Instruction::ArrayGetU(string_array_type)); // arr[i] (unsigned)
                    func.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
                        offset: 0,
                        align: 0,
                        memory_index: 0,
                    }));

                    // Increment: i++
                    func.instruction(&Instruction::LocalGet(i_local));
                    func.instruction(&Instruction::I32Const(1));
                    func.instruction(&Instruction::I32Add);
                    func.instruction(&Instruction::LocalSet(i_local));

                    // Continue loop
                    func.instruction(&Instruction::Br(0));
                    func.instruction(&Instruction::End); // end loop
                    func.instruction(&Instruction::End); // end block

                    // Call stream-write(tx, ptr, len)
                    func.instruction(&Instruction::LocalGet(tx_local));
                    func.instruction(&Instruction::LocalGet(ptr_local));
                    func.instruction(&Instruction::LocalGet(len_local));
                    func.instruction(&Instruction::Call(builder.func_idx("stream-write")));
                }
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
            "array_len" => {
                // array_len<T>(arr: Array<T>) -> i32
                // For GC arrays, uses Wasm GC array.len instruction
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                func.instruction(&Instruction::ArrayLen);
            }
            "string_ptr" => {
                // string_ptr(s: String) -> i32
                // Returns pointer to string data in linear memory
                //
                // Current: works with string literals (compile-time offset)
                // Future GC: will extract ptr from (ptr, len) pair or copy GC array
                if call.args.len() == 1 {
                    if let Expr::Literal(lit) = &call.args[0]
                        && let Literal::String(s) = &lit.value
                    {
                        let str_offset = self.get_string_offset(s);
                        func.instruction(&Instruction::I32Const(str_offset as i32));
                    } else {
                        // Non-literal - evaluate and assume first element is ptr
                        self.generate_expr_with_builder(func, &call.args[0], ctx, builder);
                        // TODO: for GC strings, copy to linear memory and return ptr
                    }
                }
            }
            "string_len" => {
                // string_len(s: String) -> i32
                // Returns length of string in bytes
                //
                // Current: works with string literals (compile-time length)
                // Future GC: will use array.len on GC array
                if call.args.len() == 1 {
                    if let Expr::Literal(lit) = &call.args[0]
                        && let Literal::String(s) = &lit.value
                    {
                        let str_len = s.len() as i32;
                        func.instruction(&Instruction::I32Const(str_len));
                    } else {
                        // Non-literal - for GC arrays use array.len
                        self.generate_expr_with_builder(func, &call.args[0], ctx, builder);
                        func.instruction(&Instruction::ArrayLen);
                    }
                }
            }
            _ => {}
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
            let mut ctx = FunctionContext::new(run_ast.params.len() as u32);
            for param in &run_ast.params {
                ctx.add_param(&param.name);
            }
            for stmt in &body.stmts {
                self.generate_stmt_with_builder(func, stmt, &mut ctx, builder);
            }
        }
    }

    /// Generate println function body
    /// Implements: pub fn println(message: String) from core::cli
    /// Parameter: str_arr (param 0: ref array<u8>)
    /// Locals:
    ///   local 1: len (i32) - string length
    ///   local 2: ptr (i32) - allocated buffer pointer
    ///   local 3: i (i32) - loop counter
    ///   local 4: ret64 (i64) - stream.new result
    ///   local 5: rx (i32) - readable stream handle
    ///   local 6: tx (i32) - writable stream handle
    ///   local 7: status (i32) - subtask status
    fn generate_println_body(&self, func: &mut Function, builder: &CoreModuleBuilder) {
        // Get indices from builder (no more hardcoded numbers)
        let string_array_type = builder.type_idx("string-array");
        let realloc_idx = builder.func_idx("realloc");
        let stream_new_idx = builder.func_idx("stream-new");
        let stream_write_idx = builder.func_idx("stream-write");
        let stream_drop_writable_idx = builder.func_idx("stream-drop-writable");
        let write_via_stream_idx = builder.func_idx("write-via-stream");
        let waitable_set_new_idx = builder.func_idx("waitable-set-new");
        let waitable_join_idx = builder.func_idx("waitable-join");
        let waitable_set_wait_idx = builder.func_idx("waitable-set-wait");
        let subtask_drop_idx = builder.func_idx("subtask-drop");

        // ============================================
        // Phase 1: Copy GC array to linear memory
        // ============================================

        // Get length from GC array: array.len
        func.instruction(&Instruction::LocalGet(0)); // str_arr
        func.instruction(&Instruction::ArrayLen);
        func.instruction(&Instruction::LocalSet(1)); // len

        // Allocate buffer: realloc(0, 0, 1, len) -> ptr
        func.instruction(&Instruction::I32Const(0)); // old_ptr
        func.instruction(&Instruction::I32Const(0)); // old_size
        func.instruction(&Instruction::I32Const(1)); // align
        func.instruction(&Instruction::LocalGet(1)); // new_size = len
        func.instruction(&Instruction::Call(realloc_idx));
        func.instruction(&Instruction::LocalSet(2)); // ptr

        // Initialize loop counter: i = 0
        func.instruction(&Instruction::I32Const(0));
        func.instruction(&Instruction::LocalSet(3)); // i = 0

        // Loop to copy bytes: while (i < len) { mem[ptr+i] = arr[i]; i++; }
        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty)); // $break
        func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty)); // $continue

        // Check condition: i >= len -> break
        func.instruction(&Instruction::LocalGet(3)); // i
        func.instruction(&Instruction::LocalGet(1)); // len
        func.instruction(&Instruction::I32GeU); // i >= len
        func.instruction(&Instruction::BrIf(1)); // break if i >= len

        // Store byte: mem[ptr + i] = arr[i]
        func.instruction(&Instruction::LocalGet(2)); // ptr
        func.instruction(&Instruction::LocalGet(3)); // i
        func.instruction(&Instruction::I32Add); // ptr + i (address)
        func.instruction(&Instruction::LocalGet(0)); // str_arr
        func.instruction(&Instruction::LocalGet(3)); // i
        func.instruction(&Instruction::ArrayGetU(string_array_type)); // arr[i] (unsigned)
        func.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // Increment: i++
        func.instruction(&Instruction::LocalGet(3)); // i
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::I32Add);
        func.instruction(&Instruction::LocalSet(3)); // i = i + 1

        // Continue loop
        func.instruction(&Instruction::Br(0));
        func.instruction(&Instruction::End); // end loop
        func.instruction(&Instruction::End); // end block

        // ============================================
        // Phase 2: Stream operations (ptr and len now in locals 2 and 1)
        // ============================================

        // 1. Create stream: stream-new() -> i64 (rx in low 32, tx in high 32)
        func.instruction(&Instruction::Call(stream_new_idx));
        func.instruction(&Instruction::LocalSet(4)); // ret64

        // Extract rx (low 32 bits)
        func.instruction(&Instruction::LocalGet(4)); // ret64
        func.instruction(&Instruction::I32WrapI64);
        func.instruction(&Instruction::LocalSet(5)); // rx

        // Extract tx (high 32 bits)
        func.instruction(&Instruction::LocalGet(4)); // ret64
        func.instruction(&Instruction::I64Const(32));
        func.instruction(&Instruction::I64ShrU);
        func.instruction(&Instruction::I32WrapI64);
        func.instruction(&Instruction::LocalSet(6)); // tx

        // 2. Call write-via-stream(rx, outptr) to start consuming from the stream
        func.instruction(&Instruction::LocalGet(5)); // rx
        func.instruction(&Instruction::I32Const(2048)); // outptr for result
        func.instruction(&Instruction::Call(write_via_stream_idx));
        func.instruction(&Instruction::LocalSet(7)); // status

        // 3. Write string to stream: stream-write(tx, ptr, len) -> status
        func.instruction(&Instruction::LocalGet(6)); // tx
        func.instruction(&Instruction::LocalGet(2)); // ptr
        func.instruction(&Instruction::LocalGet(1)); // len
        func.instruction(&Instruction::Call(stream_write_idx));
        func.instruction(&Instruction::Drop); // ignore write status

        // 4. Close writable end to signal EOF: stream-drop-writable(tx)
        func.instruction(&Instruction::LocalGet(6)); // tx
        func.instruction(&Instruction::Call(stream_drop_writable_idx));

        // 5. Wait for write-via-stream subtask to complete if still pending
        func.instruction(&Instruction::LocalGet(7)); // status
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::I32And);
        func.instruction(&Instruction::I32Eqz); // true if pending
        func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

        // Create waitable-set
        func.instruction(&Instruction::Call(waitable_set_new_idx));
        func.instruction(&Instruction::I64ExtendI32U);
        func.instruction(&Instruction::LocalSet(4)); // reuse ret64 for waitable-set

        // Join subtask to waitable-set
        func.instruction(&Instruction::LocalGet(4));
        func.instruction(&Instruction::I32WrapI64);
        func.instruction(&Instruction::LocalGet(7)); // status (subtask handle)
        func.instruction(&Instruction::Call(waitable_join_idx));

        // Wait for completion
        func.instruction(&Instruction::LocalGet(4));
        func.instruction(&Instruction::I32WrapI64);
        func.instruction(&Instruction::I32Const(2048));
        func.instruction(&Instruction::Call(waitable_set_wait_idx));
        func.instruction(&Instruction::Drop);

        // Drop subtask
        func.instruction(&Instruction::LocalGet(7)); // status (subtask handle)
        func.instruction(&Instruction::Call(subtask_drop_idx));

        func.instruction(&Instruction::End); // end if
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
                    | crate::ast::BinaryOp::Or => ValType::I32,
                    // Arithmetic ops return the operand type
                    _ => self.infer_expr_type_with_ctx(&bin.left, ctx),
                }
            }
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    match ident.name.as_str() {
                        "stream_new" => ValType::I64,
                        "i64_low32" | "i64_high32" | "string_len" | "array_len" => ValType::I32,
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
            _ => ValType::I32,
        }
    }

    /// Check if an expression produces a value on the stack
    /// Used to determine if we need to drop the result in expression statements
    fn expr_produces_value(&self, expr: &Expr) -> bool {
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
            // Calls may or may not produce values - for now assume they don't
            // (println returns unit which we don't push)
            Expr::Call(call) => {
                // Check if it's a builtin that returns void
                if let Expr::Ident(ident) = &call.callee {
                    // println doesn't produce a value
                    if ident.name == "println" {
                        return false;
                    }
                    // stream intrinsics that return values
                    if matches!(
                        ident.name.as_str(),
                        "stream_new"
                            | "stream_write"
                            | "stream_write_string"
                            | "i64_low32"
                            | "i64_high32"
                            | "array_len"
                            | "string_len"
                    ) {
                        return true;
                    }
                }
                false
            }
            // Template strings always produce values (GC array<u8>)
            Expr::TemplateString(_) => true,
            _ => false,
        }
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

        let string_array_type = builder.type_idx("string-array");

        if template.parts.is_empty() {
            // Empty template string -> empty string
            func.instruction(&Instruction::I32Const(0)); // offset
            func.instruction(&Instruction::I32Const(0)); // length
            func.instruction(&Instruction::ArrayNewData {
                array_type_index: string_array_type,
                array_data_index: 0,
            });
            return;
        }

        // Simple case: single part - no concatenation needed
        if template.parts.len() == 1 {
            self.generate_template_part(func, &template.parts[0], ctx, builder);
            return;
        }

        // TODO: Implement proper multi-part concatenation
        // For now, we'll just use the last part (incorrect but allows simple tests)
        // Strategy: Concatenate all parts into a single string
        // 1. Generate each part as a GC array (ref (array u8))
        // 2. Calculate total length
        // 3. Create new array with total length
        // 4. Copy each part into the new array

        // For simplicity, we'll use a helper approach:
        // - For each part, generate the string representation
        // - Use array operations to concatenate

        // Allocate a local to accumulate the result
        let temp_name = format!("__template_result_{}", ctx.next_local);
        let result_local = ctx.alloc_local(
            &temp_name,
            ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );

        // Start with empty array or first part
        if let Some(first_part) = template.parts.first() {
            self.generate_template_part(func, first_part, ctx, builder);
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // Concatenate remaining parts
        for part in template.parts.iter().skip(1) {
            // Generate the next part
            self.generate_template_part(func, part, ctx, builder);

            // Concatenate with result
            // For now, we'll use a simple approach: TODO implement proper concatenation
            // Stack: [next_part]
            // We need: result = concat(result, next_part)

            // TODO: Implement array concatenation
            // For now, just replace result with the new part (incorrect but allows compilation)
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // Load result
        func.instruction(&Instruction::LocalGet(result_local));
    }

    /// Generate code for a single template part (string or interpolation)
    fn generate_template_part(
        &self,
        func: &mut Function,
        part: &crate::ast::TemplatePart,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        use crate::ast::TemplatePart;

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

                // Check if this is a float literal - if so, use ryu for deterministic formatting
                if let Expr::Literal(lit_expr) = &**expr
                    && let Literal::Float(f) = lit_expr.value
                {
                    // Use ryu to format the float at compile time
                    let mut buf = ryu::Buffer::new();
                    let formatted = buf.format(f);

                    // Generate string literal for the formatted float
                    let offset = self.get_string_offset(formatted);
                    let len = formatted.len();
                    func.instruction(&Instruction::I32Const(offset as i32));
                    func.instruction(&Instruction::I32Const(len as i32));
                    func.instruction(&Instruction::ArrayNewData {
                        array_type_index: string_array_type,
                        array_data_index: 0,
                    });
                    return;
                }

                // For non-float expressions, generate the expression as-is
                self.generate_expr_with_builder(func, expr, ctx, builder);

                // TODO: Convert expression result to string based on its type
                // For now, assume it's already a string (ref (array u8))
                // This is incorrect for non-string types but allows compilation
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
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
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

        // println appends newline to string literals
        assert_eq!(codegen.string_literals.len(), 1);
        assert_eq!(codegen.string_literals[0], "Hello, world!\n");
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
