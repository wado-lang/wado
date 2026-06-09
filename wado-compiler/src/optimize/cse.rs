//! Common Subexpression Elimination (CSE) for Wado NIR
//!
//! Eliminates duplicate pure expressions within loop bodies. When the same
//! pure binary expression appears multiple times within a single loop iteration
//! and the operand locals are not modified between occurrences, the expression
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
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and walks every loop in the function, applying the
//! guard/body redundancy fold per loop. All mutations route through the
//! engine edit API (`alloc_stmt`, `alloc_expr`, `clone_expr`,
//! `set_block_stmts`, `replace_expr_kind`, `alloc_local`) so the parent map
//! and use index stay coherent.

use std::cell::Cell;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirFunction};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
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

/// A pure expression that can be CSE'd, identified by its structure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CseKey {
    Binary {
        op: NirBinaryOp,
        left: Box<CseKey>,
        right: Box<CseKey>,
    },
    Local {
        index: u32,
    },
    IntLiteral {
        value: u64,
    },
}

/// Try to build a `CseKey` from an arena expression (only for pure expressions).
fn expr_to_key(body: &Body, id: ExprId) -> Option<CseKey> {
    match &body.exprs[id].kind {
        ExprKind::Binary { left, op, right } => {
            let left_key = expr_to_key(body, *left)?;
            let right_key = expr_to_key(body, *right)?;
            Some(CseKey::Binary {
                op: *op,
                left: Box::new(left_key),
                right: Box::new(right_key),
            })
        }
        ExprKind::Local { index, .. } => Some(CseKey::Local { index: *index }),
        ExprKind::IntLiteral { value, .. } => Some(CseKey::IntLiteral { value: *value }),
        _ => None,
    }
}

/// Collect all locals referenced in a `CseKey`.
fn key_locals(key: &CseKey, locals: &mut IndexSet<u32>) {
    match key {
        CseKey::Binary { left, right, .. } => {
            key_locals(left, locals);
            key_locals(right, locals);
        }
        CseKey::Local { index } => {
            locals.insert(*index);
        }
        CseKey::IntLiteral { .. } => {}
    }
}

/// Apply CSE to a loop body. Looks for a pure binary subexpression that appears
/// in the loop guard and again in the loop body, with no modification to operands
/// between occurrences.
fn cse_loop_body(engine: &mut Engine, loop_block: BlockId) -> bool {
    // Pattern: first stmt is `if !(cond) { break; }` — extract subexprs from cond
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

    // Find binary subexpressions in the guard condition.
    let candidates = collect_binary_subexprs(engine.body, guard_expr);
    if candidates.is_empty() {
        return false;
    }

    let remaining_stmts: Vec<StmtId> = engine.body.blocks[loop_block].stmts[1..].to_vec();
    for (key, type_id, span) in &candidates {
        // Skip trivial single-local expressions (no benefit in CSE).
        if matches!(key, CseKey::Local { .. }) {
            continue;
        }

        let mut used_locals = IndexSet::default();
        key_locals(key, &mut used_locals);

        // Check if any of the remaining stmts contain the same expression AND
        // the used locals are not modified before that occurrence.
        if has_matching_expr(engine.body, &remaining_stmts, key, &used_locals) {
            // Create a new local for the CSE'd expression via the engine, so
            // the `locals` list grows coherently with the body.
            let cse_local_name_seed = format!("__cse_{}", engine.locals().len());
            let cse_local_idx = engine.alloc_local(
                cse_local_name_seed.clone(),
                *type_id,
                /* is_mut */ false,
            );
            let cse_local_name = cse_local_name_seed;

            // Clone the matching expression out of the guard for the Let value.
            let match_id = extract_matching_expr(engine.body, guard_expr, key).unwrap();
            let value = engine.clone_expr(match_id);
            let let_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: cse_local_name.clone(),
                    local_index: cse_local_idx,
                    is_mut: false,
                    is_reactive: false,
                    type_id: *type_id,
                    value,
                    skip_value_copy: false,
                },
                *span,
            );

            // Replace the expression everywhere it occurs (guard + remaining
            // body) with a reference to the CSE local.
            replace_matching_stmt(
                engine,
                first_stmt,
                key,
                cse_local_idx,
                &cse_local_name,
                *type_id,
            );
            for stmt in &remaining_stmts {
                replace_matching_stmt(engine, *stmt, key, cse_local_idx, &cse_local_name, *type_id);
            }

            // Insert the Let at the beginning of the loop body. The
            // `set_block_stmts` re-parents / re-enqueues the new statement
            // list (the others stay parented to this block as before).
            let mut new_stmts = engine.body.blocks[loop_block].stmts.clone();
            new_stmts.insert(0, let_stmt);
            engine.set_block_stmts(loop_block, new_stmts);

            return true; // One CSE per loop per pass (the outer loop iterates).
        }
    }

    false
}

