//! Select lowering optimization for Wado TIR
//!
//! Post-optimization rewrite that converts simple `if cond { a } else { b }` expressions
//! to `builtin::select(cond, a, b)`, which emits the branchless Wasm `select` instruction.
//! Both branches must be pure (no side effects, no traps) since `select` evaluates both
//! operands eagerly.

use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, TirBlock, TirExpr, TirExprKind, TirStmtKind, TypeId,
    TypeTable,
};

use crate::tir_visitor::{TirOptVisitor, opt_walk_expr};

/// Run select lowering on all functions.
pub fn select_lowering(project: &mut FlatPackage) {
    let mut visitor = SelectLoweringVisitor;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            visitor.visit_block(body);
        }
    }
}

struct SelectLoweringVisitor;

impl TirOptVisitor for SelectLoweringVisitor {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        let mut changed = false;
        if let TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &mut expr.kind
            && let Some(select_call) =
                try_lower_to_select(condition, then_branch, else_branch, expr.type_id, expr.span)
        {
            *expr = select_call;
            changed = true;
            changed |= self.visit_expr(expr);
            return changed;
        }
        changed |= opt_walk_expr(self, expr);
        changed
    }
}

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

fn try_lower_to_select(
    condition: &TirExpr,
    then_branch: &TirBlock,
    else_branch: &Option<TirBlock>,
    result_type: TypeId,
    span: crate::token::Span,
) -> Option<TirExpr> {
    let else_block = else_branch.as_ref()?;

    if result_type == TypeTable::UNIT {
        return None;
    }

    let true_val = try_select_value(then_branch)?;
    let false_val = try_select_value(else_block)?;

    let func_ref = FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "select".to_string(),
        monomorph_info: Some(MonomorphInfo {
            generic_name: "select".to_string(),
            impl_type_args: vec![result_type],
            method_type_args: vec![],
            is_blanket: false,
        }),
        method_info: None,
    };

    Some(TirExpr::new(
        TirExprKind::Call {
            func: func_ref,
            type_args: vec![result_type],
            args: vec![
                CallArg::new(condition.clone(), false),
                CallArg::new(true_val.clone(), false),
                CallArg::new(false_val.clone(), false),
            ],
        },
        result_type,
        span,
    ))
}
