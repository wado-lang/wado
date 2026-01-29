//! Wasm building utilities for code generation
//!
//! This module provides index-tracking wrappers around wasm-encoder types,
//! eliminating hardcoded magic numbers in codegen.

use std::collections::{HashMap, HashSet};

use wasm_encoder::{
    ArrayType, CompositeInnerType, CompositeType, ConstExpr, EntityType, ExportKind, ExportSection,
    FieldType, FunctionSection, GlobalSection, GlobalType, ImportSection, MemoryType, NameMap,
    NameSection, ProducersField, ProducersSection, StorageType, SubType, TypeSection, ValType,
};

// ============================================================================
// Rec Group Types
// ============================================================================

/// Kind of type within a rec group (for mutually recursive types)
#[derive(Debug, Clone)]
pub enum RecTypeKind {
    /// Struct type with fields
    Struct(Vec<FieldType>),
    /// Array type with element type
    Array(FieldType),
}

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
    globals: GlobalSection,
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

    // Global tracking
    global_names: HashMap<String, u32>,
    next_global_idx: u32,

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
            globals: GlobalSection::new(),
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
            global_names: HashMap::new(),
            next_global_idx: 0,
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
    /// Uses `is_final`: false to allow more flexible subtyping with exact types
    pub fn define_gc_struct_type(&mut self, name: &str, fields: &[FieldType]) -> u32 {
        use wasm_encoder::StructType;
        let idx = self.next_type_idx;
        self.types.ty().subtype(&SubType {
            is_final: false, // Non-final allows (ref (exact $T)) to be subtype of (ref $T)
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

    /// Define a GC struct subtype (with a supertype) and return its index
    /// The subtype must include all fields from the supertype as a prefix
    pub fn define_gc_struct_subtype(
        &mut self,
        name: &str,
        supertype_idx: u32,
        fields: &[FieldType],
    ) -> u32 {
        use wasm_encoder::StructType;
        let idx = self.next_type_idx;
        self.types.ty().subtype(&SubType {
            is_final: true, // Subtypes are final (no further subtyping)
            supertype_idx: Some(supertype_idx),
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

    /// Reserve a type index for future use (for forward references in rec groups)
    pub fn reserve_type_idx(&mut self, name: &str) -> u32 {
        let idx = self.next_type_idx;
        self.type_names.insert(name.to_string(), idx);
        idx
    }

    /// Define a rec group containing multiple mutually recursive types.
    /// Types within the rec group can forward-reference each other.
    ///
    /// Each element in `types` is (name, `RecTypeKind`) where `RecTypeKind` specifies
    /// whether it's a struct or array type.
    pub fn define_rec_group(&mut self, types: &[(String, RecTypeKind)]) -> Vec<u32> {
        use wasm_encoder::StructType;

        let base_idx = self.next_type_idx;
        let mut indices = Vec::with_capacity(types.len());

        // Pre-register all type names so they can be looked up during rec group construction
        for (i, (name, _)) in types.iter().enumerate() {
            let idx = base_idx + i as u32;
            self.type_names.insert(name.clone(), idx);
            indices.push(idx);
        }

        // Build SubType list for rec group
        let subtypes: Vec<SubType> = types
            .iter()
            .map(|(_, kind)| match kind {
                RecTypeKind::Struct(fields) => SubType {
                    is_final: false,
                    supertype_idx: None,
                    composite_type: CompositeType {
                        inner: CompositeInnerType::Struct(StructType {
                            fields: fields.clone().into_boxed_slice(),
                        }),
                        shared: false,
                        descriptor: None,
                        describes: None,
                    },
                },
                RecTypeKind::Array(field_type) => SubType {
                    is_final: true,
                    supertype_idx: None,
                    composite_type: CompositeType {
                        inner: CompositeInnerType::Array(ArrayType(*field_type)),
                        shared: false,
                        descriptor: None,
                        describes: None,
                    },
                },
            })
            .collect();

        // Emit the rec group
        self.types.ty().rec(subtypes);

        self.next_type_idx += types.len() as u32;
        indices
    }

    /// Get the next type index without allocating it (for planning rec groups)
    pub fn peek_next_type_idx(&self) -> u32 {
        self.next_type_idx
    }

    /// Import a function and return its function index.
    /// The function type is looked up by name (import name == type name).
    pub fn import_func(&mut self, module: &str, name: &str) -> u32 {
        let type_idx = self.type_idx(name);
        self.imports
            .import(module, name, EntityType::Function(type_idx));
        let func_idx = self.next_func_idx;
        self.func_names.insert(name.to_string(), func_idx);
        self.func_type_names
            .insert(name.to_string(), name.to_string());
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

    /// Try to get type index by name, returns None if not found
    pub fn try_type_idx(&self, name: &str) -> Option<u32> {
        self.type_names.get(name).copied()
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

    /// Define a global variable and return its index
    pub fn define_global(
        &mut self,
        name: &str,
        val_type: ValType,
        mutable: bool,
        init: ConstExpr,
    ) -> u32 {
        let idx = self.next_global_idx;
        let global_type = GlobalType {
            val_type,
            mutable,
            shared: false,
        };
        self.globals.global(global_type, &init);
        self.global_names.insert(name.to_string(), idx);
        self.next_global_idx += 1;
        idx
    }

    /// Get global index by name
    pub fn global_idx(&self, name: &str) -> u32 {
        *self
            .global_names
            .get(name)
            .unwrap_or_else(|| panic!("unknown global: {name}"))
    }

    /// Try to get global index by name, returns None if not found
    pub fn try_global_idx(&self, name: &str) -> Option<u32> {
        self.global_names.get(name).copied()
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

    /// Get the globals section (for module building)
    pub fn globals(&self) -> &GlobalSection {
        &self.globals
    }

    /// Check if any globals have been defined
    pub fn has_globals(&self) -> bool {
        self.next_global_idx > 0
    }

    /// Get the exports section (for module building)
    pub fn exports(&self) -> &ExportSection {
        &self.exports
    }

    /// Build the name section from tracked type and function names
    pub fn build_name_section(&self, module_name: &str) -> NameSection {
        let mut names = NameSection::new();

        // Module name (must come first according to spec)
        names.module(module_name);

        // Function names
        let mut func_names = NameMap::new();
        for (name, &idx) in &self.func_names {
            func_names.append(idx, name);
        }
        names.functions(&func_names);

        // Type names (must come after functions according to spec)
        let mut type_names = NameMap::new();
        for (name, &idx) in &self.type_names {
            type_names.append(idx, name);
        }
        names.types(&type_names);

        names
    }

    /// Build the producers section with language and compiler metadata
    ///
    /// This is a standard custom section that records toolchain information.
    /// See: <https://github.com/WebAssembly/tool-conventions/blob/main/ProducersSection.md>
    #[must_use]
    pub fn build_producers_section() -> ProducersSection {
        let mut language = ProducersField::new();
        language.value("Wado", "");

        let mut processed_by = ProducersField::new();
        processed_by.value(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

        let mut producers = ProducersSection::new();
        producers.field("language", &language);
        producers.field("processed-by", &processed_by);

        producers
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
/// Used alongside wasm-encoder's `ComponentBuilder` to eliminate magic numbers.
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

    /// Get the current component instance count
    pub fn instance_count(&self) -> u32 {
        self.next_instance_idx
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

    /// Check if a core function exists
    pub fn has_core_func(&self, name: &str) -> bool {
        self.core_func_names.contains_key(name)
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

    /// Skip a component-level function index without registering a name.
    /// Use this when builder operations consume a function index (e.g., export).
    pub fn skip_comp_func_idx(&mut self) {
        self.next_comp_func_idx += 1;
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
