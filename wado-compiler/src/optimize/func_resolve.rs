//! Canonical function-identity resolution for the optimizer.
//!
//! One function entity is addressed two ways: structurally at a call site (a
//! [`FunctionRef`], carrying `module_source` + `name` + monomorph/method info)
//! and densely in the function store (its [`FuncId`] index in
//! `NirPackage::functions`). [`FuncResolver`] is the single bridge between them:
//! it maps a function's *canonical mangled identity* (`(module_source,
//! full_name)`) to its [`FuncId`], so a `FunctionRef` resolves to a dense
//! integer once and callers key on the id instead of re-deriving and comparing
//! mangled-name strings.
//!
//! Using `full_name()` — not the bare `name` — makes the identity unique: two
//! same-named methods on different types share a bare `name` but differ in
//! `method_info`, so bare-name keying conflates them. `FuncResolver` keys on the
//! mangled `full_name`, the same identity the static call graph ([`super::gate`])
//! uses, so a method resolves to exactly its own id.
//!
//! This is the resolver brick of the `FuncId` migration (see
//! `docs/wep-2026-06-28-function-identity.md`): a later phase stamps each call
//! node with the id this resolves, so `full_name()` is computed once per call
//! site rather than per analysis lookup.

use cranelift_entity::EntityRef;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::{FuncId, FunctionRef, NirFunction};
use crate::nir_package::NirPackage;

/// `(module_source, full_name) → FuncId` over a package's functions. Built
/// once; resolution is a hash lookup, not a body walk.
pub(super) struct FuncResolver {
    ids: IndexMap<(ModuleSource, String), FuncId>,
}

impl FuncResolver {
    /// Build the resolver from the current function store. `FuncId` is the
    /// index in `project.functions`, matching [`super::gate`]'s call graph.
    pub(super) fn build(project: &NirPackage) -> Self {
        let mut ids = IndexMap::default();
        for (i, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            ids.insert(def_key(&func), FuncId::new(i));
        }
        Self { ids }
    }

    /// Resolve a call-site reference to its function's dense id, or `None` for a
    /// callee outside the package (extern / builtin) — callers stay conservative.
    pub(super) fn resolve(&self, func: &FunctionRef) -> Option<FuncId> {
        self.ids
            .get(&(func.module_source.clone(), func.full_name()))
            .copied()
    }
}

/// The canonical identity key of a defined function.
fn def_key(func: &NirFunction) -> (ModuleSource, String) {
    (
        func.module_source.clone(),
        FunctionRef::from_resolved(func, func.module_source.clone()).full_name(),
    )
}
