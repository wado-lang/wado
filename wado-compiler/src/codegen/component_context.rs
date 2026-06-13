//! Component Model index tracking context.
//!
//! Provides `ComponentModelContext`, which tracks component-level indices
//! (types, instances, core functions, modules) to eliminate hardcoded magic
//! numbers when building Wasm Components with wasm-encoder.

use crate::hashmap::IndexMap;

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
            type_names: IndexMap::default(),
            next_type_idx: 0,
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
