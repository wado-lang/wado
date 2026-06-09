//! Common Subexpression Elimination (CSE) for Wado NIR
//!
//! Eliminates duplicate pure expressions within loop bodies. When the same
//! pure binary expression appears in both the loop guard and the body, and
//! the operand locals are not reassigned between occurrences, the expression
//! is computed once and reused via a local variable.
//!
//! Example:
//! ```text
//! loop {
//!     if !((p * p) <= limit) { break; }
//!     let mut multiple = (p * p);   // same expression
//!     ...
//! }
//! ```
//! →
//! ```text
//! loop {
//!     let __cse_0 = (p * p);
//!     if !(__cse_0 <= limit) { break; }
//!     let mut multiple = __cse_0;
//!     ...
//! }
//! ```
//!
//! Identity check: two expressions are "the same" iff `engine.value(e1) ==
//! engine.value(e2)`. The ValueGraph already encodes reassignment between
//! occurrences, so this pass needs no separate modification guard.
//!
//! Runs as a per-function standalone engine session whose `apply_block`
//! fires once at the body root and walks every loop in the function,
//! applying the guard/body redundancy fold per loop.

use std::cell::Cell;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};
use crate::nir::NirFunction;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
use crate::tir::TypeId;
use crate::token::Span;

pub fn eliminate_common_subexprs(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::Cse, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = CseRule {
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function CSE walk at the body root.
pub(super) struct CseRule {
    applied: Cell<bool>,
}

impl Rule for CseRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        let mut func_changed = false;
        cse_in_block(engine, root, &mut func_changed);
        func_changed
    }
}

fn cse_in_block(engine: &mut Engine, block: BlockId, changed: &mut bool) {
    let stmts = engine.body.blocks[block].stmts.clone();
    for stmt in stmts {
        cse_in_stmt(engine, stmt, changed);
    }
}

fn cse_in_stmt(engine: &mut Engine, stmt: StmtId, changed: &mut bool) {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Loop { body: loop_block } => {
            let loop_block = *loop_block;
            // First recurse into inner loops, then apply CSE to this loop body.
            cse_in_block(engine, loop_block, changed);
            *changed |= cse_loop_body(engine, loop_block);
        }
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let then_block = *then_block;
            let else_block = *else_block;
            cse_in_block(engine, then_block, changed);
            if let Some(eb) = else_block {
                cse_in_block(engine, eb, changed);
            }
        }
        StmtKind::LabeledBlock { block, .. } => {
            let block = *block;
            cse_in_block(engine, block, changed);
        }
        _ => {}
    }
}

/// Apply CSE to a single loop body. Looks for a pure binary subexpression
/// that appears in the loop guard and again in the loop body and shares the
/// same engine [`ValueId`] (i.e., no reassignment of any operand between).
fn cse_loop_body(engine: &mut Engine, loop_block: BlockId) -> bool {
    if engine.body.blocks[loop_block].stmts.is_empty() {
        return false;
    }
    let first_stmt = engine.body.blocks[loop_block].stmts[0];

    // Extract the guard condition expression (a break guard: `if !(cond) { break; }`).
    let guard_expr = match &engine.body.stmts[first_stmt].kind {
        StmtKind::If {
            condition,
            then_block,
            ..
        } => {
            let then_block = *then_block;
            let is_break_guard = engine.body.blocks[then_block].stmts.len() == 1
                && matches!(
                    engine.body.stmts[engine.body.blocks[then_block].stmts[0]].kind,
                    StmtKind::Break { .. }
                );
            if !is_break_guard {
                return false;
            }
            *condition
        }
        _ => return false,
    };

    // Collect every pure binary subexpression of the guard, paired with its
    // ValueId. We intentionally restrict candidates to `Binary` so a single-
    // local read (whose VN is just an `Opaque` / `Local` reaching def) does
    // not get hoisted — there is no benefit in CSE'ing a single local read.
    let candidates = collect_binary_candidates(engine, guard_expr);
    if candidates.is_empty() {
        return false;
    }

    let remaining_stmts: Vec<StmtId> = engine.body.blocks[loop_block].stmts[1..].to_vec();
    for (cand_expr, cand_vn, type_id, span) in candidates {
        // Walk the remaining stmts looking for any expression that resolves
        // to the same ValueId. The VN already encodes reaching-def info, so
        // there is no separate "are the operands still unchanged?" check —
        // a reassignment between guard and use would produce a different VN
        // for the later use.
        if !any_stmt_contains_value(engine, &remaining_stmts, cand_vn) {
            continue;
        }

        // Create a fresh `__cse_N` local through the engine so the locals
        // list grows coherently.
        let cse_local_name = format!("__cse_{}", engine.locals().len());
        let cse_local_idx =
            engine.alloc_local(cse_local_name.clone(), type_id, /* is_mut */ false);

        // Clone the matching expression out of the guard for the Let value.
        let value = engine.clone_expr(cand_expr);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name: cse_local_name.clone(),
                local_index: cse_local_idx,
                is_mut: false,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: false,
            },
            span,
        );

        // Replace every matching subexpression (in the guard's stmt and in
        // every remaining stmt) with a `Local(cse_local_idx)` read.
        replace_matching_in_stmt(
            engine,
            first_stmt,
            cand_vn,
            cse_local_idx,
            &cse_local_name,
            type_id,
        );
        for s in &remaining_stmts {
            replace_matching_in_stmt(engine, *s, cand_vn, cse_local_idx, &cse_local_name, type_id);
        }

        // Insert the Let at the start of the loop body.
        let mut new_stmts = engine.body.blocks[loop_block].stmts.clone();
        new_stmts.insert(0, let_stmt);
        engine.set_block_stmts(loop_block, new_stmts);

        return true; // One CSE per loop per pass (the outer loop iterates).
    }

    false
}

