//! Wasm building utilities for code generation
//!
//! This module provides index-tracking wrappers around wasm-encoder types,
//! eliminating hardcoded magic numbers in codegen.

use std::collections::{HashMap, HashSet};

use wasm_encoder::{
    ArrayType, CompositeInnerType, CompositeType, EntityType, ExportKind, ExportSection, FieldType,
    FunctionSection, HeapType, ImportSection, MemoryType, NameMap, NameSection, RefType,
    StorageType, SubType, TypeSection, ValType,
};

// ============================================================================
// CoreModuleBuilder - Builder for Wasm core modules with dynamic index allocation
// ============================================================================

/// Builder for Wasm core modules with dynamic index allocation.
/// Eliminates hardcoded type/function indices by tracking them by name.
pub struct CoreModuleBuilder {
    // Wasm sections
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    #[allow(dead_code)]
    names: NameSection,

    // Type tracking
    type_names: HashMap<String, u32>,
    next_type_idx: u32,

    // Function tracking
    func_names: HashMap<String, u32>,
    func_type_names: HashMap<String, String>,
    type_has_return: HashMap<String, bool>,
    type_return_type: HashMap<String, ValType>,
    next_func_idx: u32,
    /// Number of imported functions (for branch hint calculation)
    pub import_func_count: u32,

    // Memory tracking
    pub has_memory: bool,

    // Access control: functions from core:internal (require explicit import)
    pub internal_functions: HashSet<String>,
}

