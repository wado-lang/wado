//! Reference elimination optimization for Wado TIR
//!
//! Eliminates unnecessary reference bindings introduced during function inlining.
//! After inlining, we often have patterns like:
//!
//! ```text
//! let self: &Array<T> = &arr;
//! ... self.repr ...
//! ```
//!
//! This can be optimized to:
//!
//! ```text
//! ... arr.repr ...
//! ```
//!
//! The algorithm uses a two-pass approach that processes ALL ref bindings
//! simultaneously, avoiding the O(K × N) cost of processing each binding
//! separately (where K = number of bindings, N = body size).
//!
//! Pass 1 (analyze): Single traversal to collect all `let r = &v` bindings
//!   and classify every use of each `r` as field-access-only or not.
//! Pass 2 (transform): Single traversal to replace eliminable field accesses
//!   and remove dead let statements.

use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};
use crate::hashmap::IndexMap;

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// Local index of the original variable (`local_var` in `let r = &local_var`)
    target_local: u32,
    /// Name of the original variable
    target_name: String,
    /// True until a non-field-access use is found
    eliminable: bool,
}

/// Pass 1: Collect all ref bindings and analyze uses in a single traversal.
///
/// Walks the entire function body once, building an `IndexMap<u32, RefInfo>` for
/// every `let r: &T = &v` binding found. Simultaneously, for every expression
/// that uses a tracked local, checks whether the use is a field access. If any
/// non-field-access use is found, marks the binding as non-eliminable.
fn analyze_refs_in_block(block: &TirBlock, refs: &mut IndexMap<u32, RefInfo>) {
    for stmt in &block.stmts {
        // Check if this statement defines a new ref binding
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Unary { op, expr } = &value.kind
            && matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
            && let TirExprKind::Local { index, name } = &expr.kind
        {
            refs.insert(
                *local_index,
                RefInfo {
                    target_local: *index,
                    target_name: name.clone(),
                    eliminable: true,
                },
            );
        }
        // Analyze uses within this statement
        analyze_uses_in_stmt(stmt, refs);
    }
}

fn analyze_uses_in_stmt(stmt: &TirStmt, refs: &mut IndexMap<u32, RefInfo>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => analyze_uses_in_expr(value, refs),
        TirStmtKind::Expr(expr) => analyze_uses_in_expr(expr, refs),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_uses_in_expr(condition, refs);
            analyze_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirStmtKind::Loop { body } => analyze_refs_in_block(body, refs),
        TirStmtKind::LabeledBlock { block, .. } => analyze_refs_in_block(block, refs),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            analyze_uses_in_expr(scrutinee, refs);
            analyze_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => analyze_uses_in_expr(value, refs),
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn analyze_uses_in_expr(expr: &TirExpr, refs: &mut IndexMap<u32, RefInfo>) {
    match &expr.kind {
        // Field access on a tracked ref local: this is the pattern we want to optimize.
        // The use is acceptable (field-access-only), so we DON'T mark it as non-eliminable.
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if let TirExprKind::Local { index, .. } = &inner.kind
                && refs.contains_key(index)
            {
                // This is a field access on a tracked ref - acceptable use, skip.
                return;
            }
            // Not a field access on tracked ref - recurse normally
            analyze_uses_in_expr(inner, refs);
        }
        // Direct use of a tracked ref local (not through field access): non-eliminable
        TirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get_mut(index) {
                info.eliminable = false;
            }
        }
        // Recursive cases
        TirExprKind::Binary { left, right, .. } => {
            analyze_uses_in_expr(left, refs);
            analyze_uses_in_expr(right, refs);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            analyze_uses_in_expr(inner, refs);
        }
        TirExprKind::Assign { target, value } => {
            analyze_uses_in_expr(target, refs);
            analyze_uses_in_expr(value, refs);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            analyze_uses_in_expr(receiver, refs);
            for arg in args {
                analyze_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            analyze_uses_in_expr(callee, refs);
            for arg in args {
                analyze_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            analyze_uses_in_expr(functor, refs);
        }
        TirExprKind::Index { expr: inner, index } => {
            analyze_uses_in_expr(inner, refs);
            analyze_uses_in_expr(index, refs);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            analyze_refs_in_block(block, refs);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_uses_in_expr(condition, refs);
            analyze_refs_in_block(then_branch, refs);
            if let Some(eb) = else_branch {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_uses_in_expr(&field.value, refs);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                analyze_uses_in_expr(elem, refs);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_uses_in_expr(payload_expr, refs);
            }
        }
        TirExprKind::Closure { body, .. } => {
            analyze_uses_in_expr(body, refs);
        }
        TirExprKind::Match { expr: inner, arms } => {
            analyze_uses_in_expr(inner, refs);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_uses_in_expr(guard, refs);
                }
                analyze_uses_in_expr(&arm.body, refs);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_uses_in_expr(scrutinee, refs);
            for arm in arms {
                analyze_refs_in_block(arm, refs);
            }
            analyze_refs_in_block(default, refs);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Pass 2: Replace field accesses and remove dead bindings in a single traversal.
///
/// For each eliminable binding `let r = &v`, replaces `r.field` with `v.field`
/// and removes the dead `let r` statement.
fn transform_block(block: &mut TirBlock, eliminable: &IndexMap<u32, RefInfo>) {
    // Remove dead let statements for eliminable bindings
    block.stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !eliminable.contains_key(local_index)
        } else {
            true
        }
    });

    for stmt in &mut block.stmts {
        transform_stmt(stmt, eliminable);
    }
}

