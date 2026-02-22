//! Reference elimination optimization for Wado TIR
//!
//! This module eliminates unnecessary reference bindings that are introduced
//! during function inlining. After inlining, we often have patterns like:
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
//! The optimization works by:
//! 1. Finding all `let ref_var: &T = &local_var` bindings
//! 2. Checking if all uses of `ref_var` are field accesses
//! 3. Replacing those field accesses with field accesses on the original variable
//! 4. Removing the now-dead reference bindings

use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};
use indexmap::IndexSet;

/// Information about a reference binding that may be eliminable.
/// Pattern: `let ref_var: &T = &local_var` or `let ref_var: &mut T = &mut local_var`
#[derive(Debug)]
struct RefBinding {
    /// Local index of the reference variable (`ref_var`)
    ref_local: u32,
    /// Local index of the original variable (`local_var`)
    target_local: u32,
    /// Name of the original variable (for reconstruction)
    target_name: String,
    /// Whether this is a mutable reference
    #[allow(dead_code)]
    is_mut: bool,
}

/// Analyze a Let statement to see if it binds a reference to a local variable.
fn analyze_ref_binding(stmt: &TirStmt) -> Option<RefBinding> {
    let TirStmtKind::Let {
        local_index, value, ..
    } = &stmt.kind
    else {
        return None;
    };

    // Check if value is &local or &mut local
    let TirExprKind::Unary { op, expr } = &value.kind else {
        return None;
    };

    let is_mut = match op {
        TirUnaryOp::Ref => false,
        TirUnaryOp::MutRef => true,
        _ => return None,
    };

    // The inner expression must be a local variable
    let TirExprKind::Local { index, name } = &expr.kind else {
        return None;
    };

    Some(RefBinding {
        ref_local: *local_index,
        target_local: *index,
        target_name: name.clone(),
        is_mut,
    })
}

/// Check if an expression is a use of the given local variable.
fn is_local_use(expr: &TirExpr, local_index: u32) -> bool {
    matches!(&expr.kind, TirExprKind::Local { index, .. } if *index == local_index)
}

