//! Common Subexpression Elimination (CSE) for Wado TIR
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

use crate::hashmap::IndexSet;
use crate::nir::{
    NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirStmt, NirStmtKind,
};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;
use crate::token::Span;

pub fn eliminate_common_subexprs(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= cse_function(&mut func);
    }
    changed
}

fn cse_function(func: &mut NirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    let mut changed = false;
    cse_in_block(body, &mut func.local_count, &mut func.locals, &mut changed);
    changed
}

fn cse_in_block(
    block: &mut NirBlock,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    changed: &mut bool,
) {
    for stmt in &mut block.stmts {
        cse_in_stmt(stmt, local_count, locals, changed);
    }
}

fn cse_in_stmt(
    stmt: &mut NirStmt,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    changed: &mut bool,
) {
    match &mut stmt.kind {
        NirStmtKind::Loop { body } => {
            // First recurse into inner loops
            cse_in_block(body, local_count, locals, changed);
            // Then apply CSE to this loop body
            *changed |= cse_loop_body(body, local_count, locals);
        }
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            cse_in_block(then_block, local_count, locals, changed);
            if let Some(eb) = else_block {
                cse_in_block(eb, local_count, locals, changed);
            }
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            cse_in_block(block, local_count, locals, changed);
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

/// Try to build a `CseKey` from a TIR expression (only for pure expressions).
fn expr_to_key(expr: &NirExpr) -> Option<CseKey> {
    match &expr.kind {
        NirExprKind::Binary { left, op, right } => {
            let left_key = expr_to_key(left)?;
            let right_key = expr_to_key(right)?;
            Some(CseKey::Binary {
                op: *op,
                left: Box::new(left_key),
                right: Box::new(right_key),
            })
        }
        NirExprKind::Local { index, .. } => Some(CseKey::Local { index: *index }),
        NirExprKind::IntLiteral { value, .. } => Some(CseKey::IntLiteral { value: *value }),
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
fn cse_loop_body(body: &mut NirBlock, local_count: &mut u32, locals: &mut Vec<NirLocal>) -> bool {
    // Pattern: first stmt is `if !(cond) { break; }` — extract subexprs from cond
    if body.stmts.is_empty() {
        return false;
    }

    // Extract the guard condition expression
    let guard_expr = match &body.stmts[0].kind {
        NirStmtKind::If {
            condition,
            then_block,
            ..
        } => {
            // Check this is a break guard: if !(cond) { break; }
            let is_break_guard = then_block.stmts.len() == 1
                && matches!(then_block.stmts[0].kind, NirStmtKind::Break { .. });
            if !is_break_guard {
                return false;
            }
            condition
        }
        _ => return false,
    };

    // Find binary subexpressions in the guard condition
    let candidates = collect_binary_subexprs(guard_expr);
    if candidates.is_empty() {
        return false;
    }

    // For each candidate, check if it appears in the remaining loop body
    let remaining_stmts = &body.stmts[1..];
    for (key, type_id, span) in &candidates {
        // Collect locals used by this expression
        let mut used_locals = IndexSet::default();
        key_locals(key, &mut used_locals);

        // Skip trivial single-local expressions (no benefit in CSE)
        if matches!(key, CseKey::Local { .. }) {
            continue;
        }

        // Check if any of the remaining stmts contain the same expression AND
        // that used locals are not modified before that occurrence
        if has_matching_expr(remaining_stmts, key, &used_locals) {
            // Create a new local for the CSE'd expression
            let cse_local_idx = *local_count;
            *local_count += 1;
            let cse_local_name = format!("__cse_{cse_local_idx}");
            locals.push(NirLocal {
                name: cse_local_name.clone(),
                type_id: *type_id,
                is_mut: false,
            });

            // Build the Let statement for the CSE local (clone the expression from guard)
            let cse_expr = extract_matching_expr(guard_expr, key).unwrap();
            let let_stmt = NirStmt::new(
                NirStmtKind::Let {
                    name: cse_local_name.clone(),
                    local_index: cse_local_idx,
                    is_mut: false,
                    is_reactive: false,
                    type_id: *type_id,
                    value: cse_expr,
                    skip_value_copy: false,
                },
                *span,
            );

            // Replace the expression in the guard condition
            let cse_local_expr_kind = NirExprKind::Local {
                index: cse_local_idx,
                name: cse_local_name,
            };
            replace_matching_expr(&mut body.stmts[0], key, &cse_local_expr_kind, *type_id);

            // Replace matching expressions in the remaining body
            for stmt in &mut body.stmts[1..] {
                replace_matching_expr(stmt, key, &cse_local_expr_kind, *type_id);
            }

            // Insert the Let at the beginning of the loop body
            body.stmts.insert(0, let_stmt);

            return true; // One CSE per loop per pass (will iterate)
        }
    }

    false
}

/// Collect all pure binary subexpressions from an expression.
fn collect_binary_subexprs(expr: &NirExpr) -> Vec<(CseKey, TypeId, Span)> {
    let mut result = Vec::new();
    collect_binary_subexprs_rec(expr, &mut result);
    result
}

fn collect_binary_subexprs_rec(expr: &NirExpr, result: &mut Vec<(CseKey, TypeId, Span)>) {
    if let NirExprKind::Binary { left, right, .. } = &expr.kind {
        if let Some(key) = expr_to_key(expr) {
            result.push((key, expr.type_id, expr.span));
        }
        collect_binary_subexprs_rec(left, result);
        collect_binary_subexprs_rec(right, result);
    }
    // Also recurse into Unary (e.g., `!(p * p <= limit)`)
    if let NirExprKind::Unary { expr: inner, .. } = &expr.kind {
        collect_binary_subexprs_rec(inner, result);
    }
}

/// Check if any statement in the list contains the same expression,
/// and the used locals are not modified before that occurrence.
fn has_matching_expr(stmts: &[NirStmt], key: &CseKey, used_locals: &IndexSet<u32>) -> bool {
    for stmt in stmts {
        // Check if this statement modifies any of the used locals
        if stmt_modifies_any(stmt, used_locals) {
            // Locals modified before we found a match — not safe
            // But the expression might still appear in this stmt before the modification.
            // Conservative: check the stmt for the expression first.
            if stmt_contains_expr(stmt, key) {
                return true;
            }
            return false;
        }
        if stmt_contains_expr(stmt, key) {
            return true;
        }
    }
    false
}

/// Check if a statement modifies any of the given locals.
fn stmt_modifies_any(stmt: &NirStmt, locals: &IndexSet<u32>) -> bool {
    match &stmt.kind {
        NirStmtKind::Expr(e) => expr_modifies_any(e, locals),
        NirStmtKind::Let { value, .. } => expr_modifies_any(value, locals),
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_modifies_any(condition, locals)
                || block_modifies_any(then_block, locals)
                || else_block
                    .as_ref()
                    .is_some_and(|eb| block_modifies_any(eb, locals))
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            block_modifies_any(body, locals)
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            value.as_ref().is_some_and(|v| expr_modifies_any(v, locals))
        }
        NirStmtKind::LetDestructure { value, .. } => expr_modifies_any(value, locals),
        NirStmtKind::Continue => false,
    }
}

fn block_modifies_any(block: &NirBlock, locals: &IndexSet<u32>) -> bool {
    block.stmts.iter().any(|s| stmt_modifies_any(s, locals))
}

fn expr_modifies_any(expr: &NirExpr, locals: &IndexSet<u32>) -> bool {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            if let NirExprKind::Local { index, .. } = &target.kind
                && locals.contains(index)
            {
                return true;
            }
            expr_modifies_any(target, locals) || expr_modifies_any(value, locals)
        }
        NirExprKind::Binary { left, right, .. } => {
            expr_modifies_any(left, locals) || expr_modifies_any(right, locals)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => expr_modifies_any(inner, locals),
        NirExprKind::Call { args, .. } => args.iter().any(|a| expr_modifies_any(&a.expr, locals)),
        NirExprKind::MethodCall { receiver, args, .. } => {
            expr_modifies_any(receiver, locals)
                || args.iter().any(|a| expr_modifies_any(&a.expr, locals))
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_modifies_any(condition, locals)
                || block_modifies_any(then_branch, locals)
                || else_branch
                    .as_ref()
                    .is_some_and(|eb| block_modifies_any(eb, locals))
        }
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            block_modifies_any(b, locals)
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => expr_modifies_any(inner, locals) || expr_modifies_any(index, locals),
        _ => false,
    }
}

/// Check if a statement contains an expression matching the given key.
fn stmt_contains_expr(stmt: &NirStmt, key: &CseKey) -> bool {
    match &stmt.kind {
        NirStmtKind::Expr(e) => expr_contains(e, key),
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            expr_contains(value, key)
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_contains(condition, key)
                || block_contains(then_block, key)
                || else_block
                    .as_ref()
                    .is_some_and(|eb| block_contains(eb, key))
        }
        // Nested Loop: treat as opaque. CSE only proves the expression's
        // operands aren't modified between the outer guard and this point;
        // an inner loop iterates and may modify them across its own
        // iterations, which a pre-computed cache outside the inner loop
        // can't see (cf. hfs_inline_break_no_writeback.wado).
        NirStmtKind::Loop { .. } => false,
        NirStmtKind::LabeledBlock { block: body, .. } => block_contains(body, key),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            value.as_ref().is_some_and(|v| expr_contains(v, key))
        }
        NirStmtKind::Continue => false,
    }
}

