//! TIR rewrite optimizations for Wado
//!
//! This module provides lightweight TIR rewrites that run after optimization:
//!
//! - **Select Lowering**: Converts simple `if cond { a } else { b }` expressions to
//!   `builtin::select(cond, a, b)` which emits the branchless Wasm `select` instruction.
//!   Both branches must be pure (no side effects, no traps) since `select` evaluates
//!   both operands eagerly.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, MonomorphInfo, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId,
    TypeTable,
};

/// Run all post-optimization TIR rewrites in a single pass over all functions.
pub fn rewrite(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();

            if let Some(ref mut body) = func.body {
                rewrite_block(body);
            }
        }
    }
}

fn rewrite_block(block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= rewrite_stmt(stmt);
    }
    changed
}

fn rewrite_stmt(stmt: &mut TirStmt) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetPattern { value, .. } => {
            rewrite_expr(value)
        }
        TirStmtKind::Expr(expr) => rewrite_expr(expr),
        TirStmtKind::Return { value } => value.as_mut().is_some_and(rewrite_expr),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = rewrite_expr(condition);
            changed |= rewrite_block(then_block);
            if let Some(eb) = else_block {
                changed |= rewrite_block(eb);
            }
            changed
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_block(body)
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = rewrite_expr(scrutinee);
            changed |= rewrite_block(then_block);
            if let Some(eb) = else_block {
                changed |= rewrite_block(eb);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => value.as_mut().is_some_and(rewrite_expr),
        TirStmtKind::Continue => false,
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn rewrite_expr(expr: &mut TirExpr) -> bool {
    let mut changed = false;

    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= rewrite_expr(left);
            changed |= rewrite_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= rewrite_expr(inner);
        }
        TirExprKind::Assign { target, value } => {
            changed |= rewrite_expr(target);
            changed |= rewrite_expr(value);
        }
        TirExprKind::Index { expr: inner, index } => {
            changed |= rewrite_expr(inner);
            changed |= rewrite_expr(index);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= rewrite_expr(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= rewrite_expr(receiver);
            for arg in args {
                changed |= rewrite_expr(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            changed |= rewrite_expr(callee);
            for arg in args {
                changed |= rewrite_expr(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= rewrite_expr(functor);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= rewrite_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            // Try select lowering before recursing into branches
            if let Some(select_call) =
                try_lower_to_select(condition, then_branch, else_branch, expr.type_id, expr.span)
            {
                *expr = select_call;
                changed |= rewrite_expr(expr);
                return changed;
            }
            changed |= rewrite_expr(condition);
            changed |= rewrite_block(then_branch);
            if let Some(eb) = else_branch {
                changed |= rewrite_block(eb);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= rewrite_expr(inner);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= rewrite_expr(guard);
                }
                changed |= rewrite_expr(&mut arm.body);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= rewrite_expr(&mut field.value);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                changed |= rewrite_expr(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= rewrite_expr(p);
            }
        }
        TirExprKind::Closure { body, .. } => {
            changed |= rewrite_expr(body);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= rewrite_expr(value);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= rewrite_expr(scrutinee);
            for arm in arms {
                changed |= rewrite_block(arm);
            }
            changed |= rewrite_block(default);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }

    changed
}

/// Check if a TIR expression is side-effect-free and suitable for `select` operands.
///
/// The Wasm `select` instruction evaluates both operands eagerly, so both must be
/// pure (no side effects, no traps). We conservatively accept only:
/// - Local variable reads
/// - Literals (int, float, bool, char)
fn is_select_eligible_expr(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Local { .. }
            | TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
    )
}

/// Try to extract a single pure expression from a block for select optimization.
///
/// Returns `Some(expr)` if the block contains exactly one `Expr` statement
/// whose expression is side-effect-free.
fn try_select_value(block: &TirBlock) -> Option<&TirExpr> {
    if block.stmts.len() != 1 {
        return None;
    }
    if let TirStmtKind::Expr(expr) = &block.stmts[0].kind
        && is_select_eligible_expr(expr)
    {
        return Some(expr);
    }
    None
}

/// Try to transform an `If` expression into a `builtin::select` call.
///
/// Returns `Some(call_expr)` if the if-expression is eligible:
/// - Has both then and else branches
/// - Both branches are single pure expressions
/// - Result type is not unit
fn try_lower_to_select(
    condition: &TirExpr,
    then_branch: &TirBlock,
    else_branch: &Option<TirBlock>,
    result_type: TypeId,
    span: crate::token::Span,
) -> Option<TirExpr> {
    let else_block = else_branch.as_ref()?;

    // Unit-typed if-expressions are statements, not value-producing selects
    if result_type == TypeTable::UNIT {
        return None;
    }

    let true_val = try_select_value(then_branch)?;
    let false_val = try_select_value(else_block)?;

    // Construct: builtin::select(condition, true_val, false_val)
    let func_ref = FunctionRef::External {
        module_source: ModuleSource::builtin(),
        name: "select".to_string(),
        monomorph_info: Some(MonomorphInfo {
            generic_name: "select".to_string(),
            type_args: vec![result_type],
            is_blanket: false,
        }),
        method_info: None,
    };

    Some(TirExpr::new(
        TirExprKind::Call {
            func: func_ref,
            type_args: vec![result_type],
            args: vec![condition.clone(), true_val.clone(), false_val.clone()],
            param_is_mut: vec![false, false, false],
        },
        result_type,
        span,
    ))
}
