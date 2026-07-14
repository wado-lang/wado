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

use super::callgraph::CallGraph;
use super::funcset::FuncKeySet;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    FunctionKind, FunctionRef, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind,
    TirStmt, TirStmtKind, TirUnaryOp, TypeTable, matches_builtin,
};
use crate::tir_visitor::TirRefVisitor;

fn is_container_alias_read(name: &str, monomorph_info: Option<&MonomorphInfo>) -> bool {
    matches_builtin(name, monomorph_info, "array_get")
        || matches_builtin(name, monomorph_info, "array_get_ref")
}

/// Oracle the freshness checker consults for a call's return convention.
pub struct OwnedCalls<'a> {
    returns_owned: &'a FuncKeySet,
    returns_self_projection: &'a FuncKeySet,
}

impl<'a> OwnedCalls<'a> {
    pub fn new(returns_owned: &'a FuncKeySet, returns_self_projection: &'a FuncKeySet) -> Self {
        Self {
            returns_owned,
            returns_self_projection,
        }
    }

    /// Whether a call to `func` yields an owned (fresh) value. A core builtin
    /// allocates or computes a fresh result — except the container-alias reads
    /// `array_get` / `array_get_ref`, which borrow an element in place. A body
    /// function is owned iff the fixpoint proved it so; extern / opaque callees
    /// default to borrowed.
    pub fn is_owned(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
            return !is_container_alias_read(&func.name, func.monomorph_info.as_ref());
        }
        self.returns_owned.contains(&func.module_source, &func.name)
    }

    /// Whether `func` returns a projection of its receiver / first parameter
    /// (`build(&self) -> List { return *self }`, an accessor `first(&self)`),
    /// so a call to it is fresh exactly when that receiver is fresh. Builtins
    /// are already owned (`is_owned`); only body functions the fixpoint proved
    /// self-projecting qualify.
    pub fn returns_self_projection(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
            return false;
        }
        self.returns_self_projection
            .contains(&func.module_source, &func.name)
    }
}

/// Per-function return conventions the fold consults: `returns_owned` (every
/// returned value is freshly materialized) and its superset
/// `returns_self_projection` (every returned value is owned *or* a projection of
/// the receiver / first parameter).
pub struct ReturnConventions {
    pub returns_owned: FuncKeySet,
    pub returns_self_projection: FuncKeySet,
}

/// Functions whose every value-return aliases the receiver / first parameter
/// (a *borrowed* projection, through `array_get` and nested accessor calls).
/// Because it admits borrowed projections it must NOT feed the move/owned
/// decision; only the read-only-share analysis consumes it.
pub fn compute_receiver_alias(project: &FlatPackage) -> FuncKeySet {
    let call_graph = CallGraph::build(project);
    let mut set = FuncKeySet::default();
    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        if set.contains(&func.module_source, &func.name) {
            return false;
        }
        let Some(body) = &func.body else { return false };
        if function_returns_receiver_alias(body, &set) {
            set.insert(func.module_source.clone(), func.name.clone());
            true
        } else {
            false
        }
    });
    set
}

fn function_returns_receiver_alias(body: &TirBlock, set: &FuncKeySet) -> bool {
    struct W<'a> {
        set: &'a FuncKeySet,
        all_alias: bool,
        saw_return: bool,
    }
    impl TirRefVisitor for W<'_> {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
                self.saw_return = true;
                if !is_receiver_projection(v, 0, self.set) {
                    self.all_alias = false;
                }
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = W {
        set,
        all_alias: true,
        saw_return: false,
    };
    w.visit_block(body);
    w.saw_return && w.all_alias
}

/// Whether `expr` aliases the storage of parameter `param`: a projection chain,
/// an `array_get` / `array_get_ref` element read of one, or a call to a
/// receiver-aliasing callee whose receiver / first argument is one.
fn is_receiver_projection(expr: &TirExpr, param: u32, set: &FuncKeySet) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == param,
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. } => is_receiver_projection(inner, param, set),
        TirExprKind::Call { func, args, .. }
            if func.module_source.is_core_builtin()
                && is_container_alias_read(&func.name, func.monomorph_info.as_ref()) =>
        {
            args.first()
                .is_some_and(|a| is_receiver_projection(&a.expr, param, set))
        }
        TirExprKind::MethodCall { func, receiver, .. }
            if set.contains(&func.module_source, &func.name) =>
        {
            is_receiver_projection(receiver, param, set)
        }
        TirExprKind::Call { func, args, .. } if set.contains(&func.module_source, &func.name) => {
            args.first()
                .is_some_and(|a| is_receiver_projection(&a.expr, param, set))
        }
        _ => false,
    }
}

