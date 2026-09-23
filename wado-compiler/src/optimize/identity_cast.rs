//! Identity rewrites. `e as T` is `e` where the two types share one
//! representation head, and `*&e` is `e` where what it reads is a reference.
//! Erasure, monomorphization and inlining leave such wrappers, and each hides
//! the operand's shape from every rule that matches on one.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{ExprId, ExprKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::ResolvedType;

pub(super) struct IdentityCastRule;

impl Rule for IdentityCastRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        let ExprKind::Cast {
            expr: inner,
            target_type,
        } = &e.body.exprs[id].kind
        else {
            return false;
        };
        let (inner, target_type) = (*inner, *target_type);
        let source_type = e.body.operand_type(inner);
        let Some(types) = e.value_graph_type_table() else {
            return false;
        };
        types.share_common_base(source_type, target_type) && e.redirect_expr(id, inner)
    }
}

/// A dereference yielding a reference copies nothing, so `*&e` is `e` itself.
/// The blanket `impl … for &T` forwarding through `(*self)` leaves one per
/// inlined call.
pub(super) struct IdentityDerefRule;

impl Rule for IdentityDerefRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        let ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: borrowed,
        } = &e.body.exprs[id].kind
        else {
            return false;
        };
        let Some(borrow) = borrowed.as_expr() else {
            return false;
        };
        let ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } = &e.body.exprs[borrow].kind
        else {
            return false;
        };
        let inner = *inner;
        let Some(types) = e.value_graph_type_table() else {
            return false;
        };
        matches!(
            types.get(e.body.exprs[id].type_id),
            ResolvedType::Ref(_) | ResolvedType::MutRef(_)
        ) && e.redirect_expr(id, inner)
    }
}
