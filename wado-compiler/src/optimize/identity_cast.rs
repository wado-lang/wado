//! Identity rewrites. `e as T` is `e` where the two types share one
//! representation head, and `*&e` is `e`.
//! Erasure, monomorphization and inlining leave such wrappers, and each hides
//! the operand's shape from every rule that matches on one.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{ExprId, ExprKind};
use crate::nir_engine::{Engine, Rule};

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

/// `*&e` is `e` itself: a dereference copies nothing, since `plan` already made
/// every copy explicit. The blanket `impl … for &T` forwarding through
/// `(*self)` leaves one per inlined call, and a `&self` method returning
/// `*self` one per inlined value it hands back.
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
        e.redirect_expr(id, inner)
    }
}