/// Collect all pure binary subexpressions from an expression.
fn collect_binary_subexprs(body: &Body, expr: ExprId) -> Vec<(CseKey, TypeId, Span)> {
    let mut result = Vec::new();
    collect_binary_subexprs_rec(body, expr, &mut result);
    result
}

fn collect_binary_subexprs_rec(
    body: &Body,
    expr: ExprId,
    result: &mut Vec<(CseKey, TypeId, Span)>,
) {
    if let ExprKind::Binary { left, right, .. } = &body.exprs[expr].kind {
        let (left, right) = (*left, *right);
        if let Some(key) = expr_to_key(body, expr) {
            result.push((key, body.exprs[expr].type_id, body.exprs[expr].span));
        }
        collect_binary_subexprs_rec(body, left, result);
        collect_binary_subexprs_rec(body, right, result);
    }
    // Also recurse into Unary (e.g. `!(p * p <= limit)`).
    if let ExprKind::Unary { expr: inner, .. } = &body.exprs[expr].kind {
        collect_binary_subexprs_rec(body, *inner, result);
    }
}

/// Check if any statement in the list contains the same expression, and the
/// used locals are not modified before that occurrence.
fn has_matching_expr(
    body: &Body,
    stmts: &[StmtId],
    key: &CseKey,
    used_locals: &IndexSet<u32>,
) -> bool {
    for &stmt in stmts {
        if stmt_modifies_any(body, stmt, used_locals) {
            // Locals modified before a match — only safe if the expression
            // appears in this stmt before the modification; conservatively
            // check this stmt for the expression first.
            if stmt_contains_expr(body, stmt, key) {
                return true;
            }
            return false;
        }
        if stmt_contains_expr(body, stmt, key) {
            return true;
        }
    }
    false
}

fn stmt_modifies_any(body: &Body, stmt: StmtId, locals: &IndexSet<u32>) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e) => expr_modifies_any(body, *e, locals),
        StmtKind::Let { value, .. } => expr_modifies_any(body, *value, locals),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_modifies_any(body, *condition, locals)
                || block_modifies_any(body, *then_block, locals)
                || else_block.is_some_and(|eb| block_modifies_any(body, eb, locals))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            block_modifies_any(body, *b, locals)
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.is_some_and(|v| expr_modifies_any(body, v, locals))
        }
        StmtKind::LetDestructure { value, .. } => expr_modifies_any(body, *value, locals),
        StmtKind::Continue => false,
    }
}

fn block_modifies_any(body: &Body, block: BlockId, locals: &IndexSet<u32>) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| stmt_modifies_any(body, *s, locals))
}

fn expr_modifies_any(body: &Body, expr: ExprId, locals: &IndexSet<u32>) -> bool {
    match &body.exprs[expr].kind {
        ExprKind::Assign { target, value } => {
            if let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                && locals.contains(index)
            {
                return true;
            }
            expr_modifies_any(body, *target, locals) || expr_modifies_any(body, *value, locals)
        }
        ExprKind::Binary { left, right, .. } => {
            expr_modifies_any(body, *left, locals) || expr_modifies_any(body, *right, locals)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => expr_modifies_any(body, *inner, locals),
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_modifies_any(body, a.expr, locals)),
        ExprKind::MethodCall { receiver, args, .. } => {
            expr_modifies_any(body, *receiver, locals)
                || args.iter().any(|a| expr_modifies_any(body, a.expr, locals))
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_modifies_any(body, *condition, locals)
                || block_modifies_any(body, *then_branch, locals)
                || else_branch.is_some_and(|eb| block_modifies_any(body, eb, locals))
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            block_modifies_any(body, *b, locals)
        }
        ExprKind::Index { expr: inner, index } => {
            expr_modifies_any(body, *inner, locals) || expr_modifies_any(body, *index, locals)
        }
        _ => false,
    }
}

