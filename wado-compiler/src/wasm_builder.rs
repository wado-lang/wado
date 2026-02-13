//! Wasm building utilities for code generation.
//!
//! `CoreModuleBuilder` collects WIR types (not `wasm_encoder` sections) and
//! provides name→index mapping during code generation. After planning and
//! codegen, call `into_parts()` to extract accumulated WIR data for
//! `WirModule` construction.

use indexmap::{IndexMap, IndexSet};

use wasm_encoder::{ExportKind, FieldType, StorageType, ValType};

use crate::wir::{
    WirConstExpr, WirExport, WirGlobal, WirImport, WirNames, WirRecGroupEntry, WirRecGroupKind,
    WirTypeDef,
};

// ============================================================================
// Rec Group Types (used by codegen to specify rec group entries)
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
// WirModuleParts — extracted data from CoreModuleBuilder
// ============================================================================

/// Accumulated WIR data extracted from `CoreModuleBuilder` for `WirModule` construction.
pub struct WirModuleParts {
    pub types: Vec<WirTypeDef>,
    pub imports: Vec<WirImport>,
    pub func_type_indices: Vec<u32>,
    pub globals: Vec<WirGlobal>,
    pub exports: Vec<WirExport>,
    pub import_func_count: u32,
    pub has_memory: bool,
    pub names: WirNames,
}

// ============================================================================
// CoreModuleBuilder — collects WIR types with name→index mapping
// ============================================================================

/// Builder for Wasm core modules with dynamic index allocation.
///
/// Collects WIR types and provides name→index mapping during code generation.
/// Does not touch `wasm_encoder` sections — that's `emit_module`'s job.
pub struct CoreModuleBuilder {
    // Accumulated WIR data
    types: Vec<WirTypeDef>,
    imports: Vec<WirImport>,
    func_type_indices: Vec<u32>,
    globals: Vec<WirGlobal>,
    exports: Vec<WirExport>,

    // Type tracking (name → index)
    type_names: IndexMap<String, u32>,
    next_type_idx: u32,

    // Function tracking (name → index)
    func_names: IndexMap<String, u32>,
    func_type_names: IndexMap<String, String>,
    type_has_return: IndexMap<String, bool>,
    type_return_type: IndexMap<String, ValType>,
    next_func_idx: u32,
    /// Number of imported functions (for branch hint calculation)
    pub import_func_count: u32,

    // Global tracking (name → index)
    global_names: IndexMap<String, u32>,
    next_global_idx: u32,
    /// Globals that are initialized with null (need `ref.as_non_null` on access)
    nullable_globals: IndexSet<String>,

    // Memory tracking
    pub has_memory: bool,

    // Access control: functions from core:internal (require explicit import)
    pub internal_functions: IndexSet<String>,
}

impl CoreModuleBuilder {
    /// Create a new builder with all indices starting at 0
    pub fn new() -> Self {
        Self {
            types: Vec::new(),
            imports: Vec::new(),
            func_type_indices: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            type_names: IndexMap::new(),
            next_type_idx: 0,
            func_names: IndexMap::new(),
            func_type_names: IndexMap::new(),
            type_has_return: IndexMap::new(),
            type_return_type: IndexMap::new(),
            next_func_idx: 0,
            import_func_count: 0,
            global_names: IndexMap::new(),
            next_global_idx: 0,
            nullable_globals: IndexSet::new(),
            has_memory: false,
            internal_functions: IndexSet::new(),
        }
    }

    /// Mark a function as being from core:internal (requires explicit import to call)
    pub fn mark_as_internal(&mut self, name: &str) {
        self.internal_functions.insert(name.to_string());
    }

