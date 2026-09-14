//! Identity cast: `e as T` is `e` where the two types share one representation
//! head. Erasure and monomorphization leave such casts, and the wrapper hides
//! the operand's shape from every rule that matches on one.

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
