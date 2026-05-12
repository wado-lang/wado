//! Rewrite `match` on `i128` / `u128` scrutinees into if-else chains as
//! the TIR → NIR translator walks them.
//!
//! Wasm has no native 128-bit comparison; the rewrite emits explicit
//! `i128::from_i64(...)` literal constructors and `i128^Eq::eq` /
//! `u128^Eq::eq` method calls, which the optimizer / `wir_build`
//! lowers further.

use std::cell::RefCell;
use std::rc::Rc;

use crate::lower::wide_int_literal::{create_i128_literal, create_u128_literal};
use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirLiteralPattern, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

/// True when the scrutinee type is the i128/u128 wrapper struct and at
/// least one arm uses an i128/u128 literal pattern. Wildcard / binding
/// arms alone don't trigger the rewrite — a plain `match x { _ => ... }`
/// on a wide-int value is fine to lower as a normal NIR `Match`.
pub(super) fn should_rewrite(
    scrutinee_type: TypeId,
    arms: &[TirMatchArm],
    type_table: &TypeTable,
) -> bool {
    let is_wide_int = match type_table.get(scrutinee_type) {
        ResolvedType::Struct { name, .. } => name == "i128" || name == "u128",
        _ => false,
    };
    if !is_wide_int {
        return false;
    }
    arms.iter().any(|arm| {
        matches!(
            &arm.pattern,
            TirPattern::Literal(TirLiteralPattern::I128(_) | TirLiteralPattern::U128(_))
        )
    })
}

/// Build the if-else chain as TIR. The caller then runs the result
/// through the translator's normal `convert_expr` so arm bodies and
/// nested wide-int matches get processed uniformly.
pub(super) fn build_if_chain(
    scrutinee: &TirExpr,
    arms: &[TirMatchArm],
    result_type_id: TypeId,
    span: Span,
    type_table: &Rc<RefCell<TypeTable>>,
) -> TirExpr {
    let mut else_expr: Option<TirExpr> = None;
    for arm in arms.iter().rev() {
        match &arm.pattern {
            TirPattern::Literal(TirLiteralPattern::I128(value)) => {
                let literal_expr = create_i128_literal(*value, scrutinee.type_id, span);
                let eq_call =
                    create_i128_eq_call(scrutinee.clone(), literal_expr, type_table, span);
                let condition = with_guard(eq_call, arm.guard.as_ref(), span);
                else_expr = Some(build_if(
                    condition,
                    &arm.body,
                    else_expr,
                    result_type_id,
                    span,
                ));
            }
            TirPattern::Literal(TirLiteralPattern::U128(value)) => {
                let literal_expr = create_u128_literal(*value, scrutinee.type_id, span);
                let eq_call =
                    create_u128_eq_call(scrutinee.clone(), literal_expr, type_table, span);
                let condition = with_guard(eq_call, arm.guard.as_ref(), span);
                else_expr = Some(build_if(
                    condition,
                    &arm.body,
                    else_expr,
                    result_type_id,
                    span,
                ));
            }
            TirPattern::Wildcard | TirPattern::Binding { .. } => {
                // Bindings on wide-int scrutinees act as catch-alls in
                // this rewrite: the binding name (if any) is lowered to
                // a `Local` reference to the scrutinee local via the
                // outer pattern pipeline, so the body already refers to
                // the right value here.
                if let Some(guard) = &arm.guard {
                    else_expr = Some(build_if(
                        guard.clone(),
                        &arm.body,
                        else_expr,
                        result_type_id,
                        span,
                    ));
                } else {
                    else_expr = Some(arm.body.clone());
                }
            }
            _ => {
                // Other patterns (tuple, variant) shouldn't appear for i128/u128
                // scrutinees; keep the body as the else for safety.
                else_expr = Some(arm.body.clone());
            }
        }
    }
    else_expr.expect("wide-int match has at least one arm")
}

fn with_guard(condition: TirExpr, guard: Option<&TirExpr>, span: Span) -> TirExpr {
    match guard {
        None => condition,
        Some(guard) => TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::And,
                left: Box::new(condition),
                right: Box::new(guard.clone()),
            },
            TypeTable::BOOL,
            span,
        ),
    }
}

fn build_if(
    condition: TirExpr,
    body: &TirExpr,
    else_expr: Option<TirExpr>,
    result_type_id: TypeId,
    span: Span,
) -> TirExpr {
    let then_block = expr_to_block(body, span);
    let else_block = else_expr.as_ref().map(|e| expr_to_block(e, span));
    TirExpr::new(
        TirExprKind::If {
            condition: Box::new(condition),
            then_branch: then_block,
            else_branch: else_block,
        },
        result_type_id,
        span,
    )
}

fn expr_to_block(expr: &TirExpr, span: Span) -> TirBlock {
    TirBlock {
        stmts: vec![TirStmt::new(TirStmtKind::Expr(expr.clone()), span)],
        span,
    }
}

fn create_i128_eq_call(
    left: TirExpr,
    right: TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let left_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(left.type_id));
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(left),
        },
        left_ref_type,
        span,
    );
    let right_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(right.type_id));
    let arg_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(right),
        },
        right_ref_type,
        span,
    );
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: ModuleSource::int128(),
                name: "i128^Eq::eq".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "i128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            vec![],
            vec![CallArg::new(arg_ref, false)],
        ),
        TypeTable::BOOL,
        span,
    )
}

fn create_u128_eq_call(
    left: TirExpr,
    right: TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let left_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(left.type_id));
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(left),
        },
        left_ref_type,
        span,
    );
    let right_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(right.type_id));
    let arg_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(right),
        },
        right_ref_type,
        span,
    );
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: ModuleSource::int128(),
                name: "u128^Eq::eq".to_string(),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "u128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            vec![],
            vec![CallArg::new(arg_ref, false)],
        ),
        TypeTable::BOOL,
        span,
    )
}
