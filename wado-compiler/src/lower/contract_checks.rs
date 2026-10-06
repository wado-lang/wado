//! Apply `-f contract-checks` to each `if builtin::contract_checks() { … }`
//! before the plan: its condition becomes `true` where checks are on, and the
//! statement is deleted outright where they are off. Left as an `if false`, its
//! body would count toward the inliner's size estimate before any pass prunes
//! it, and a disabled check must cost nothing.

use crate::flat_package::FlatPackage;
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
    matches!(&expr.kind, TirExprKind::Call { func, .. } if func.is_builtin_named("contract_checks"))
}

/// The condition of `stmt` when it is `if builtin::contract_checks() { … }`
/// with no `else`.
fn check_condition(stmt: &mut TirStmt) -> Option<&mut TirExpr> {
    let condition = match &mut stmt.kind {
        TirStmtKind::If {
            condition,
            else_block: None,
            ..
        } => condition,
        TirStmtKind::Expr(TirExpr {
            kind:
                TirExprKind::If {
                    condition,
                    else_branch: None,
                    ..
                },
            ..
        }) => condition.as_mut(),
        _ => return None,
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
        // Only the standard library can call it (`internal`), and only as a
        // check: elsewhere the folded `false` would cost what a check may not.
        assert!(
            !is_contract_checks_call(expr),
            "`builtin::contract_checks()` outside `if builtin::contract_checks() {{ … }}`"
        );
        self.walk_expr(expr);
    }
}
