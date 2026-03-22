//! Condition Implication — eliminates conditions implied false by guards.
//!
//! When a loop guard proves `i < bound`, any inner condition `i >= bound` is
//! known false and can be replaced with `false`. The existing `const_branch_prune`
//! pass then removes the dead branch on the next iteration.
//!
//! Also handles dominating if-conditions: when `if (var + offset) < bound { ... }`,
//! bounds checks `(var + k) >= bound` for `k <= offset` inside the then-block
//! are known false.
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

/// A dominating if-condition that proves `var + k < bound` for all `k <= max_offset`.
/// Extracted from `if (var + max_offset) < bound { then_block }`.
#[derive(Clone)]
struct DominatingGuard {
    /// Local index of the base variable
    var: u32,
    /// Maximum offset proven: `var + max_offset < bound`
    max_offset: i64,
    /// Local index of the bound
    bound: u32,
}

#[derive(Clone)]
enum Def {
    /// `let x = local(y)` — simple copy
    Copy(u32),
    /// `let x = y + const_val`
    AddConst(u32, i64),
    /// `let x = y & const_val` — bitmask, result is in `[0, mask]`
    BitAndConst(i64),
    /// `let x = int_literal` — known constant
    IntConst(i64),
    /// `let x = obj.field` — field access on a local
    FieldAccess { local: u32, field_index: u32 },
    /// Struct literal: maps `field_index` → source for fields
    StructLit(IndexMap<u32, FieldSource>),
}

#[derive(Clone)]
enum FieldSource {
    Local(u32),
    Const(i64),
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
        changed |= eliminate_bitmask_bounded(stmt, defs);
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

    // Collect defs from the loop body before eliminating
    let mut loop_defs = defs.clone();
    for stmt in &body.stmts {
        record_def_from_stmt(stmt, &mut loop_defs);
        record_defs_from_nested(stmt, &mut loop_defs);
    }

    // Extract the loop guard from the first statement
    let guard = extract_loop_guard(&body.stmts);

    if let Some(guard) = &guard {
        // Eliminate implied conditions in the loop body (skip the guard itself)
        for stmt in body.stmts.iter_mut().skip(1) {
            changed |= eliminate_in_stmt(stmt, guard, &[], &loop_defs);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body
    for stmt in &mut body.stmts {
        changed |= eliminate_bitmask_bounded(stmt, &loop_defs);
    }

    // Recurse into nested loops
    for stmt in &mut body.stmts {
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
        is_strict,
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
            } else if *op == TirBinaryOp::BitAnd {
                if let TirExprKind::IntLiteral { value: val, .. } = &right.kind {
                    defs.insert(*local_index, Def::BitAndConst(*val as i64));
                } else if let TirExprKind::IntLiteral { value: val, .. } = &left.kind {
                    defs.insert(*local_index, Def::BitAndConst(*val as i64));
                }
            }
        }
        TirExprKind::IntLiteral { value: val, .. } => {
            defs.insert(*local_index, Def::IntConst(*val as i64));
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
            field_map.insert(f.field_index, FieldSource::Local(*index));
        } else if let TirExprKind::IntLiteral { value, .. } = &f.value.kind {
            field_map.insert(f.field_index, FieldSource::Const(*value as i64));
        }
    }
    if !field_map.is_empty() {
        defs.insert(local_index, Def::StructLit(field_map));
    }
}

