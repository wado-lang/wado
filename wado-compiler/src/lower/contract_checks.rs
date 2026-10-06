//! Fold `builtin::contract_checks()` to the build's `-f contract-checks`
//! answer before NIR, so the optimizer sees a disabled check as a dead branch.

use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::tir::{TirExpr, TirExprKind};
use crate::tir_visitor::TirMutVisitor;

/// Replace every `builtin::contract_checks()` call in `flat` with
/// `contract_checks`.
pub fn lower(flat: &FlatPackage, contract_checks: bool) {
    flat.visit_bodies_mut(&mut ContractChecksLowering { contract_checks });
}

struct ContractChecksLowering {
    contract_checks: bool,
}

impl TirMutVisitor for ContractChecksLowering {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Call { func, .. } = &expr.kind
            && func.module_source == ModuleSource::builtin()
            && func.name == "contract_checks"
        {
            expr.kind = TirExprKind::BoolLiteral(self.contract_checks);
            return;
        }
        self.walk_expr(expr);
    }
}
