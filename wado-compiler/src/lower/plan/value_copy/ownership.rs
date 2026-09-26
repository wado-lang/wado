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
use super::confine::ConfinedParams;
use super::funcset::{FuncKeyMap, FuncKeySet};
use super::place::{carries_storage, is_reference, may_carry_storage, param_position};
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::analyze::{
    builtin_projected_params, is_owned_value, returned_value,
};
use crate::lower::plan::value_copy::place::ReturnPaths;
use crate::lower::plan::value_copy::{analyze, hands_out_payload};
use crate::tir::{
    BuiltinDeclarations, FunctionKind, FunctionRef, ReturnConvention, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirParam, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Whether any input names storage the caller still reaches, which is what a
/// result can be a part of: `struct_field_get(v: &T, i) -> F` reads a field of
/// what `v` points at, while `array_new(len) -> Array<T>` allocates.
fn reads_through_reference(func: &TirFunction, type_table: &TypeTable) -> bool {
    func.params
        .iter()
        .any(|p| is_reference(p.type_id, type_table))
}

/// Whether a bodyless declaration's result may be storage its caller still
/// owns, asked over monomorphized TIR. A declaration answering no is fresh
/// without declaring anything.
fn hands_out_storage(func: &TirFunction, type_table: &TypeTable) -> bool {
    reads_through_reference(func, type_table) && carries_storage(func.return_type, type_table)
}

/// [`hands_out_storage`] asked at the declaration, before monomorphization
/// narrows the types: a body-less declaration answering yes owes
/// `#[result(part_of = p)]`, or `#[result(owned)]` that it allocates. Silence
/// reads as "allocates", which elides copies, so it is not a safe default here.
pub fn owes_return_convention(
    params: &[TirParam],
    return_type: TypeId,
    type_table: &TypeTable,
) -> bool {
    params.iter().any(|p| is_reference(p.type_id, type_table))
        && may_carry_storage(return_type, type_table)
}

/// Whether `func` declares `#[result(owned)]`. Only a declaration with no body
/// is asked: a body is inferred from below, and would be free to contradict
/// what it declared.
fn declares_owned(func: &TirFunction) -> bool {
    func.body.is_none() && func.declared_return_convention == Some(ReturnConvention::Owned)
}

/// Oracle the freshness checker consults for a call's return convention.
pub struct OwnedCalls<'a> {
    returns_owned: &'a FuncKeySet,
    returns_projection: &'a FuncKeyMap<Vec<usize>>,
    builtins: &'a BuiltinDeclarations,
    indirect_owned_returns: Option<&'a IndexSet<TypeId>>,
}