/// Collect every `Binary` subexpression of `expr` paired with its ValueId.
/// Recurses through Binary/Unary so candidates like `!(p * p <= limit)`
/// are reached. Two-pass: pass 1 collects candidates under `&body`;
/// pass 2 attaches ValueIds (which needs `&mut engine`).
fn collect_binary_candidates(
    engine: &mut Engine,
    expr: ExprId,
) -> Vec<(ExprId, ValueId, TypeId, Span)> {
    let mut binary_exprs: Vec<(ExprId, TypeId, Span)> = Vec::new();
    {
        let body: &Body = &*engine.body;
        let mut stack: Vec<ExprId> = vec![expr];
        while let Some(e) = stack.pop() {
            match &body.exprs[e].kind {
                ExprKind::Binary { left, right, .. } => {
                    binary_exprs.push((e, body.exprs[e].type_id, body.exprs[e].span));
                    stack.push(*left);
                    stack.push(*right);
                }
                ExprKind::Unary { expr: inner, .. } => {
                    stack.push(*inner);
                }
                _ => {}
            }
        }
    }
    // Drop impure candidates (a `Binary` whose ValueId is `None` has an
    // impure child like a `Call`).
    binary_exprs
        .into_iter()
        .filter_map(|(e, type_id, span)| engine.value(e).map(|vn| (e, vn, type_id, span)))
        .collect()
}

/// Does any expression in any of `stmts` resolve to `target`?
fn any_stmt_contains_value(engine: &mut Engine, stmts: &[StmtId], target: ValueId) -> bool {
    for &s in stmts {
        let mut exprs: Vec<ExprId> = Vec::new();
        collect_exprs_in_stmt(engine.body, s, &mut exprs);
        for e in exprs {
            if engine.value(e) == Some(target) {
                return true;
            }
        }
    }
    false
}

/// Replace every expression in `stmt`'s subtree whose ValueId equals
/// `target` with a `Local { cse_local_idx, cse_local_name }` read. Re-asserts
/// the new type on the rewritten node (Local takes a `TypeId` and the
/// CSE'd value may differ from the surrounding context's type).
fn replace_matching_in_stmt(
    engine: &mut Engine,
    stmt: StmtId,
    target: ValueId,
    cse_local_idx: u32,
    cse_local_name: &str,
    type_id: TypeId,
) {
    // Snapshot matches before rewriting — see `Engine::value`'s
    // invalidation contract.
    let mut exprs: Vec<ExprId> = Vec::new();
    collect_exprs_in_stmt(engine.body, stmt, &mut exprs);
    let matches: Vec<ExprId> = exprs
        .into_iter()
        .filter(|&e| engine.value(e) == Some(target))
        .collect();
    for e in matches {
        // `replace_expr_kind` doesn't touch `type_id`; write it directly.
        engine.body.exprs[e].type_id = type_id;
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: cse_local_idx,
                name: cse_local_name.to_string(),
            },
        );
    }
}

/// Walk `stmt`'s subtree, collecting `ExprId`s of every node CSE may
/// rewrite. Nested `Loop` statements are deliberately opaque: an outer
/// loop's CSE does not reach into them, since the same expression
/// computes different values per inner-loop iteration.
fn collect_exprs_in_stmt(body: &Body, stmt: StmtId, out: &mut Vec<ExprId>) {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e) => collect_exprs_in_expr(body, *e, out),
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            collect_exprs_in_expr(body, *value, out);
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_exprs_in_expr(body, *condition, out);
            collect_exprs_in_block(body, *then_block, out);
            if let Some(eb) = else_block {
                collect_exprs_in_block(body, *eb, out);
            }
        }
        // Inner loops are opaque per the rationale on `collect_exprs_in_stmt`.
        StmtKind::Loop { .. } => {}
        StmtKind::LabeledBlock { block, .. } => collect_exprs_in_block(body, *block, out),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_exprs_in_expr(body, *v, out);
            }
        }
        StmtKind::Continue => {}
    }
}

fn collect_exprs_in_block(body: &Body, block: BlockId, out: &mut Vec<ExprId>) {
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        collect_exprs_in_stmt(body, s, out);
    }
}

fn collect_exprs_in_expr(body: &Body, expr: ExprId, out: &mut Vec<ExprId>) {
    out.push(expr);
    let mut kids: Vec<NodeRef> = Vec::new();
    body.for_each_child(NodeRef::Expr(expr), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => collect_exprs_in_expr(body, e, out),
            NodeRef::Block(b) => collect_exprs_in_block(body, b, out),
            NodeRef::Stmt(s) => collect_exprs_in_stmt(body, s, out),
            NodeRef::Pat(_) => {}
        }
    }
}
