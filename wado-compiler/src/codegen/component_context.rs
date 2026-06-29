//! Component Model index tracking context.
//!
//! Provides `ComponentModelContext`, which tracks component-level indices
//! (types, instances, core functions, modules) to eliminate hardcoded magic
//! numbers when building Wasm Components with wasm-encoder.

use wasm_encoder::PrimitiveValType;

use crate::hashmap::IndexMap;

/// Structural key for a defined Component Model type, so a given structure
/// resolves to one interned type index regardless of how many producers ask for
/// it (e.g. the world-export lift and the `Client` import both need
/// `result<own<response>, error-code>`). A [`CmTypeKey::Leaf`] references an
/// already-emitted index and never emits.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum CmTypeKey {
    Leaf(u32),
    /// A primitive value type (or `string`), emitted inline — never a defined type.
    Primitive(PrimitiveValType),
    Own(Box<CmTypeKey>),
    Option(Box<CmTypeKey>),
    Result {
        ok: Option<Box<CmTypeKey>>,
        err: Option<Box<CmTypeKey>>,
    },
    Future(Box<CmTypeKey>),
    Stream(Box<CmTypeKey>),
    List(Box<CmTypeKey>),
    Tuple(Vec<CmTypeKey>),
}

/// Tracks component-level indices for types, instances, and core functions.
/// Used alongside wasm-encoder's `ComponentBuilder` to eliminate magic numbers.
pub struct ComponentModelContext {
    // Component type indices
    type_names: IndexMap<String, u32>,
    next_type_idx: u32,

    cm_type_interner: IndexMap<CmTypeKey, u32>,

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

    /// Named CM types `(cm_name, type_idx)` that a `--lib` default-interface
    /// export defines top-level, collected by `emit_world_exports` and
    /// re-exported into the default-interface instance by
    /// `append_interface_instance_exports`. Empty for the WASI worlds, whose
    /// interface types come from imported instances.
    lib_export_types: Vec<(String, u32)>,
}

impl ComponentModelContext {
    /// Create a new context with all indices starting at 0
    pub fn new() -> Self {
        Self {
            type_names: IndexMap::default(),
            next_type_idx: 0,
            cm_type_interner: IndexMap::default(),
            instance_names: IndexMap::default(),
            next_instance_idx: 0,
            core_func_names: IndexMap::default(),
            next_core_func_idx: 0,
            core_memory_idx: None,
            comp_func_names: IndexMap::default(),
            next_comp_func_idx: 0,
            core_module_names: IndexMap::default(),
            next_core_module_idx: 0,
            core_instance_names: IndexMap::default(),
            next_core_instance_idx: 0,
            lib_export_types: Vec::new(),
        }
    }

    /// Record a named CM type defined by a `--lib` default-interface export, to
    /// be re-exported into the interface instance.
    pub fn push_lib_export_type(&mut self, cm_name: String, idx: u32) {
        self.lib_export_types.push((cm_name, idx));
    }

    /// The named CM types defined by `--lib` default-interface exports.
    pub fn lib_export_types(&self) -> &[(String, u32)] {
        &self.lib_export_types
    }

    /// Register a component type and return its index
    pub fn register_type(&mut self, name: &str) -> u32 {
        let idx = self.next_type_idx;
        self.type_names.insert(name.to_string(), idx);
        self.next_type_idx += 1;
        idx
    }

    /// Reserve a component type index without binding a name.
    pub fn register_anon_type(&mut self) -> u32 {
        let idx = self.next_type_idx;
        self.next_type_idx += 1;
        idx
    }

    pub fn intern_lookup(&self, key: &CmTypeKey) -> Option<u32> {
        self.cm_type_interner.get(key).copied()
    }

    pub fn intern_record(&mut self, key: CmTypeKey, idx: u32) {
        self.cm_type_interner.insert(key, idx);
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

    /// Check whether a component instance is registered under `name`.
    pub fn has_instance(&self, name: &str) -> bool {
        self.instance_names.contains_key(name)
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