/// Least fixpoint over the two return conventions. Seeds the always-owned
/// callees (value-copy helpers clone; builtins except the container-alias
/// reads `array_get` / `array_get_ref` allocate) and grows: a body function
/// becomes owned once every value it returns is owned,
/// and self-projecting once every value it returns is owned *or* a projection of
/// its first parameter (`return *self`). `returns_owned` is a subset of
/// `returns_self_projection`.
pub fn compute_return_conventions(project: &FlatPackage) -> ReturnConventions {
    let type_table = project.type_table.borrow();

    let mut owned = FuncKeySet::default();
    for func in &project.functions {
        let func = func.borrow();
        let is_helper = matches!(func.kind, FunctionKind::ValueCopy { .. });
        let is_builtin = func.module_source.is_core_builtin() || func.module_source.is_wasm_asset();
        if is_helper
            || (is_builtin && !is_container_alias_read(&func.name, func.monomorph_info.as_ref()))
        {
            owned.insert(func.module_source.clone(), func.name.clone());
        }
    }
    let mut self_proj = owned.clone();

    let call_graph = CallGraph::build(project);
    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        let already_owned = owned.contains(&func.module_source, &func.name);
        let already_self_proj = self_proj.contains(&func.module_source, &func.name);
        if already_owned && already_self_proj {
            return false;
        }
        let Some(body) = &func.body else {
            return false;
        };
        let (ret_owned, ret_self_proj) = {
            let oracle = OwnedCalls::new(&owned, &self_proj);
            function_return_convention(body, &func.params, &oracle, &type_table)
        };
        let mut changed = false;
        if !already_owned && ret_owned {
            self_proj.insert(func.module_source.clone(), func.name.clone());
            owned.insert(func.module_source.clone(), func.name.clone());
            changed = true;
        }
        if !already_self_proj && ret_self_proj {
            self_proj.insert(func.module_source.clone(), func.name.clone());
            changed = true;
        }
        changed
    });

    ReturnConventions {
        returns_owned: owned,
        returns_self_projection: self_proj,
    }
}

/// The function's `(returns_owned, returns_self_projection)` convention: whether
/// every returned value is owned, and whether every returned value is owned *or*
/// a projection of the first parameter (`return *self`). Judged against the
/// callee convention `oracle` and the fresh-local set (Let bindings and
/// match-arm bindings that destructure an owned source).
fn function_return_convention(
    body: &TirBlock,
    params: &[crate::tir::TirParam],
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> (bool, bool) {
    let fresh = compute_fresh_locals(body, params, oracle, type_table);
    let mut walker = ReturnWalker {
        fresh: &fresh,
        oracle,
        type_table,
        all_owned: true,
        all_owned_or_self_proj: true,
    };
    walker.visit_block(body);
    (walker.all_owned, walker.all_owned_or_self_proj)
}

/// Walk every `return value` and classify its operand: owned, or a projection of
/// the first parameter (`return *self`). Only `return` delivers a function's
/// result — Wado value-returning functions always use an explicit `return`. A
/// `break value` is internal to a loop or a labeled-block expression (e.g. the
/// `break: __b` inside a `[1,2,3]` sequence literal that is itself the payload of
/// a returned `Ok(...)`), so its freshness is judged by `is_owned_value` on the
/// enclosing return expression, not here — checking it against the
/// function-level fresh set would spuriously poison the return.
struct ReturnWalker<'a> {
    fresh: &'a IndexSet<u32>,
    oracle: &'a OwnedCalls<'a>,
    type_table: &'a TypeTable,
    all_owned: bool,
    all_owned_or_self_proj: bool,
}

impl TirRefVisitor for ReturnWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(v) } = &stmt.kind
            && !super::analyze::is_owned_value(v, self.fresh, self.oracle, self.type_table)
        {
            self.all_owned = false;
            if !is_projection_of_param(v, 0) {
                self.all_owned_or_self_proj = false;
            }
        }
        self.walk_stmt(stmt);
    }
}

/// True when `expr` is a projection — a `deref` / field / index / payload / cast
/// chain — rooted at parameter `param`, so it aliases that parameter's storage.
/// A projection of a fresh receiver is itself fresh, which is what lets a call to
/// a self-projecting callee be treated as fresh when its receiver is.
pub(super) fn is_projection_of_param(expr: &TirExpr, param: u32) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == param,
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. } => is_projection_of_param(inner, param),
        _ => false,
    }
}

/// Fresh (owned-rooted) non-parameter locals: a `let` bound to an owned value,
/// or a match-arm binding that destructures an owned scrutinee. Optimistic least
/// fixpoint — every bound local starts owned and is dropped once a source proves
/// borrowed (a source may reference another local whose ownership is still
/// shrinking).
fn compute_fresh_locals(
    body: &TirBlock,
    params: &[crate::tir::TirParam],
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> IndexSet<u32> {
    let n_params = u32::try_from(params.len()).unwrap_or(u32::MAX);
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
    // A by-value parameter is owned: returning it is owned because a returned
    // parameter is never confined, so the caller always copies it in.
    for p in params {
        if !matches!(
            type_table.get(p.type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        ) {
            fresh.insert(p.local_index);
        }
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
