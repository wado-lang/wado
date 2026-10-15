//! Apply `-f contract-checks` to each contract check an `unchecked` twin
//! opens with, `if $contract_checks() { … }`, before the plan: its condition
//! becomes `true` where checks are on, and the statement is deleted outright
//! where they are off. Left as an `if false`, its body would count toward the
//! inliner's size estimate before any pass prunes it, and a disabled check
//! must cost nothing.

use crate::flat_package::FlatPackage;
use crate::name::CONTRACT_CHECKS_INTRINSIC;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;

/// Keep the contract checks in `flat` where `contract_checks`, and delete them
/// where not.
pub fn lower(flat: &FlatPackage, contract_checks: bool) {
    flat.visit_bodies_mut(&mut ContractChecksLowering { contract_checks });
}

struct ContractChecksLowering {
    contract_checks: bool,
}

fn is_contract_checks_call(expr: &TirExpr) -> bool {
    matches!(&expr.kind, TirExprKind::Call { func, .. } if func.is_builtin_named(CONTRACT_CHECKS_INTRINSIC))
}

/// The condition of `stmt` when it is a contract check.
fn check_condition(stmt: &mut TirStmt) -> Option<&mut TirExpr> {
    let TirStmtKind::If {
        condition,
        else_block: None,
        ..
    } = &mut stmt.kind
    else {
        return None;
    };
    is_contract_checks_call(condition).then_some(condition)
}

impl TirMutVisitor for ContractChecksLowering {
    fn visit_block(&mut self, block: &mut TirBlock) {
        let contract_checks = self.contract_checks;
        block.stmts.retain_mut(|stmt| match check_condition(stmt) {
            Some(condition) => {
                condition.kind = TirExprKind::BoolLiteral(true);
                contract_checks
            }
            None => true,
        });
        self.walk_block(block);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        assert!(
            !is_contract_checks_call(expr),
            "`{CONTRACT_CHECKS_INTRINSIC}()` outside the contract check reify opens a twin with"
        );
        self.walk_expr(expr);
    }
}
