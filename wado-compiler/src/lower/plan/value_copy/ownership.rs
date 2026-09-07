//! Return-convention (ownership) analysis over monomorphized TIR (WEP
//! 2026-05-21). A function *returns owned* when every value it can return is
//! freshly materialized rather than a borrowed projection of a `&`/global
//! parameter, so the value-copy fold can consume its result as a move.
//!
//! Runs caller-side at insertion time, so copies are placed precisely instead
//! of copying every call result and recovering it later. That is deliberate: the
//! fold materializes only at owner-entry sites, leaving a mutable-place accessor
//! like `arr[i].field.push(x)` aliased, which copy-on-extract cannot do.

use super::callgraph::CallGraph;
use super::funcset::FuncKeySet;
use super::place::{carries_storage, is_reference};
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    FunctionKind, FunctionRef, ResolvedType, ReturnConvention, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable, matches_builtin,
};
use crate::tir_visitor::TirRefVisitor;

/// Whether a bodyless declaration's result may be storage its caller still
/// owns. Only a reference input can carry storage out, since a by-value one is
/// already deep-copied at the call: `struct_field_get(v: &T, i) -> F` reads a
/// field of what `v` points at, while `array_new(len) -> Array<T>` allocates.
///
/// For a `core:builtin` this is the *obligation*, checked at the declaration —
/// one that answers yes must say which parameter with `#[returns(part_of(p))]`,
/// or `#[returns(owned)]` that it allocates. Here it seeds the bodyless
/// declarations the obligation does not cover, which never hand storage out.
pub fn hands_out_storage(func: &TirFunction, type_table: &TypeTable) -> bool {
    let has_ref_input = func
        .params
        .iter()
        .any(|p| is_reference(p.type_id, type_table));
    has_ref_input && carries_storage(func.return_type, type_table)
}

/// Whether `func` declares `#[returns(owned)]`. Only a declaration with no body
/// is asked: a body is inferred from below, and would be free to contradict
/// what it declared.
fn declares_owned(func: &TirFunction) -> bool {
    func.body.is_none() && func.declared_return_convention == Some(ReturnConvention::Owned)
}

/// What each builtin declared with `#[returns(...)]`, resolved from a call.
/// Reads [`FlatPackage::builtin_return_conventions`], which link snapshots
/// before monomorphization drops the generic declarations.
#[derive(Default)]
pub struct BuiltinConventions(IndexMap<String, ReturnConvention>);

impl BuiltinConventions {
    pub fn collect(project: &FlatPackage) -> Self {
        Self(project.builtin_return_conventions.clone())
    }

    /// The convention declared for `func`, or `None` where it declared none.
    pub fn get(&self, func: &FunctionRef) -> Option<ReturnConvention> {
        self.0
            .iter()
            .find(|(base, _)| matches_builtin(&func.name, func.monomorph_info.as_ref(), base))
            .map(|(_, convention)| *convention)
    }

    /// The parameter a builtin's result is a component of, for a call that
    /// declared `#[returns(part_of(p))]`.
    pub fn part_of(&self, func: &FunctionRef) -> Option<usize> {
        match self.get(func) {
            Some(ReturnConvention::PartOf(param)) => Some(param),
            Some(ReturnConvention::Owned) | None => None,
        }
    }
}

/// Oracle the freshness checker consults for a call's return convention.
pub struct OwnedCalls<'a> {
    returns_owned: &'a FuncKeySet,
    returns_self_projection: &'a FuncKeySet,
    builtin_conventions: &'a BuiltinConventions,
    indirect_owned_returns: Option<&'a IndexSet<TypeId>>,
}

impl<'a> OwnedCalls<'a> {
    pub fn new(
        returns_owned: &'a FuncKeySet,
        returns_self_projection: &'a FuncKeySet,
        builtin_conventions: &'a BuiltinConventions,
    ) -> Self {
        Self {
            returns_owned,
            returns_self_projection,
            builtin_conventions,
            indirect_owned_returns: None,
        }
    }

