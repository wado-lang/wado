//! TIR rewrite optimizations for Wado
//!
//! This module provides lightweight TIR rewrites that don't warrant their own module:
//!
//! 1. **Labeled Block Simplification**: Eliminates trivial `label: { break label: expr; }`
//!    patterns (common after inlining) by replacing them with just `expr`.
//!
//! 2. **Select Lowering**: Converts simple `if cond { a } else { b }` expressions to
//!    `builtin::select(cond, a, b)` which emits the branchless Wasm `select` instruction.
//!    Both branches must be pure (no side effects, no traps) since `select` evaluates
//!    both operands eagerly.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, MonomorphInfo, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId,
    TypeTable,
};

/// Run all post-optimization TIR rewrites in a single pass over all functions.
///
/// For each function, this performs:
/// - Labeled block simplification (`L: { break L: expr; }` -> `expr`)
pub fn rewrite(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();

            if let Some(ref mut body) = func.body {
                simplify_labeled_blocks_in_block(body);
            }
        }
    }
}

fn simplify_labeled_blocks_in_block(block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= simplify_labeled_blocks_in_stmt(stmt);
    }
    changed
}

fn simplify_labeled_blocks_in_stmt(stmt: &mut TirStmt) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => simplify_labeled_blocks_in_expr(value),
        TirStmtKind::Expr(expr) => simplify_labeled_blocks_in_expr(expr),
        TirStmtKind::Return { value } => {
            value.as_mut().is_some_and(simplify_labeled_blocks_in_expr)
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = simplify_labeled_blocks_in_expr(condition);
            changed |= simplify_labeled_blocks_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
            changed
        }
        TirStmtKind::Loop { body } => simplify_labeled_blocks_in_block(body),
        TirStmtKind::LabeledBlock { block, .. } => simplify_labeled_blocks_in_block(block),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = simplify_labeled_blocks_in_expr(scrutinee);
            changed |= simplify_labeled_blocks_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => {
            value.as_mut().is_some_and(simplify_labeled_blocks_in_expr)
        }
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => simplify_labeled_blocks_in_expr(value),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn simplify_labeled_blocks_in_expr(expr: &mut TirExpr) -> bool {
    let mut changed = false;

    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= simplify_labeled_blocks_in_expr(left);
            changed |= simplify_labeled_blocks_in_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
        }
        TirExprKind::Assign { target, value } => {
            changed |= simplify_labeled_blocks_in_expr(target);
            changed |= simplify_labeled_blocks_in_expr(value);
        }
        TirExprKind::Index { expr: inner, index } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
            changed |= simplify_labeled_blocks_in_expr(index);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= simplify_labeled_blocks_in_expr(receiver);
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            changed |= simplify_labeled_blocks_in_expr(callee);
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= simplify_labeled_blocks_in_expr(functor);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= simplify_labeled_blocks_in_block(block);
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
                // Re-process the new Call expression
                changed |= simplify_labeled_blocks_in_expr(expr);
                return changed;
            }
            changed |= simplify_labeled_blocks_in_expr(condition);
            changed |= simplify_labeled_blocks_in_block(then_branch);
            if let Some(eb) = else_branch {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= simplify_labeled_blocks_in_expr(guard);
                }
                changed |= simplify_labeled_blocks_in_expr(&mut arm.body);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= simplify_labeled_blocks_in_expr(&mut field.value);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                changed |= simplify_labeled_blocks_in_expr(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= simplify_labeled_blocks_in_expr(p);
            }
        }
        TirExprKind::Closure { body, .. } => {
            changed |= simplify_labeled_blocks_in_expr(body);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= simplify_labeled_blocks_in_expr(value);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= simplify_labeled_blocks_in_expr(scrutinee);
            for arm in arms {
                changed |= simplify_labeled_blocks_in_block(arm);
            }
            changed |= simplify_labeled_blocks_in_block(default);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
    }

    // Simplify: `label: { break label: expr; }` → `expr`
    if let TirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Break {
            label: Some(break_label),
            value: Some(_),
        } = &block.stmts[0].kind
        && break_label == label
    {
        let TirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Break {
            value: Some(inner), ..
        } = stmts.remove(0).kind
        else {
            unreachable!();
        };
        *expr = inner;
        changed = true;
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
        }),
        method_info: None,
    };

    Some(TirExpr::new(
        TirExprKind::Call {
            func: func_ref,
            type_args: vec![result_type],
            args: vec![condition.clone(), true_val.clone(), false_val.clone()],
        },
        result_type,
        span,
    ))
}
