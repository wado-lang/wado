//! Dead Return Value Elimination for Wado NIR.
//!
//! Converts non-void functions whose return value is always immediately
//! dropped at every call site into void-returning functions. Every
//! `Return { value: Some(expr) }` becomes `Return { value: None }` once
//! `expr` is verified pure, and call sites stay structurally identical
//! (the call expression now produces `Unit`).
//!
//! NIR analog of `wir_optimize/drve.rs`. Running at NIR exposes the freshly
//! dead expressions to the rest of the fixed-point loop. Especially useful
//! after `inline` collapses a `Result<(), Error>`-returning helper whose
//! callers all `_ = helper(args);`-style discard the result — DCE can then
//! remove the (now unreferenced) `Ok(())` constructor wholesale.
//!
//! Conservative scope:
//!
//! - Skips the same pinned set as DAE.
//! - Requires the body to end with an explicit `Return { value: Some(_) }`
//!   so we never have to reason about an implicit trailing-value return.
//! - Requires every other `Return` in the body to also carry a pure value.
//! - Requires every call site to appear as a top-level statement
//!   (`StmtKind::Expr(Call(f, ...))` or `StmtKind::Expr(MethodCall(f,
//!   ...))`); any nested or `Let`-bound use disqualifies the candidate.
//! - Requires at least one observed call site (otherwise DCE will delete
//!   the function anyway and there is nothing to optimise).
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the function-body
//! walks (purity check, call-site validation, void rewrite, call retype) read
//! and mutate the arena `Body` directly. Global initializers are arena `Body`s
//! too (Phase 5 group 1a), so their use-scan (`scan_node`) and retype
//! (`retype_calls`) reuse the same arena routines.

use crate::hashmap::IndexSet;
use crate::nir::{FunctionKind, NirFunction};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query;
use super::dae;
use super::gate::{FunctionGate, FunctionId};

type FnKey = dae::FnKey;

pub fn eliminate_dead_return_values(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let pinned = collect_pinned(project);
    let type_table = project.type_table.borrow();

    let mut candidates: IndexSet<FnKey> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_eligible(&func, &pinned, &type_table) {
            continue;
        }
        if let Some(body) = &func.body
            && has_only_pure_returns_with_explicit_tail(body)
        {
            candidates.insert((func.module_source.clone(), func.name.clone()));
        }
    }
    drop(type_table);
    if candidates.is_empty() {
        return false;
    }

    let confirmed = validate_call_sites(project, candidates);
    if confirmed.is_empty() {
        return false;
    }

    // drve is interprocedural but not gate-skipped; it reports exactly the
    // functions it touched (converted callees + retyped callers) so the gated
    // passes re-examine only those instead of every function via `bump_all`.
    // The call graph is unaffected, so no refresh is needed.
    let touched = apply_drve(project, &confirmed);
    for idx in touched {
        gate.mark_changed(FunctionId::new(idx));
    }
    true
}

fn is_eligible(func: &NirFunction, pinned: &IndexSet<FnKey>, type_table: &TypeTable) -> bool {
    if func.body.is_none() {
        return false;
    }
    if func.is_export
        || func.is_cm_export
        || func.is_cm_binding
        || func.is_dispatch_wrapper
        || func.is_ambient
        || func.is_async
    {
        return false;
    }
    if !matches!(func.kind, FunctionKind::Regular) {
        return false;
    }
    if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
        return false;
    }
    if func.return_type == TypeTable::UNIT || func.return_type == TypeTable::NEVER {
        return false;
    }
    // Match the WIR-level DRVE scope: only convert returns that allocate
    // a heap-typed value (struct / variant / array). Primitive-returning
    // helpers like `fn f() -> i32 { return c.threshold + c.scale; }` save
    // nothing from being voided and can break test fixtures that assert
    // post-optimizer body shape.
    if !is_heap_alloc_return(func.return_type, type_table) {
        return false;
    }
    // Trait methods share a signature contract with sibling impls and the
    // trait declaration; rewriting their return type breaks dispatch.
    if func
        .method_info
        .as_ref()
        .is_some_and(|mi| mi.trait_name.is_some())
    {
        return false;
    }
    if pinned.contains(&(func.module_source.clone(), func.name.clone())) {
        return false;
    }
    true
}

fn is_heap_alloc_return(type_id: crate::tir::TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Struct { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::GenericInstance { .. }
    )
}