fn block_contains(block: &NirBlock, key: &CseKey) -> bool {
    block.stmts.iter().any(|s| stmt_contains_expr(s, key))
}

fn expr_contains(expr: &NirExpr, key: &CseKey) -> bool {
    if expr_to_key(expr).as_ref() == Some(key) {
        return true;
    }
    match &expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            expr_contains(left, key) || expr_contains(right, key)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => expr_contains(inner, key),
        NirExprKind::Assign { target, value } => {
            expr_contains(target, key) || expr_contains(value, key)
        }
        NirExprKind::Call { args, .. } => args.iter().any(|a| expr_contains(&a.expr, key)),
        NirExprKind::MethodCall { receiver, args, .. } => {
            expr_contains(receiver, key) || args.iter().any(|a| expr_contains(&a.expr, key))
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_contains(condition, key)
                || block_contains(then_branch, key)
                || else_branch
                    .as_ref()
                    .is_some_and(|eb| block_contains(eb, key))
        }
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            block_contains(b, key)
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => expr_contains(inner, key) || expr_contains(index, key),
        _ => false,
    }
}

/// Extract (clone) the first matching expression from a TIR expression tree.
fn extract_matching_expr(expr: &NirExpr, key: &CseKey) -> Option<NirExpr> {
    if expr_to_key(expr).as_ref() == Some(key) {
        return Some(expr.clone());
    }
    match &expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            extract_matching_expr(left, key).or_else(|| extract_matching_expr(right, key))
        }
        NirExprKind::Unary { expr: inner, .. } => extract_matching_expr(inner, key),
        _ => None,
    }
}

