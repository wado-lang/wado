//! Condition Implication — eliminates conditions implied false by loop guards.
//!
//! When a loop guard proves `i < bound`, any inner condition `i >= bound` is
//! known false and can be replaced with `false`. The existing `const_branch_prune`
//! pass then removes the dead branch on the next iteration.
//!
//! This subsumes the WIR-level `bounds_check` pass, handling both strict `<`
//! and inclusive `<=` guard patterns.

use crate::hashmap::IndexMap;
use crate::project::Project;
use crate::tir::{
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp,
};

struct LoopGuard {
    /// Local index of the induction variable (e.g., `i`)
    var: u32,
    /// Local index of the bound variable (e.g., `_licm_used_25`)
    bound: u32,
    /// `true` for `<` (strict), `false` for `<=` (inclusive)
    is_strict: bool,
}

#[derive(Clone)]
enum Def {
    /// `let x = local(y)` — simple copy
    Copy(u32),
    /// `let x = y + const_val`
    AddConst(u32, i64),
    /// `let x = obj.field` — field access on a local
    FieldAccess { local: u32, field_index: u32 },
    /// Struct literal: maps field_index → local_index for fields that are simple locals
    StructLit(IndexMap<u32, u32>),
}

type DefMap = IndexMap<u32, Def>;

pub fn eliminate_implied_conditions(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= process_function(&mut func);
        }
    }
    changed
}

fn process_function(func: &mut TirFunction) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut defs = DefMap::default();
    process_block(body, &mut defs)
}

fn process_block(block: &mut TirBlock, defs: &mut DefMap) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        record_def_from_stmt(stmt, defs);
        record_defs_from_nested(stmt, defs);
        changed |= process_stmt(stmt, defs);
    }
    changed
}

fn process_stmt(stmt: &mut TirStmt, defs: &mut DefMap) -> bool {
    // Record definitions from let bindings
    record_def_from_stmt(stmt, defs);

    match &mut stmt.kind {
        TirStmtKind::Loop { body } => process_loop(body, defs),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = process_block(then_block, defs);
            if let Some(else_block) = else_block {
                changed |= process_block(else_block, defs);
            }
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => process_block(block, defs),
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = process_block(then_block, defs);
            if let Some(else_block) = else_block {
                changed |= process_block(else_block, defs);
            }
            changed
        }
        _ => false,
    }
}

fn process_loop(body: &mut TirBlock, defs: &mut DefMap) -> bool {
    // First, record defs inside the loop body (for copies like `let index = i`)
    // and recurse into nested structures
    let mut changed = false;

    // Extract the loop guard from the first statement
    let guard = extract_loop_guard(&body.stmts);

    if let Some(guard) = &guard {
        // Collect defs from the loop body before eliminating
        let mut loop_defs = defs.clone();
        for stmt in body.stmts.iter() {
            record_def_from_stmt(stmt, &mut loop_defs);
            record_defs_from_nested(stmt, &mut loop_defs);
        }

        // Eliminate implied conditions in the loop body (skip the guard itself)
        for stmt in body.stmts.iter_mut().skip(1) {
            changed |= eliminate_in_stmt(stmt, guard, &loop_defs);
        }
    }

    // Recurse into nested loops
    for stmt in body.stmts.iter_mut() {
        changed |= process_stmt_nested_loops(stmt, defs);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(stmt: &mut TirStmt, defs: &mut DefMap) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Loop { body } => process_loop(body, defs),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = false;
            for s in &mut then_block.stmts {
                changed |= process_stmt_nested_loops(s, defs);
            }
            if let Some(else_block) = else_block {
                for s in &mut else_block.stmts {
                    changed |= process_stmt_nested_loops(s, defs);
                }
            }
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            let mut changed = false;
            for s in &mut block.stmts {
                changed |= process_stmt_nested_loops(s, defs);
            }
            changed
        }
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = false;
            for s in &mut then_block.stmts {
                changed |= process_stmt_nested_loops(s, defs);
            }
            if let Some(else_block) = else_block {
                for s in &mut else_block.stmts {
                    changed |= process_stmt_nested_loops(s, defs);
                }
            }
            changed
        }
        _ => false,
    }
}

