//! A variant whose case is a compile-time fact decides its own dispatch: the
//! arm that case takes becomes the payload binding it is, and a `VariantTest` /
//! `VariantTag` over it folds to a constant. A value copy of a variant and an
//! `if let` over a fresh construct are the shapes it takes.

use crate::const_eval::Value;
use crate::nir_arena::{
    ArmData, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::tir::{PrimitiveType, TypeId};

use super::arena_query::{is_pure_nontrapping_operand_typed, single_payload_binding};

pub(super) struct KnownCaseRule;

impl Rule for KnownCaseRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        match &engine.body.exprs[id].kind {
            ExprKind::Match { expr, .. } => {
                let scrutinee = *expr;
                rewrite_match(engine, id, scrutinee)
            }
            ExprKind::VariantTest {
                expr, case_index, ..
            } => {
                let (scrutinee, tested) = (*expr, *case_index);
                let Some(index) = known_index(engine, scrutinee) else {
                    return false;
                };
                droppable(engine, scrutinee)
                    && engine.replace_expr_with_value(id, Value::Bool(index == tested))
            }
            ExprKind::VariantTag { expr } => {
                let scrutinee = *expr;
                let Some(index) = known_index(engine, scrutinee) else {
                    return false;
                };
                let tag = Value::Int {
                    value: u64::from(index),
                    prim: PrimitiveType::I32,
                };
                droppable(engine, scrutinee) && engine.replace_expr_with_value(id, tag)
            }
            _ => false,
        }
    }
}

/// Take the first arm the case can take: the ones before it are refused, and
/// the ones after are unreachable once that arm is certain. Left as a match,
/// lowering would still test the case and trap on the answer it cannot get.
fn rewrite_match(engine: &mut Engine, id: ExprId, scrutinee: Operand) -> bool {
    let Some(construct) = known_construct(engine, scrutinee) else {
        return false;
    };
    let Some((case_index, case_name)) = construct_case(engine.body, construct) else {
        return false;
    };
    let case_name = case_name.to_owned();
    let ExprKind::Match { arms, .. } = &engine.body.exprs[id].kind else {
        return false;
    };
    let Some(head) = arms
        .iter()
        .position(|arm| !refuses(engine.body, arm.pattern, &case_name))
    else {
        return false;
    };
    let certain = certain_arm(engine.body, &arms[head], &case_name).filter(|&(bound, payload)| {
        match bound {
            // A binding declared at another type is one match ergonomics
            // wrapped: the payload read would not fit it.
            Some(local) => engine.locals()[local as usize].type_id == payload,
            // With nothing to bind, the collapse drops the scrutinee.
            None => droppable(engine, scrutinee),
        }
    });
    if let Some((bound, payload_type)) = certain {
        let arm_body = arms[head].body;
        return collapse_to_binding(
            engine,
            id,
            scrutinee,
            case_index,
            payload_type,
            bound,
            arm_body,
        );
    }
    let kept: Vec<ArmData> = arms
        .iter()
        .filter(|arm| !refuses(engine.body, arm.pattern, &case_name))
        .cloned()
        .collect();
    if kept.len() == arms.len() {
        return false;
    }
    engine.replace_expr_kind(
        id,
        ExprKind::Match {
            expr: scrutinee,
            arms: kept,
        },
    );
    true
}

/// The payload binding an arm makes (`None` for one that binds nothing) and the
/// payload's type, for an arm the case takes whatever the payload holds. A
/// guard or a pattern that reads into the payload leaves the arm uncertain.
fn certain_arm(body: &Body, arm: &ArmData, case: &str) -> Option<(Option<u32>, TypeId)> {
    if arm.guard.is_some() {
        return None;
    }
    let PatKind::Variant {
        variant_name,
        bindings,
        payload_type,
        ..
    } = &body.pats[arm.pattern].kind
    else {
        return None;
    };
    if variant_name != case {
        return None;
    }
    Some((single_payload_binding(body, bindings)?, *payload_type))
}

/// Rewrite the match as a block that binds the payload and runs the body.
fn collapse_to_binding(
    engine: &mut Engine,
    id: ExprId,
    scrutinee: Operand,
    case_index: u32,
    payload_type: TypeId,
    bound: Option<u32>,
    arm_body: Operand,
) -> bool {
    let span = engine.body.exprs[id].span;
    let result_type = engine.body.exprs[id].type_id;
    let mut stmts = Vec::with_capacity(2);
    if let Some(local_index) = bound {
        let payload = engine.alloc_expr(
            ExprKind::VariantPayload {
                expr: scrutinee,
                case_index,
                payload_type,
            },
            payload_type,
            span,
        );
        let name = engine.locals()[local_index as usize].name.clone();
        stmts.push(engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id: payload_type,
                value: Operand::Expr(payload),
                // The payload is read out of the scrutinee, as the pattern
                // binding this replaces read it.
                skip_value_copy: true,
            },
            span,
        ));
    }
    stmts.push(engine.alloc_stmt(StmtKind::Expr(arm_body), span));
    let block = engine.alloc_block(stmts, span);
    engine.replace_expr_kind(id, ExprKind::plain_block(block, result_type, "known_case"));
    true
}

/// The case index a value is known to hold.
fn known_index(engine: &mut Engine, operand: Operand) -> Option<u32> {
    let construct = known_construct(engine, operand)?;
    Some(construct_case(engine.body, construct)?.0)
}

/// The construction `operand` is known to read back: itself, or the one a
/// single-assignment local was bound to. A local written again, or written
/// through a reference, is not one — `local_has_one_version` is that test.
fn known_construct(engine: &mut Engine, operand: Operand) -> Option<ExprId> {
    let expr = operand.as_expr()?;
    if construct_case(engine.body, expr).is_some() {
        return Some(expr);
    }
    let ExprKind::Local { index, .. } = engine.body.exprs[expr].kind else {
        return None;
    };
    if !engine.local_has_one_version(index) {
        return None;
    }
    let def = engine.local_def(index)?;
    let StmtKind::Let { value, .. } = engine.body.stmts[def].kind else {
        return None;
    };
    let value = value.as_expr()?;
    construct_case(engine.body, value)
        .is_some()
        .then_some(value)
}

fn construct_case(body: &Body, expr: ExprId) -> Option<(u32, &str)> {
    let ExprKind::VariantConstruct {
        case_index,
        case_name,
        ..
    } = &body.exprs[expr].kind
    else {
        return None;
    };
    Some((*case_index, case_name))
}

/// A fold reads the case out of the scrutinee and drops it, so nothing may
/// observe it running.
fn droppable(engine: &Engine, scrutinee: Operand) -> bool {
    is_pure_nontrapping_operand_typed(engine.body, scrutinee, engine.value_graph_type_table())
}

/// Whether `pattern` refuses every value of case `case`. Only a case name
/// decides that; a binding, a wildcard, or a nested refutation may still match,
/// so anything else is kept.
fn refuses(body: &Body, pattern: PatId, case: &str) -> bool {
    match &body.pats[pattern].kind {
        PatKind::Variant { variant_name, .. } => variant_name != case,
        PatKind::Or(_) => {
            let mut all = true;
            body.for_each_child(NodeRef::Pat(pattern), |child| {
                if let NodeRef::Pat(alternative) = child {
                    all &= refuses(body, alternative, case);
                }
            });
            all
        }
        PatKind::Wildcard
        | PatKind::Binding { .. }
        | PatKind::Literal(_)
        | PatKind::Tuple(..)
        | PatKind::Enum { .. }
        | PatKind::Struct { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => false,
    }
}