/// Track all uses of a local variable in an expression.
/// Returns (`is_only_field_access`, `uses_count`)
/// If `is_only_field_access` is true, all uses are field accesses.
fn track_local_uses_in_expr(expr: &TirExpr, local_index: u32) -> (bool, u32) {
    match &expr.kind {
        TirExprKind::Local { index, .. } if *index == local_index => {
            // Direct use of the local (not through field access) - not eliminable
            (false, 1)
        }
        TirExprKind::FieldAccess {
            expr: inner,
            ..
        } => {
            if is_local_use(inner, local_index) {
                // Field access on the local - this is what we want to optimize
                (true, 1)
            } else {
                // Field access on something else, recurse
                track_local_uses_in_expr(inner, local_index)
            }
        }
        // Recursively check nested expressions
        TirExprKind::Binary { left, right, .. } => {
            let (l_ok, l_count) = track_local_uses_in_expr(left, local_index);
            let (r_ok, r_count) = track_local_uses_in_expr(right, local_index);
            (l_ok && r_ok, l_count + r_count)
        }
        TirExprKind::Unary { expr: inner, .. } => track_local_uses_in_expr(inner, local_index),
        TirExprKind::Cast { expr: inner, .. } => track_local_uses_in_expr(inner, local_index),
        TirExprKind::Assign { target, value } => {
            let (t_ok, t_count) = track_local_uses_in_expr(target, local_index);
            let (v_ok, v_count) = track_local_uses_in_expr(value, local_index);
            (t_ok && v_ok, t_count + v_count)
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }

        | TirExprKind::CmRawCall { args, .. } => {
            let mut total = 0;
            let mut all_ok = true;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let (r_ok, r_count) = track_local_uses_in_expr(receiver, local_index);
            let mut total = r_count;
            let mut all_ok = r_ok;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::IndirectCall { callee, args } => {
            let (c_ok, c_count) = track_local_uses_in_expr(callee, local_index);
            let mut total = c_count;
            let mut all_ok = c_ok;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            track_local_uses_in_expr(functor, local_index)
        }
        TirExprKind::Index { expr: inner, index } => {
            let (i_ok, i_count) = track_local_uses_in_expr(inner, local_index);
            let (x_ok, x_count) = track_local_uses_in_expr(index, local_index);
            (i_ok && x_ok, i_count + x_count)
        }
        TirExprKind::Block(block) => track_local_uses_in_block(block, local_index),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (c_ok, c_count) = track_local_uses_in_expr(condition, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_branch, local_index);
            let (e_ok, e_count) = else_branch
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (c_ok && t_ok && e_ok, c_count + t_count + e_count)
        }
        TirExprKind::StructLiteral { fields, .. } => {
            let mut total = 0;
            let mut all_ok = true;
            for field in fields {
                let (ok, count) = track_local_uses_in_expr(&field.value, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            let mut total = 0;
            let mut all_ok = true;
            for elem in elements {
                let (ok, count) = track_local_uses_in_expr(elem, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::OptionSome { value } => track_local_uses_in_expr(value, local_index),
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                track_local_uses_in_expr(payload_expr, local_index)
            } else {
                (true, 0)
            }
        }
        TirExprKind::Move { expr } => track_local_uses_in_expr(expr, local_index),
        TirExprKind::LabeledBlock { block, .. } => track_local_uses_in_block(block, local_index),
        TirExprKind::Closure { body, .. } => track_local_uses_in_expr(body, local_index),
        TirExprKind::Match { expr: inner, arms } => {
            let (i_ok, i_count) = track_local_uses_in_expr(inner, local_index);
            let mut total = i_count;
            let mut all_ok = i_ok;
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    let (ok, count) = track_local_uses_in_expr(guard, local_index);
                    all_ok = all_ok && ok;
                    total += count;
                }
                let (ok, count) = track_local_uses_in_expr(&arm.body, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::GlobalVarSet { value, .. } => track_local_uses_in_expr(value, local_index),
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => track_local_uses_in_expr(expr, local_index),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let (s_ok, s_count) = track_local_uses_in_expr(scrutinee, local_index);
            let mut total = s_count;
            let mut all_ok = s_ok;
            for arm in arms {
                let (ok, count) = track_local_uses_in_block(arm, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            let (d_ok, d_count) = track_local_uses_in_block(default, local_index);
            (all_ok && d_ok, total + d_count)
        }
        // Leaf nodes - no uses
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. } // Different local
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => (true, 0),
    }
}

/// Track all uses of a local variable in a block.
fn track_local_uses_in_block(block: &TirBlock, local_index: u32) -> (bool, u32) {
    let mut total = 0;
    let mut all_ok = true;
    for stmt in &block.stmts {
        let (ok, count) = track_local_uses_in_stmt(stmt, local_index);
        all_ok = all_ok && ok;
        total += count;
    }
    (all_ok, total)
}

/// Track all uses of a local variable in a statement.
fn track_local_uses_in_stmt(stmt: &TirStmt, local_index: u32) -> (bool, u32) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => track_local_uses_in_expr(value, local_index),
        TirStmtKind::Expr(expr) => track_local_uses_in_expr(expr, local_index),
        TirStmtKind::Return { value } => value
            .as_ref()
            .map_or((true, 0), |v| track_local_uses_in_expr(v, local_index)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (c_ok, c_count) = track_local_uses_in_expr(condition, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_block, local_index);
            let (e_ok, e_count) = else_block
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (c_ok && t_ok && e_ok, c_count + t_count + e_count)
        }
        TirStmtKind::Loop { body } => track_local_uses_in_block(body, local_index),
        TirStmtKind::LabeledBlock { block, .. } => track_local_uses_in_block(block, local_index),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let (s_ok, s_count) = track_local_uses_in_expr(scrutinee, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_block, local_index);
            let (e_ok, e_count) = else_block
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (s_ok && t_ok && e_ok, s_count + t_count + e_count)
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .map_or((true, 0), |v| track_local_uses_in_expr(v, local_index)),
        TirStmtKind::Continue => (true, 0),
        TirStmtKind::LetPattern { value, .. } => track_local_uses_in_expr(value, local_index),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Replace field accesses on `ref_local` with field accesses on `target_local`.
fn replace_ref_field_access_in_expr(
    expr: &mut TirExpr,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if is_local_use(inner, ref_local) {
                // Replace the inner local with the target local
                **inner = TirExpr::new(
                    TirExprKind::Local {
                        index: target_local,
                        name: target_name.to_string(),
                    },
                    inner.type_id, // Keep the type - codegen handles ref vs value
                    inner.span,
                );
            } else {
                replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_ref_field_access_in_expr(left, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(right, ref_local, target_local, target_name);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
        }
        TirExprKind::Assign { target, value } => {
            replace_ref_field_access_in_expr(target, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_ref_field_access_in_expr(receiver, ref_local, target_local, target_name);
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            replace_ref_field_access_in_expr(callee, ref_local, target_local, target_name);
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            replace_ref_field_access_in_expr(functor, ref_local, target_local, target_name);
        }
        TirExprKind::Index { expr: inner, index } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(index, ref_local, target_local, target_name);
        }
        TirExprKind::Block(block) => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_ref_field_access_in_expr(condition, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_branch, ref_local, target_local, target_name);
            if let Some(eb) = else_branch {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_ref_field_access_in_expr(
                    &mut field.value,
                    ref_local,
                    target_local,
                    target_name,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_ref_field_access_in_expr(elem, ref_local, target_local, target_name);
            }
        }
        TirExprKind::OptionSome { value } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                replace_ref_field_access_in_expr(
                    payload_expr,
                    ref_local,
                    target_local,
                    target_name,
                );
            }
        }
        TirExprKind::Move { expr } => {
            replace_ref_field_access_in_expr(expr, ref_local, target_local, target_name);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirExprKind::Closure { body, .. } => {
            replace_ref_field_access_in_expr(body, ref_local, target_local, target_name);
        }
        TirExprKind::Match { expr: inner, arms } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    replace_ref_field_access_in_expr(guard, ref_local, target_local, target_name);
                }
                replace_ref_field_access_in_expr(
                    &mut arm.body,
                    ref_local,
                    target_local,
                    target_name,
                );
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            replace_ref_field_access_in_expr(expr, ref_local, target_local, target_name);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            replace_ref_field_access_in_expr(scrutinee, ref_local, target_local, target_name);
            for arm in arms {
                replace_ref_field_access_in_block(arm, ref_local, target_local, target_name);
            }
            replace_ref_field_access_in_block(default, ref_local, target_local, target_name);
        }
        // Leaf nodes - nothing to replace
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Replace field accesses in a block.
fn replace_ref_field_access_in_block(
    block: &mut TirBlock,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    for stmt in &mut block.stmts {
        replace_ref_field_access_in_stmt(stmt, ref_local, target_local, target_name);
    }
}

/// Replace field accesses in a statement.
fn replace_ref_field_access_in_stmt(
    stmt: &mut TirStmt,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirStmtKind::Expr(expr) => {
            replace_ref_field_access_in_expr(expr, ref_local, target_local, target_name);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_ref_field_access_in_expr(v, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_ref_field_access_in_expr(condition, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_block, ref_local, target_local, target_name);
            if let Some(eb) = else_block {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_ref_field_access_in_block(body, ref_local, target_local, target_name);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_ref_field_access_in_expr(scrutinee, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_block, ref_local, target_local, target_name);
            if let Some(eb) = else_block {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_ref_field_access_in_expr(v, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Eliminate unnecessary reference bindings in a function.
/// After inlining, we often have patterns like:
///   let self: &Array<T> = &arr;
///   ... self.repr ...
/// This can be optimized to:
///   ... arr.repr ...
fn eliminate_refs_in_function(func: &mut TirFunction, _type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // First pass: find all ref bindings
    let mut ref_bindings: Vec<RefBinding> = Vec::new();
    collect_ref_bindings(&body.stmts, &mut ref_bindings);

    if ref_bindings.is_empty() {
        return false;
    }

    // Second pass: for each ref binding, check if all uses are field accesses
    let mut eliminable_bindings: Vec<RefBinding> = Vec::new();
    for binding in ref_bindings {
        let (all_field_access, _count) = track_local_uses_in_block(body, binding.ref_local);
        if all_field_access {
            eliminable_bindings.push(binding);
        }
    }

    if eliminable_bindings.is_empty() {
        return false;
    }

    // Third pass: replace field accesses and remove dead bindings
    for binding in &eliminable_bindings {
        replace_ref_field_access_in_block(
            body,
            binding.ref_local,
            binding.target_local,
            &binding.target_name,
        );
    }

    // Fourth pass: remove the now-dead Let statements
    // We need to handle nested blocks, so we do this recursively
    let dead_locals: IndexSet<u32> = eliminable_bindings.iter().map(|b| b.ref_local).collect();
    remove_dead_ref_bindings(&mut body.stmts, &dead_locals);
    true
}

/// Collect ref bindings from statements (only at the top level of each block).
fn collect_ref_bindings(stmts: &[TirStmt], bindings: &mut Vec<RefBinding>) {
    for stmt in stmts {
        if let Some(binding) = analyze_ref_binding(stmt) {
            bindings.push(binding);
        }
        // Also check nested blocks
        collect_ref_bindings_in_stmt(stmt, bindings);
    }
}

/// Collect ref bindings from nested blocks within a statement.
fn collect_ref_bindings_in_stmt(stmt: &TirStmt, bindings: &mut Vec<RefBinding>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirStmtKind::Expr(expr) => {
            collect_ref_bindings_in_expr(expr, bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_ref_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_ref_bindings_in_expr(condition, bindings);
            collect_ref_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_ref_bindings(&body.stmts, bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_ref_bindings_in_expr(scrutinee, bindings);
            collect_ref_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_ref_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Collect ref bindings from nested blocks within an expression.
fn collect_ref_bindings_in_expr(expr: &TirExpr, bindings: &mut Vec<RefBinding>) {
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_ref_bindings_in_expr(condition, bindings);
            collect_ref_bindings(&then_branch.stmts, bindings);
            if let Some(eb) = else_branch {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_ref_bindings_in_expr(receiver, bindings);
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_ref_bindings_in_expr(callee, bindings);
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_ref_bindings_in_expr(functor, bindings);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_ref_bindings_in_expr(left, bindings);
            collect_ref_bindings_in_expr(right, bindings);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Assign { target, value } => {
            collect_ref_bindings_in_expr(target, bindings);
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Index { expr: inner, index } => {
            collect_ref_bindings_in_expr(inner, bindings);
            collect_ref_bindings_in_expr(index, bindings);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_ref_bindings_in_expr(&field.value, bindings);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_ref_bindings_in_expr(elem, bindings);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_ref_bindings_in_expr(payload_expr, bindings);
            }
        }
        TirExprKind::Move { expr } => {
            collect_ref_bindings_in_expr(expr, bindings);
        }
        TirExprKind::Closure { body, .. } => {
            collect_ref_bindings_in_expr(body, bindings);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_ref_bindings_in_expr(inner, bindings);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_ref_bindings_in_expr(guard, bindings);
                }
                collect_ref_bindings_in_expr(&arm.body, bindings);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            collect_ref_bindings_in_expr(expr, bindings);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_ref_bindings_in_expr(scrutinee, bindings);
            for arm in arms {
                collect_ref_bindings(&arm.stmts, bindings);
            }
            collect_ref_bindings(&default.stmts, bindings);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Remove Let statements for dead reference locals.
fn remove_dead_ref_bindings(stmts: &mut Vec<TirStmt>, dead_locals: &IndexSet<u32>) {
    stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !dead_locals.contains(local_index)
        } else {
            true
        }
    });

    // Recursively process nested blocks
    for stmt in stmts {
        remove_dead_ref_bindings_in_stmt(stmt, dead_locals);
    }
}

/// Remove dead ref bindings from nested blocks in a statement.
fn remove_dead_ref_bindings_in_stmt(stmt: &mut TirStmt, dead_locals: &IndexSet<u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::Expr(expr) => {
            remove_dead_ref_bindings_in_expr(expr, dead_locals);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                remove_dead_ref_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remove_dead_ref_bindings_in_expr(condition, dead_locals);
            remove_dead_ref_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Loop { body } => {
            remove_dead_ref_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            remove_dead_ref_bindings_in_expr(scrutinee, dead_locals);
            remove_dead_ref_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_dead_ref_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Remove dead ref bindings from nested blocks in an expression.
fn remove_dead_ref_bindings_in_expr(expr: &mut TirExpr, dead_locals: &IndexSet<u32>) {
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_dead_ref_bindings_in_expr(condition, dead_locals);
            remove_dead_ref_bindings(&mut then_branch.stmts, dead_locals);
            if let Some(eb) = else_branch {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            remove_dead_ref_bindings_in_expr(receiver, dead_locals);
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            remove_dead_ref_bindings_in_expr(callee, dead_locals);
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            remove_dead_ref_bindings_in_expr(functor, dead_locals);
        }
        TirExprKind::Binary { left, right, .. } => {
            remove_dead_ref_bindings_in_expr(left, dead_locals);
            remove_dead_ref_bindings_in_expr(right, dead_locals);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Assign { target, value } => {
            remove_dead_ref_bindings_in_expr(target, dead_locals);
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Index { expr: inner, index } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
            remove_dead_ref_bindings_in_expr(index, dead_locals);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remove_dead_ref_bindings_in_expr(&mut field.value, dead_locals);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                remove_dead_ref_bindings_in_expr(elem, dead_locals);
            }
        }
        TirExprKind::OptionSome { value } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                remove_dead_ref_bindings_in_expr(payload_expr, dead_locals);
            }
        }
        TirExprKind::Move { expr } => {
            remove_dead_ref_bindings_in_expr(expr, dead_locals);
        }
        TirExprKind::Closure { body, .. } => {
            remove_dead_ref_bindings_in_expr(body, dead_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    remove_dead_ref_bindings_in_expr(guard, dead_locals);
                }
                remove_dead_ref_bindings_in_expr(&mut arm.body, dead_locals);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            remove_dead_ref_bindings_in_expr(expr, dead_locals);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            remove_dead_ref_bindings_in_expr(scrutinee, dead_locals);
            for arm in arms {
                remove_dead_ref_bindings(&mut arm.stmts, dead_locals);
            }
            remove_dead_ref_bindings(&mut default.stmts, dead_locals);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Eliminate unnecessary reference bindings in all functions.
///
/// This is the main entry point for reference elimination optimization.
/// It iterates through all modules and functions, eliminating reference
/// bindings that are only used for field access.
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