/// Extract a loop guard from the first statement of a loop body.
///
/// Matches: `if !(var < bound) { break LABEL; }` → guard `var < bound`
///      or: `if !(var <= bound) { break LABEL; }` → guard `var <= bound`
fn extract_loop_guard(stmts: &[TirStmt]) -> Option<LoopGuard> {
    let first = stmts.first()?;
    let TirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &first.kind
    else {
        return None;
    };

    // then_block must be a single Break statement
    if then_block.stmts.len() != 1 {
        return None;
    }
    matches!(&then_block.stmts[0].kind, TirStmtKind::Break { .. }).then_some(())?;

    // condition must be `Not(Binary(var, Lt|LtEq, bound))`
    let TirExprKind::Unary {
        op: TirUnaryOp::Not,
        expr: inner,
    } = &condition.kind
    else {
        return None;
    };

    let TirExprKind::Binary { left, op, right } = &inner.kind else {
        return None;
    };

    let (is_strict, var_expr, bound_expr) = match op {
        TirBinaryOp::Lt => (true, left, right),
        TirBinaryOp::LtEq => (false, left, right),
        _ => return None,
    };

    let TirExprKind::Local { index: var, .. } = &var_expr.kind else {
        return None;
    };
    let TirExprKind::Local { index: bound, .. } = &bound_expr.kind else {
        return None;
    };

    Some(LoopGuard {
        var: *var,
        bound: *bound,
        is_strict: is_strict,
    })
}

fn record_def_from_stmt(stmt: &TirStmt, defs: &mut DefMap) {
    let TirStmtKind::Let {
        local_index, value, ..
    } = &stmt.kind
    else {
        return;
    };

    // Unwrap LabeledBlock to find the actual defining expression
    // (e.g., `let arr = __inline_...: { ...; break LABEL: StructLiteral { ... }; }`)
    let effective = unwrap_labeled_block_value(value);

    match &effective.kind {
        TirExprKind::Local { index, .. } => {
            defs.insert(*local_index, Def::Copy(*index));
        }
        TirExprKind::Binary { left, op, right } => {
            if let (
                TirExprKind::Local { index: lhs, .. },
                TirBinaryOp::Add,
                TirExprKind::IntLiteral { value: val, .. },
            ) = (&left.kind, op, &right.kind)
            {
                defs.insert(*local_index, Def::AddConst(*lhs, *val as i64));
            }
        }
        TirExprKind::FieldAccess {
            expr, field_index, ..
        } => {
            if let TirExprKind::Local { index, .. } = &expr.kind {
                defs.insert(
                    *local_index,
                    Def::FieldAccess {
                        local: *index,
                        field_index: *field_index,
                    },
                );
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            record_struct_lit_def(*local_index, fields, defs);
        }
        _ => {}
    }
}

fn record_struct_lit_def(
    local_index: u32,
    fields: &[crate::tir::TirStructField],
    defs: &mut DefMap,
) {
    let mut field_map = IndexMap::default();
    for f in fields {
        if let TirExprKind::Local { index, .. } = &f.value.kind {
            field_map.insert(f.field_index, *index);
        }
    }
    if !field_map.is_empty() {
        defs.insert(local_index, Def::StructLit(field_map));
    }
}

/// Unwrap a LabeledBlock expression to find the value from its break statement.
/// `LABEL: { ...; break LABEL: expr; }` → `expr`
fn unwrap_labeled_block_value(expr: &TirExpr) -> &TirExpr {
    if let TirExprKind::LabeledBlock { block, label, .. } = &expr.kind {
        // Find the break statement that returns a value from this block
        for stmt in &block.stmts {
            if let TirStmtKind::Break {
                label: Some(break_label),
                value: Some(val),
            } = &stmt.kind
            {
                if break_label == label {
                    // Recursively unwrap in case of nested labeled blocks
                    return unwrap_labeled_block_value(val);
                }
            }
        }
    }
    expr
}

/// Record defs from nested blocks within a statement (e.g., labeled blocks in expressions).
fn record_defs_from_nested(stmt: &TirStmt, defs: &mut DefMap) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            record_defs_from_expr(value, defs);
        }
        TirStmtKind::Expr(expr) => {
            record_defs_from_expr(expr, defs);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                record_def_from_stmt(s, defs);
                record_defs_from_nested(s, defs);
            }
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            for s in &then_block.stmts {
                record_def_from_stmt(s, defs);
                record_defs_from_nested(s, defs);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    record_def_from_stmt(s, defs);
                    record_defs_from_nested(s, defs);
                }
            }
        }
        _ => {}
    }
}