/// Unwrap a `LabeledBlock` expression to find the value from its break statement.
/// `LABEL: { ...; break LABEL: expr; }` → `expr`
fn unwrap_labeled_block_value(expr: &TirExpr) -> &TirExpr {
    if let TirExprKind::LabeledBlock { block, label, .. } = &expr.kind {
        // Find the break statement that returns a value from this block
        for stmt in &block.stmts {
            if let TirStmtKind::Break {
                label: Some(break_label),
                value: Some(val),
            } = &stmt.kind
                && break_label == label
            {
                // Recursively unwrap in case of nested labeled blocks
                return unwrap_labeled_block_value(val);
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
            condition,
            then_block,
            else_block,
        } => {
            record_defs_from_expr(condition, defs);
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
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            record_defs_from_expr(scrutinee, defs);
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
        TirStmtKind::Return { value: Some(expr) }
        | TirStmtKind::Break {
            value: Some(expr), ..
        } => {
            record_defs_from_expr(expr, defs);
        }
        _ => {}
    }
}

fn record_defs_from_expr(expr: &TirExpr, defs: &mut DefMap) {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            for s in &block.stmts {
                record_def_from_stmt(s, defs);
                record_defs_from_nested(s, defs);
            }
        }
        TirExprKind::Binary { left, right, .. }
        | TirExprKind::Assign {
            target: left,
            value: right,
        }
        | TirExprKind::Index {
            expr: left,
            index: right,
        } => {
            record_defs_from_expr(left, defs);
            record_defs_from_expr(right, defs);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. }
        | TirExprKind::TupleSpread { expr: inner, .. } => {
            record_defs_from_expr(inner, defs);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            record_defs_from_expr(condition, defs);
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
        TirExprKind::Call { args, .. } => {
            for arg in args {
                record_defs_from_expr(&arg.expr, defs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            record_defs_from_expr(receiver, defs);
            for arg in args {
                record_defs_from_expr(&arg.expr, defs);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                record_defs_from_expr(&f.value, defs);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::CmRawCall { args: elements, .. } => {
            for e in elements {
                record_defs_from_expr(e, defs);
            }
        }
        TirExprKind::VariantConstruct {
            payload: Some(inner),
            ..
        } => {
            record_defs_from_expr(inner, defs);
        }
        _ => {}
    }
}

/// Eliminate implied-false conditions within a statement.
fn eliminate_in_stmt(
    stmt: &mut TirStmt,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    // Check if this stmt itself is a bounds check pattern
    if let TirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &mut stmt.kind
        && is_panic_block(then_block)
        && is_implied_false_by_any(condition, guard, dom_guards, defs)
    {
        *condition = TirExpr {
            kind: TirExprKind::BoolLiteral(false),
            type_id: condition.type_id,
            span: condition.span,
        };
        return true;
    }

    // Recurse into sub-structures
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => eliminate_in_expr(value, guard, dom_guards, defs),
        TirStmtKind::Expr(expr) => eliminate_in_expr(expr, guard, dom_guards, defs),
        TirStmtKind::Return { value: Some(expr) }
        | TirStmtKind::Break {
            value: Some(expr), ..
        } => eliminate_in_expr(expr, guard, dom_guards, defs),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = eliminate_in_expr(condition, guard, dom_guards, defs);
            // If condition is `(var + offset) < bound`, create a dominating guard
            // for the then-block to eliminate bounds checks on var+k for k <= offset.
            let dom = extract_dominating_guard(condition, defs);
            if let Some(dg) = dom {
                let mut extended = dom_guards.to_vec();
                extended.push(dg);
                changed |= eliminate_in_block(then_block, guard, &extended, defs);
            } else {
                changed |= eliminate_in_block(then_block, guard, dom_guards, defs);
            }
            if let Some(eb) = else_block {
                changed |= eliminate_in_block(eb, guard, dom_guards, defs);
            }
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            eliminate_in_block(block, guard, dom_guards, defs)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = eliminate_in_expr(scrutinee, guard, dom_guards, defs);
            changed |= eliminate_in_block(then_block, guard, dom_guards, defs);
            if let Some(eb) = else_block {
                changed |= eliminate_in_block(eb, guard, dom_guards, defs);
            }
            changed
        }
        _ => false,
    }
}

fn eliminate_in_block(
    block: &mut TirBlock,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= eliminate_in_stmt(stmt, guard, dom_guards, defs);
    }
    changed
}

fn eliminate_in_expr(
    expr: &mut TirExpr,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    match &mut expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            eliminate_in_block(block, guard, dom_guards, defs)
        }
        TirExprKind::Binary { left, right, .. }
        | TirExprKind::Assign {
            target: left,
            value: right,
        }
        | TirExprKind::Index {
            expr: left,
            index: right,
        } => {
            let mut c = eliminate_in_expr(left, guard, dom_guards, defs);
            c |= eliminate_in_expr(right, guard, dom_guards, defs);
            c
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. }
        | TirExprKind::TupleSpread { expr: inner, .. } => {
            eliminate_in_expr(inner, guard, dom_guards, defs)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut c = eliminate_in_expr(condition, guard, dom_guards, defs);
            let dom = extract_dominating_guard(condition, defs);
            if let Some(dg) = dom {
                let mut extended = dom_guards.to_vec();
                extended.push(dg);
                c |= eliminate_in_block(then_branch, guard, &extended, defs);
            } else {
                c |= eliminate_in_block(then_branch, guard, dom_guards, defs);
            }
            if let Some(eb) = else_branch {
                c |= eliminate_in_block(eb, guard, dom_guards, defs);
            }
            c
        }
        TirExprKind::Call { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= eliminate_in_expr(&mut arg.expr, guard, dom_guards, defs);
            }
            c
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let mut c = eliminate_in_expr(receiver, guard, dom_guards, defs);
            for arg in args {
                c |= eliminate_in_expr(&mut arg.expr, guard, dom_guards, defs);
            }
            c
        }
        TirExprKind::StructLiteral { fields, .. } => {
            let mut c = false;
            for f in fields {
                c |= eliminate_in_expr(&mut f.value, guard, dom_guards, defs);
            }
            c
        }
        TirExprKind::TupleLiteral { elements } => {
            let mut c = false;
            for e in elements {
                c |= eliminate_in_expr(e, guard, dom_guards, defs);
            }
            c
        }
        TirExprKind::VariantConstruct {
            payload: Some(inner),
            ..
        } => eliminate_in_expr(inner, guard, dom_guards, defs),
        _ => false,
    }
}

