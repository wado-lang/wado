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
use super::needs_value_copy;
use super::place::is_reference;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, MonomorphInfo, ResolvedType, ReturnConvention, TirBlock,
    TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
    matches_builtin,
};
use crate::tir_visitor::TirRefVisitor;

/// The array element reads that name a slot of their first argument in place,
/// so a place walk projects one `Index` further in — and, in the seed below,
/// the builtins the `returns_owned` *set* leaves out for the place and modref
/// walks that read it by name.
///
/// Whether a *call* is fresh is not this list's question: [`OwnedCalls::is_owned`]
/// reads that off the call itself, so no builtin can be missing from it.
pub(super) fn is_container_alias_read(name: &str, monomorph_info: Option<&MonomorphInfo>) -> bool {
    matches_builtin(name, monomorph_info, "array_get_value")
        || matches_builtin(name, monomorph_info, "array_get_ref")
        || matches_builtin(name, monomorph_info, "array_get_ref_mut")
}

/// Whether a builtin call's result may be storage its caller still owns. A
/// by-value argument is deep-copied at the call, so only a reference argument
/// can carry storage out: `struct_field_get(v: &T, i) -> F` reads a field of
/// what `v` points at, while `array_new(len) -> Array<T>` allocates and
/// `black_box(value: T) -> T` hands back the copy it was given.
///
/// Read off the call rather than a list of names, so a builtin added later is
/// conservative by default — a missing answer costs a copy, never value
/// semantics. A builtin declaration does not survive to here: monomorphization
/// drops the generic and materializes an instance only for the few a later
/// phase rewrites, so the call is the only thing that always answers.
fn hands_out_arg_storage(result_type: TypeId, args: &[CallArg], type_table: &TypeTable) -> bool {
    let carries_storage =
        is_reference(result_type, type_table) || needs_value_copy(result_type, type_table);
    carries_storage
        && args
            .iter()
            .any(|a| is_reference(a.expr.type_id, type_table))
}

/// Whether `func` declares `#[returns(owned)]`. Only a declaration with no body
/// is asked: a body is inferred from below, and would be free to contradict
/// what it declared.
fn declares_owned(func: &TirFunction) -> bool {
    func.body.is_none() && func.declared_return_convention == Some(ReturnConvention::Owned)
}

/// Whether `func` is a compiler intrinsic rather than a compiled declaration.
fn is_builtin_module(func: &FunctionRef) -> bool {
    func.module_source.is_core_builtin() || func.module_source.is_wasm_asset()
}

/// The builtins that declared `#[returns(owned)]`, by base name: they allocate
/// while reading through a reference, which [`hands_out_arg_storage`] cannot
/// tell from a read of one.
///
/// Best-effort, and safely so — collected from whatever declarations the
/// package still holds, and a name it misses only costs that call a copy.
#[derive(Default)]
pub struct OwnedBuiltins(IndexSet<String>);

impl OwnedBuiltins {
    pub fn collect(project: &FlatPackage) -> Self {
        let mut names = IndexSet::default();
        for func in &project.functions {
            let func = func.borrow();
            if !declares_owned(&func) {
                continue;
            }
            let base = func
                .monomorph_info
                .as_ref()
                .map_or(func.name.as_str(), |m| m.generic_name.as_str());
            names.insert(base.to_string());
        }
        Self(names)
    }

    fn contains(&self, func: &FunctionRef) -> bool {
        self.0
            .iter()
            .any(|base| matches_builtin(&func.name, func.monomorph_info.as_ref(), base))
    }
}

/// Oracle the freshness checker consults for a call's return convention.
pub struct OwnedCalls<'a> {
    returns_owned: &'a FuncKeySet,
    returns_self_projection: &'a FuncKeySet,
    owned_builtins: &'a OwnedBuiltins,
    indirect_owned_returns: Option<&'a IndexSet<TypeId>>,
}

impl<'a> OwnedCalls<'a> {
    pub fn new(
        returns_owned: &'a FuncKeySet,
        returns_self_projection: &'a FuncKeySet,
        owned_builtins: &'a OwnedBuiltins,
    ) -> Self {
        Self {
            returns_owned,
            returns_self_projection,
            owned_builtins,
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

    /// Whether a call to `func` yields an owned (fresh) value. A builtin
    /// answers from the call itself ([`hands_out_arg_storage`]), plus the
    /// declared [`OwnedBuiltins`] exceptions; a body function is owned iff the
    /// fixpoint proved it so, and an extern / opaque callee defaults to
    /// borrowed.
    pub fn is_owned(
        &self,
        func: &FunctionRef,
        result_type: TypeId,
        args: &[CallArg],
        type_table: &TypeTable,
    ) -> bool {
        if is_builtin_module(func) {
            return self.owned_builtins.contains(func)
                || !hands_out_arg_storage(result_type, args, type_table);
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
        if is_builtin_module(func) {
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
) -> FuncKeySet {
    let mut set = FuncKeySet::default();
    call_graph.solve(project, |id| {
        let func = project.functions[id as usize].borrow();
        if set.contains(&func.module_source, &func.name) {
            return false;
        }
        // A `Copy`-shaped return (an `i32` count, say) carries no storage of
        // its own to alias: a caller that reads it gets an independent value,
        // not a projection whose conflicts need tracking.
        if !super::place::is_reference(func.return_type, type_table)
            && !super::needs_value_copy(func.return_type, type_table)
        {
            return false;
        }
        let Some(body) = &func.body else { return false };
        let hands_out_payload = super::hands_out_payload(&func, return_paths);
        if function_returns_receiver_alias(body, &set, hands_out_payload) {
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
    hands_out_payload: bool,
) -> bool {
    struct W<'a> {
        set: &'a FuncKeySet,
        hands_out_payload: bool,
        all_alias: bool,
        saw_return: bool,
    }
    impl TirRefVisitor for W<'_> {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
                self.saw_return = true;
                let v = super::analyze::returned_value(v, self.hands_out_payload);
                if !is_receiver_projection(v, 0, self.set) {
                    self.all_alias = false;
                }
            }
            self.walk_stmt(stmt);
        }
    }
    let mut w = W {
        set,
        hands_out_payload,
        all_alias: true,
        saw_return: false,
    };
    w.visit_block(body);
    w.saw_return && w.all_alias
}

/// Whether `expr` aliases the storage of parameter `param`: a projection chain,
/// an `array_get_value` / `array_get_ref` element read of one, or a call to a
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
        TirExprKind::Call { func, args, .. } if set.contains(&func.module_source, &func.name) => {
            args.first()
                .is_some_and(|a| is_receiver_projection(&a.expr, param, set))
        }
        _ => false,
    }
}

/// The two return conventions, component by component. Seeds the callees that
/// have no body to settle — a value-copy helper clones, a builtin allocates
/// unless it is a container-alias read, and a bodyless declaration may declare
/// `#[returns(owned)]` — then settles each strongly connected component of the
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
    owned_builtins: &OwnedBuiltins,
) -> ReturnConventions {
    let type_table = project.type_table.borrow();

    let mut owned = FuncKeySet::default();
    for func in &project.functions {
        let func = func.borrow();
        let is_helper = matches!(func.kind, FunctionKind::ValueCopy { .. });
        let is_builtin = func.module_source.is_core_builtin() || func.module_source.is_wasm_asset();
        if is_helper
            || declares_owned(&func)
            || (is_builtin && !is_container_alias_read(&func.name, func.monomorph_info.as_ref()))
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
            owned_builtins,
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
    owned_builtins: &OwnedBuiltins,
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
            let oracle = OwnedCalls::new(owned, self_proj, owned_builtins);
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
