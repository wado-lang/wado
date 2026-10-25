//! Drop every cold marker under `-f no-cold-paths`, the author's and the
//! compiler's alike, before any pass reads one, so the build shows what the
//! optimizer does with no marker to go on. `match_to_switch` places none
//! either.

use crate::flat_package::FlatPackage;
use crate::name::{COLD_PATH_INTRINSIC, KNOWN_COLD_PATH_INTRINSIC};
use crate::tir::{TirBlock, TirExprKind, TirStmt, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;

pub fn drop_markers(flat: &FlatPackage) {
    flat.visit_bodies_mut(&mut MarkerDrop);
}

struct MarkerDrop;

impl TirMutVisitor for MarkerDrop {
    fn visit_block(&mut self, block: &mut TirBlock) {
        block.stmts.retain(|stmt| !is_marker(stmt));
        self.walk_block(block);
    }
}

fn is_marker(stmt: &TirStmt) -> bool {
    matches!(&stmt.kind, TirStmtKind::Expr(expr)
        if matches!(&expr.kind, TirExprKind::Call { func, .. }
            if func.is_builtin_named(COLD_PATH_INTRINSIC)
                || func.is_builtin_named(KNOWN_COLD_PATH_INTRINSIC)))
}
