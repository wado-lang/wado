// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use crate::ast::{
    Block, CallExpr, Expr, Function as AstFunction, Item, Literal, Module as AstModule, Stmt,
    TemplatePart, Type,
};
use crate::builtin_registry::{BuiltinFunctionInfo, BuiltinRegistry};
use crate::bundled::wado_bundled_wasm;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::stdlib;
use crate::symbol::SymbolTable;
use crate::wasi_registry::{WasiFunctionInfo, WasiRegistry, build_local_alias_name};
use crate::wasm_postprocess;
use crate::world_registry::{WorldExportInfo, WorldRegistry};
use heck::ToKebabCase;
use std::collections::HashMap;
use wasm_encoder::{
    Alias, ArrayType, BranchHint, BranchHints, CanonicalOption, CodeSection, ComponentBuilder,
    ComponentExportKind, ComponentOuterAliasKind, ComponentValType, CompositeInnerType,
    CompositeType, ConstExpr, DataCountSection, DataSection, DataSegment, DataSegmentMode,
    EntityType, ExportKind, ExportSection, FieldType, Function, FunctionSection, HeapType,
    ImportSection, InstanceType, Instruction, MemArg, MemorySection, MemoryType, Module, ModuleArg,
    NameMap, NameSection, PrimitiveValType, RefType, StorageType, SubType, TypeBounds, TypeSection,
    ValType,
};
use wasmparser::{Validator, WasmFeatures};

/// Code generator that produces Component Model components
/// Targets WASI P3
pub struct Codegen {
    string_literals: Vec<String>,
    /// Source code for extracting span text (for power-assert messages)
    source_code: String,
    /// Registry of WASI imports from lib/wasi/*.wado
    wasi_registry: WasiRegistry,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    builtin_registry: BuiltinRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    world_registry: WorldRegistry,
    /// Type index for string-array (GC array<u8>), set when types are defined
    string_array_type_idx: u32,
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
            semantic_type_map: HashMap::new(),
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
            semantic_type_map: HashMap::new(),
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