/// Eliminate bounds checks where the index is masked by a bitmask and the bound
/// exceeds the mask's maximum value.
///
/// Pattern: `if (x & MASK) >= BOUND { panic(...) }` where `BOUND > MASK >= 0`
/// Since `(x & MASK)` is always in `[0, MASK]`, `(x & MASK) >= BOUND` is always false.
fn eliminate_bitmask_bounded(stmt: &mut TirStmt, defs: &DefMap) -> bool {
    if let TirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &mut stmt.kind
        && is_panic_block(then_block)
        && is_bitmask_bounded(condition, defs)
    {
        *condition = TirExpr {
            kind: TirExprKind::BoolLiteral(false),
            type_id: condition.type_id,
            span: condition.span,
        };
        return true;
    }

    // Recurse into sub-structures
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => eliminate_bitmask_in_expr(value, defs),
        TirStmtKind::Expr(expr) => eliminate_bitmask_in_expr(expr, defs),
        TirStmtKind::Return { value: Some(expr) }
        | TirStmtKind::Break {
            value: Some(expr), ..
        } => eliminate_bitmask_in_expr(expr, defs),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = eliminate_bitmask_in_expr(condition, defs);
            for s in &mut then_block.stmts {
                changed |= eliminate_bitmask_bounded(s, defs);
            }
            if let Some(eb) = else_block {
                for s in &mut eb.stmts {
                    changed |= eliminate_bitmask_bounded(s, defs);
                }
            }
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            let mut changed = false;
            for s in &mut block.stmts {
                changed |= eliminate_bitmask_bounded(s, defs);
            }
            changed
        }
        TirStmtKind::Loop { body } => {
            let mut changed = false;
            for s in &mut body.stmts {
                changed |= eliminate_bitmask_bounded(s, defs);
            }
            changed
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = eliminate_bitmask_in_expr(scrutinee, defs);
            for s in &mut then_block.stmts {
                changed |= eliminate_bitmask_bounded(s, defs);
            }
            if let Some(eb) = else_block {
                for s in &mut eb.stmts {
                    changed |= eliminate_bitmask_bounded(s, defs);
                }
            }
            changed
        }
        _ => false,
    }
}

fn eliminate_bitmask_in_expr(expr: &mut TirExpr, defs: &DefMap) -> bool {
    match &mut expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            let mut changed = false;
            for s in &mut block.stmts {
                changed |= eliminate_bitmask_bounded(s, defs);
            }
            changed
        }
        TirExprKind::Binary { left, right, .. }
        | TirExprKind::Assign {
            target: left,
            value: right,
        }
        | TirExprKind::Index {
            expr: left,
            index: right,
        } => {
            let mut c = eliminate_bitmask_in_expr(left, defs);
            c |= eliminate_bitmask_in_expr(right, defs);
            c
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. }
        | TirExprKind::TupleSpread { expr: inner, .. } => eliminate_bitmask_in_expr(inner, defs),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut c = eliminate_bitmask_in_expr(condition, defs);
            for s in &mut then_branch.stmts {
                c |= eliminate_bitmask_bounded(s, defs);
            }
            if let Some(eb) = else_branch {
                for s in &mut eb.stmts {
                    c |= eliminate_bitmask_bounded(s, defs);
                }
            }
            c
        }
        TirExprKind::Call { args, .. } => {
            let mut c = false;
            for arg in args {
                c |= eliminate_bitmask_in_expr(&mut arg.expr, defs);
            }
            c
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let mut c = eliminate_bitmask_in_expr(receiver, defs);
            for arg in args {
                c |= eliminate_bitmask_in_expr(&mut arg.expr, defs);
            }
            c
        }
        TirExprKind::StructLiteral { fields, .. } => {
            let mut c = false;
            for f in fields {
                c |= eliminate_bitmask_in_expr(&mut f.value, defs);
            }
            c
        }
        TirExprKind::TupleLiteral { elements } => {
            let mut c = false;
            for e in elements {
                c |= eliminate_bitmask_in_expr(e, defs);
            }
            c
        }
        TirExprKind::VariantConstruct {
            payload: Some(inner),
            ..
        } => eliminate_bitmask_in_expr(inner, defs),
        _ => false,
    }
}

