//! Rewrite `match` on an `i128` / `u128` scrutinee into an if-else chain as the
//! TIR → NIR translator walks it. Wasm has no 128-bit comparison, so each arm
//! becomes a comparison `convert_expr` turns into a prelude `Eq` / `Ord` call.

use std::cell::RefCell;
use std::rc::Rc;

use crate::lower::wide_int_literal::create_literal;
use crate::tir::{
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirLiteralPattern, TirMatchArm, TirPattern,
    TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::token::Span;

/// True for a wide-int scrutinee with at least one arm that tests its value. A
/// `match x { _ => … }` lowers fine as a normal NIR `Match`.
pub(super) fn should_rewrite(
    scrutinee_type: TypeId,
    arms: &[TirMatchArm],
    type_table: &TypeTable,
) -> bool {
    if type_table.wide_int_item(scrutinee_type).is_none() {
        return false;
    }
    arms.iter().any(|arm| {
        matches!(
            &arm.pattern,
            TirPattern::Literal(TirLiteralPattern::I128(_) | TirLiteralPattern::U128(_))
                | TirPattern::Range { .. }
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
        // The refutable shapes differ only in the condition they test.
        if let Some(condition) = arm_condition(&arm.pattern, scrutinee, span, type_table) {
            let condition = with_guard(condition, arm.guard.as_ref(), span);
            else_expr = Some(build_if(
                condition,
                &arm.body,
                else_expr,
                result_type_id,
                span,
            ));
            continue;
        }
        match &arm.pattern {
            TirPattern::Wildcard => {
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
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // The bound local must hold the scrutinee value before
                // the guard / body run. Emit `{ let <name> = <scrut>;
                // <guarded_or_body> }` as a Block expression — same
                // shape `lower::translate::pattern` synthesizes for normal
                // `Binding` lowering.
                let payload = if let Some(guard) = &arm.guard {
                    build_if(guard.clone(), &arm.body, else_expr, result_type_id, span)
                } else {
                    arm.body.clone()
                };
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: *type_id,
                        value: scrutinee.clone(),
                        skip_value_copy: true,
                    },
                    span,
                );
                let payload_stmt = TirStmt::new(TirStmtKind::Expr(payload), span);
                let block = TirBlock {
                    stmts: vec![let_stmt, payload_stmt],
                    span,
                };
                else_expr = Some(TirExpr::new(
                    TirExprKind::Block(block),
                    result_type_id,
                    span,
                ));
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

/// The condition an arm's pattern tests, or `None` for one that always matches
/// and so needs no test.
fn arm_condition(
    pattern: &TirPattern,
    scrutinee: &TirExpr,
    span: Span,
    type_table: &Rc<RefCell<TypeTable>>,
) -> Option<TirExpr> {
    match pattern {
        TirPattern::Literal(TirLiteralPattern::I128(value)) => Some(compare(
            scrutinee,
            TirBinaryOp::Eq,
            *value,
            span,
            type_table,
        )),
        TirPattern::Literal(TirLiteralPattern::U128(value)) => Some(compare(
            scrutinee,
            TirBinaryOp::Eq,
            value.cast_signed(),
            span,
            type_table,
        )),
        TirPattern::Range {
            start,
            end,
            inclusive,
            ..
        } => {
            let upper_op = if *inclusive {
                TirBinaryOp::LtEq
            } else {
                TirBinaryOp::Lt
            };
            Some(and(
                compare(scrutinee, TirBinaryOp::GtEq, *start, span, type_table),
                compare(scrutinee, upper_op, *end, span, type_table),
                span,
            ))
        }
        _ => None,
    }
}

/// `scrutinee <op> <bits>` as a plain `Binary`. `convert_expr`'s wide-int arm
/// turns it — and the literal operand — into the calls the prelude provides.
fn compare(
    scrutinee: &TirExpr,
    op: TirBinaryOp,
    bits: i128,
    span: Span,
    type_table: &Rc<RefCell<TypeTable>>,
) -> TirExpr {
    let item = type_table
        .borrow()
        .wide_int_item(scrutinee.type_id)
        .expect("the caller checked the scrutinee is a wide integer");
    let literal = create_literal(item, bits, scrutinee.type_id, &type_table.borrow(), span);
    TirExpr::new(
        TirExprKind::Binary {
            op,
            left: Box::new(scrutinee.clone()),
            right: Box::new(literal),
        },
        TypeTable::BOOL,
        span,
    )
}

fn and(left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        },
        TypeTable::BOOL,
        span,
    )
}

fn with_guard(condition: TirExpr, guard: Option<&TirExpr>, span: Span) -> TirExpr {
    match guard {
        None => condition,
        Some(guard) => and(condition, guard.clone(), span),
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