    /// Attach the indirect-call verdict (see [`compute_indirect_owned_returns`]).
    /// Left off during the return-convention fixpoint, whose own result it is
    /// derived from; the fold attaches it.
    pub fn with_indirect(mut self, indirect_owned_returns: &'a IndexSet<TypeId>) -> Self {
        self.indirect_owned_returns = Some(indirect_owned_returns);
        self
    }

    /// Whether an indirect call yielding `return_type` is owned. Every callable
    /// value is a closure functor by this point, so the question is whether
    /// every closure `__call` of that return type returns owned.
    pub fn indirect_is_owned(&self, return_type: TypeId) -> bool {
        self.indirect_owned_returns
            .is_some_and(|set| set.contains(&return_type))
    }

    /// Whether a call to `func` yields an owned (fresh) value. A `core:builtin`
    /// answers from its declaration: one that hands out an argument's storage
    /// must say `#[returns(part_of(p))]`, so anything that did not is fresh. A
    /// body function is owned iff the fixpoint proved it so, and an extern /
    /// opaque callee defaults to borrowed.
    pub fn is_owned(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_core_builtin() {
            return self.builtin_conventions.part_of(func).is_none();
        }
        self.returns_owned.contains(&func.module_source, &func.name)
    }

    /// Whether `func` returns a projection of its receiver / first parameter
    /// (`build(&self) -> List { return *self }`, an accessor `first(&self)`),
    /// so a call to it is fresh exactly when that receiver is fresh. Only a body
    /// function the fixpoint proved self-projecting qualifies: a bodyless one
    /// either allocates (already owned) or reads through a reference whose
    /// referent is unrelated to the argument's freshness.
    pub fn returns_self_projection(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_builtin() {
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
/// (a *borrowed* projection, through `array_get_value` and nested accessor calls).
/// Because it admits borrowed projections it must NOT feed the move / owned
/// decision; the read-only-share analysis and pattern lowering consume it.
pub fn compute_receiver_alias(
    project: &FlatPackage,
    call_graph: &CallGraph,
    return_paths: &super::place::ReturnPaths,
    type_table: &TypeTable,
    builtins: &BuiltinConventions,
) -> FuncKeySet {
    let mut set = FuncKeySet::default();
    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        if set.contains(&func.module_source, &func.name) {
            return false;
        }
        // Nothing to alias means no conflicts to track.
        if !carries_storage(func.return_type, type_table) {
            return false;
        }
        let Some(body) = &func.body else { return false };
        let hands_out_payload = super::hands_out_payload(&func, return_paths);
        if function_returns_receiver_alias(body, &set, builtins, hands_out_payload) {
            set.insert(func.module_source.clone(), func.name.clone());
            true
        } else {
            false
        }
    });
    set
}

fn function_returns_receiver_alias(
    body: &TirBlock,
    set: &FuncKeySet,
    builtins: &BuiltinConventions,
    hands_out_payload: bool,
) -> bool {
    struct W<'a> {
        set: &'a FuncKeySet,
        builtins: &'a BuiltinConventions,
        hands_out_payload: bool,
        all_alias: bool,
        saw_return: bool,
    }
    impl TirRefVisitor for W<'_> {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
                self.saw_return = true;
                let v = super::analyze::returned_value(v, self.hands_out_payload);
                if !is_receiver_projection(v, 0, self.set, self.builtins) {
                    self.all_alias = false;
                }
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = W {
        set,
        builtins,
        hands_out_payload,
        all_alias: true,
        saw_return: false,
    };
    w.visit_block(body);
    w.saw_return && w.all_alias
}