/// Check if a statement contains an expression matching the given key.
fn stmt_contains_expr(body: &Body, stmt: StmtId, key: &CseKey) -> bool {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e) => expr_contains(body, *e, key),
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            expr_contains(body, *value, key)
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_contains(body, *condition, key)
                || block_contains(body, *then_block, key)
                || else_block.is_some_and(|eb| block_contains(body, eb, key))
        }
        // Nested Loop: treat as opaque (an inner loop may modify operands
        // across its own iterations, which a pre-computed cache can't see).
        StmtKind::Loop { .. } => false,
        StmtKind::LabeledBlock { block, .. } => block_contains(body, *block, key),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.is_some_and(|v| expr_contains(body, v, key))
        }
        StmtKind::Continue => false,
    }
}

fn block_contains(body: &Body, block: BlockId, key: &CseKey) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| stmt_contains_expr(body, *s, key))
}

fn expr_contains(body: &Body, expr: ExprId, key: &CseKey) -> bool {
    if expr_to_key(body, expr).as_ref() == Some(key) {
        return true;
    }
    match &body.exprs[expr].kind {
        ExprKind::Binary { left, right, .. } => {
            expr_contains(body, *left, key) || expr_contains(body, *right, key)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => expr_contains(body, *inner, key),
        ExprKind::Assign { target, value } => {
            expr_contains(body, *target, key) || expr_contains(body, *value, key)
        }
        ExprKind::Call { args, .. } => args.iter().any(|a| expr_contains(body, a.expr, key)),
        ExprKind::MethodCall { receiver, args, .. } => {
            expr_contains(body, *receiver, key)
                || args.iter().any(|a| expr_contains(body, a.expr, key))
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_contains(body, *condition, key)
                || block_contains(body, *then_branch, key)
                || else_branch.is_some_and(|eb| block_contains(body, eb, key))
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            block_contains(body, *b, key)
        }
        ExprKind::Index { expr: inner, index } => {
            expr_contains(body, *inner, key) || expr_contains(body, *index, key)
        }
        _ => false,
    }
}

/// Find the first arena expression matching `key`, returning its id.
fn extract_matching_expr(body: &Body, expr: ExprId, key: &CseKey) -> Option<ExprId> {
    if expr_to_key(body, expr).as_ref() == Some(key) {
        return Some(expr);
    }
    match &body.exprs[expr].kind {
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            extract_matching_expr(body, left, key)
                .or_else(|| extract_matching_expr(body, right, key))
        }
        ExprKind::Unary { expr: inner, .. } => {
            let inner = *inner;
            extract_matching_expr(body, inner, key)
        }
        _ => None,
    }
}

/// Replace all occurrences of the expression matching `key` with a reference to
/// the CSE local, throughout `stmt`. Recursion sets mirror the former tree
/// `replace_matching_expr` / `replace_in_expr` exactly.
fn replace_matching_stmt(
    engine: &mut Engine,
    stmt: StmtId,
    key: &CseKey,
    idx: u32,
    name: &str,
    type_id: TypeId,
) {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Expr(e) => {
            let e = *e;
            replace_in_expr(engine, e, key, idx, name, type_id);
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            let value = *value;
            replace_in_expr(engine, value, key, idx, name, type_id);
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            replace_in_expr(engine, condition, key, idx, name, type_id);
            replace_in_block(engine, then_block, key, idx, name, type_id);
            if let Some(eb) = else_block {
                replace_in_block(engine, eb, key, idx, name, type_id);
            }
        }
        // Nested Loop: do not descend (matches `stmt_contains_expr`'s opaque
        // treatment).
        StmtKind::Loop { .. } => {}
        StmtKind::LabeledBlock { block, .. } => {
            let block = *block;
            replace_in_block(engine, block, key, idx, name, type_id);
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            if let Some(v) = *value {
                replace_in_expr(engine, v, key, idx, name, type_id);
            }
        }
        StmtKind::Continue => {}
    }
}

