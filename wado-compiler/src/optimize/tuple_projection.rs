//! Tuple projection: `[a, b, c].1` → `b`.
//!
//! A tuple literal has no identity — it is a value aggregate with no address
//! and no mutation path — so a field read of one built in place is that
//! element. The unselected elements are dropped, which requires them pure and
//! non-trapping.
//!
//! Synthesis emits this shape wherever a reflected method returns a field pack
//! and one field is wanted (`ReflectStruct::defaults()`, inlined at its call
//! site), and inlining plants it wherever a multi-value return meets its
//! destructuring.

use crate::nir_arena::{ExprId, ExprKind};
use crate::nir_engine::{Engine, Rule};

use super::arena_query::is_pure_nontrapping_operand_typed;

pub(super) struct TupleProjectionRule;

impl Rule for TupleProjectionRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        // An assign target is a place: `t.1 = x` names the tuple, not a value.
        if e.is_assign_target(id) {
            return false;
        }
        let ExprKind::FieldAccess {
            expr, field_index, ..
        } = &e.body.exprs[id].kind
        else {
            return false;
        };
        let (Some(agg), index) = (expr.as_expr(), *field_index as usize) else {
            return false;
        };
        let ExprKind::TupleLiteral { elements } = &e.body.exprs[agg].kind else {
            return false;
        };
        let Some(&selected) = elements.get(index) else {
            return false;
        };
        let elements = elements.clone();
        let types = e.value_graph_type_table();
        let droppable = elements
            .iter()
            .enumerate()
            .all(|(i, &op)| i == index || is_pure_nontrapping_operand_typed(e.body, op, types));
        droppable && e.redirect_expr(id, selected)
    }
}