/// Whether `expr` aliases the storage of parameter `param`: a projection chain,
/// a member read of one, or a call to a receiver-aliasing callee whose
/// receiver / first argument is one.
///
/// A deref may peel only the parameter's own reference, the rule
/// [`is_projection_of_param`] follows. Deeper in the chain it reads a reference
/// *stored in* that storage and lands on whoever lent it, a different place.
/// Answering the wrong place is not the safe side. `last_use::source_path`
/// makes a binding a share candidate gated on the place this names, so a place
/// nothing writes reads as no conflict and the copy is dropped.
fn is_receiver_projection(
    expr: &TirExpr,
    param: u32,
    set: &FuncKeySet,
    builtins: &BuiltinConventions,
) -> bool {
    let recurse = |inner| is_receiver_projection(inner, param, set, builtins);
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == param,
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => matches!(inner.kind, TirExprKind::Local { index, .. } if index == param),
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. } => recurse(inner),
        // A builtin hands out the parameter its `#[returns(part_of(p))]` names,
        // which need not be the first: `struct_field_get(v, i)` reads `v`.
        TirExprKind::Call { func, args, .. } if func.module_source.is_core_builtin() => builtins
            .part_of(func)
            .and_then(|p| args.get(p))
            .is_some_and(|a| recurse(&a.expr)),
        TirExprKind::Call { func, args, .. } if set.contains(&func.module_source, &func.name) => {
            args.first().is_some_and(|a| recurse(&a.expr))
        }
        _ => false,
    }
}

/// The two return conventions, component by component. Seeds the callees that
/// have no body to settle — a value-copy helper clones, a declaration says
/// `#[returns(owned)]`, and a builtin that cannot hand storage out allocates —
/// then settles each strongly connected component of the
/// call graph with its callees already decided: a body function is owned when
/// every value it returns is owned, and self-projecting when every value it
/// returns is owned *or* a projection of its first parameter (`return *self`).
/// `returns_owned` is a subset of `returns_self_projection`.
///
/// Inside a component the answer is assumed and then disproved, not built up: a
/// cycle whose members return each other's result has no base to build from,
/// and the returns that avoid the cycle are what decide it either way.
pub fn compute_return_conventions(
    project: &FlatPackage,
    call_graph: &CallGraph,
    return_paths: &super::place::ReturnPaths,
    builtin_conventions: &BuiltinConventions,
) -> ReturnConventions {
    let type_table = project.type_table.borrow();

    let mut owned = FuncKeySet::default();
    for func in &project.functions {
        let func = func.borrow();
        let is_helper = matches!(func.kind, FunctionKind::ValueCopy { .. });
        let is_builtin = func.module_source.is_builtin();
        if is_helper
            || declares_owned(&func)
            || (is_builtin && !hands_out_storage(&func, &type_table))
        {
            owned.insert(func.module_source.clone(), func.name.clone());
        }
    }
    let mut self_proj = owned.clone();

    for component in call_graph.sccs() {
        settle_component(
            &component,
            project,
            call_graph,
            return_paths,
            &type_table,
            builtin_conventions,
            &mut owned,
            &mut self_proj,
        );
    }

    ReturnConventions {
        returns_owned: owned,
        returns_self_projection: self_proj,
    }
}