fn record_defs_from_expr(expr: &TirExpr, defs: &mut DefMap) {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                record_def_from_stmt(s, defs);
                record_defs_from_nested(s, defs);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            record_defs_from_expr(left, defs);
            record_defs_from_expr(right, defs);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            record_defs_from_expr(inner, defs);
        }
        TirExprKind::Assign { target, value } => {
            record_defs_from_expr(target, defs);
            record_defs_from_expr(value, defs);
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            for s in &then_branch.stmts {
                record_def_from_stmt(s, defs);
                record_defs_from_nested(s, defs);
            }
            if let Some(eb) = else_branch {
                for s in &eb.stmts {
                    record_def_from_stmt(s, defs);
                    record_defs_from_nested(s, defs);
                }
            }
        }
        _ => {}
    }
}

/// Eliminate implied-false conditions within a statement.
fn eliminate_in_stmt(stmt: &mut TirStmt, guard: &LoopGuard, defs: &DefMap) -> bool {
    // Check if this stmt itself is a bounds check pattern
    if let TirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &mut stmt.kind
    {
        if is_panic_block(then_block) && is_implied_false(condition, guard, defs) {
            *condition = TirExpr {
                kind: TirExprKind::BoolLiteral(false),
                type_id: condition.type_id,
                span: condition.span.clone(),
            };
            return true;
        }
    }

    // Recurse into sub-structures
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => eliminate_in_expr(value, guard, defs),
        TirStmtKind::Expr(expr) => eliminate_in_expr(expr, guard, defs),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = eliminate_in_block(then_block, guard, defs);
            if let Some(eb) = else_block {
                changed |= eliminate_in_block(eb, guard, defs);
            }
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => eliminate_in_block(block, guard, defs),
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = eliminate_in_block(then_block, guard, defs);
            if let Some(eb) = else_block {
                changed |= eliminate_in_block(eb, guard, defs);
            }
            changed
        }
        _ => false,
    }
}

fn eliminate_in_block(block: &mut TirBlock, guard: &LoopGuard, defs: &DefMap) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= eliminate_in_stmt(stmt, guard, defs);
    }
    changed
}

fn eliminate_in_expr(expr: &mut TirExpr, guard: &LoopGuard, defs: &DefMap) -> bool {
    match &mut expr.kind {
        TirExprKind::LabeledBlock { block, .. } => eliminate_in_block(block, guard, defs),
        TirExprKind::Binary { left, right, .. } => {
            let mut c = eliminate_in_expr(left, guard, defs);
            c |= eliminate_in_expr(right, guard, defs);
            c
        }
        TirExprKind::Unary { expr: inner, .. } => eliminate_in_expr(inner, guard, defs),
        TirExprKind::Assign { target, value } => {
            let mut c = eliminate_in_expr(target, guard, defs);
            c |= eliminate_in_expr(value, guard, defs);
            c
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            let mut c = eliminate_in_block(then_branch, guard, defs);
            if let Some(eb) = else_branch {
                c |= eliminate_in_block(eb, guard, defs);
            }
            c
        }
        TirExprKind::Call { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= eliminate_in_expr(&mut arg.expr, guard, defs);
            }
            c
        }
        TirExprKind::MethodCall { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= eliminate_in_expr(&mut arg.expr, guard, defs);
            }
            c
        }
        _ => false,
    }
}