impl<'a> OwnedCalls<'a> {
    pub fn new(
        returns_owned: &'a FuncKeySet,
        returns_projection: &'a FuncKeyMap<Vec<usize>>,
        builtins: &'a BuiltinDeclarations,
    ) -> Self {
        Self {
            returns_owned,
            returns_projection,
            builtins,
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
    /// every closure `$call` of that return type returns owned.
    pub fn indirect_is_owned(&self, return_type: TypeId) -> bool {
        self.indirect_owned_returns
            .is_some_and(|set| set.contains(&return_type))
    }

    /// Whether a call to `func` yields an owned (fresh) value. A body-less
    /// declaration answers from what it stated: one that hands out an argument's
    /// storage must say `#[result(part_of = p)]`, so anything that did not is
    /// fresh. A body function is owned iff the fixpoint proved it so, and an
    /// extern / opaque callee defaults to borrowed.
    pub fn is_owned(&self, func: &FunctionRef) -> bool {
        if func.module_source.is_core_builtin() || self.builtins.declares(func) {
            return builtin_projected_params(self.builtins, func).is_none();
        }
        self.returns_owned.contains(&func.module_source, &func.name)
    }

    /// The parameters `func` returns a projection of, by position, so a call to
    /// it is fresh exactly when *those* arguments are. A body function answers
    /// from the fixpoint; a `core:builtin` from `#[result(part_of = p)]`, which
    /// says the result is a component of `p` — and a component of a place
    /// nothing else reaches is one nothing else reaches. `builtin::select`
    /// hands back either operand. A wasm asset declares none.
    pub fn projected_params(&self, func: &FunctionRef) -> Option<&'a [usize]> {
        if func.module_source.is_core_builtin() || self.builtins.declares(func) {
            return builtin_projected_params(self.builtins, func);
        }
        if func.module_source.is_wasm_asset() {
            return None;
        }
        self.returns_projection
            .get(&func.module_source, &func.name)
            .map(Vec::as_slice)
    }
}

/// Per-function return conventions the fold consults: `returns_owned` (every
/// returned value is freshly materialized) and `returns_projection` (the
/// parameters the returned values that are not owned project, so the call is
/// fresh exactly when those arguments are).
pub struct ReturnConventions {
    pub returns_owned: FuncKeySet,
    pub returns_projection: FuncKeyMap<Vec<usize>>,
}

/// Functions whose every value-return aliases the receiver / first parameter
/// (a *borrowed* projection, through `array_get_value` and nested accessor calls).
/// Because it admits borrowed projections it must NOT feed the move / owned
/// decision; the read-only-share analysis and pattern lowering consume it.
pub fn compute_receiver_alias(
    project: &FlatPackage,
    call_graph: &CallGraph,
    return_paths: &ReturnPaths,
    type_table: &TypeTable,
    builtins: &BuiltinDeclarations,
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
        let hands_out_payload = hands_out_payload(&func, return_paths);
        if function_returns_receiver_alias(body, &set, builtins, type_table, hands_out_payload) {
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
    builtins: &BuiltinDeclarations,
    type_table: &TypeTable,
    hands_out_payload: bool,
) -> bool {
    struct W<'a> {
        set: &'a FuncKeySet,
        builtins: &'a BuiltinDeclarations,
        type_table: &'a TypeTable,
        hands_out_payload: bool,
        all_alias: bool,
        saw_return: bool,
    }
    impl TirRefVisitor for W<'_> {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
                self.saw_return = true;
                let v = returned_value(v, self.hands_out_payload, self.type_table);
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
        type_table,
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
    builtins: &BuiltinDeclarations,
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
        // A builtin hands out the parameter its `#[result(part_of = p)]` names,
        // which need not be the first: `struct_field_get(v, i)` reads `v`.
        TirExprKind::Call { func, args, .. } if func.module_source.is_core_builtin() => builtins
            .part_of(&**func)
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
/// `#[result(owned)]`, and a builtin that cannot hand storage out allocates —
/// then settles each strongly connected component of the
/// call graph with its callees already decided: a body function is owned when
/// every value it returns is owned, and projects parameters `P` when every
/// value it returns that is not owned is a projection of some of `P`.
/// A parameter its callers pass uncopied (`confined`) is not owned.
///
/// Inside a component `owned` is assumed and then disproved, not built up: a
/// cycle whose members return each other's result has no base to build from,
/// and the returns that avoid the cycle are what decide it either way.
pub fn compute_return_conventions(
    project: &FlatPackage,
    call_graph: &CallGraph,
    return_paths: &ReturnPaths,
    builtins: &BuiltinDeclarations,
    confined: &ConfinedParams,
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
    let mut projection = FuncKeyMap::default();

    for component in call_graph.sccs() {
        settle_component(
            &component,
            project,
            call_graph,
            return_paths,
            &type_table,
            builtins,
            confined,
            &mut owned,
            &mut projection,
        );
    }

    ReturnConventions {
        returns_owned: owned,
        returns_projection: projection,
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
    return_paths: &ReturnPaths,
    type_table: &TypeTable,
    builtins: &BuiltinDeclarations,
    confined: &ConfinedParams,
    owned: &mut FuncKeySet,
    projection: &mut FuncKeyMap<Vec<usize>>,
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
    // Only `owned` is assumed. It is the stronger verdict, so a cycle resolves
    // as optimistically through it, and the parameters a member projects have
    // no value to assume — the first walk of each is what proposes them.
    for &id in &members {
        let func = project.functions[id as usize].borrow();
        owned.insert(func.module_source.clone(), func.name.clone());
    }

    let mut queue: std::collections::VecDeque<u32> = members.iter().copied().collect();
    let mut queued: IndexSet<u32> = in_component.clone();
    while let Some(id) = queue.pop_front() {
        queued.swap_remove(&id);
        let func = project.functions[id as usize].borrow();
        let held_owned = owned.contains(&func.module_source, &func.name);
        let held_projection = projection.get(&func.module_source, &func.name).cloned();
        if !held_owned && held_projection.is_none() {
            continue;
        }
        let (ret_owned, ret_projection) = {
            let oracle = OwnedCalls::new(owned, projection, builtins);
            let hands_out_payload = hands_out_payload(&func, return_paths);
            function_return_convention(
                &func,
                &confined.borrowed_locals(&func),
                &oracle,
                type_table,
                hands_out_payload,
            )
        };
        let mut dropped = false;
        if held_owned && !ret_owned {
            owned.remove(&func.module_source, &func.name);
            dropped = true;
        }
        // A walk that names other parameters than the ones held refutes both,
        // the same as naming none: the verdict is which arguments to test.
        match (held_projection, ret_projection) {
            (Some(held), found) if found.as_ref() != Some(&held) => {
                projection.remove(&func.module_source, &func.name);
                owned.remove(&func.module_source, &func.name);
                dropped = true;
            }
            (None, Some(found)) => {
                projection.insert(func.module_source.clone(), func.name.clone(), found);
            }
            (Some(_), _) | (None, None) => {}
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
/// `$call` is an ordinary function, so those are the complete target set, and
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

/// Whether every returned value is owned, and which parameters the ones that
/// are not project (`return *self`, `return builtin::hole_get(t, i)`, the
/// payload `unwrap` binds out of `self`). Judged against the callee convention
/// `oracle` and the fresh-local set (Let bindings and match-arm bindings that
/// destructure an owned source). `borrowed` are the by-value parameters the
/// callers pass uncopied.
fn function_return_convention(
    func: &TirFunction,
    borrowed: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
    hands_out_payload: bool,
) -> (bool, Option<Vec<usize>>) {
    let body = func.body.as_ref().expect("members have bodies");
    let bindings = BindingCollector::collect(body, func.params.len());
    let fresh = compute_fresh_locals(&bindings, &func.params, borrowed, oracle, type_table);
    let mut walker = ReturnWalker {
        projector: Projector {
            bindings: &bindings,
            fresh: &fresh,
            oracle,
            type_table,
            params: &func.params,
        },
        hands_out_payload,
        all_owned: true,
        projected: Some(IndexSet::default()),
    };
    walker.visit_block(body);
    let projected = walker.projected.filter(|ps| !ps.is_empty()).map(|ps| {
        let mut ps: Vec<usize> = ps.into_iter().collect();
        ps.sort_unstable();
        ps
    });
    (walker.all_owned, projected)
}

/// Walk every `return value` and classify its operand: owned, or a projection of
/// parameters (`return *self`). Only `return` delivers a function's
/// result — Wado value-returning functions always use an explicit `return`. A
/// `break value` is internal to a loop or a labeled-block expression (e.g. the
/// `break: $b` inside a `[1,2,3]` sequence literal that is itself the payload of
/// a returned `Ok(...)`), so its freshness is judged by `is_owned_value` on the
/// enclosing return expression, not here — checking it against the
/// function-level fresh set would spuriously poison the return.
struct ReturnWalker<'a> {
    projector: Projector<'a>,
    hands_out_payload: bool,
    all_owned: bool,
    /// The parameters the not-owned returns project, or `None` once one of
    /// them projects none.
    projected: Option<IndexSet<usize>>,
}

impl TirRefVisitor for ReturnWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(v) } = &stmt.kind {
            let p = &self.projector;
            let v = returned_value(v, self.hands_out_payload, p.type_table);
            if !is_owned_value(v, p.fresh, p.oracle, p.type_table) {
                self.all_owned = false;
                let found = p
                    .projected(v, &mut IndexSet::default())
                    .filter(|ps| !ps.is_empty());
                self.projected = match (self.projected.take(), found) {
                    (Some(mut held), Some(found)) => {
                        held.extend(found);
                        Some(held)
                    }
                    _ => None,
                };
            }
        }
        self.walk_stmt(stmt);
    }
}

/// Which parameters a value is a projection of, so it is fresh exactly when
/// those arguments are.
struct Projector<'a> {
    bindings: &'a BindingCollector,
    fresh: &'a IndexSet<u32>,
    oracle: &'a OwnedCalls<'a>,
    type_table: &'a TypeTable,
    params: &'a [TirParam],
}

impl Projector<'_> {
    /// The parameters `expr` projects, by position: a field / index / payload /
    /// cast chain rooted at one, a binding of such a chain, a call handing
    /// such a chain back, or a match whose arms yield one. An owned value
    /// projects none. `None` is a value that names storage other than a
    /// parameter's. `visiting` holds the bindings being resolved, which a
    /// cycle through them adds nothing to.
    ///
    /// Only the parameter's own reference may be peeled (`*self`): a deref
    /// deeper in the chain reads a reference *stored in* that storage, and its
    /// referent is somebody else's — a template shape holds each hole as a `&V`
    /// field, so `*v.h0` leaves `v` entirely however fresh `v` is.
    fn projected(&self, expr: &TirExpr, visiting: &mut IndexSet<u32>) -> Option<IndexSet<usize>> {
        if is_owned_value(expr, self.fresh, self.oracle, self.type_table) {
            return Some(IndexSet::default());
        }
        let param =
            |index: u32| param_position(self.params, index).map(|p| std::iter::once(p).collect());
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                if let Some(p) = param(*index) {
                    return Some(p);
                }
                if !visiting.insert(*index) {
                    return Some(IndexSet::default());
                }
                let sources = self.bindings.sources_of(*index);
                let projected = if sources.is_empty() {
                    None
                } else {
                    self.union(sources, visiting)
                };
                visiting.swap_remove(index);
                projected
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => match inner.kind {
                TirExprKind::Local { index, .. } => param(index),
                _ => None,
            },
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => self.projected(inner, visiting),
            TirExprKind::Call { func, args, .. } => {
                let handed_back = self.oracle.projected_params(func)?;
                let args: Option<Vec<&TirExpr>> = handed_back
                    .iter()
                    .map(|&p| args.get(p).map(|a| &a.expr))
                    .collect();
                self.union(args?, visiting)
            }
            TirExprKind::Match { arms, .. } => self.union(
                arms.iter()
                    .filter(|arm| !self.type_table.is_never(arm.body.type_id))
                    .map(|arm| &arm.body)
                    .collect(),
                visiting,
            ),
            _ => None,
        }
    }

    fn union(&self, exprs: Vec<&TirExpr>, visiting: &mut IndexSet<u32>) -> Option<IndexSet<usize>> {
        let mut all = IndexSet::default();
        for e in exprs {
            all.extend(self.projected(e, visiting)?);
        }
        Some(all)
    }
}