/// Check if `(index >= bound)` is provably false because index is bitmask-bounded.
///
/// `(x & MASK) >= BOUND` is false when `MASK >= 0` and `BOUND > MASK`.
fn is_bitmask_bounded(condition: &TirExpr, defs: &DefMap) -> bool {
    let TirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };

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

    // Find the maximum value of check_var (if bitmask-bounded)
    let Some(max_val) = resolve_max_value(*check_var, defs) else {
        return false;
    };

    // Find the constant value of check_bound
    let Some(bound_val) = resolve_constant(*check_bound, defs) else {
        return false;
    };

    // If max possible value < bound, then `check_var >= bound` is always false
    bound_val > 0 && max_val < bound_val
}

/// Resolve the maximum possible value of a variable through definition chains.
/// Returns `Some(max)` if the variable is provably bounded by `max`.
fn resolve_max_value(var: u32, defs: &DefMap) -> Option<i64> {
    resolve_max_value_inner(var, defs, 0)
}

fn resolve_max_value_inner(var: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&var) {
        Some(Def::BitAndConst(mask)) if *mask >= 0 => Some(*mask),
        Some(Def::Copy(next)) => resolve_max_value_inner(*next, defs, depth + 1),
        Some(Def::IntConst(val)) => Some(*val),
        _ => None,
    }
}

/// Resolve a variable to a constant value through definition chains.
fn resolve_constant(var: u32, defs: &DefMap) -> Option<i64> {
    resolve_constant_inner(var, defs, 0)
}

fn resolve_constant_inner(var: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&var) {
        Some(Def::IntConst(val)) => Some(*val),
        Some(Def::Copy(next)) => resolve_constant_inner(*next, defs, depth + 1),
        Some(Def::FieldAccess { local, field_index }) => {
            resolve_constant_through_struct(*local, *field_index, defs, depth)
        }
        _ => None,
    }
}