/// Check if a condition is implied false by the loop guard.
///
/// For a `<` guard (`var < bound`):
///   `check_var >= check_bound` is false when both resolve to the same locals.
///
/// For a `<=` guard (`var <= limit`):
///   `check_var >= check_bound` is false when check_var resolves to var
///   AND check_bound resolves to `limit + 1`.
fn is_implied_false(condition: &TirExpr, guard: &LoopGuard, defs: &DefMap) -> bool {
    let TirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };

    // We're looking for `check_var >= check_bound`
    if *op != TirBinaryOp::GtEq {
        return false;
    }

    let TirExprKind::Local {
        index: check_var, ..
    } = &left.kind
    else {
        return false;
    };
    let TirExprKind::Local {
        index: check_bound, ..
    } = &right.kind
    else {
        return false;
    };

    // check_var must resolve to the guard's induction variable
    if !resolves_to(*check_var, guard.var, defs) {
        return false;
    }

    if guard.is_strict {
        // For `<` guard: check_bound must resolve to the same bound
        resolves_to(*check_bound, guard.bound, defs)
    } else {
        // For `<=` guard: check_bound must resolve to `guard.bound + 1`
        resolves_to_plus_one(*check_bound, guard.bound, defs)
    }
}

const MAX_CHAIN_DEPTH: usize = 10;

/// Check if `source` resolves to `target` by following copy chains.
fn resolves_to(source: u32, target: u32, defs: &DefMap) -> bool {
    resolves_to_inner(source, target, defs, 0)
}

fn resolves_to_inner(source: u32, target: u32, defs: &DefMap, depth: usize) -> bool {
    if source == target {
        return true;
    }
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolves_to_inner(*next, target, defs, depth + 1),
        _ => false,
    }
}

/// Check if `source` resolves to `target + 1` by following definition chains.
///
/// Handles chains like:
///   `_licm_used_9` → FieldAccess(arr, .used) → StructLit(arr).used → `n` → AddConst(limit, 1)
fn resolves_to_plus_one(source: u32, target: u32, defs: &DefMap) -> bool {
    resolves_to_plus_one_inner(source, target, defs, 0)
}

fn resolves_to_plus_one_inner(source: u32, target: u32, defs: &DefMap, depth: usize) -> bool {
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolves_to_plus_one_inner(*next, target, defs, depth + 1),
        Some(Def::AddConst(base, 1)) => resolves_to_inner(*base, target, defs, depth + 1),
        Some(Def::FieldAccess { local, field_index }) => {
            // Follow: `_licm_used = arr.used` → look up arr's struct literal
            if let Some(Def::StructLit(fields)) = defs.get(local) {
                if let Some(field_local) = fields.get(field_index) {
                    return resolves_to_plus_one_inner(*field_local, target, defs, depth + 1);
                }
            }
            // Also follow through copies of the struct local
            if let Some(Def::Copy(next)) = defs.get(local) {
                if let Some(Def::StructLit(fields)) = defs.get(next) {
                    if let Some(field_local) = fields.get(field_index) {
                        return resolves_to_plus_one_inner(
                            *field_local,
                            target,
                            defs,
                            depth + 1,
                        );
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Check if a block consists of a panic call (bounds check failure path).
fn is_panic_block(block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| match &s.kind {
        TirStmtKind::Expr(expr) => is_panic_call(expr),
        _ => false,
    })
}

fn is_panic_call(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Call { func, .. } => func.name.contains("panic"),
        _ => false,
    }
}