    /// Take the collected branch hints (consumes them)
    fn take_branch_hints(&mut self) -> Vec<(u32, bool)> {
        std::mem::take(&mut self.branch_hints)
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
    type_return_type: HashMap<String, ValType>, // type_name -> return ValType
    next_func_idx: u32,
    /// Number of imported functions (for branch hint calculation)
    import_func_count: u32,

    // Memory tracking
    has_memory: bool,

    // Access control: functions from core:internal (require explicit import)
    internal_functions: std::collections::HashSet<String>,
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
            type_return_type: HashMap::new(),
            next_func_idx: 0,
            import_func_count: 0,
            has_memory: false,
            internal_functions: std::collections::HashSet::new(),
        }
    }

    /// Mark a function as being from core:internal (requires explicit import to call)
    fn mark_as_internal(&mut self, name: &str) {
        self.internal_functions.insert(name.to_string());
    }

    /// Check if a function is from core:internal
    fn is_internal_function(&self, name: &str) -> bool {
        self.internal_functions.contains(name)
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
        if let Some(ret_type) = results.first() {
            self.type_return_type.insert(name.to_string(), *ret_type);
        }
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
        self.import_func_count += 1;
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

    /// Get the return type of a function, returns None if function not found or has no return
    fn func_return_type(&self, name: &str) -> Option<ValType> {
        self.func_type_names
            .get(name)
            .and_then(|type_name| self.type_return_type.get(type_name))
            .copied()
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

    /// Check if a component-level function exists
    fn has_comp_func(&self, name: &str) -> bool {
        self.comp_func_names.contains_key(name)
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
        Self::new_with_source(String::new())
    }
}

/// Convert a snake_case identifier to kebab-case for Component Model
fn to_kebab_case(name: &str) -> String {
    name.to_kebab_case()
}

impl Codegen {
    /// Create a new code generator with source code for power-assert messages
    pub fn new_with_source(source_code: String) -> Self {
        Self {
            string_literals: Vec::new(),
            source_code,
            wasi_registry: WasiRegistry::new(),
            builtin_registry: BuiltinRegistry::new(),
            world_registry: WorldRegistry::new(),
            string_array_type_idx: 0, // Set when types are defined
        }
    }

    /// Get the source text for a span (for power-assert messages)
    fn get_source_text(&self, span: &crate::token::Span) -> String {
        if span.start < self.source_code.len() && span.end <= self.source_code.len() {
            self.source_code[span.start..span.end].to_string()
        } else {
            // Fallback for spans from other modules (stdlib)
            String::from("<unknown>")
        }
    }

    /// Extract interesting sub-expressions for power-assert display
    /// Returns pairs of (display_name, expression) for values to show
    fn extract_intermediate_values<'a>(&self, expr: &'a Expr) -> Vec<(String, &'a Expr)> {
        let mut values = Vec::new();
        self.collect_intermediate_values(expr, &mut values, true);
        values
    }

    /// Recursively collect intermediate values from an expression
    /// is_root: true for the top-level condition expression
    fn collect_intermediate_values<'a>(
        &self,
        expr: &'a Expr,
        values: &mut Vec<(String, &'a Expr)>,
        is_root: bool,
    ) {
        match expr {
            Expr::Binary(bin) => {
                // Recursively collect from operands
                self.collect_intermediate_values(&bin.left, values, false);
                self.collect_intermediate_values(&bin.right, values, false);

                // Add the binary expression itself if it's NOT the root comparison
                // (the root is shown as "condition: ..." so we don't need to show it again)
                if !is_root {
                    let source = self.get_source_text(&bin.span);
                    values.push((source, expr));
                }
            }
            Expr::Ident(ident) => {
                // Always show identifiers - they're the most useful values
                values.push((ident.name.clone(), expr));
            }
            Expr::Call(call) => {
                // Show function call results
                let source = self.get_source_text(&call.span);
                values.push((source, expr));
            }
            Expr::MethodCall(call) => {
                // Show method call results
                let source = self.get_source_text(&call.span);
                values.push((source, expr));
            }
            Expr::FieldAccess(access) => {
                // Show field access results
                let source = self.get_source_text(&access.span);
                values.push((source, expr));
            }
            Expr::Index(idx) => {
                // Show index access results
                let source = self.get_source_text(&idx.span);
                values.push((source, expr));
            }
            Expr::Unary(unary) => {
                // Recurse into the operand
                self.collect_intermediate_values(&unary.expr, values, false);
                // Also show the unary expression itself
                let source = self.get_source_text(&unary.span);
                values.push((source, expr));
            }
            Expr::Cast(cast) => {
                // Recurse into the expression being cast
                self.collect_intermediate_values(&cast.expr, values, false);
            }
            // Literals don't need to be shown - their value is obvious from source
            Expr::Literal(_) => {}
            // Skip complex expressions that don't make sense to show
            Expr::Block(_)
            | Expr::If(_)
            | Expr::Match(_)
            | Expr::Closure(_)
            | Expr::TemplateString(_)
            | Expr::Assign(_) => {}
        }
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

    /// Generate Component Model binary Wasm
    pub fn generate_wasm(&mut self, module: &AstModule) -> Vec<u8> {
        // Build WASI registry from stdlib (if not already built)
        if self.wasi_registry.is_empty() {
            self.build_wasi_registry_from_stdlib();
        }

        // Build builtin registry from stdlib (if not already built)
        if self.builtin_registry.is_empty() {
            self.build_builtin_registry_from_stdlib();
        }

        // First pass: collect string literals
        self.collect_strings(module);

        // Generate binary Wasm (updates string_array_type_idx)
        let wasm = self.generate_component(module);

        // Validate the generated Wasm (catches codegen bugs early)
        Self::validate_wasm(&wasm);

        wasm
    }

    /// Build WASI registry from embedded stdlib
    ///
    /// Parses the embedded wasi:* modules and registers their effect methods.
    /// This is used when generating Wasm without explicit module loading.
    fn build_wasi_registry_from_stdlib(&mut self) {
        fn parse_module(source: &str) -> AstModule {
            let mut lexer = Lexer::new(source);
            let tokens = lexer.tokenize().expect("lexer error in stdlib");
            let mut parser = Parser::new(tokens);
            parser.parse().expect("parser error in stdlib")
        }

        // Parse and register wasi:cli (required for version and stdout/stderr)
        let wasi_cli = parse_module(stdlib::WASI_CLI);
        let cli_path = vec!["wasi".to_string(), "cli".to_string()];
        self.register_wasi_module(&cli_path, &wasi_cli);

        // Parse and register wasi:clocks
        let wasi_clocks = parse_module(stdlib::WASI_CLOCKS);
        let clocks_path = vec!["wasi".to_string(), "clocks".to_string()];
        self.register_wasi_module(&clocks_path, &wasi_clocks);

        // Note: wasi:filesystem is not loaded here because it uses `flags`
        // which the parser doesn't support yet.
    }

    /// Register effects from a single WASI module
    fn register_wasi_module(&mut self, _module_path: &[String], module: &AstModule) {
        // First, collect type aliases from this module
        let mut type_aliases: HashMap<String, Type> = HashMap::new();
        for item in &module.items {
            if let Item::Type(alias) = item {
                type_aliases.insert(alias.name.clone(), alias.ty.clone());
            }
        }

        // Helper to resolve a type through aliases
        let resolve_type = |ty: &Type| -> Type {
            match ty {
                Type::Named(named) => {
                    if let Some(resolved) = type_aliases.get(&named.name) {
                        resolved.clone()
                    } else {
                        ty.clone()
                    }
                }
                _ => ty.clone(),
            }
        };

        // Register effect methods with resolved types
        for item in &module.items {
            if let Item::Effect(effect) = item {
                for method in &effect.methods {
                    if let Some(wasi) = method.attrs.first().and_then(|a| a.wasi_import.as_ref()) {
                        let params: Vec<(String, Type)> = method
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), resolve_type(&p.ty)))
                            .collect();

                        let return_type = method.return_type.as_ref().map(&resolve_type);

                        self.wasi_registry.register(
                            &effect.name,
                            &method.name,
                            wasi,
                            method.is_async,
                            params,
                            return_type,
                        );
                    }
                }
            }
        }

        // Register world definitions
        for item in &module.items {
            if let Item::World(world) = item {
                self.world_registry.register(world);
            }
        }
    }

    /// Build builtin registry from embedded stdlib
    ///
    /// Parses lib/core/builtin.wado and registers function signatures
    /// for type inference during code generation.
    fn build_builtin_registry_from_stdlib(&mut self) {
        let source = stdlib::CORE_BUILTIN;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error in core:builtin");
        let mut parser = Parser::new(tokens);
        let module = parser.parse().expect("parser error in core:builtin");

        for item in &module.items {
            if let Item::Function(func) = item {
                self.builtin_registry.register(func);
            }
        }
    }

    /// Generate WASI imports dynamically from the registry
    ///
    /// This generates Component Model imports based on the WASI registry data
    /// populated from lib/wasi/*.wado files.
    fn generate_wasi_imports(&self, builder: &mut ComponentBuilder, ctx: &mut ComponentContext) {
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
                .all(|f| self.is_wasi_function_supported(f));

            if !all_functions_supported {
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

        // Always import stdout/stderr if not already imported from registry
        // These are needed by core infrastructure (panic, log functions)
        self.ensure_stdout_stderr_imported(builder, ctx, cli_version);
    }

    /// Ensure stdout and stderr are imported (for panic/logging support)
    ///
    /// These are needed by core infrastructure even if wasi:cli isn't explicitly imported.
    /// Uses the version from the registry.
    fn ensure_stdout_stderr_imported(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentContext,
        cli_version: &str,
    ) {
        // Import stdout if not already imported
        let stdout_local_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if !ctx.has_comp_func(&stdout_local_name) {
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

        // Import stderr if not already imported
        let stderr_local_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if !ctx.has_comp_func(&stderr_local_name) {
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

    /// Check if a Wado type is supported for CM parameter generation
    ///
    /// Note: Result is NOT included here because it's a return-type-only construct.
    /// Type aliases (like Instant, Duration) should already be resolved to their
    /// underlying types before this check.
    fn is_param_type_supported(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(named) => matches!(
                named.name.as_str(),
                "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
            ),
            Type::Generic(generic) => matches!(generic.name.as_str(), "Stream"),
            _ => false,
        }
    }

    /// Check if a return type is supported for CM generation
    ///
    /// Type aliases (like Instant, Duration) should already be resolved to their
    /// underlying types before this check.
    fn is_return_type_supported(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(named) => matches!(
                named.name.as_str(),
                "i32"
                    | "i64"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "char"
                    | "String"
            ),
            Type::Generic(generic) => matches!(generic.name.as_str(), "Stream" | "Result"),
            _ => false,
        }
    }

    /// Check if all types in a WASI function are supported for CM generation
    fn is_wasi_function_supported(&self, func: &WasiFunctionInfo) -> bool {
        // Check all parameter types (Result not allowed in params)
        for (_, ty) in &func.params {
            if !self.is_param_type_supported(ty) {
                return false;
            }
        }
        // Check return type if present (Result allowed)
        if let Some(ret_ty) = &func.return_type
            && !self.is_return_type_supported(ret_ty)
        {
            return false;
        }
        true
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

    /// Generate Component Model binary Wasm with support for multiple modules
    ///
    /// This version supports compiling multiple Wado modules into a single Wasm component.
    /// Functions from imported local modules are included in the generated code.
    /// Functions from implicit modules are only accessible via qualified names.
    pub fn generate_wasm_with_modules(
        &mut self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        symbols: &SymbolTable,
        implicit_modules: &std::collections::HashSet<Vec<String>>,
    ) -> Vec<u8> {
        // Build registries from stdlib (always available)
        self.build_wasi_registry_from_stdlib();
        self.build_builtin_registry_from_stdlib();

        // First pass: collect string literals from all modules
        self.collect_strings(main_module);
        for (_, module) in loaded_modules {
            self.collect_strings(module);
        }

        // Generate binary Wasm with multi-module support
        let wasm = self.generate_component_with_modules(
            main_module,
            loaded_modules,
            symbols,
            implicit_modules,
        );

        // Validate the generated Wasm (catches codegen bugs early)
        Self::validate_wasm(&wasm);

        wasm
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
                // Collect strings from condition (for intermediate value display)
                self.collect_strings_from_expr(&assert_stmt.condition);

                // Collect strings from optional message
                if let Some(msg) = &assert_stmt.message {
                    self.collect_strings_from_expr(msg);
                }

                // Collect static strings for power-assert message
                let condition_source = self.get_source_text(&assert_stmt.condition.span());

                // Helper to add string if not already present
                let add_str = |strings: &mut Vec<String>, s: String| {
                    if !strings.contains(&s) {
                        strings.push(s);
                    }
                };

                // Header strings
                add_str(&mut self.string_literals, "Assertion failed:\n".to_string());
                add_str(&mut self.string_literals, "Assertion failed: ".to_string());
                add_str(&mut self.string_literals, "\n".to_string());

                // Condition line
                let condition_line = format!("condition: {}\n", condition_source);
                add_str(&mut self.string_literals, condition_line);

                // Intermediate value strings
                for (name, _) in self.extract_intermediate_values(&assert_stmt.condition) {
                    let name_prefix = format!("{}: ", name);
                    add_str(&mut self.string_literals, name_prefix);
                }
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
    fn generate_component(&mut self, ast_module: &AstModule) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentContext::new();

        // Get the CLI version from the registry
        let cli_version = self
            .wasi_registry
            .get_cli_version()
            .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

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
        let types_import_path = format!("wasi:cli/types@{}", cli_version);
        builder.import(
            &types_import_path,
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
        let stdout_import_path = format!("wasi:cli/stdout@{}", cli_version);
        builder.import(
            &stdout_import_path,
            wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
        );

        // Alias write-via-stream from stdout instance (component func)
        let stdout_comp_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        ctx.register_comp_func(&stdout_comp_name);
        builder.alias_export(
            ctx.instance_idx("stdout"),
            "write-via-stream",
            ComponentExportKind::Func,
        );

        // ========================================
        // Import stderr instance from WASI P3
        // stderr has the same interface type as stdout
        // ========================================
        ctx.register_instance("stderr");
        let stderr_import_path = format!("wasi:cli/stderr@{}", cli_version);
        builder.import(
            &stderr_import_path,
            wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
        );

        // Alias write-via-stream from stderr instance (component func)
        let stderr_comp_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        ctx.register_comp_func(&stderr_comp_name);
        builder.alias_export(
            ctx.instance_idx("stderr"),
            "write-via-stream",
            ComponentExportKind::Func,
        );

        // ========================================
        // Import monotonic-clock from WASI P3
        // ========================================
        // Type for monotonic-clock instance: { now: func() -> u64 }
        let monotonic_clock_instance_type = ctx.register_type("monotonic-clock-instance");
        {
            let (_, enc) = builder.ty(Some("monotonic-clock-instance"));
            let mut instance_type = InstanceType::new();
            // Type 0 within instance: func() -> u64 (sync function)
            instance_type
                .ty()
                .function()
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Primitive(PrimitiveValType::U64)));
            instance_type.export("now", wasm_encoder::ComponentTypeRef::Func(0));
            enc.instance(&instance_type);
        }

        // Import monotonic-clock instance from WASI P3
        ctx.register_instance("monotonic-clock");
        let clock_version = self
            .wasi_registry
            .get_package_version("clocks")
            .unwrap_or(cli_version);
        let clock_import_path = format!("wasi:clocks/monotonic-clock@{}", clock_version);
        builder.import(
            &clock_import_path,
            wasm_encoder::ComponentTypeRef::Instance(monotonic_clock_instance_type),
        );

        // Alias now from monotonic-clock instance (component func)
        let monotonic_clock_comp_name = build_local_alias_name("clocks", "MonotonicClock", "now");
        ctx.register_comp_func(&monotonic_clock_comp_name);
        builder.alias_export(
            ctx.instance_idx("monotonic-clock"),
            "now",
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

        // Lower write-via-stream component func to core func (stdout)
        let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
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

        // Lower write-via-stream component func to core func (stderr)
        let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
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
        // Pre-compute WASI function names to ensure they live long enough
        let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
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
                &stdout_func_name,
                ExportKind::Func,
                ctx.core_func_idx(&stdout_func_name),
            ),
            (
                &stderr_func_name,
                ExportKind::Func,
                ctx.core_func_idx(&stderr_func_name),
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
        // Conditionally add monotonic-clock-now if it was registered
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
                    // Currently only Stdout, Stderr, and MonotonicClock effects are supported (or no effects)
                    if !f.effects.is_empty() {
                        let has_unsupported_effects = f
                            .effects
                            .iter()
                            .any(|e| e != "Stdout" && e != "Stderr" && e != "MonotonicClock");
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
        &mut self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        symbols: &SymbolTable,
        implicit_modules: &std::collections::HashSet<Vec<String>>,
    ) -> Vec<u8> {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentContext::new();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // Generate WASI imports dynamically from registry
        self.generate_wasi_imports(&mut builder, &mut ctx);

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
            implicit_modules,
        );
        ctx.register_core_module("main-mod");
        builder.core_module_raw(Some("main-mod"), &main_core_module);

        // Create wasi instance with stream intrinsics + lowered WASI function + async intrinsics
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
        // Conditionally add monotonic-clock-now if it was registered
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
        &mut self,
        main_module: &AstModule,
        loaded_modules: &[(&Vec<String>, &AstModule)],
        _symbols: &SymbolTable,
        string_data: &[u8],
        _implicit_modules: &std::collections::HashSet<Vec<String>>,
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

        // Builtin function types - derived from core/builtin.wado
        // Only builtins with #[canonical("...")] attribute are imported
        for func in self.builtin_registry.imported_builtins() {
            let canonical_name = func.canonical_name.as_ref().unwrap();
            let params = Self::builtin_func_to_core_params(func);
            let results = Self::builtin_func_to_core_results(func);
            builder.define_func_type(canonical_name, &params, &results);
        }

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        self.string_array_type_idx =
            builder.define_gc_array_type("string-array", StorageType::I8, true);

        // WASI effect function types - derived from wasi/*.wado definitions
        for interface in self.wasi_registry.interfaces() {
            for func in &interface.functions {
                let local_name = func.local_alias_name();
                let params = Self::wasi_func_to_core_params(func);
                let results = Self::wasi_func_to_core_results(func);
                builder.define_func_type(&local_name, &params, &results);
            }
        }

        // Types for user-defined functions
        let string_array_idx = builder.type_idx("string-array");
        for (_, func, qualified_name) in &all_funcs {
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            // Never type (!) has no Wasm return type - the function never returns
            let return_types: Vec<ValType> = if let Some(ret_ty) = &func.return_type {
                if self.is_never_type(ret_ty) {
                    vec![]
                } else {
                    vec![self.wado_type_to_wasm_with_idx(ret_ty, string_array_idx)]
                }
            } else {
                vec![]
            };
            builder.define_func_type(qualified_name, &param_types, &return_types);
        }

        // World export types - derived from Command world in wasi/cli.wado
        if let Some(run_export) = self.world_registry.get_export("Command", "run") {
            let params = Self::world_export_to_core_params(run_export);
            let results = Self::world_export_to_core_results(run_export);
            builder.define_func_type(&run_export.name, &params, &results);
        }

        // Add types section to module
        module.section(&builder.types);

        // ========================================
        // Import section
        // ========================================
        builder.import_func("wasi", "stream-new", "stream-new");
        builder.import_func("wasi", "stream-write", "stream-write");
        builder.import_func("wasi", "stream-drop-writable", "stream-drop-writable");
        builder.import_func("wasi", "stream-drop-readable", "stream-drop-readable");
        // Always import stdout/stderr - they're needed by core infrastructure (panic, logging)
        let stdout_import_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        let stderr_import_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        builder.import_func("wasi", &stdout_import_name, &stdout_import_name);
        builder.import_func("wasi", &stderr_import_name, &stderr_import_name);
        builder.import_func("wasi", "task-return", "task-return");
        builder.import_func("wasi", "waitable-set-new", "waitable-set-new");
        builder.import_func("wasi", "waitable-join", "waitable-join");
        builder.import_func("wasi", "waitable-set-wait", "waitable-set-wait");
        builder.import_func("wasi", "subtask-drop", "subtask-drop");
        // Only import monotonic-clock-now if the interface is registered
        if self.wasi_registry.has_interface("monotonic-clock") {
            let monotonic_import_name = build_local_alias_name("clocks", "MonotonicClock", "now");
            builder.import_func("wasi", &monotonic_import_name, &monotonic_import_name);
        }
        builder.import_func("env", "realloc", "realloc");
        builder.import_func("env", "f64_to_buffer", "f64_to_buffer");
        builder.import_func("env", "f32_to_buffer", "f32_to_buffer");
        builder.import_memory("env", "memory", 1);
        module.section(&builder.imports);

        // ========================================
        // Function section
        // ========================================

        // Register all user-defined functions
        let internal_path = vec!["core".to_string(), "internal".to_string()];
        for (module_path, func, qualified_name) in &all_funcs {
            let func_idx = builder.define_func(qualified_name, qualified_name);
            let is_from_internal = module_path == &internal_path;

            // Register simple name alias for all functions EXCEPT internal
            // Internal functions require explicit import to be accessible.
            if qualified_name != &func.name && !is_from_internal {
                builder.define_func_alias(&func.name, func_idx);
            }

            // Track internal functions for access control
            if is_from_internal {
                builder.mark_as_internal(&func.name);
            }
        }

        // Register aliases for explicitly imported functions from core:internal
        // Only for user code modules (not privileged modules like prelude/internal).
        // Privileged modules use the qualified name fallback in generate_call_with_builder.
        let prelude_path = vec!["core".to_string(), "prelude".to_string()];
        for (module_path, module) in loaded_modules
            .iter()
            .chain(std::iter::once(&(&vec![], main_module)))
        {
            // Skip privileged modules - they use qualified name fallback
            let is_privileged = **module_path == internal_path || **module_path == prelude_path;
            if is_privileged {
                continue;
            }

            for item in &module.items {
                if let crate::ast::Item::Use(use_decl) = item
                    && use_decl.source == "core:internal"
                {
                    for use_item in &use_decl.items {
                        if let crate::ast::UseItem::Simple { name, alias } = use_item {
                            let qualified_name = format!("{}::{}", internal_path.join("::"), name);
                            if let Some(func_idx) = builder.try_func_idx(&qualified_name) {
                                let alias_name = alias.as_ref().unwrap_or(name);
                                builder.define_func_alias(alias_name, func_idx);
                            }
                        }
                    }
                }
            }
        }

        // run function for the wasi:cli Command world
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

        // Build func_return_types map for local collection phase
        // This is needed to infer types for function calls before builder is fully populated
        let mut func_return_types: HashMap<String, ValType> = HashMap::new();
        for (_module_path, func, qualified_name) in &all_funcs {
            if let Some(ret_type) = builder.func_return_type(qualified_name) {
                func_return_types.insert(qualified_name.clone(), ret_type);
                // Also add simple name alias
                if qualified_name != &func.name {
                    func_return_types.insert(func.name.clone(), ret_type);
                }
            }
        }

        // Code section and branch hints collection
        let mut code = CodeSection::new();
        let mut all_branch_hints: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();

        // User-defined functions from all modules
        // Function indices for hints: imported functions come first, then user-defined
        let import_count = builder.import_func_count;
        for (idx, (module_path, func, _)) in all_funcs.iter().enumerate() {
            let (wasm_func, hints) =
                self.generate_user_function(func, &builder, module_path, &func_return_types);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((import_count + idx as u32, hints));
            }
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
                self.collect_locals_from_block(body, &mut func_ctx, &func_return_types);
                // Pre-allocate scratch locals for builtins (including float conversion)
                let string_array_type = builder.type_idx("string-array");
                self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
                // Pre-allocate locals for assert statements (power-assert needs cached values)
                self.preallocate_assert_locals(body, &mut func_ctx, string_array_type);
                func_ctx.get_local_decls()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let mut run_func = Function::new(local_decls);
        self.generate_run_body_instructions_p3_with_builder(
            &mut run_func,
            main_module,
            &builder,
            &func_return_types,
        );

        // Call task.return to complete the async task
        let task_return_idx = builder.func_idx("task-return");
        run_func.instruction(&Instruction::I32Const(0));
        run_func.instruction(&Instruction::Call(task_return_idx));
        run_func.instruction(&Instruction::End);
        code.function(&run_func);

        // Branch hints section (must come before code section)
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

        // Name section (uses builder's tracked names)
        let names = builder.build_name_section();
        module.section(&names);

        module.finish()
    }

    /// Build main module for WASI P3 with write-via-stream
    fn build_main_module_p3(&mut self, ast_module: &AstModule, string_data: &[u8]) -> Vec<u8> {
        let mut module = Module::new();
        let mut builder = CoreModuleBuilder::new();

        // Collect user-defined functions
        let user_funcs = self.collect_user_functions(ast_module);

        // ========================================
        // Define types using the builder
        // ========================================

        // Builtin function types - derived from core/builtin.wado
        // Only builtins with #[canonical("...")] attribute are imported
        for func in self.builtin_registry.imported_builtins() {
            let canonical_name = func.canonical_name.as_ref().unwrap();
            let params = Self::builtin_func_to_core_params(func);
            let results = Self::builtin_func_to_core_results(func);
            builder.define_func_type(canonical_name, &params, &results);
        }

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        self.string_array_type_idx =
            builder.define_gc_array_type("string-array", StorageType::I8, true);

        // WASI effect function types - derived from wasi/*.wado definitions
        for interface in self.wasi_registry.interfaces() {
            for func in &interface.functions {
                let local_name = func.local_alias_name();
                let params = Self::wasi_func_to_core_params(func);
                let results = Self::wasi_func_to_core_results(func);
                builder.define_func_type(&local_name, &params, &results);
            }
        }

        // Types for user-defined functions
        for func in &user_funcs {
            // Convert Wado params to Wasm types
            let string_array_idx = builder.type_idx("string-array");
            let param_types: Vec<ValType> = func
                .params
                .iter()
                .map(|p| self.wado_type_to_wasm_with_idx(&p.ty, string_array_idx))
                .collect();
            // Never type (!) has no Wasm return type - the function never returns
            let return_types: Vec<ValType> = if let Some(ret_ty) = &func.return_type {
                if self.is_never_type(ret_ty) {
                    vec![]
                } else {
                    vec![self.wado_type_to_wasm_with_idx(ret_ty, string_array_idx)]
                }
            } else {
                vec![]
            };
            builder.define_func_type(&func.name, &param_types, &return_types);
        }

        // World export types - derived from Command world in wasi/cli.wado
        if let Some(run_export) = self.world_registry.get_export("Command", "run") {
            let params = Self::world_export_to_core_params(run_export);
            let results = Self::world_export_to_core_results(run_export);
            builder.define_func_type(&run_export.name, &params, &results);
        }

        // Add types section to module
        module.section(&builder.types);

        // ========================================
        // Import section
        // ========================================
        builder.import_func("wasi", "stream-new", "stream-new");
        builder.import_func("wasi", "stream-write", "stream-write");
        builder.import_func("wasi", "stream-drop-writable", "stream-drop-writable");
        builder.import_func("wasi", "stream-drop-readable", "stream-drop-readable");
        // Always import stdout/stderr - they're needed by core infrastructure (panic, logging)
        let stdout_import_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        let stderr_import_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        builder.import_func("wasi", &stdout_import_name, &stdout_import_name);
        builder.import_func("wasi", &stderr_import_name, &stderr_import_name);
        builder.import_func("wasi", "task-return", "task-return");
        builder.import_func("wasi", "waitable-set-new", "waitable-set-new");
        builder.import_func("wasi", "waitable-join", "waitable-join");
        builder.import_func("wasi", "waitable-set-wait", "waitable-set-wait");
        builder.import_func("wasi", "subtask-drop", "subtask-drop");
        // Only import monotonic-clock-now if the interface is registered
        if self.wasi_registry.has_interface("monotonic-clock") {
            let monotonic_import_name = build_local_alias_name("clocks", "MonotonicClock", "now");
            builder.import_func("wasi", &monotonic_import_name, &monotonic_import_name);
        }
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

        // Build func_return_types map for local collection phase
        let mut func_return_types: HashMap<String, ValType> = HashMap::new();
        for func in &user_funcs {
            if let Some(ret_type) = builder.func_return_type(&func.name) {
                func_return_types.insert(func.name.clone(), ret_type);
            }
        }

        // Code section and branch hints collection
        let mut code = CodeSection::new();
        let mut all_branch_hints: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();

        // User-defined functions
        // Function indices for hints: imported functions come first, then user-defined
        let import_count = builder.import_func_count;
        let empty_path: &[String] = &[];
        for (idx, func) in user_funcs.iter().enumerate() {
            let (wasm_func, hints) =
                self.generate_user_function(func, &builder, empty_path, &func_return_types);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((import_count + idx as u32, hints));
            }
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
                self.collect_locals_from_block(body, &mut func_ctx, &func_return_types);
                // Pre-allocate scratch locals for builtins (including float conversion)
                let string_array_type = builder.type_idx("string-array");
                self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);
                // Pre-allocate locals for assert statements (power-assert needs cached values)
                self.preallocate_assert_locals(body, &mut func_ctx, string_array_type);
                func_ctx.get_local_decls()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let mut run_func = Function::new(local_decls);

        // Generate body from AST (calls to println, etc.)
        self.generate_run_body_instructions_p3_with_builder(
            &mut run_func,
            ast_module,
            &builder,
            &func_return_types,
        );

        // Call task.return to complete the async task
        // For result unit with no payload, pass discriminant directly (0 = ok)
        let task_return_idx = builder.func_idx("task-return");
        run_func.instruction(&Instruction::I32Const(0)); // 0 = ok discriminant
        run_func.instruction(&Instruction::Call(task_return_idx));

        // No return value - task.return already provided the result
        run_func.instruction(&Instruction::End);
        code.function(&run_func);

        // Branch hints section (must come before code section)
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
                // For complex types, use a ref type (string-array)
                "String" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
                }),
                _ => ValType::I32,
            },
            crate::ast::Type::Generic(generic) => match generic.name.as_str() {
                "Array" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
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

    /// Check if a type is the never type (!)
    /// Functions with ! return type never return, so they have no Wasm return type.
    fn is_never_type(&self, ty: &crate::ast::Type) -> bool {
        matches!(ty, crate::ast::Type::Named(named) if named.name == "!")
    }

    fn wado_type_to_wasm_with_idx(&self, ty: &crate::ast::Type, string_array_idx: u32) -> ValType {
        match ty {
            crate::ast::Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" => ValType::I32,
                "i64" | "u64" | "Instant" | "Duration" => ValType::I64, // Instant/Duration are u64 type aliases from wasi:clocks
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

    /// Generate a user-defined function and return branch hints collected during generation
    fn generate_user_function(
        &self,
        ast_func: &crate::ast::Function,
        builder: &CoreModuleBuilder,
        module_path: &[String],
        func_return_types: &HashMap<String, ValType>,
    ) -> (Function, Vec<(u32, bool)>) {
        // First pass: analyze function body to collect locals
        let mut func_ctx =
            FunctionContext::with_module_path(ast_func.params.len() as u32, module_path.to_vec());

        // Set return type for ref.as_non_null handling in return statements
        if let Some(ret_ty) = &ast_func.return_type {
            func_ctx.set_return_type(
                self.wado_type_to_wasm_with_idx(ret_ty, self.string_array_type_idx),
            );
        }

        // Add parameters to context
        for param in &ast_func.params {
            let param_type = self.wado_type_to_wasm_primitive(&param.ty);
            func_ctx.add_param(&param.name, param_type);
        }

        // Collect local variables from body
        if let Some(body) = &ast_func.body {
            self.collect_locals_from_block(body, &mut func_ctx, func_return_types);
        }

        // Pre-allocate scratch locals that builtins might need
        // These are needed by builtins that allocate locals at runtime
        let string_array_type = builder.type_idx("string-array");
        self.preallocate_builtin_scratch_locals(&mut func_ctx, string_array_type);

        // Pre-allocate locals for assert statements (power-assert needs cached values)
        if let Some(body) = &ast_func.body {
            self.preallocate_assert_locals(body, &mut func_ctx, string_array_type);
        }

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

        // Collect branch hints from the context
        let branch_hints = gen_ctx.take_branch_hints();
        (wasm_func, branch_hints)
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

    /// Pre-allocate locals needed for assert statements in a block
    fn preallocate_assert_locals(
        &self,
        block: &Block,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        for stmt in &block.stmts {
            self.preallocate_assert_locals_from_stmt(stmt, ctx, string_array_type);
        }
    }

    /// Pre-allocate locals from a single statement (recursively handles nested blocks)
    fn preallocate_assert_locals_from_stmt(
        &self,
        stmt: &Stmt,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        match stmt {
            Stmt::Assert(assert_stmt) => {
                // Pre-allocate locals for intermediate values
                let intermediate_values = self.extract_intermediate_values(&assert_stmt.condition);
                for (name, expr) in &intermediate_values {
                    let val_type = self.infer_expr_type_with_ctx(expr, ctx, None);
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
            Stmt::For(for_stmt) => {
                if let Some(init) = &for_stmt.init {
                    self.preallocate_assert_locals_from_stmt(init, ctx, string_array_type);
                }
                self.preallocate_assert_locals(&for_stmt.body, ctx, string_array_type);
            }
            Stmt::While(while_stmt) => {
                self.preallocate_assert_locals(&while_stmt.body, ctx, string_array_type);
            }
            Stmt::If(if_stmt) => {
                self.preallocate_assert_locals(&if_stmt.then_block, ctx, string_array_type);
                if let Some(else_block) = &if_stmt.else_block {
                    self.preallocate_assert_locals(else_block, ctx, string_array_type);
                }
            }
            _ => {}
        }
    }

    /// Collect local variables from a block
    fn collect_locals_from_block(
        &self,
        block: &Block,
        ctx: &mut FunctionContext,
        func_return_types: &HashMap<String, ValType>,
    ) {
        for stmt in &block.stmts {
            self.collect_locals_from_stmt(stmt, ctx, func_return_types);
        }
    }

    /// Collect local variables from a statement
    fn collect_locals_from_stmt(
        &self,
        stmt: &Stmt,
        ctx: &mut FunctionContext,
        func_return_types: &HashMap<String, ValType>,
    ) {
        match stmt {
            Stmt::Let(let_stmt) => {
                // Use explicit type annotation if present, otherwise infer from value
                let val_type = if let Some(ty) = &let_stmt.ty {
                    self.wado_type_to_wasm_primitive(ty)
                } else {
                    self.infer_expr_type_with_ctx_with_funcs(
                        &let_stmt.value,
                        ctx,
                        func_return_types,
                    )
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
                    self.collect_locals_from_stmt(init, ctx, func_return_types);
                }
                self.collect_locals_from_block(&for_stmt.body, ctx, func_return_types);
            }
            Stmt::While(while_stmt) => {
                self.collect_locals_from_block(&while_stmt.body, ctx, func_return_types);
            }
            Stmt::If(if_stmt) => {
                self.collect_locals_from_block(&if_stmt.then_block, ctx, func_return_types);
                if let Some(else_block) = &if_stmt.else_block {
                    self.collect_locals_from_block(else_block, ctx, func_return_types);
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
                    self.infer_expr_type_with_ctx(&let_stmt.value, ctx, Some(builder))
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
                    let expr_type = self.infer_expr_type_with_ctx(value, ctx, None);
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
                // Record branch hint offset before emitting the if instruction
                let if_offset = func.byte_len() as u32;
                ctx.consume_branch_hint(if_offset);
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
                // Power-assert: Cache intermediate values, then check condition
                // If false, build detailed error message and panic

                // 1. Extract intermediate values to cache
                let intermediate_values = self.extract_intermediate_values(&assert_stmt.condition);
                let mut cached_locals: Vec<(String, u32, ValType)> = Vec::new();
                let mut expr_cache: HashMap<usize, u32> = HashMap::new();

                // 2. Evaluate and cache each intermediate value
                for (name, expr) in &intermediate_values {
                    let val_type = self.infer_expr_type_with_ctx(expr, ctx, None);
                    let local_idx =
                        ctx.alloc_local(&format!("__assert_{}", name.replace(' ', "_")), val_type);
                    self.generate_expr_with_builder(func, expr, ctx, builder);
                    func.instruction(&Instruction::LocalSet(local_idx));
                    cached_locals.push((name.clone(), local_idx, val_type));
                    // Store in cache for later lookup (by expression pointer)
                    let expr_ptr = *expr as *const Expr as usize;
                    expr_cache.insert(expr_ptr, local_idx);
                }

                // 3. Evaluate condition using cached values (prevents re-evaluation of side effects)
                let cond_local = ctx.alloc_local("__assert_cond", ValType::I32);
                self.generate_expr_with_cache(
                    func,
                    &assert_stmt.condition,
                    &expr_cache,
                    ctx,
                    builder,
                );
                func.instruction(&Instruction::LocalSet(cond_local));

                // 4. Check condition: if (!condition) { ... }
                func.instruction(&Instruction::LocalGet(cond_local));
                func.instruction(&Instruction::I32Eqz);

                // Set branch hint: failure is unlikely
                ctx.set_branch_hint(false);
                let if_offset = func.byte_len() as u32;
                ctx.consume_branch_hint(if_offset);

                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

                // 5. Build power-assert message and panic
                let condition_source = self.get_source_text(&assert_stmt.condition.span());
                self.generate_assert_message(
                    func,
                    &condition_source,
                    assert_stmt.message.as_ref(),
                    &cached_locals,
                    ctx,
                    builder,
                );

                // 6. Call panic (assumes panic function is available)
                func.instruction(&Instruction::Call(builder.func_idx("panic")));

                // 7. Unreachable (panic never returns)
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
                // Infer both operand types for potential type coercion
                let left_type = self.infer_expr_type_with_ctx(&bin.left, ctx, Some(builder));
                let right_type = self.infer_expr_type_with_ctx(&bin.right, ctx, Some(builder));

                // Determine the target type for the operation
                // If one operand is i64 and the other is i32, promote to i64
                let operand_type = if left_type == ValType::I64 || right_type == ValType::I64 {
                    ValType::I64
                } else {
                    left_type
                };

                // Generate left operand with potential promotion
                self.generate_expr_with_builder(func, &bin.left, ctx, builder);
                if operand_type == ValType::I64 && left_type == ValType::I32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

                // Generate right operand with potential promotion
                self.generate_expr_with_builder(func, &bin.right, ctx, builder);
                if operand_type == ValType::I64 && right_type == ValType::I32 {
                    func.instruction(&Instruction::I64ExtendI32S);
                }

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
                        // Negate: generate appropriate instruction based on operand type
                        let operand_type =
                            self.infer_expr_type_with_ctx(&un.expr, ctx, Some(builder));
                        match operand_type {
                            ValType::F64 => {
                                func.instruction(&Instruction::F64Neg);
                            }
                            ValType::F32 => {
                                func.instruction(&Instruction::F32Neg);
                            }
                            ValType::I64 => {
                                // For i64: multiply by -1
                                func.instruction(&Instruction::I64Const(-1));
                                func.instruction(&Instruction::I64Mul);
                            }
                            _ => {
                                // For i32 and other types: multiply by -1
                                func.instruction(&Instruction::I32Const(-1));
                                func.instruction(&Instruction::I32Mul);
                            }
                        }
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
                // Special case: integer literal cast to i64/u64 - generate I64Const directly
                // to avoid truncation through i32
                let target_type_name = self.get_primitive_type_name(&cast.target_type);
                if let Expr::Literal(lit) = &cast.expr
                    && let Literal::Int(n) = &lit.value
                    && matches!(
                        target_type_name.as_str(),
                        "i64" | "u64" | "Instant" | "Duration"
                    )
                {
                    func.instruction(&Instruction::I64Const(*n));
                } else {
                    self.generate_expr_with_builder(func, &cast.expr, ctx, builder);
                    self.generate_type_cast(func, &cast.expr, &cast.target_type, ctx);
                }
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
        let source_wasm_type = self.infer_expr_type_with_ctx(source_expr, ctx, None);
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
                // For write-via-stream (stdout or stderr), we start the operation but defer wait to function end
                let stdout_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
                let stderr_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
                if wasi_func_name == stdout_name || wasi_func_name == stderr_name {
                    self.generate_write_via_stream_start(func, ctx, builder, func_idx);
                } else {
                    func.instruction(&Instruction::Call(func_idx));
                }
                return;
            }

            // Then try user-defined functions
            let func_idx = if let Some(idx) = builder.try_func_idx(&ident.name) {
                idx
            } else {
                // Simple name not found. If caller is from a privileged module
                // (core:internal or core:prelude), try qualified name fallback.
                let caller_module = &ctx.current_module_path;
                let is_privileged = caller_module == &["core".to_string(), "internal".to_string()]
                    || caller_module == &["core".to_string(), "prelude".to_string()];

                if is_privileged && builder.is_internal_function(&ident.name) {
                    let qualified_name = format!("core::internal::{}", ident.name);
                    builder.try_func_idx(&qualified_name).unwrap_or_else(|| {
                        panic!("unknown function: {}", ident.name);
                    })
                } else {
                    // Unknown function - this should have been caught by the analyzer
                    panic!("unknown function: {}", ident.name);
                }
            };

            // Generate arguments
            for arg in &call.args {
                self.generate_expr_with_builder(func, arg, ctx, builder);
            }
            func.instruction(&Instruction::Call(func_idx));
        }
    }

    /// Resolve an effect function call to its local alias name
    ///
    /// Maps Wado effect function syntax (Effect::method) to WASI function names.
    /// The mapping is based on the `#[wasi(...)]` attributes in wasi/*.wado modules.
    /// Returns the unified name format: `wasi:{package}/{EffectName}::{method_name}`
    fn resolve_effect_function(&self, name: &str) -> Option<String> {
        // Check if this is an effect function call (contains ::)
        if !name.contains("::") {
            return None;
        }

        // Look up in the WASI registry and return the local alias name
        self.wasi_registry.resolve(name)
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

        // Check if this builtin has a canonical name (imported CM function)
        if let Some(builtin_info) = self.builtin_registry.get(builtin_name)
            && let Some(canonical_name) = &builtin_info.canonical_name {
                // Generate all arguments
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                // Call the imported function
                func.instruction(&Instruction::Call(builder.func_idx(canonical_name)));
                return;
            }

        // Handle builtins that compile to Wasm instructions or have special logic
        match builtin_name {
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

            // Control flow
            "unreachable" => {
                // unreachable() -> ! (traps immediately)
                func.instruction(&Instruction::Unreachable);
            }
            "effect_wait" => {
                // effect_wait() - Wait for pending async effects to complete
                // This should be called before unreachable in functions with effects
                self.generate_effect_wait(func, ctx, builder);
            }

            // Ambient logging builtins (for log/log_error functions that bypass effect system)
            "call_indirect_stdout_write_via_stream" => {
                // call_indirect_stdout_write_via_stream(rx: i32) - call stdout write-via-stream
                // Used by log() for ambient stdout logging without requiring Stdout effect
                // If stdout isn't available (wasi:cli not imported), this is a no-op
                let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
                if let Some(func_idx) = builder.try_func_idx(&stdout_func_name) {
                    for arg in &call.args {
                        self.generate_expr_with_builder(func, arg, ctx, builder);
                    }
                    self.generate_write_via_stream_start(func, ctx, builder, func_idx);
                } else {
                    // stdout not available - drop the argument and push dummy return value
                    // (generate_write_via_stream_start pushes i32(0) as dummy value)
                    for arg in &call.args {
                        self.generate_expr_with_builder(func, arg, ctx, builder);
                    }
                    func.instruction(&Instruction::Drop); // drop the rx argument
                    func.instruction(&Instruction::I32Const(0)); // push dummy return value
                }
            }
            "call_indirect_stderr_write_via_stream" => {
                // call_indirect_stderr_write_via_stream(rx: i32) - call stderr write-via-stream
                // Used by log_error() for ambient stderr logging without requiring Stderr effect
                // If stderr isn't available (wasi:cli not imported), this is a no-op
                let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
                if let Some(func_idx) = builder.try_func_idx(&stderr_func_name) {
                    for arg in &call.args {
                        self.generate_expr_with_builder(func, arg, ctx, builder);
                    }
                    self.generate_write_via_stream_start(func, ctx, builder, func_idx);
                } else {
                    // stderr not available - drop the argument and push dummy return value
                    // (generate_write_via_stream_start pushes i32(0) as dummy value)
                    for arg in &call.args {
                        self.generate_expr_with_builder(func, arg, ctx, builder);
                    }
                    func.instruction(&Instruction::Drop); // drop the rx argument
                    func.instruction(&Instruction::I32Const(0)); // push dummy return value
                }
            }

            // Branch hinting builtins
            "likely" => {
                // likely(cond: bool) -> bool
                // Hints to the runtime that the condition is likely true (branch taken)
                // The condition value is passed through unchanged
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                // Set pending hint: true = branch likely taken
                ctx.set_branch_hint(true);
            }
            "unlikely" => {
                // unlikely(cond: bool) -> bool
                // Hints to the runtime that the condition is unlikely true (branch not taken)
                // The condition value is passed through unchanged
                for arg in &call.args {
                    self.generate_expr_with_builder(func, arg, ctx, builder);
                }
                // Set pending hint: false = branch unlikely taken
                ctx.set_branch_hint(false);
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
        func_return_types: &HashMap<String, ValType>,
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
            self.collect_locals_from_block(body, &mut ctx, func_return_types);
            // Pre-allocate scratch locals for builtins (must match the local collection phase)
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_builtin_scratch_locals(&mut ctx, string_array_type);

            for stmt in &body.stmts {
                self.generate_stmt_with_builder(func, stmt, &mut ctx, builder);
            }
        }
    }

    /// Get the Wasm return type for a builtin function from the registry
    /// Returns None if the function is not found or has no return type
    fn get_builtin_return_type(&self, name: &str) -> Option<ValType> {
        let builtin_name = name.strip_prefix("builtin::").unwrap_or(name);
        let return_type = self.builtin_registry.get_return_type(builtin_name)?;

        // Convert AST Type to ValType
        Some(self.ast_type_to_wasm_valtype(return_type))
    }

    /// Convert an AST Type to a Wasm ValType
    fn ast_type_to_wasm_valtype(&self, ty: &Type) -> ValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" | "char" | "u8" | "i8" | "u16" | "i16" => ValType::I32,
                "i64" | "u64" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                "String" => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx), // string-array type index
                }),
                _ => ValType::I32, // Default fallback
            },
            _ => ValType::I32, // Default fallback for complex types
        }
    }

    /// Convert a WASI function type to Core Wasm params
    ///
    /// For async functions, an extra i32 param (outptr) is added per Component Model ABI.
    /// For sync functions, params are mapped directly.
    fn wasi_func_to_core_params(func: &WasiFunctionInfo) -> Vec<ValType> {
        let mut params: Vec<ValType> = func
            .params
            .iter()
            .map(|(_, ty)| Self::wasi_type_to_valtype(ty))
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
    fn wasi_func_to_core_results(func: &WasiFunctionInfo) -> Vec<ValType> {
        if func.is_async {
            // Async functions return a subtask handle (i32)
            vec![ValType::I32]
        } else if let Some(ret_ty) = &func.return_type {
            vec![Self::wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Convert a Wado type to ValType for WASI function signatures
    ///
    /// This is a simplified version that doesn't need string_array_type_idx
    /// because WASI function parameters and returns don't use String directly.
    fn wasi_type_to_valtype(ty: &Type) -> ValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" | "u32" | "bool" | "char" | "u8" | "i8" | "u16" | "i16" => ValType::I32,
                "i64" | "u64" | "Instant" | "Duration" => ValType::I64,
                "f32" => ValType::F32,
                "f64" => ValType::F64,
                // Stream handles are i32
                _ => ValType::I32,
            },
            Type::Generic(generic) => match generic.name.as_str() {
                // Stream<T> is represented as i32 handle
                "Stream" => ValType::I32,
                // Result<T, E> is represented as i32 discriminant
                "Result" => ValType::I32,
                // Future<T> is represented as i32 handle
                "Future" => ValType::I32,
                // Tuple types map to i32 for simplicity (struct pointer)
                "Tuple" => ValType::I32,
                _ => ValType::I32,
            },
            Type::Tuple(_) => ValType::I32,
            _ => ValType::I32,
        }
    }

    /// Convert a builtin function type to Core Wasm params
    fn builtin_func_to_core_params(func: &BuiltinFunctionInfo) -> Vec<ValType> {
        func.params
            .iter()
            .map(|(_, ty)| Self::wasi_type_to_valtype(ty))
            .collect()
    }

    /// Convert a builtin function type to Core Wasm results
    fn builtin_func_to_core_results(func: &BuiltinFunctionInfo) -> Vec<ValType> {
        if func.diverges {
            // Diverging functions have no return type
            vec![]
        } else if let Some(ret_ty) = &func.return_type {
            vec![Self::wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Convert a world export function type to Core Wasm params
    ///
    /// For async exports, the core function has no params (async uses task_return).
    /// For sync exports, params are mapped directly.
    fn world_export_to_core_params(export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no params in core (lifted signature differs)
            vec![]
        } else {
            export
                .params
                .iter()
                .map(|(_, ty)| Self::wasi_type_to_valtype(ty))
                .collect()
        }
    }

    /// Convert a world export function type to Core Wasm results
    ///
    /// For async exports, there's no return (result passed via task_return).
    /// For sync exports, the return type is mapped directly.
    fn world_export_to_core_results(export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no return in core (use task_return)
            vec![]
        } else if let Some(ret_ty) = &export.return_type {
            vec![Self::wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Infer expression type with function context (for looking up variable types)
    /// If builder is provided, can look up user function return types
    fn infer_expr_type_with_ctx(
        &self,
        expr: &Expr,
        ctx: &FunctionContext,
        builder: Option<&CoreModuleBuilder>,
    ) -> ValType {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Int(_) => ValType::I32,
                Literal::Float(_) => ValType::F64,
                Literal::Bool(_) => ValType::I32,
                Literal::Char(_) => ValType::I32, // Unicode code point as i32
                Literal::String(_) => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
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
                // Comparison and logical ops always return i32 (bool)
                // Bitwise and arithmetic ops return the operand type
                match bin.op {
                    crate::ast::BinaryOp::Eq
                    | crate::ast::BinaryOp::NotEq
                    | crate::ast::BinaryOp::Lt
                    | crate::ast::BinaryOp::LtEq
                    | crate::ast::BinaryOp::Gt
                    | crate::ast::BinaryOp::GtEq
                    | crate::ast::BinaryOp::And
                    | crate::ast::BinaryOp::Or => ValType::I32,
                    // Bitwise and arithmetic ops return the operand type
                    _ => self.infer_expr_type_with_ctx(&bin.left, ctx, builder),
                }
            }
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    // First, check if it's a builtin function
                    if ident.name.starts_with("builtin::") {
                        if let Some(ret_type) = self.get_builtin_return_type(&ident.name) {
                            return ret_type;
                        }
                        // Builtins with no return type (void) - return I32 as default
                        return ValType::I32;
                    }

                    // Check if it's a user-defined function with known return type
                    if let Some(b) = builder
                        && let Some(ret_type) = b.func_return_type(&ident.name)
                    {
                        return ret_type;
                    }
                    ValType::I32
                } else {
                    ValType::I32
                }
            }
            Expr::TemplateString(_) => {
                // Template strings return a GC array<u8> (same as string literals)
                ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
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
            Expr::Unary(unary) => {
                // For negation (-), the type is the same as the operand
                // For logical not (!) and bitwise not (~), the type is i32
                match unary.op {
                    crate::ast::UnaryOp::Neg => {
                        self.infer_expr_type_with_ctx(&unary.expr, ctx, builder)
                    }
                    crate::ast::UnaryOp::Not | crate::ast::UnaryOp::BitNot => ValType::I32,
                    _ => ValType::I32,
                }
            }
            _ => ValType::I32,
        }
    }

    /// Infer expression type using a function return type map (for local collection phase)
    /// This is used before the CoreModuleBuilder has function types populated
    fn infer_expr_type_with_ctx_with_funcs(
        &self,
        expr: &Expr,
        ctx: &FunctionContext,
        func_return_types: &HashMap<String, ValType>,
    ) -> ValType {
        match expr {
            Expr::Literal(lit) => match &lit.value {
                Literal::Int(_) => ValType::I32,
                Literal::Float(_) => ValType::F64,
                Literal::Bool(_) => ValType::I32,
                Literal::Char(_) => ValType::I32,
                Literal::String(_) => ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self.string_array_type_idx),
                }),
                Literal::Null => ValType::I32,
                Literal::Unit => ValType::I32,
            },
            Expr::Ident(ident) => {
                if let Some(ty) = ctx.get_local_type(&ident.name) {
                    ty
                } else {
                    ValType::I32
                }
            }
            Expr::Binary(bin) => match bin.op {
                // Comparison and logical ops always return i32 (bool)
                crate::ast::BinaryOp::Eq
                | crate::ast::BinaryOp::NotEq
                | crate::ast::BinaryOp::Lt
                | crate::ast::BinaryOp::LtEq
                | crate::ast::BinaryOp::Gt
                | crate::ast::BinaryOp::GtEq
                | crate::ast::BinaryOp::And
                | crate::ast::BinaryOp::Or => ValType::I32,
                // Bitwise and arithmetic ops return the operand type
                _ => self.infer_expr_type_with_ctx_with_funcs(&bin.left, ctx, func_return_types),
            },
            Expr::Call(call) => {
                if let Expr::Ident(ident) = &call.callee {
                    // First, check if it's a builtin function
                    if ident.name.starts_with("builtin::") {
                        if let Some(ret_type) = self.get_builtin_return_type(&ident.name) {
                            return ret_type;
                        }
                        // Builtins with no return type (void) - return I32 as default
                        return ValType::I32;
                    }

                    // Check user-defined function return types
                    if let Some(&ret_type) = func_return_types.get(&ident.name) {
                        return ret_type;
                    }
                    ValType::I32
                } else {
                    ValType::I32
                }
            }
            Expr::TemplateString(_) => ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(self.string_array_type_idx),
            }),
            Expr::Cast(cast) => {
                let target_name = self.get_primitive_type_name(&cast.target_type);
                match target_name.as_str() {
                    "i32" | "u32" | "bool" => ValType::I32,
                    "i64" | "u64" => ValType::I64,
                    "f32" => ValType::F32,
                    "f64" => ValType::F64,
                    _ => ValType::I32,
                }
            }
            Expr::Unary(unary) => {
                // For negation (-), the type is the same as the operand
                // For logical not (!) and bitwise not (~), the type is i32
                match unary.op {
                    crate::ast::UnaryOp::Neg => self.infer_expr_type_with_ctx_with_funcs(
                        &unary.expr,
                        ctx,
                        func_return_types,
                    ),
                    crate::ast::UnaryOp::Not | crate::ast::UnaryOp::BitNot => ValType::I32,
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
                | "unreachable"
                | "effect_wait"
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
                    array_type_index: self.string_array_type_idx,
                    array_data_index: 0, // Data segment 0
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

        // Strategy: Use string_concat from core:internal for concatenation
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

        // Concatenate remaining parts using string_concat from core:internal
        for part in template.parts.iter().skip(1) {
            // Push result (first argument to string_concat)
            // Convert nullable ref to non-nullable since string_concat expects non-nullable
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);

            // Generate the next part (second argument to string_concat)
            self.generate_template_part(func, part, ctx, builder);

            // Call string_concat(result, part) -> new result
            // The function is registered as "string_concat" (alias) or "core::internal::string_concat" (qualified)
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));

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
                let expr_type = self.infer_expr_type_with_ctx(expr, ctx, Some(builder));

                match expr_type {
                    ValType::F64 => {
                        // Float-to-string conversion using core:internal::f64_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(
                            builder.func_idx("core::internal::f64_to_string"),
                        ));
                    }
                    ValType::F32 => {
                        // Float-to-string conversion using core:internal::f32_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(
                            builder.func_idx("core::internal::f32_to_string"),
                        ));
                    }
                    ValType::I64 => {
                        // i64-to-string conversion using core:internal::i64_to_string
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        func.instruction(&Instruction::Call(
                            builder.func_idx("core::internal::i64_to_string"),
                        ));
                    }
                    ValType::I32 => {
                        // For i32, we need to check if it's actually a bool or char literal
                        let semantic_type = self.infer_semantic_type(expr, ctx);
                        self.generate_expr_with_builder(func, expr, ctx, builder);
                        match semantic_type {
                            SemanticType::Bool => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("core::internal::bool_to_string"),
                                ));
                            }
                            SemanticType::Char => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("core::internal::char_to_string"),
                                ));
                            }
                            SemanticType::I32 | SemanticType::Other => {
                                func.instruction(&Instruction::Call(
                                    builder.func_idx("core::internal::i32_to_string"),
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
                        func.instruction(&Instruction::Call(
                            builder.func_idx("core::internal::i32_to_string"),
                        ));
                    }
                }
            }
        }
    }

    /// Generate power-assert message using cached local values
    /// Format: "Assertion failed: [message]\ncondition: <source>\n<var>: <value>\n..."
    fn generate_assert_message(
        &self,
        func: &mut Function,
        condition_source: &str,
        message_expr: Option<&Expr>,
        cached_locals: &[(String, u32, ValType)],
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let string_array_type = builder.type_idx("string-array");

        // Allocate result local for message accumulation
        let result_local = ctx.alloc_local(
            "__assert_msg",
            ValType::Ref(RefType {
                nullable: false,
                heap_type: HeapType::Concrete(string_array_type),
            }),
        );

        // 1. Start with header
        if let Some(msg_expr) = message_expr {
            // "Assertion failed: "
            self.generate_string_from_data(func, "Assertion failed: ", builder);
            func.instruction(&Instruction::LocalSet(result_local));

            // Append user's message
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);
            self.generate_expr_with_builder(func, msg_expr, ctx, builder);
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));
            func.instruction(&Instruction::LocalSet(result_local));

            // Append newline
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);
            self.generate_string_from_data(func, "\n", builder);
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));
            func.instruction(&Instruction::LocalSet(result_local));
        } else {
            // "Assertion failed:\n"
            self.generate_string_from_data(func, "Assertion failed:\n", builder);
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // 2. Append condition source: "condition: <source>\n"
        func.instruction(&Instruction::LocalGet(result_local));
        func.instruction(&Instruction::RefAsNonNull);
        let condition_line = format!("condition: {}\n", condition_source);
        self.generate_string_from_data(func, &condition_line, builder);
        func.instruction(&Instruction::Call(
            builder.func_idx("core::internal::string_concat"),
        ));
        func.instruction(&Instruction::LocalSet(result_local));

        // 3. For each cached value, append "<name>: <value>\n"
        for (name, local_idx, val_type) in cached_locals {
            // Append "<name>: "
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);
            let name_prefix = format!("{}: ", name);
            self.generate_string_from_data(func, &name_prefix, builder);
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));
            func.instruction(&Instruction::LocalSet(result_local));

            // Append value (convert to string based on type)
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);
            func.instruction(&Instruction::LocalGet(*local_idx));
            self.generate_value_to_string(func, val_type, builder);
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));
            func.instruction(&Instruction::LocalSet(result_local));

            // Append newline
            func.instruction(&Instruction::LocalGet(result_local));
            func.instruction(&Instruction::RefAsNonNull);
            self.generate_string_from_data(func, "\n", builder);
            func.instruction(&Instruction::Call(
                builder.func_idx("core::internal::string_concat"),
            ));
            func.instruction(&Instruction::LocalSet(result_local));
        }

        // 4. Final result on stack
        func.instruction(&Instruction::LocalGet(result_local));
        func.instruction(&Instruction::RefAsNonNull);
    }

    /// Generate a string from data section
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

    /// Generate code to convert a value to string based on its Wasm type
    fn generate_value_to_string(
        &self,
        func: &mut Function,
        val_type: &ValType,
        builder: &CoreModuleBuilder,
    ) {
        match val_type {
            ValType::I32 => {
                func.instruction(&Instruction::Call(
                    builder.func_idx("core::internal::i32_to_string"),
                ));
            }
            ValType::I64 => {
                func.instruction(&Instruction::Call(
                    builder.func_idx("core::internal::i64_to_string"),
                ));
            }
            ValType::F32 => {
                func.instruction(&Instruction::Call(
                    builder.func_idx("core::internal::f32_to_string"),
                ));
            }
            ValType::F64 => {
                func.instruction(&Instruction::Call(
                    builder.func_idx("core::internal::f64_to_string"),
                ));
            }
            ValType::Ref(_) => {
                // Assume it's already a string - no conversion needed
            }
            ValType::V128 => {
                // Not supported - treat as i32
                func.instruction(&Instruction::Call(
                    builder.func_idx("core::internal::i32_to_string"),
                ));
            }
        }
    }

    /// Generate expression code using cached values where available
    /// This prevents re-evaluation of side-effectful expressions in assert conditions
    fn generate_expr_with_cache(
        &self,
        func: &mut Function,
        expr: &Expr,
        cache: &HashMap<usize, u32>, // Maps expression pointer to local index
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Check if this exact expression was cached (by pointer address)
        let expr_ptr = expr as *const Expr as usize;
        if let Some(&local_idx) = cache.get(&expr_ptr) {
            func.instruction(&Instruction::LocalGet(local_idx));
            return;
        }

        // Otherwise, recursively generate with cache lookups
        match expr {
            Expr::Binary(bin) => {
                self.generate_expr_with_cache(func, &bin.left, cache, ctx, builder);
                self.generate_expr_with_cache(func, &bin.right, cache, ctx, builder);

                // Infer operand type to select correct instructions
                let operand_type = self.infer_expr_type_with_ctx(&bin.left, ctx, Some(builder));

                match operand_type {
                    ValType::F64 => match bin.op {
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
                        _ => func.instruction(&Instruction::I32Const(0)),
                    },
                    ValType::F32 => match bin.op {
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
                        _ => func.instruction(&Instruction::I32Const(0)),
                    },
                    ValType::I64 => match bin.op {
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
                    },
                    _ => match bin.op {
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
                    },
                };
            }
            // For non-binary expressions, fall back to normal generation
            // (they should have been cached if they have side effects)
            _ => {
                self.generate_expr_with_builder(func, expr, ctx, builder);
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
        let mut codegen = Codegen::default();
        codegen.collect_strings(&ast);

        // String literals are collected as-is
        assert_eq!(codegen.string_literals.len(), 1);
        assert_eq!(codegen.string_literals[0], "Hello, world!");
    }

    #[test]
    fn test_generate_binary() {
        // Test with simple code that doesn't require imported modules
        let ast = parse(
            r#"
            fn add(a: i32, b: i32) -> i32 {
                return a + b;
            }

            fn run() {
                let result = add(1, 2);
            }
        "#,
        );

        let _symbols = analyze(&ast);
        let mut codegen = Codegen::default();
        // generate_wasm() automatically validates the output
        let wasm = codegen.generate_wasm(&ast);

        // Verify it starts with Wasm magic number
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");
    }

    #[test]
    fn test_generate_wat() {
        // Test with simple code that doesn't require imported modules
        let ast = parse(
            r#"
            fn add(a: i32, b: i32) -> i32 {
                return a + b;
            }

            fn run() {
                let result = add(1, 2);
            }
        "#,
        );

        let _symbols = analyze(&ast);
        let mut codegen = Codegen::default();
        let wat = codegen.generate_wat(&ast);

        // Verify it produces valid WAT
        assert!(wat.contains("(component"));
    }
}