    /// Define a function type and return its index
    pub fn define_func_type(&mut self, name: &str, params: &[ValType], results: &[ValType]) -> u32 {
        let idx = self.next_type_idx;
        self.types.push(WirTypeDef::Func {
            params: params.to_vec(),
            results: results.to_vec(),
        });
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
        self.types.push(WirTypeDef::GcArray { element, mutable });
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Define a GC struct type and return its index.
    /// Uses `is_final: false` to allow more flexible subtyping with exact types.
    pub fn define_gc_struct_type(&mut self, name: &str, fields: &[FieldType]) -> u32 {
        let idx = self.next_type_idx;
        self.types.push(WirTypeDef::GcStruct {
            fields: fields.to_vec(),
            is_final: false, // Non-final allows (ref (exact $T)) to be subtype of (ref $T)
            supertype_idx: None,
        });
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Define a GC struct subtype (with a supertype) and return its index.
    /// The subtype must include all fields from the supertype as a prefix.
    pub fn define_gc_struct_subtype(
        &mut self,
        name: &str,
        supertype_idx: u32,
        fields: &[FieldType],
    ) -> u32 {
        let idx = self.next_type_idx;
        self.types.push(WirTypeDef::GcStruct {
            fields: fields.to_vec(),
            is_final: true, // Subtypes are final (no further subtyping)
            supertype_idx: Some(supertype_idx),
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
        let base_idx = self.next_type_idx;
        let mut indices = Vec::with_capacity(types.len());

        // Pre-register all type names so they can be looked up during rec group construction
        for (i, (name, _)) in types.iter().enumerate() {
            let idx = base_idx + i as u32;
            self.type_names.insert(name.clone(), idx);
            indices.push(idx);
        }

        // Build WIR rec group entries
        let entries: Vec<WirRecGroupEntry> = types
            .iter()
            .map(|(_, kind)| WirRecGroupEntry {
                kind: match kind {
                    RecTypeKind::Struct(fields) => WirRecGroupKind::Struct(fields.clone()),
                    RecTypeKind::Array(field_type) => WirRecGroupKind::Array(*field_type),
                },
            })
            .collect();

        self.types.push(WirTypeDef::RecGroup(entries));
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
        self.imports.push(WirImport::Func {
            module: module.to_string(),
            name: name.to_string(),
            type_idx,
        });
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
        self.imports.push(WirImport::Memory {
            module: module.to_string(),
            name: name.to_string(),
            min,
        });
        self.has_memory = true;
    }

    /// Define a function (adds to function section) and return its index
    pub fn define_func(&mut self, name: &str, type_name: &str) -> u32 {
        let type_idx = self.type_idx(type_name);
        self.func_type_indices.push(type_idx);
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
        self.exports.push(WirExport {
            name: export_name.to_string(),
            kind: ExportKind::Func,
            index: func_idx,
        });
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
    ///
    /// If `is_nullable` is true, the global is marked as requiring `ref.as_non_null`
    /// when accessed (for lazy-initialized reference type globals).
    pub fn define_global(
        &mut self,
        name: &str,
        val_type: ValType,
        mutable: bool,
        init: WirConstExpr,
        is_nullable: bool,
    ) -> u32 {
        let idx = self.next_global_idx;
        self.globals.push(WirGlobal {
            name: name.to_string(),
            val_type,
            mutable,
            init,
        });
        self.global_names.insert(name.to_string(), idx);
        if is_nullable {
            self.nullable_globals.insert(name.to_string());
        }
        self.next_global_idx += 1;
        idx
    }

    /// Check if a global was defined as nullable (needs `ref.as_non_null` on access)
    pub fn is_nullable_global(&self, name: &str) -> bool {
        self.nullable_globals.contains(name)
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

    /// Check if any globals have been defined
    pub fn has_globals(&self) -> bool {
        self.next_global_idx > 0
    }

    /// Check if a function type has a return value
    pub fn type_has_return(&self, type_name: &str) -> Option<bool> {
        self.type_has_return.get(type_name).copied()
    }

    /// Get the return type of a function type
    pub fn type_return_type(&self, type_name: &str) -> Option<ValType> {
        self.type_return_type.get(type_name).copied()
    }

    /// Get the type name for a function
    pub fn func_type_name(&self, func_name: &str) -> Option<&str> {
        self.func_type_names.get(func_name).map(String::as_str)
    }

    /// Extract accumulated WIR data for `WirModule` construction.
    ///
    /// Consumes the builder. After this call, use the returned `WirModuleParts`
    /// to construct a `WirModule` together with function bodies.
    pub fn into_parts(self) -> WirModuleParts {
        let names = WirNames {
            func_names: self
                .func_names
                .into_iter()
                .map(|(name, idx)| (idx, name))
                .collect(),
            type_names: self
                .type_names
                .into_iter()
                .map(|(name, idx)| (idx, name))
                .collect(),
        };
        WirModuleParts {
            types: self.types,
            imports: self.imports,
            func_type_indices: self.func_type_indices,
            globals: self.globals,
            exports: self.exports,
            import_func_count: self.import_func_count,
            has_memory: self.has_memory,
            names,
        }
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
    type_names: IndexMap<String, u32>,
    next_type_idx: u32,

    // Component instance indices
    instance_names: IndexMap<String, u32>,
    next_instance_idx: u32,

    // Core function indices (at component level - aliased/lowered functions)
    core_func_names: IndexMap<String, u32>,
    next_core_func_idx: u32,

    // Core memory index
    core_memory_idx: Option<u32>,

    // Component-level function indices (lifted functions)
    comp_func_names: IndexMap<String, u32>,
    next_comp_func_idx: u32,

    // Core module indices
    core_module_names: IndexMap<String, u32>,
    next_core_module_idx: u32,

    // Core instance indices
    core_instance_names: IndexMap<String, u32>,
    next_core_instance_idx: u32,
}

impl ComponentModelContext {
    /// Create a new context with all indices starting at 0
    pub fn new() -> Self {
        Self {
            type_names: IndexMap::new(),
            next_type_idx: 0,
            instance_names: IndexMap::new(),
            next_instance_idx: 0,
            core_func_names: IndexMap::new(),
            next_core_func_idx: 0,
            core_memory_idx: None,
            comp_func_names: IndexMap::new(),
            next_comp_func_idx: 0,
            core_module_names: IndexMap::new(),
            next_core_module_idx: 0,
            core_instance_names: IndexMap::new(),
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

    /// Check if a component type exists
    pub fn has_type(&self, name: &str) -> bool {
        self.type_names.contains_key(name)
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

    /// Register an additional name for an existing component-level function.
    /// This does NOT consume a new component function index.
    pub fn alias_comp_func(&mut self, existing_name: &str, alias_name: &str) {
        let idx = self.comp_func_idx(existing_name);
        self.comp_func_names.insert(alias_name.to_string(), idx);
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
