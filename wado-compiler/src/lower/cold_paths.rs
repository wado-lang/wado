//! Drop every cold marker under `-f no-cold-paths`, the author's and the
//! compiler's alike, before any pass reads one, so the build shows what the
//! optimizer does with no marker to go on. `match_to_switch` places none
//! either.

use crate::flat_package::FlatPackage;
use crate::synthesis::common::{is_cold_marker_expr, is_cold_marker_stmt};
use crate::tir::{TirBlock, TirExpr, TirExprKind};
use crate::tir_visitor::TirMutVisitor;

pub fn drop_markers(flat: &FlatPackage) {
    flat.visit_bodies_mut(&mut MarkerDrop);
}

struct MarkerDrop;

impl TirMutVisitor for MarkerDrop {
    fn visit_block(&mut self, block: &mut TirBlock) {
        block.stmts.retain(|stmt| !is_cold_marker_stmt(stmt));
        self.walk_block(block);
    }

    /// A marker in value position, a `match` arm's whole body, is the `()` it
    /// evaluates to.
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if is_cold_marker_expr(expr) {
            expr.kind = TirExprKind::Unit;
        }
        self.walk_expr(expr);
    }
}
