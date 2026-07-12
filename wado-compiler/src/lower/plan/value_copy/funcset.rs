//! Borrow-keyed function-identity containers for the value-copy analyses.
//!
//! A function is identified by `(module_source, name)` — the same pair
//! `ownership::func_key` folds into a `FunctionId`. Keying an `IndexMap` /
//! `IndexSet` by `FunctionId` forces every lookup to first *build* that key:
//! clone the `ModuleSource` and heap-allocate `name.to_string()`. In the
//! interprocedural fixpoints and the per-call fold walk that lookup runs on
//! every call node, so the allocation dominates.
//!
//! Splitting the key into a two-level `module -> name -> _` map lets a lookup
//! borrow both halves (`ModuleSource` by reference, `name` as `&str` via
//! `String: Borrow<str>`), so membership tests allocate nothing while staying
//! byte-for-byte equivalent to the `FunctionId`-keyed map.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;

/// A borrow-keyed map from `(module_source, name)` to a dense `u32` id.
#[derive(Default)]
pub struct FuncIndex {
    by_module: IndexMap<ModuleSource, IndexMap<String, u32>>,
}

impl FuncIndex {
    pub fn id(&self, module: &ModuleSource, name: &str) -> Option<u32> {
        self.by_module.get(module)?.get(name).copied()
    }

    pub fn insert(&mut self, module: ModuleSource, name: String, id: u32) {
        self.by_module.entry(module).or_default().insert(name, id);
    }
}

/// A set of functions keyed by `(module_source, name)`, queryable by borrow.
#[derive(Default, Clone, PartialEq)]
pub struct FuncKeySet {
    by_module: IndexMap<ModuleSource, IndexSet<String>>,
}

impl FuncKeySet {
    pub fn contains(&self, module: &ModuleSource, name: &str) -> bool {
        self.by_module
            .get(module)
            .is_some_and(|names| names.contains(name))
    }

    /// Insert `(module, name)`; returns whether it was newly added.
    pub fn insert(&mut self, module: ModuleSource, name: String) -> bool {
        self.by_module.entry(module).or_default().insert(name)
    }
}

/// A map from `(module_source, name)` to `V`, queryable by borrow.
pub struct FuncKeyMap<V> {
    by_module: IndexMap<ModuleSource, IndexMap<String, V>>,
}

impl<V> Default for FuncKeyMap<V> {
    fn default() -> Self {
        Self {
            by_module: IndexMap::default(),
        }
    }
}

impl<V> FuncKeyMap<V> {
    pub fn get(&self, module: &ModuleSource, name: &str) -> Option<&V> {
        self.by_module.get(module)?.get(name)
    }

    pub fn insert(&mut self, module: ModuleSource, name: String, value: V) {
        self.by_module.entry(module).or_default().insert(name, value);
    }

    /// Transform every value, preserving keys.
    pub fn map_values<U>(self, mut f: impl FnMut(V) -> U) -> FuncKeyMap<U> {
        let by_module = self
            .by_module
            .into_iter()
            .map(|(module, names)| {
                (
                    module,
                    names.into_iter().map(|(name, v)| (name, f(v))).collect(),
                )
            })
            .collect();
        FuncKeyMap { by_module }
    }
}