impl CoreModuleBuilder {
    /// Create a new builder with all indices starting at 0
    pub fn new() -> Self {
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
            internal_functions: HashSet::new(),
        }
    }

    /// Mark a function as being from core:internal (requires explicit import to call)
    pub fn mark_as_internal(&mut self, name: &str) {
        self.internal_functions.insert(name.to_string());
    }

    /// Define a function type and return its index
    pub fn define_func_type(&mut self, name: &str, params: &[ValType], results: &[ValType]) -> u32 {
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
    pub fn define_gc_array_type(&mut self, name: &str, element: StorageType, mutable: bool) -> u32 {
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

    /// Define a GC struct type and return its index
    pub fn define_gc_struct_type(&mut self, name: &str, fields: &[FieldType]) -> u32 {
        use wasm_encoder::StructType;
        let idx = self.next_type_idx;
        self.types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Struct(StructType {
                    fields: fields.to_vec().into_boxed_slice(),
                }),
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
    pub fn import_func(&mut self, module: &str, name: &str, type_name: &str) -> u32 {
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
    pub fn import_memory(&mut self, module: &str, name: &str, min: u64) {
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
    pub fn define_func(&mut self, name: &str, type_name: &str) -> u32 {
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
    pub fn define_func_alias(&mut self, alias_name: &str, func_idx: u32) {
        self.func_names.insert(alias_name.to_string(), func_idx);
    }

    /// Export a function
    pub fn export_func(&mut self, export_name: &str, func_name: &str) {
        let func_idx = self.func_idx(func_name);
        self.exports.export(export_name, ExportKind::Func, func_idx);
    }

    /// Get type index by name
    pub fn type_idx(&self, name: &str) -> u32 {
        *self
            .type_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown type: {name}"))
    }

    /// Get function index by name
    pub fn func_idx(&self, name: &str) -> u32 {
        *self
            .func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown function: {name}"))
    }

    /// Try to get function index by name, returns None if not found
    pub fn try_func_idx(&self, name: &str) -> Option<u32> {
        self.func_names.get(name).copied()
    }

    /// Get all registered function names (for debugging)
    #[allow(dead_code)]
    pub fn func_names_iter(&self) -> impl Iterator<Item = &String> {
        self.func_names.keys()
    }

    /// Add a function name to the name section (names are automatically tracked)
    #[allow(dead_code)]
    pub fn add_func_name(&mut self, _func_name: &str) {
        // Names are tracked in func_names during define_func/import_func
        // build_name_section() uses func_names to build the name section
    }

    /// Get access to the types section for complex type definitions
    #[allow(dead_code)]
    pub fn types_mut(&mut self) -> &mut TypeSection {
        &mut self.types
    }

    /// Get access to the imports section
    #[allow(dead_code)]
    pub fn imports_mut(&mut self) -> &mut ImportSection {
        &mut self.imports
    }

    /// Get access to the functions section
    #[allow(dead_code)]
    pub fn functions_mut(&mut self) -> &mut FunctionSection {
        &mut self.functions
    }

    /// Get access to the exports section
    #[allow(dead_code)]
    pub fn exports_mut(&mut self) -> &mut ExportSection {
        &mut self.exports
    }

    /// Get the types section (for module building)
    pub fn types(&self) -> &TypeSection {
        &self.types
    }

    /// Get the imports section (for module building)
    pub fn imports(&self) -> &ImportSection {
        &self.imports
    }

    /// Get the functions section (for module building)
    pub fn functions(&self) -> &FunctionSection {
        &self.functions
    }

    /// Get the exports section (for module building)
    pub fn exports(&self) -> &ExportSection {
        &self.exports
    }

    /// Build the name section from tracked function names
    pub fn build_name_section(&self) -> NameSection {
        let mut names = NameSection::new();
        let mut func_names = NameMap::new();
        for (name, &idx) in &self.func_names {
            func_names.append(idx, name);
        }
        names.functions(&func_names);
        names
    }

    /// Create a RefType for the string array (GC array<u8>)
    pub fn string_ref_type(&self) -> RefType {
        RefType {
            nullable: false,
            heap_type: HeapType::Concrete(self.type_idx("string-array")),
        }
    }

    /// Create a ValType for the string array (GC array<u8>)
    #[allow(dead_code)]
    pub fn string_val_type(&self) -> ValType {
        ValType::Ref(self.string_ref_type())
    }
}

impl Default for CoreModuleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// ComponentModelContext - Tracks component-level indices
// ============================================================================

/// Tracks component-level indices for types, instances, and core functions.
/// Used alongside wasm-encoder's ComponentBuilder to eliminate magic numbers.
pub struct ComponentModelContext {
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

impl ComponentModelContext {
    /// Create a new context with all indices starting at 0
    pub fn new() -> Self {
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
    pub fn register_type(&mut self, name: &str) -> u32 {
        let idx = self.next_type_idx;
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Get component type index by name
    pub fn type_idx(&self, name: &str) -> u32 {
        *self
            .type_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component type: {name}"))
    }

    /// Register a component instance and return its index
    pub fn register_instance(&mut self, name: &str) -> u32 {
        let idx = self.next_instance_idx;
        self.instance_names.insert(name.to_string(), idx);
        self.next_instance_idx += 1;
        idx
    }

    /// Get component instance index by name
    pub fn instance_idx(&self, name: &str) -> u32 {
        *self
            .instance_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component instance: {name}"))
    }

    /// Register a core function (at component level) and return its index
    pub fn register_core_func(&mut self, name: &str) -> u32 {
        let idx = self.next_core_func_idx;
        self.core_func_names.insert(name.to_string(), idx);
        self.next_core_func_idx += 1;
        idx
    }

    /// Get core function index by name
    pub fn core_func_idx(&self, name: &str) -> u32 {
        *self
            .core_func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core function: {name}"))
    }

    /// Set the core memory index
    pub fn set_memory(&mut self, idx: u32) {
        self.core_memory_idx = Some(idx);
    }

    /// Get the core memory index
    pub fn memory_idx(&self) -> u32 {
        self.core_memory_idx.expect("memory not set")
    }

    /// Register a component-level function (lifted) and return its index
    pub fn register_comp_func(&mut self, name: &str) -> u32 {
        let idx = self.next_comp_func_idx;
        self.comp_func_names.insert(name.to_string(), idx);
        self.next_comp_func_idx += 1;
        idx
    }

    /// Get component-level function index by name
    pub fn comp_func_idx(&self, name: &str) -> u32 {
        *self
            .comp_func_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown component function: {name}"))
    }

    /// Check if a component-level function exists
    pub fn has_comp_func(&self, name: &str) -> bool {
        self.comp_func_names.contains_key(name)
    }

    /// Register a core module and return its index
    pub fn register_core_module(&mut self, name: &str) -> u32 {
        let idx = self.next_core_module_idx;
        self.core_module_names.insert(name.to_string(), idx);
        self.next_core_module_idx += 1;
        idx
    }

    /// Get core module index by name
    pub fn core_module_idx(&self, name: &str) -> u32 {
        *self
            .core_module_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core module: {name}"))
    }

    /// Register a core instance and return its index
    pub fn register_core_instance(&mut self, name: &str) -> u32 {
        let idx = self.next_core_instance_idx;
        self.core_instance_names.insert(name.to_string(), idx);
        self.next_core_instance_idx += 1;
        idx
    }

    /// Get core instance index by name
    pub fn core_instance_idx(&self, name: &str) -> u32 {
        *self
            .core_instance_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown core instance: {name}"))
    }
}

impl Default for ComponentModelContext {
    fn default() -> Self {
        Self::new()
    }
}