/// Fresh (owned-rooted) non-parameter locals: a `let` bound to an owned value,
/// or a match-arm binding that destructures an owned scrutinee. Optimistic least
/// fixpoint — every bound local starts owned and is dropped once a source proves
/// borrowed (a source may reference another local whose ownership is still
/// shrinking).
fn compute_fresh_locals(
    collector: &BindingCollector,
    params: &[TirParam],
    borrowed: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> IndexSet<u32> {
    let mut fresh: IndexSet<u32> = collector.let_sources.keys().copied().collect();
    for (local, _) in &collector.match_sources {
        fresh.insert(*local);
    }
    // A by-value parameter is owned, the caller having copied or moved it in,
    // unless the caller passes it uncopied.
    //
    // `is_reference`, not a `ResolvedType` match: `boxing` has already rewritten
    // some `&T` onto `Box<T>` in place, and one it reached would read here as a
    // parameter the caller handed a copy of.
    for p in params {
        if !is_reference(p.type_id, type_table) && !borrowed.contains(&p.local_index) {
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
                    .all(|s| is_owned_value(s, &fresh, oracle, type_table))
            {
                fresh.swap_remove(&local);
                changed = true;
            }
        }
        for (local, scrut) in &collector.match_sources {
            if fresh.contains(local) && !is_owned_value(scrut, &fresh, oracle, type_table) {
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

impl BindingCollector {
    fn collect(body: &TirBlock, n_params: usize) -> Self {
        let mut collector = Self {
            n_params: u32::try_from(n_params).expect("a parameter count fits in u32"),
            let_sources: IndexMap::default(),
            match_sources: Vec::new(),
        };
        collector.visit_block(body);
        collector
    }

    /// Every value `local` is bound from.
    fn sources_of(&self, local: u32) -> Vec<&TirExpr> {
        let lets = self.let_sources.get(&local).into_iter().flatten();
        let scrutinees = self
            .match_sources
            .iter()
            .filter(move |(l, _)| *l == local)
            .map(|(_, scrut)| scrut);
        lets.chain(scrutinees).collect()
    }
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
                analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
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