fn transform_stmt(stmt: &mut TirStmt, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => transform_expr(value, eliminable),
        TirStmtKind::Expr(expr) => transform_expr(expr, eliminable),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            transform_expr(condition, eliminable);
            transform_block(then_block, eliminable);
            if let Some(eb) = else_block {
                transform_block(eb, eliminable);
            }
        }
        TirStmtKind::Loop { body } => transform_block(body, eliminable),
        TirStmtKind::LabeledBlock { block, .. } => transform_block(block, eliminable),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            transform_expr(scrutinee, eliminable);
            transform_block(then_block, eliminable);
            if let Some(eb) = else_block {
                transform_block(eb, eliminable);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => transform_expr(value, eliminable),
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn transform_expr(expr: &mut TirExpr, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            // Check if the inner expression is a local that should be replaced
            if let TirExprKind::Local { index, .. } = &inner.kind
                && let Some(info) = eliminable.get(index)
            {
                **inner = TirExpr::new(
                    TirExprKind::Local {
                        index: info.target_local,
                        name: info.target_name.clone(),
                    },
                    inner.type_id,
                    inner.span,
                );
                return;
            }
            transform_expr(inner, eliminable);
        }
        TirExprKind::Binary { left, right, .. } => {
            transform_expr(left, eliminable);
            transform_expr(right, eliminable);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            transform_expr(inner, eliminable);
        }
        TirExprKind::Assign { target, value } => {
            transform_expr(target, eliminable);
            transform_expr(value, eliminable);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            transform_expr(receiver, eliminable);
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            transform_expr(callee, eliminable);
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            transform_expr(functor, eliminable);
        }
        TirExprKind::Index { expr: inner, index } => {
            transform_expr(inner, eliminable);
            transform_expr(index, eliminable);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            transform_block(block, eliminable);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            transform_expr(condition, eliminable);
            transform_block(then_branch, eliminable);
            if let Some(eb) = else_branch {
                transform_block(eb, eliminable);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                transform_expr(&mut field.value, eliminable);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                transform_expr(elem, eliminable);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                transform_expr(payload_expr, eliminable);
            }
        }
        TirExprKind::Closure { body, .. } => {
            transform_expr(body, eliminable);
        }
        TirExprKind::Match { expr: inner, arms } => {
            transform_expr(inner, eliminable);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    transform_expr(guard, eliminable);
                }
                transform_expr(&mut arm.body, eliminable);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            transform_expr(scrutinee, eliminable);
            for arm in arms {
                transform_block(arm, eliminable);
            }
            transform_block(default, eliminable);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Eliminate unnecessary reference bindings in a single function.
fn eliminate_refs_in_function(func: &mut TirFunction, _type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Pass 1: Collect all ref bindings and analyze uses in a single traversal.
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_refs_in_block(body, &mut refs);

    // Filter to only eliminable bindings
    let eliminable: IndexMap<u32, RefInfo> = refs
        .into_iter()
        .filter(|(_, info)| info.eliminable)
        .collect();

    if eliminable.is_empty() {
        return false;
    }

    // Pass 2: Replace field accesses and remove dead bindings in a single traversal.
    transform_block(body, &eliminable);
    true
}

/// Eliminate unnecessary reference bindings in all functions.
///
/// Main entry point for reference elimination optimization.
pub fn eliminate_unnecessary_refs(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= eliminate_refs_in_function(&mut func, &type_table);
        }
    }
    changed
}