fn resolve_constant_through_struct(
    local: u32,
    field_index: u32,
    defs: &DefMap,
    depth: usize,
) -> Option<i64> {
    let struct_def = match defs.get(&local) {
        Some(Def::StructLit(fields)) => Some(fields),
        Some(Def::Copy(next)) => {
            if let Some(Def::StructLit(fields)) = defs.get(next) {
                Some(fields)
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(fields) = struct_def
        && let Some(source) = fields.get(&field_index)
    {
        match source {
            FieldSource::Const(val) => Some(*val),
            FieldSource::Local(local) => resolve_constant_inner(*local, defs, depth + 1),
        }
    } else {
        None
    }
}

/// Check if a condition is implied false by the loop guard.
///
/// For a `<` guard (`var < bound`):
///   `check_var >= check_bound` is false when both resolve to the same locals.
///
/// For a `<=` guard (`var <= limit`):
///   `check_var >= check_bound` is false when `check_var` resolves to var
///   AND `check_bound` resolves to `limit + 1`.
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

/// Check if a condition is implied false by the loop guard OR any dominating guard.
fn is_implied_false_by_any(
    condition: &TirExpr,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    if is_implied_false(condition, guard, defs) {
        return true;
    }
    for dg in dom_guards {
        if is_implied_by_dominating_guard(condition, dg, defs) {
            return true;
        }
    }
    false
}

/// Check if `check_var >= check_bound` is implied false by a dominating guard.
///
/// A dominating guard `(var + max_offset) < bound` proves that
/// `var + k < bound` for all `k <= max_offset` (assuming non-negative offsets).
/// So `(var + k) >= bound` is false for `k <= max_offset`.
fn is_implied_by_dominating_guard(
    condition: &TirExpr,
    dg: &DominatingGuard,
    defs: &DefMap,
) -> bool {
    let TirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };
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

    // check_bound must resolve to the dominating guard's bound
    if !resolves_to(*check_bound, dg.bound, defs) {
        return false;
    }

    // check_var must resolve to `dg.var + k` where `k <= dg.max_offset`
    resolve_offset_from(*check_var, dg.var, defs)
        .is_some_and(|offset| offset >= 0 && offset <= dg.max_offset)
}

/// Extract a dominating guard from an if-condition.
///
/// Matches: `(var + offset) < bound` → `DominatingGuard` { var, `max_offset`: offset, bound }
///      or: `var < bound` → `DominatingGuard` { var, `max_offset`: 0, bound }
fn extract_dominating_guard(condition: &TirExpr, defs: &DefMap) -> Option<DominatingGuard> {
    let TirExprKind::Binary { left, op, right } = &condition.kind else {
        return None;
    };
    if *op != TirBinaryOp::Lt {
        return None;
    }
    let TirExprKind::Local {
        index: bound_var, ..
    } = &right.kind
    else {
        return None;
    };

    // Left side: either `var` or `var + offset`
    match &left.kind {
        TirExprKind::Local { index: var, .. } => Some(DominatingGuard {
            var: *var,
            max_offset: 0,
            bound: *bound_var,
        }),
        TirExprKind::Binary {
            left: inner_left,
            op: TirBinaryOp::Add,
            right: inner_right,
        } => {
            let TirExprKind::Local { index: var, .. } = &inner_left.kind else {
                return None;
            };
            // Offset can be a literal or a local resolving to a constant
            let offset = match &inner_right.kind {
                TirExprKind::IntLiteral { value, .. } => *value as i64,
                TirExprKind::Local { index, .. } => resolve_constant(*index, defs)?,
                _ => return None,
            };
            if offset < 0 {
                return None;
            }
            Some(DominatingGuard {
                var: *var,
                max_offset: offset,
                bound: *bound_var,
            })
        }
        _ => None,
    }
}

/// Resolve the offset of `source` from `base`: if `source` resolves to `base + k`, return `Some(k)`.
/// If `source` resolves directly to `base`, return `Some(0)`.
fn resolve_offset_from(source: u32, base: u32, defs: &DefMap) -> Option<i64> {
    resolve_offset_from_inner(source, base, defs, 0)
}

fn resolve_offset_from_inner(source: u32, base: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if source == base {
        return Some(0);
    }
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolve_offset_from_inner(*next, base, defs, depth + 1),
        Some(Def::AddConst(var, offset)) => {
            let base_offset = resolve_offset_from_inner(*var, base, defs, depth + 1)?;
            Some(base_offset + offset)
        }
        _ => None,
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
        Some(Def::FieldAccess {
            local: src_local,
            field_index: src_field,
        }) => {
            // Two locals derived from the same field access on the same base are equal.
            // This handles the pattern where LICM hoists `obj.field` to a new local while
            // the original `let x = obj.field` already exists — both read the same value.
            resolves_field_access_to(target, *src_local, *src_field, defs, depth)
        }
        _ => false,
    }
}

/// Check if `target` also resolves to `FieldAccess(base_local, field_index)`.
fn resolves_field_access_to(
    target: u32,
    base_local: u32,
    field_index: u32,
    defs: &DefMap,
    depth: usize,
) -> bool {
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&target) {
        Some(Def::Copy(next)) => {
            resolves_field_access_to(*next, base_local, field_index, defs, depth + 1)
        }
        Some(Def::FieldAccess {
            local: tgt_local,
            field_index: tgt_field,
        }) => {
            *tgt_field == field_index && resolves_to_inner(*tgt_local, base_local, defs, depth + 1)
        }
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
            let field_source = resolve_field_source(*local, *field_index, defs);
            match field_source {
                Some(FieldSource::Local(field_local)) => {
                    resolves_to_plus_one_inner(field_local, target, defs, depth + 1)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn resolve_field_source(local: u32, field_index: u32, defs: &DefMap) -> Option<FieldSource> {
    let struct_def = match defs.get(&local) {
        Some(Def::StructLit(fields)) => Some(fields),
        Some(Def::Copy(next)) => {
            if let Some(Def::StructLit(fields)) = defs.get(next) {
                Some(fields)
            } else {
                None
            }
        }
        _ => None,
    };
    struct_def
        .and_then(|fields| fields.get(&field_index))
        .cloned()
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