/// Replace all occurrences of the expression matching `key` with a local reference.
fn replace_matching_expr(
    stmt: &mut NirStmt,
    key: &CseKey,
    replacement: &NirExprKind,
    type_id: TypeId,
) {
    match &mut stmt.kind {
        NirStmtKind::Expr(e) => replace_in_expr(e, key, replacement, type_id),
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            replace_in_expr(value, key, replacement, type_id);
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_in_expr(condition, key, replacement, type_id);
            replace_in_block(then_block, key, replacement, type_id);
            if let Some(eb) = else_block {
                replace_in_block(eb, key, replacement, type_id);
            }
        }
        // Nested Loop: do not descend — matches `stmt_contains_expr`'s
        // opaque treatment, so we never replace an occurrence the matcher
        // refused to look at.
        NirStmtKind::Loop { .. } => {}
        NirStmtKind::LabeledBlock { block: body, .. } => {
            replace_in_block(body, key, replacement, type_id);
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_in_expr(v, key, replacement, type_id);
            }
        }
        NirStmtKind::Continue => {}
    }
}

fn replace_in_block(
    block: &mut NirBlock,
    key: &CseKey,
    replacement: &NirExprKind,
    type_id: TypeId,
) {
    for stmt in &mut block.stmts {
        replace_matching_expr(stmt, key, replacement, type_id);
    }
}

fn replace_in_expr(expr: &mut NirExpr, key: &CseKey, replacement: &NirExprKind, type_id: TypeId) {
    if expr_to_key(expr).as_ref() == Some(key) {
        expr.kind = replacement.clone();
        expr.type_id = type_id;
        return;
    }
    match &mut expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            replace_in_expr(left, key, replacement, type_id);
            replace_in_expr(right, key, replacement, type_id);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => {
            replace_in_expr(inner, key, replacement, type_id);
        }
        NirExprKind::Assign { target, value } => {
            replace_in_expr(target, key, replacement, type_id);
            replace_in_expr(value, key, replacement, type_id);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                replace_in_expr(&mut arg.expr, key, replacement, type_id);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            replace_in_expr(receiver, key, replacement, type_id);
            for arg in args {
                replace_in_expr(&mut arg.expr, key, replacement, type_id);
            }
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_in_expr(condition, key, replacement, type_id);
            replace_in_block(then_branch, key, replacement, type_id);
            if let Some(eb) = else_branch {
                replace_in_block(eb, key, replacement, type_id);
            }
        }
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            replace_in_block(b, key, replacement, type_id);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            replace_in_expr(inner, key, replacement, type_id);
            replace_in_expr(index, key, replacement, type_id);
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            replace_in_expr(callee, key, replacement, type_id);
            for arg in args {
                replace_in_expr(arg, key, replacement, type_id);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                replace_in_expr(&mut f.value, key, replacement, type_id);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                replace_in_expr(elem, key, replacement, type_id);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                replace_in_expr(p, key, replacement, type_id);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            replace_in_expr(inner, key, replacement, type_id);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    replace_in_expr(guard, key, replacement, type_id);
                }
                replace_in_expr(&mut arm.body, key, replacement, type_id);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            replace_in_expr(scrutinee, key, replacement, type_id);
            for arm in arms {
                replace_in_block(arm, key, replacement, type_id);
            }
            replace_in_block(default, key, replacement, type_id);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            replace_in_expr(value, key, replacement, type_id);
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_in_expr(arg, key, replacement, type_id);
            }
        }
        _ => {}
    }
}