/// True when the body ends with an explicit `Return { value: Some(_) }` and
/// every `Return` in the body (including the tail) carries a pure value.
fn has_only_pure_returns_with_explicit_tail(body: &Body) -> bool {
    let root = body.root;
    let Some(&last) = body.blocks[root].stmts.last() else {
        return false;
    };
    if !matches!(&body.stmts[last].kind, StmtKind::Return { value: Some(_) }) {
        return false;
    }
    // Every return reachable in the body must carry a pure value — a
    // `Return { value: None }` would mean a void exit path, structurally
    // inconsistent for a non-void signature.
    let mut stack = vec![NodeRef::Block(root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node {
            match &body.stmts[s].kind {
                StmtKind::Return { value: None } => return false,
                StmtKind::Return { value: Some(v) } => {
                    if !arena_query::is_pure_expr(body, *v) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    true
}

fn collect_pinned(project: &NirPackage) -> IndexSet<FnKey> {
    dae::collect_pinned(project)
}

// ──────────────────────────────────────────────────────────────────────────────
// Call-site validation
// ──────────────────────────────────────────────────────────────────────────────

fn validate_call_sites(project: &NirPackage, mut candidates: IndexSet<FnKey>) -> IndexSet<FnKey> {
    let mut ctx = ValidateCtx {
        candidates: &candidates,
        rejected: IndexSet::default(),
        observed: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            ctx.block(body, body.root);
        }
    }
    // A global initializer can never be a `_ = call(...)` drop site — any
    // appearance of a candidate there consumes its result, disqualifying it.
    // `scan_node` rejects every candidate used as a `Call` / `MethodCall`.
    for global in &project.globals {
        ctx.scan_node(
            global.initializer.body(),
            NodeRef::Block(global.initializer.body().root),
        );
    }
    let ValidateCtx {
        rejected, observed, ..
    } = ctx;
    for r in &rejected {
        candidates.shift_remove(r);
    }
    candidates.retain(|k| observed.contains(k));
    candidates
}

/// Arena walk that recognises the "top-level Expr(Call)" drop-position pattern.
/// At a statement boundary, `Expr(Call(f, args))` / `Expr(MethodCall(f, …))`
/// observe `f` in drop position; their args / receiver are use-scanned.
/// Everything else (Let values, return values, nested expression trees) is
/// use-scanned, where any candidate appearance rejects it.
struct ValidateCtx<'a> {
    candidates: &'a IndexSet<FnKey>,
    rejected: IndexSet<FnKey>,
    observed: IndexSet<FnKey>,
}

impl ValidateCtx<'_> {
    fn block(&mut self, body: &Body, block: BlockId) {
        let stmts = body.blocks[block].stmts.clone();
        for s in stmts {
            self.stmt(body, s);
        }
    }

    fn stmt(&mut self, body: &Body, stmt: StmtId) {
        if let StmtKind::Expr(e) = &body.stmts[stmt].kind {
            let e = *e;
            let (call_key, scan): (Option<FnKey>, Vec<ExprId>) = match &body.exprs[e].kind {
                ExprKind::Call { func, args, .. } => (
                    Some((func.module_source.clone(), func.name.clone())),
                    args.iter().map(|a| a.expr).collect(),
                ),
                ExprKind::MethodCall {
                    func,
                    receiver,
                    args,
                    ..
                } => {
                    let mut scan = vec![*receiver];
                    scan.extend(args.iter().map(|a| a.expr));
                    (Some((func.module_source.clone(), func.name.clone())), scan)
                }
                // Not a top-level call: the whole expression is a use.
                _ => (None, vec![e]),
            };
            if let Some(key) = call_key
                && self.candidates.contains(&key)
            {
                self.observed.insert(key);
            }
            for s in scan {
                self.scan_node(body, NodeRef::Expr(s));
            }
            return;
        }
        // Non-Expr stmt: blocks re-enter the drop-position logic; expr / pattern
        // children are use-scanned.
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
        for c in kids {
            match c {
                NodeRef::Block(b) => self.block(body, b),
                NodeRef::Expr(_) | NodeRef::Pat(_) => self.scan_node(body, c),
                NodeRef::Stmt(_) => {}
            }
        }
    }

    /// Reject every candidate that appears as a `Call` / `MethodCall` anywhere
    /// in the subtree at `node` (value position).
    fn scan_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Call { func, .. } | ExprKind::MethodCall { func, .. } =
                &body.exprs[id].kind
        {
            let key = (func.module_source.clone(), func.name.clone());
            if self.candidates.contains(&key) {
                self.rejected.insert(key);
            }
        }
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.scan_node(body, c);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Application
// ──────────────────────────────────────────────────────────────────────────────

/// Applies the rewrites and returns the indices of every function whose body or
/// signature changed (converted callees + callers whose call sites were
/// retyped), so the caller can mark exactly those dirty in the gate. The call
/// graph is unaffected: drve only voids returns and retypes call expressions,
/// never adding or removing an edge.
fn apply_drve(project: &mut NirPackage, confirmed: &IndexSet<FnKey>) -> Vec<usize> {
    let mut touched: IndexSet<usize> = IndexSet::default();
    // Step A: convert each confirmed candidate to void return. The candidate
    // filter guarantees every reachable `Return { value: Some(_) }` carries a
    // pure expression, so dropping its value is observably equivalent.
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let key = (func.module_source.clone(), func.name.clone());
        if !confirmed.contains(&key) {
            continue;
        }
        func.return_type = TypeTable::UNIT;
        if let Some(body) = func.body.as_mut() {
            void_returns(body);
        }
        touched.insert(i);
    }

    // Step B: refresh `type_id` at every call site of a converted function.
    // Without this, `Expr(Call(f))` in stmt position still claims the old
    // return type and `wir_build::translate.rs` wraps the call in `Drop`,
    // underflowing the Wasm stack.
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut()
            && retype_calls(body, confirmed)
        {
            touched.insert(i);
        }
    }
    for global in &mut project.globals {
        retype_calls(global.initializer.body_mut(), confirmed);
    }
    touched.into_iter().collect()
}

/// Rewrite every reachable `Return { value: Some(_) }` to `Return { value: None }`.
fn void_returns(body: &mut Body) {
    let mut returns = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && matches!(&body.stmts[s].kind, StmtKind::Return { value: Some(_) })
        {
            returns.push(s);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    for s in returns {
        body.stmts[s].kind = StmtKind::Return { value: None };
    }
}

/// Set `type_id` to `Unit` at every `Call` / `MethodCall` of a confirmed
/// function in the body.
fn retype_calls(body: &mut Body, confirmed: &IndexSet<FnKey>) -> bool {
    let mut targets = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Call { func, .. } | ExprKind::MethodCall { func, .. } =
                &body.exprs[id].kind
        {
            let key = (func.module_source.clone(), func.name.clone());
            if confirmed.contains(&key) {
                targets.push(id);
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    let changed = !targets.is_empty();
    for id in targets {
        body.exprs[id].type_id = TypeTable::UNIT;
    }
    changed
}
