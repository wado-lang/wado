//! Return-convention (ownership) analysis over monomorphized TIR
//! (WEP 2026-05-21, value-copy client).
//!
//! A function *returns owned* when every value it can return is a freshly
//! materialized value — a literal, a construction, a copy clone, a moved local,
//! or the owned result of another call — rather than a *borrowed projection* of
//! a `&`/global parameter (an accessor like `index_value(self: &List, i) -> T {
//! return self.repr[i] }`). The value-copy fold consults this so a call whose
//! callee returns owned is treated as fresh: consuming its result into an owner
//! is a move, no defensive copy needed.
//!
//! This is the caller-side, single-phase replacement for the old
//! `optimize::escape` `returns_fresh` fixpoint. It runs at insertion time
//! (`lower::plan::value_copy`) so the fold inserts copies precisely, rather than
//! copying every call result and recovering it in a later `optimize` pass. It is
//! caller-side by design: the fold only materializes at owner-entry sites, so a
//! mutable-place accessor (`arr[i].field.push(x)`) is never a materialization
//! and its element stays aliased — which the callee-side copy-on-extract model
//! cannot achieve.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FreeFunctionName, FunctionId};
use crate::tir::{
    FunctionKind, FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// The canonical id of a function, matching `nir::FunctionRef::function_id`
/// (module + name; the mangled name is unique post-monomorphization).
pub fn func_key(module: &ModuleSource, name: &str) -> FunctionId {
    FunctionId::Free(FreeFunctionName::from_module_source(module, name))
}

/// Oracle the freshness checker consults for a call's return convention.
pub struct OwnedCalls<'a> {
    returns_owned: &'a IndexSet<FunctionId>,
}

impl<'a> OwnedCalls<'a> {
    pub fn new(returns_owned: &'a IndexSet<FunctionId>) -> Self {
        Self { returns_owned }
    }

    /// Whether a call to `func` yields an owned (fresh) value. A core builtin
    /// allocates or computes a fresh result — except `array_get`, which reads an
    /// element in place and aliases its container. A body function is owned iff
    /// the fixpoint proved it so; extern / opaque callees default to borrowed.
    pub fn is_owned(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
            return func.name != "array_get";
        }
        self.returns_owned
            .contains(&func_key(&func.module_source, &func.name))
    }
}

/// Least fixpoint over "the function returns an owned value". Seeds the
/// always-owned callees (value-copy helpers clone; builtins except `array_get`
/// allocate) and grows: a body function becomes owned once every value it
/// returns is owned given the current owned set.
pub fn compute_returns_owned(project: &FlatPackage) -> IndexSet<FunctionId> {
    let type_table = project.type_table.borrow();

    let mut owned: IndexSet<FunctionId> = IndexSet::default();
    for func in &project.functions {
        let func = func.borrow();
        let is_helper = matches!(func.kind, FunctionKind::ValueCopy { .. });
        let is_builtin = func.module_source.is_core_builtin() || func.module_source.is_wasm_asset();
        if is_helper || (is_builtin && func.name != "array_get") {
            owned.insert(func_key(&func.module_source, &func.name));
        }
    }

    loop {
        let mut newly_owned: Vec<FunctionId> = Vec::new();
        {
            let oracle = OwnedCalls::new(&owned);
            for func in &project.functions {
                let func = func.borrow();
                let key = func_key(&func.module_source, &func.name);
                if owned.contains(&key) {
                    continue;
                }
                let Some(body) = &func.body else {
                    continue;
                };
                let n_params = u32::try_from(func.params.len()).unwrap_or(u32::MAX);
                if function_returns_owned(body, n_params, &oracle, &type_table) {
                    newly_owned.push(key);
                }
            }
        }
        if newly_owned.is_empty() {
            break;
        }
        for key in newly_owned {
            owned.insert(key);
        }
    }

    owned
}

/// Whether every value the function can return is owned, given the callee
/// convention `oracle` and the fresh-local set (Let bindings and match-arm
/// bindings that destructure an owned source).
fn function_returns_owned(
    body: &TirBlock,
    n_params: u32,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let fresh = compute_fresh_locals(body, n_params, oracle, type_table);
    let mut walker = ReturnWalker {
        fresh: &fresh,
        oracle,
        type_table,
        all_owned: true,
    };
    walker.visit_block(body);
    walker.all_owned
}

/// Walk every `return value` and require its operand owned. Only `return`
/// delivers a function's result — Wado value-returning functions always use an
/// explicit `return`. A `break value` is internal to a loop or a labeled-block
/// expression (e.g. the `break: __b` inside a `[1,2,3]` sequence literal that is
/// itself the payload of a returned `Ok(...)`), so its freshness is judged by
/// `is_owned_value` on the enclosing return expression, not here — checking it
/// against the function-level fresh set would spuriously poison the return.
struct ReturnWalker<'a> {
    fresh: &'a IndexSet<u32>,
    oracle: &'a OwnedCalls<'a>,
    type_table: &'a TypeTable,
    all_owned: bool,
}

impl TirRefVisitor for ReturnWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(v) } = &stmt.kind
            && !super::analyze::is_owned_value(v, self.fresh, self.oracle, self.type_table)
        {
            self.all_owned = false;
        }
        self.walk_stmt(stmt);
    }
}

/// Fresh (owned-rooted) non-parameter locals: a `let` bound to an owned value,
/// or a match-arm binding that destructures an owned scrutinee. Optimistic least
/// fixpoint — every bound local starts owned and is dropped once a source proves
/// borrowed (a source may reference another local whose ownership is still
/// shrinking).
fn compute_fresh_locals(
    body: &TirBlock,
    n_params: u32,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> IndexSet<u32> {
    let mut collector = BindingCollector {
        n_params,
        let_sources: IndexMap::default(),
        match_sources: Vec::new(),
    };
    collector.visit_block(body);

    let mut fresh: IndexSet<u32> = collector.let_sources.keys().copied().collect();
    for (local, _) in &collector.match_sources {
        fresh.insert(*local);
    }

    let mut changed = true;
    while changed {
        changed = false;
        for (&local, sources) in &collector.let_sources {
            if fresh.contains(&local)
                && !sources
                    .iter()
                    .all(|s| super::analyze::is_owned_value(s, &fresh, oracle, type_table))
            {
                fresh.swap_remove(&local);
                changed = true;
            }
        }
        for (local, scrut) in &collector.match_sources {
            if fresh.contains(local)
                && !super::analyze::is_owned_value(scrut, &fresh, oracle, type_table)
            {
                fresh.swap_remove(local);
                changed = true;
            }
        }
    }
    fresh
}

/// Collects, per non-parameter local, the value expressions it is bound from:
/// `let` initializers and — for a match-arm binding — the scrutinee it
/// destructures (fresh iff the scrutinee is). The `TirRefVisitor` trait borrows
/// nodes for less than the body's lifetime, so the sources are cloned; this is a
/// one-time analysis pass.
struct BindingCollector {
    n_params: u32,
    let_sources: IndexMap<u32, Vec<TirExpr>>,
    match_sources: Vec<(u32, TirExpr)>,
}

impl TirRefVisitor for BindingCollector {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && *local_index >= self.n_params
        {
            self.let_sources
                .entry(*local_index)
                .or_default()
                .push(value.clone());
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Match { expr: scrut, arms } = &expr.kind {
            for arm in arms {
                let mut binds: IndexSet<u32> = IndexSet::default();
                super::analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
                for b in binds {
                    if b >= self.n_params {
                        self.match_sources.push((b, (**scrut).clone()));
                    }
                }
            }
        }
        self.walk_expr(expr);
    }
}
