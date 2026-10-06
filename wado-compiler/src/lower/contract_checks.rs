//! Fold `builtin::contract_checks()` to the build's `-f contract-checks`
//! answer before NIR. Where checks are off, an `if builtin::contract_checks()`
//! statement without an `else` is deleted outright: left as an `if false`, its
//! body counts toward the inliner's size estimate before any pass prunes it,
//! and a disabled check must cost nothing.

use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;

/// Replace every `builtin::contract_checks()` call in `flat` with
/// `contract_checks`, deleting the checks it guards when that is `false`.
pub fn lower(flat: &FlatPackage, contract_checks: bool) {
    flat.visit_bodies_mut(&mut ContractChecksLowering { contract_checks });
}

struct ContractChecksLowering {
    contract_checks: bool,
}

fn is_contract_checks_call(expr: &TirExpr) -> bool {
    matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source == ModuleSource::builtin() && func.name == "contract_checks")
}

/// Whether `stmt` is `if builtin::contract_checks() { … }` with no `else`.
fn is_check(stmt: &TirStmt) -> bool {
    match &stmt.kind {
        TirStmtKind::If {
            condition,
            else_block: None,
            ..
        } => is_contract_checks_call(condition),
        TirStmtKind::Expr(TirExpr {
            kind:
                TirExprKind::If {
                    condition,
                    else_branch: None,
                    ..
                },
            ..
        }) => is_contract_checks_call(condition),
        _ => false,
    }
}

impl TirMutVisitor for ContractChecksLowering {
    fn visit_block(&mut self, block: &mut TirBlock) {
        if !self.contract_checks {
            block.stmts.retain(|stmt| !is_check(stmt));
        }
        self.walk_block(block);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if is_contract_checks_call(expr) {
            expr.kind = TirExprKind::BoolLiteral(self.contract_checks);
            return;
        }
        self.walk_expr(expr);
    }
}