fn replace_in_block(
    engine: &mut Engine,
    block: BlockId,
    key: &CseKey,
    idx: u32,
    name: &str,
    type_id: TypeId,
) {
    let stmts = engine.body.blocks[block].stmts.clone();
    for stmt in stmts {
        replace_matching_stmt(engine, stmt, key, idx, name, type_id);
    }
}

fn replace_in_expr(
    engine: &mut Engine,
    expr: ExprId,
    key: &CseKey,
    idx: u32,
    name: &str,
    type_id: TypeId,
) {
    if expr_to_key(engine.body, expr).as_ref() == Some(key) {
        // The CSE'd subtree's `type_id` may differ from the surrounding node's
        // (the matched node had its own type before substitution). Set it
        // directly on the arena — the engine tracks parent / use-index
        // coherence on kinds only, so a type-id write is independent.
        engine.body.exprs[expr].type_id = type_id;
        engine.replace_expr_kind(
            expr,
            ExprKind::Local {
                index: idx,
                name: name.to_string(),
            },
        );
        return;
    }
    match &engine.body.exprs[expr].kind {
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            replace_in_expr(engine, left, key, idx, name, type_id);
            replace_in_expr(engine, right, key, idx, name, type_id);
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => {
            let inner = *inner;
            replace_in_expr(engine, inner, key, idx, name, type_id);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            replace_in_expr(engine, target, key, idx, name, type_id);
            replace_in_expr(engine, value, key, idx, name, type_id);
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for a in args {
                replace_in_expr(engine, a, key, idx, name, type_id);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            replace_in_expr(engine, receiver, key, idx, name, type_id);
            for a in args {
                replace_in_expr(engine, a, key, idx, name, type_id);
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            replace_in_expr(engine, condition, key, idx, name, type_id);
            replace_in_block(engine, then_branch, key, idx, name, type_id);
            if let Some(eb) = else_branch {
                replace_in_block(engine, eb, key, idx, name, type_id);
            }
        }
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            replace_in_block(engine, b, key, idx, name, type_id);
        }
        ExprKind::Index { expr: inner, index } => {
            let (inner, index) = (*inner, *index);
            replace_in_expr(engine, inner, key, idx, name, type_id);
            replace_in_expr(engine, index, key, idx, name, type_id);
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            replace_in_expr(engine, callee, key, idx, name, type_id);
            for a in args {
                replace_in_expr(engine, a, key, idx, name, type_id);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            let fields: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for f in fields {
                replace_in_expr(engine, f, key, idx, name, type_id);
            }
        }
        ExprKind::TupleLiteral { elements, .. } => {
            let elements = elements.clone();
            for elem in elements {
                replace_in_expr(engine, elem, key, idx, name, type_id);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                replace_in_expr(engine, p, key, idx, name, type_id);
            }
        }
        ExprKind::Match { expr: inner, arms } => {
            let inner = *inner;
            let mut targets: Vec<ExprId> = Vec::new();
            for arm in arms {
                if let Some(guard) = arm.guard {
                    targets.push(guard);
                }
                targets.push(arm.body);
            }
            replace_in_expr(engine, inner, key, idx, name, type_id);
            for t in targets {
                replace_in_expr(engine, t, key, idx, name, type_id);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            replace_in_expr(engine, scrutinee, key, idx, name, type_id);
            for arm in arms {
                replace_in_block(engine, arm, key, idx, name, type_id);
            }
            replace_in_block(engine, default, key, idx, name, type_id);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            let value = *value;
            replace_in_expr(engine, value, key, idx, name, type_id);
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for a in args {
                replace_in_expr(engine, a, key, idx, name, type_id);
            }
        }
        _ => {}
    }
}