/// Settle one component: assume every member holds both conventions, then drop
/// the ones a walk of their returns refutes, re-checking each member that calls
/// a dropped one. Every callee outside the component is already decided, so a
/// member's verdict only ever moves down and the loop terminates.
fn settle_component(
    component: &[u32],
    project: &FlatPackage,
    call_graph: &CallGraph,
    return_paths: &super::place::ReturnPaths,
    type_table: &TypeTable,
    builtin_conventions: &BuiltinConventions,
    owned: &mut FuncKeySet,
    self_proj: &mut FuncKeySet,
) {
    let members: Vec<u32> = component
        .iter()
        .copied()
        .filter(|&id| project.functions[id as usize].borrow().body.is_some())
        .collect();
    if members.is_empty() {
        return;
    }
    let in_component: IndexSet<u32> = members.iter().copied().collect();
    for &id in &members {
        let func = project.functions[id as usize].borrow();
        owned.insert(func.module_source.clone(), func.name.clone());
        self_proj.insert(func.module_source.clone(), func.name.clone());
    }

    let mut queue: std::collections::VecDeque<u32> = members.iter().copied().collect();
    let mut queued: IndexSet<u32> = in_component.clone();
    while let Some(id) = queue.pop_front() {
        queued.swap_remove(&id);
        let func = project.functions[id as usize].borrow();
        let held_owned = owned.contains(&func.module_source, &func.name);
        let held_self_proj = self_proj.contains(&func.module_source, &func.name);
        if !held_owned && !held_self_proj {
            continue;
        }
        let body = func.body.as_ref().expect("members have bodies");
        let (ret_owned, ret_self_proj) = {
            let oracle = OwnedCalls::new(owned, self_proj, builtin_conventions);
            let hands_out_payload = super::hands_out_payload(&func, return_paths);
            function_return_convention(body, &func.params, &oracle, type_table, hands_out_payload)
        };
        let mut dropped = false;
        // `owned` is a subset of `self_proj`: losing the weaker verdict loses
        // the stronger one with it.
        if held_owned && !ret_owned {
            owned.remove(&func.module_source, &func.name);
            dropped = true;
        }
        if held_self_proj && !ret_self_proj {
            self_proj.remove(&func.module_source, &func.name);
            owned.remove(&func.module_source, &func.name);
            dropped = true;
        }
        if !dropped {
            continue;
        }
        for &caller in call_graph.callers_of(id) {
            if in_component.contains(&caller) && queued.insert(caller) {
                queue.push_back(caller);
            }
        }
    }
}

/// Return types for which *every* possible indirect-call target returns owned.
/// `lower::plan::closure` turns every callable value into a functor whose
/// `__call` is an ordinary function, so those are the complete target set, and
/// an indirect call reaches only targets of its own return type. Derived from
/// `returns_owned` after that fixpoint settles, never feeding back into it.
pub fn compute_indirect_owned_returns(
    project: &FlatPackage,
    returns_owned: &FuncKeySet,
) -> IndexSet<TypeId> {
    let mut owned_returns: IndexSet<TypeId> = IndexSet::default();
    let mut borrowed_returns: IndexSet<TypeId> = IndexSet::default();
    for func in &project.functions {
        let func = func.borrow();
        if !func.is_closure_call() {
            continue;
        }
        if returns_owned.contains(&func.module_source, &func.name) {
            owned_returns.insert(func.return_type);
        } else {
            borrowed_returns.insert(func.return_type);
        }
    }
    owned_returns.retain(|ty| !borrowed_returns.contains(ty));
    owned_returns
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
    hands_out_payload: bool,
) -> (bool, bool) {
    let fresh = compute_fresh_locals(body, params, oracle, type_table);
    let mut walker = ReturnWalker {
        fresh: &fresh,
        oracle,
        type_table,
        hands_out_payload,
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
    hands_out_payload: bool,
    all_owned: bool,
    all_owned_or_self_proj: bool,
}

impl TirRefVisitor for ReturnWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
            let v = super::analyze::returned_value(v, self.hands_out_payload);
            if !super::analyze::is_owned_value(v, self.fresh, self.oracle, self.type_table) {
                self.all_owned = false;
                if !is_projection_of_param(v, 0) {
                    self.all_owned_or_self_proj = false;
                }
            }
        }
        self.walk_stmt(stmt);
    }
}

/// True when `expr` is a projection — a field / index / payload / cast chain —
/// rooted at parameter `param`, so it aliases that parameter's storage. A
/// projection of a fresh receiver is itself fresh, which is what lets a call to
/// a self-projecting callee be treated as fresh when its receiver is.
///
/// Only the parameter's own reference may be peeled (`*self`): a deref deeper in
/// the chain reads a reference *stored in* that storage, and its referent is
/// somebody else's — a template shape holds each hole as a `&V` field, so
/// `*v.h0` leaves `v` entirely however fresh `v` is.
pub(super) fn is_projection_of_param(expr: &TirExpr, param: u32) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => *index == param,
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => matches!(inner.kind, TirExprKind::Local { index, .. } if index == param),
        TirExprKind::FieldAccess { expr: inner, .. }
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
